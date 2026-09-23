//! `jlo selfupdate`: the binary replaces itself from a GitHub release.
//!
//! This used to be six lines of shell that re-ran `install.sh` from
//! `refs/heads/main` over `curl | bash` - no version check, no verification of
//! jlo's own artifact, and an exit status that reported success on an empty
//! download. It is a Rust command now, reusing the pieces the JDK path already
//! has: the `ureq` stack, the streaming SHA256, and `extract`.
//!
//! The order below is not incidental, and each step exists because of a way
//! the obvious arrangement fails:
//!
//! 1. **Read the receipt.** It says whether this install is one jlo may touch
//!    at all - a local build or a package-manager install is not.
//! 2. **Resolve the tag once**, from the `/releases/latest` redirect. Both
//!    assets are then fetched from `/releases/download/<tag>/`. Discovery via
//!    `latest` followed by a download from `latest/download` are two separate
//!    resolutions, and a release published between them yields a tarball from
//!    one release and a checksum from another.
//! 3. **Compare against `CARGO_PKG_VERSION` and stop early** when there is
//!    nothing to do. Nothing to reload in that case: the wrapper gets the
//!    end marker alone, anyone else nothing.
//! 4. **Verify, then stage** - into a directory beside the target file, never
//!    `std::env::temp_dir()`, because `rename` is atomic only within one
//!    filesystem.
//! 5. **`exec` the *staged* binary** with the hidden install verb, which
//!    publishes it the way it publishes a binary `install.sh` staged: `rename`
//!    under the lock, then the layout. The running process carries its *own*
//!    `include_str!` templates, so a process that renames a new binary into
//!    place and then writes the shell files itself would install new code
//!    beside old scripts. There is exactly one generator and one publisher,
//!    and a new binary that cannot start publishes nothing.
//!
//! See `docs/adr/0006`. Two invariants are easy to break by accident and have
//! their own comments below: the lock fd must not carry `FD_CLOEXEC`, or the
//! lock is dropped at exactly the moment the new binary starts publishing; and
//! the base URL is injectable so the whole path is testable against mockito.

use crate::CommandError;
use crate::extract;
use crate::install::{self, Layout, Lock};
use crate::ui::{self, InstallUi};
use anyhow::{Context, Result, anyhow, bail};
use std::cmp::Ordering;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use ureq::Agent;

/// The release root. `JLO_RELEASE_API_URL` overrides it, mirroring
/// `JLO_ADOPTIUM_API_URL` (ADR-0002), which is what makes the whole path
/// testable offline - there is no spare release to point a test at.
pub(crate) const RELEASES_URL: &str = "https://github.com/java-loader/jlo/releases";

/// release-please tags the crate, not the repository, so the tag is
/// `jlo-bin-v0.3.0` and not a bare `v0.3.0`. A tag that does not carry this
/// prefix is an error rather than a guess: the version it yields decides
/// whether we overwrite the binary the user is running.
const TAG_PREFIX: &str = "jlo-bin-v";

const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Install methods `selfupdate` recognises. Anything else in the receipt stops
/// the update, for the same reason a malformed receipt does.
const METHOD_INSTALLER: &str = "installer";
const METHOD_LOCAL: &str = "local";
const METHOD_PACKAGE_MANAGER: &str = "package-manager";

// ---------------------------------------------------------------------------
// The remote
// ---------------------------------------------------------------------------

/// The single point of contact with the GitHub release host, the way
/// `AdoptiumClient` is for Adoptium. ADR-0002's "one network seam" is no
/// longer literally one, but the shape it asked for holds: one type per
/// remote, each with an injectable base URL and mockito coverage.
pub(crate) struct ReleaseClient {
    agent: Agent,
    base_url: String,
}

impl std::fmt::Debug for ReleaseClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReleaseClient")
            .field("base_url", &self.base_url)
            .finish_non_exhaustive()
    }
}

impl ReleaseClient {
    pub(crate) fn new(base_url: impl Into<String>) -> Self {
        Self {
            agent: crate::adoptium::agent(),
            base_url: base_url.into(),
        }
    }

    /// The tag of the latest release, read from the `Location` header of
    /// `/releases/latest`.
    ///
    /// Not the REST API: no token, no 60/hr unauthenticated rate limit to hit
    /// from a shared CI runner, no JSON contract to keep fixtures for, and it
    /// exercises the same host `install.sh` already trusts.
    ///
    /// `max_redirects(0)` is what makes that possible at all - ureq follows
    /// redirects by default, so without it the 302 is consumed and the
    /// `Location` header never reaches this code.
    fn latest_tag(&self) -> Result<String> {
        let url = format!("{}/latest", self.base_url);
        let response = self
            .agent
            .get(&url)
            .config()
            .max_redirects(0)
            .build()
            .call()
            .with_context(|| format!("could not reach {url}"))?;

        let status = response.status();
        if !status.is_redirection() {
            bail!("{url} did not redirect to the latest release: HTTP {status}");
        }
        let location = response
            .headers()
            .get("location")
            .and_then(|value| value.to_str().ok())
            .ok_or_else(|| anyhow!("{url} answered HTTP {status} without a Location header"))?;

        tag_from_location(location)
            .ok_or_else(|| anyhow!("could not read a release tag from {location:?}"))
    }

    fn asset_url(&self, tag: &str, name: &str) -> String {
        format!("{}/download/{tag}/{name}", self.base_url)
    }

    /// The published `<package>.sha256`, as bare lowercase hex.
    ///
    /// Unlike `install.sh`, a missing checksum is fatal here. That asymmetry
    /// is deliberate: the bootstrap has to cope with releases published before
    /// checksums existed, but `selfupdate` only ever asks for a release that
    /// is newer than the running binary, so every release it can reach has
    /// one. The artifact that *becomes jlo* is the last place to fail open.
    fn fetch_checksum(&self, tag: &str, package: &str) -> Result<String> {
        let url = self.asset_url(tag, &format!("{package}.sha256"));
        let mut response = self
            .agent
            .get(&url)
            .call()
            .with_context(|| format!("could not reach {url}"))?;
        if !response.status().is_success() {
            bail!(
                "could not download the checksum from {url}: HTTP {}",
                response.status()
            );
        }
        let body = response
            .body_mut()
            .read_to_string()
            .with_context(|| format!("could not read the checksum from {url}"))?;
        parse_checksum(&body).ok_or_else(|| anyhow!("{url} is not a SHA256 checksum file"))
    }

    /// Stream the tarball into `file`, hashing as it goes, and refuse it when
    /// the digest does not match - the same shape as the JDK download.
    fn download(
        &self,
        tag: &str,
        package: &str,
        expected: &str,
        file: &mut File,
        ui: &InstallUi,
    ) -> Result<()> {
        let url = self.asset_url(tag, package);
        let mut response = self
            .agent
            .get(&url)
            .call()
            .with_context(|| format!("could not reach {url}"))?;
        if !response.status().is_success() {
            bail!("could not download {url}: HTTP {}", response.status());
        }

        let hash = crate::adoptium::stream_hashed(response.body_mut(), file, ui)?;
        file.sync_all()
            .context("could not flush the downloaded package to disk")?;

        if hash != expected {
            bail!("checksum mismatch for {package}: expected {expected}, got {hash}");
        }
        Ok(())
    }
}

/// The last non-empty path segment of a redirect target, with any query or
/// fragment dropped. GitHub answers with an absolute URL today, but the header
/// is allowed to carry a relative reference, so this parses a path rather than
/// a URL.
fn tag_from_location(location: &str) -> Option<String> {
    let path = location
        .split(['?', '#'])
        .next()
        .unwrap_or(location)
        .trim_end_matches('/');
    let tag = path.rsplit('/').next()?;
    // "latest" back again means the redirect did not resolve to a release -
    // an empty repository, say. Treated as unreadable rather than as a tag.
    if tag.is_empty() || tag == "latest" {
        return None;
    }
    Some(tag.to_string())
}

/// `<hex>  <filename>` is what `shasum -a 256` writes, and the first line is
/// all of it that matters. Rejects anything that is not exactly 64 hex digits:
/// an empty or truncated expectation must never pass for a match.
fn parse_checksum(body: &str) -> Option<String> {
    let field = body.lines().next()?.split_whitespace().next()?;
    if field.len() != 64 || !field.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    Some(field.to_ascii_lowercase())
}

fn version_from_tag(tag: &str) -> Result<&str> {
    tag.strip_prefix(TAG_PREFIX)
        .filter(|version| !version.is_empty())
        .ok_or_else(|| {
            anyhow!(
                "unrecognised release tag {tag:?}: J'Lo's releases are tagged {TAG_PREFIX}<version>."
            )
        })
}

/// The release asset for this platform, matching the names the release
/// workflow builds. `uname -m` says `arm64` on Apple silicon where Rust says
/// `aarch64`, which is why this is a table and not a format string.
fn package_name() -> Result<String> {
    let platform = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => "linux-x86_64",
        ("linux", "aarch64") => "linux-aarch64",
        ("macos", "aarch64") => "macos-arm64",
        (os, arch) => bail!("J'Lo publishes no release build for {os}/{arch}."),
    };
    Ok(format!("jlo-{platform}.tar.gz"))
}

// ---------------------------------------------------------------------------
// The command
// ---------------------------------------------------------------------------

pub(crate) fn cmd_selfupdate(wrapped: bool) -> Result<(), CommandError> {
    let layout = Layout::new(crate::jlo_home_dir()?);
    let receipt = install::load_receipt(&layout).map_err(|e| {
        CommandError::with_hint(
            e,
            "Re-run the installer to rewrite it: \
             /bin/bash -c \"$(curl -fsSL https://raw.githubusercontent.com/java-loader/jlo/refs/heads/main/install.sh)\"",
        )
    })?;
    if let Some(receipt) = &receipt {
        check_owned(receipt, &layout)?;
    }
    // Resolved before the lock and before the network: a platform with no
    // published build has no update to look for, and saying so costs nothing.
    let package = package_name()?;

    let base_url =
        std::env::var("JLO_RELEASE_API_URL").unwrap_or_else(|_| RELEASES_URL.to_string());
    let client = ReleaseClient::new(base_url);

    // Held from here across the `exec`, so nothing else can publish underneath us
    // - the binary, the generated scripts and the receipt are three
    // filesystem writes and no `rename` makes them one transaction. The
    // install verb takes the same lock, so `install.sh`, `install-local.sh`
    // and the self-heal are all shut out for the duration.
    let lock = Lock::acquire(layout.home())?;

    let tag = client.latest_tag()?;
    let latest = version_from_tag(&tag)?;
    if !is_newer(latest, VERSION)? {
        ui::created!("{} is already the latest version.", ui::jlo_mark(VERSION));
        return Ok(crate::shellenv::emit(&[], wrapped)?);
    }

    eprintln!(
        "Updating {} {} {}",
        ui::jlo_mark(VERSION),
        ui::punctuation_arrow(),
        ui::jlo_mark_bare(latest)
    );
    let staged = stage(&client, &layout, &tag, latest, &package)?;

    // Re-read under the lock, immediately before handing over. A slower
    // updater carrying an older release must not overwrite a newer install
    // that landed while it was downloading - `install.sh` and
    // `install-local.sh` write the same layout and take no lock.
    guard_against_a_newer_install(&layout, latest)?;

    publish(&layout, lock, staged, wrapped)
}

/// Whether this install is one `selfupdate` may replace.
///
/// Each refusal names what it saw. Guessing is how a package-manager install
/// gets clobbered, and a local build is precisely the case where somebody does
/// not want a release quietly written over their work.
fn check_owned(receipt: &install::Receipt, layout: &Layout) -> Result<(), CommandError> {
    match receipt.method.as_str() {
        METHOD_INSTALLER => {}
        METHOD_LOCAL => {
            return Err(CommandError::with_hint(
                anyhow!("this J'Lo was installed from a local build, not from a release."),
                "Run ./install-local.sh again to replace it, or re-run the installer to switch back to releases.",
            ));
        }
        METHOD_PACKAGE_MANAGER => {
            return Err(CommandError::with_hint(
                anyhow!("this J'Lo was installed by a package manager."),
                "Update it the same way you installed it.",
            ));
        }
        other => {
            return Err(anyhow!(
                "{:?} records an install method J'Lo does not recognise: {other:?}.",
                layout.home().join("install-receipt.json")
            )
            .into());
        }
    }

    if !crate::store::same_path(Path::new(&receipt.jlo_home), layout.home()) {
        return Err(anyhow!(
            "the install receipt in {:?} describes a different JLO_HOME ({:?}).",
            layout.home(),
            receipt.jlo_home
        )
        .into());
    }

    // A receipt whose recorded path is not the executable running means jlo
    // was moved or copied. Updating would write a release over a path this
    // process does not occupy.
    if !install::is_current_exe(Path::new(&receipt.binary)) {
        return Err(CommandError::with_hint(
            anyhow!(
                "this executable is not the install described by the receipt, which names {:?}.",
                receipt.binary
            ),
            "Run the J'Lo that lives at that path, or re-run the installer.",
        ));
    }
    Ok(())
}

/// Stop rather than downgrade when someone else published in the meantime.
///
/// The receipt is the weaker of the two signals and is checked here only for
/// completeness. The strong one is `CARGO_PKG_VERSION`, compared before the
/// download: `check_owned` has already established that this executable *is*
/// the installed binary, so `VERSION` is what is on disk whatever the receipt
/// happens to say - including in the incomplete-install state where a crashed
/// updater left a newer binary behind an older receipt.
///
/// What this re-read still catches is the case the ADR names: a publication
/// that finished while we were downloading. Since 0.4.0 that publication has
/// to go through the install verb, which takes the same lock we are holding,
/// so the remaining window is a writer that predates this release.
fn guard_against_a_newer_install(layout: &Layout, latest: &str) -> Result<()> {
    let Some(receipt) = install::load_receipt(layout)? else {
        return Ok(());
    };
    if is_newer(&receipt.version, latest)? {
        bail!(
            "J'Lo {} was installed while this update was downloading; \
             not replacing it with {latest}.",
            receipt.version
        );
    }
    Ok(())
}

fn is_newer(candidate: &str, current: &str) -> Result<bool> {
    let ordering = crate::version::compare(candidate, current).with_context(|| {
        format!("could not compare J'Lo versions {candidate:?} and {current:?}")
    })?;
    Ok(ordering == Ordering::Greater)
}

// ---------------------------------------------------------------------------
// Staging
// ---------------------------------------------------------------------------

/// A staging directory beside the target file, removed on drop.
///
/// Beside it, and not under `std::env::temp_dir()`: `rename` is atomic only
/// within one filesystem, and it fails with `EXDEV` rather than degrading.
/// "Somewhere under `$JLO_HOME`" is not enough either, because `bin/` can
/// itself be a mount point or a symlink.
///
/// The drop covers every failure up to and including an `exec` that returns.
/// A successful `exec` runs no destructors; from there the staged binary's own
/// install verb removes the directory.
#[derive(Debug)]
struct Staged {
    dir: PathBuf,
    binary: PathBuf,
}

impl Drop for Staged {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn stage(
    client: &ReleaseClient,
    layout: &Layout,
    tag: &str,
    latest: &str,
    package: &str,
) -> Result<Staged> {
    // A staged binary that verified but then died before its install verb
    // could clean up leaves a whole copy of J'Lo here, and nothing else ever
    // comes back for it: `install.sh` sweeps only its own directory.
    layout.sweep_stale_staging();
    let dir = layout.staging_dir(std::process::id());
    // A directory left behind by a killed run would otherwise make the unpack
    // below read as a success with stale contents.
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).with_context(|| format!("could not create {dir:?}"))?;
    let staged = Staged {
        binary: dir.join("jlo-bin"),
        dir,
    };

    let ui = InstallUi::jlo(latest);
    let result = fetch_and_unpack(client, &staged, tag, package, &ui);
    if result.is_err() {
        ui.abandon();
    }
    result?;

    // Executable first, then run it. `make_executable` exists for an archive
    // that did not carry the x bit, and running the binary is the one step
    // that needs it - in the other order the repair never got the chance, and
    // such an archive failed with "could not run the downloaded jlo" instead.
    make_executable(&staged.binary)?;
    verify_staged_version(&staged.binary, latest)?;
    Ok(staged)
}

fn fetch_and_unpack(
    client: &ReleaseClient,
    staged: &Staged,
    tag: &str,
    package: &str,
    ui: &InstallUi,
) -> Result<()> {
    let expected = client.fetch_checksum(tag, package)?;
    let archive = staged.dir.join(package);
    let mut file =
        File::create(&archive).with_context(|| format!("could not create {archive:?}"))?;
    client.download(tag, package, &expected, &mut file, ui)?;
    drop(file);

    extract::extract(&archive, &staged.dir, ui)?;
    // The install verb that takes over this directory removes the binary and
    // then the directory only if that emptied it, never recursively. A
    // tarball left in here would keep it standing after every update; failing
    // to remove it costs that directory and nothing else.
    let _ = fs::remove_file(&archive);
    if !staged.binary.is_file() {
        bail!("the release archive {package} did not contain a jlo-bin.");
    }
    ui.abandon();
    Ok(())
}

/// The last check before the swap: the binary we are about to publish must
/// actually be the version the tag promised.
///
/// Running it is safe by this point - the archive it came out of matched its
/// published SHA256 - and it is the only thing that can catch a release whose
/// assets do not match its tag.
fn verify_staged_version(binary: &Path, expected: &str) -> Result<()> {
    let output = std::process::Command::new(binary)
        .arg("--version")
        .output()
        .with_context(|| format!("could not run the downloaded {binary:?}"))?;
    if !output.status.success() {
        bail!("the downloaded J'Lo binary failed to report its version.");
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let reported = stdout.split_whitespace().next_back().unwrap_or_default();
    if reported != expected {
        bail!("the release tagged {TAG_PREFIX}{expected} contains J'Lo {reported}.");
    }
    Ok(())
}

fn make_executable(binary: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    fs::set_permissions(binary, fs::Permissions::from_mode(0o755))
        .with_context(|| format!("could not make {binary:?} executable"))
}

// ---------------------------------------------------------------------------
// Publication
// ---------------------------------------------------------------------------

/// Hand over to the staged binary, which publishes itself.
///
/// `exec`, not a child process: the new `jlo-bin` carries its own shell
/// templates and its own completions, so it is the only thing that can write a
/// layout guaranteed to match it. `--publish-self` is what makes it rename
/// itself into place - flushed, under the lock, and only once it is running -
/// so this process never replaces the binary itself. `--reload` is what makes
/// it print the `. jlo.sh` line on stdout for the wrapper to eval, and the
/// forwarded wrapped mode is what makes it end that with the marker the
/// wrapper looks for.
///
/// `lock` and `staged` stay alive until the call - `exec` replaces the process
/// image and runs no destructors, so the fd (and the lock on it) carries into
/// the new program, and the staged binary is still there to be run.
fn publish(layout: &Layout, lock: Lock, staged: Staged, wrapped: bool) -> Result<(), CommandError> {
    use std::os::unix::process::CommandExt as _;

    let mut command = std::process::Command::new(&staged.binary);
    if wrapped {
        command.arg(crate::shellenv::WRAPPED);
    }
    let error = command
        .arg(install::VERB)
        .arg("--publish-self")
        .arg("--reload")
        // We hold the lock; the fd carries across the exec, so the new
        // process must not try to take it again from a second open file
        // description - that would deadlock it against its own parent's lock.
        .arg("--locked")
        // Explicit rather than inherited: this process may have resolved
        // $JLO_HOME from $HOME, and the two must not disagree across the exec.
        .env("JLO_HOME", layout.home())
        .exec();
    let binary = staged.binary.clone();
    // The staging directory goes while the lock is still held.
    drop(staged);
    drop(lock);
    Err(CommandError::with_hint(
        anyhow!("the new binary {binary:?} could not be started: {error}."),
        "J'Lo was not changed; the binary you are running is still in place.",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tag_yields_its_version() {
        assert_eq!(
            version_from_tag("jlo-bin-v0.4.0").expect("version"),
            "0.4.0"
        );
    }

    /// The tag format is a contract with release-please, and reading it wrong
    /// decides whether the running binary gets overwritten. Fail loudly.
    #[test]
    fn an_unknown_tag_format_is_an_error_not_a_guess() {
        for tag in ["v0.4.0", "0.4.0", "jlo-v0.4.0", "jlo-bin-v"] {
            let err = version_from_tag(tag).expect_err(tag);
            assert!(
                format!("{err:#}").contains("unrecognised release tag"),
                "{tag}: {err:#}"
            );
        }
    }

    #[test]
    fn a_location_header_yields_the_tag() {
        assert_eq!(
            tag_from_location("https://github.com/java-loader/jlo/releases/tag/jlo-bin-v0.4.0")
                .as_deref(),
            Some("jlo-bin-v0.4.0")
        );
        assert_eq!(
            tag_from_location("/java-loader/jlo/releases/tag/jlo-bin-v0.4.0?x=1").as_deref(),
            Some("jlo-bin-v0.4.0")
        );
    }

    #[test]
    fn a_location_that_names_no_release_is_unreadable() {
        assert_eq!(
            tag_from_location("https://github.com/x/y/releases/latest"),
            None
        );
        assert_eq!(tag_from_location("/"), None);
    }

    #[test]
    fn a_checksum_file_yields_bare_lowercase_hex() {
        let hex = "a".repeat(64);
        assert_eq!(
            parse_checksum(&format!("{hex}  jlo-linux-x86_64.tar.gz\n")).as_deref(),
            Some(hex.as_str())
        );
        assert_eq!(
            parse_checksum(&format!("{}  x\n", "A".repeat(64))).as_deref(),
            Some(hex.as_str())
        );
    }

    /// An empty or truncated expectation must never pass for a match - the
    /// same rule `install.sh` enforces with its glob and its `tr`.
    #[test]
    fn a_malformed_checksum_file_is_refused() {
        for body in [
            "",
            "\n",
            "not-a-checksum  x\n",
            &format!("{}  x\n", "a".repeat(63)),
            &format!("{}  x\n", "a".repeat(65)),
            &format!("{}zz  x\n", "a".repeat(62)),
        ] {
            assert!(parse_checksum(body).is_none(), "accepted {body:?}");
        }
    }

    #[test]
    fn version_comparison_is_semver_not_string() {
        assert!(is_newer("0.10.0", "0.9.0").expect("compare"));
        assert!(!is_newer("0.4.0", "0.4.0").expect("compare"));
        assert!(!is_newer("0.3.0", "0.4.0").expect("compare"));
    }
}

#[cfg(test)]
mod http_tests {
    use super::*;
    use sha2::Digest as _;

    /// The whole discovery step depends on *not* following the redirect. If a
    /// future ureq upgrade changes the default, or the per-request override is
    /// dropped, this is what notices.
    #[test]
    fn latest_tag_does_not_follow_the_redirect() {
        let mut server = mockito::Server::new();
        let redirect = server
            .mock("GET", "/latest")
            .with_status(302)
            .with_header("location", &format!("{}/tag/jlo-bin-v1.2.3", server.url()))
            .create();
        let target = server
            .mock("GET", "/tag/jlo-bin-v1.2.3")
            .with_status(200)
            .with_body("the release page")
            .expect(0)
            .create();

        let client = ReleaseClient::new(server.url());
        assert_eq!(client.latest_tag().expect("tag"), "jlo-bin-v1.2.3");
        redirect.assert();
        target.assert();
    }

    #[test]
    fn latest_tag_reports_a_non_redirect() {
        let mut server = mockito::Server::new();
        let _mock = server.mock("GET", "/latest").with_status(404).create();

        let client = ReleaseClient::new(server.url());
        let err = client.latest_tag().expect_err("404");
        assert!(format!("{err:#}").contains("HTTP 404"), "{err:#}");
    }

    #[test]
    fn latest_tag_reports_a_redirect_without_a_location() {
        let mut server = mockito::Server::new();
        let _mock = server.mock("GET", "/latest").with_status(302).create();

        let client = ReleaseClient::new(server.url());
        let err = client.latest_tag().expect_err("no location");
        assert!(format!("{err:#}").contains("Location"), "{err:#}");
    }

    fn sha256(bytes: &[u8]) -> String {
        hex::encode(sha2::Sha256::digest(bytes))
    }

    #[test]
    fn download_writes_a_verified_file() {
        let mut server = mockito::Server::new();
        let body = b"a release tarball".to_vec();
        let _sum = server
            .mock("GET", "/download/jlo-bin-v1.0.0/pkg.tar.gz.sha256")
            .with_body(format!("{}  pkg.tar.gz\n", sha256(&body)))
            .create();
        let _pkg = server
            .mock("GET", "/download/jlo-bin-v1.0.0/pkg.tar.gz")
            .with_body(body.clone())
            .create();

        let client = ReleaseClient::new(server.url());
        let expected = client
            .fetch_checksum("jlo-bin-v1.0.0", "pkg.tar.gz")
            .expect("checksum");
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("pkg.tar.gz");
        let mut file = File::create(&path).expect("create");
        client
            .download(
                "jlo-bin-v1.0.0",
                "pkg.tar.gz",
                &expected,
                &mut file,
                &InstallUi::hidden("1.0.0"),
            )
            .expect("download");
        assert_eq!(fs::read(&path).expect("read"), body);
    }

    #[test]
    fn download_refuses_a_checksum_mismatch() {
        let mut server = mockito::Server::new();
        let _pkg = server
            .mock("GET", "/download/jlo-bin-v1.0.0/pkg.tar.gz")
            .with_body("something else")
            .create();

        let client = ReleaseClient::new(server.url());
        let dir = tempfile::tempdir().expect("tempdir");
        let mut file = File::create(dir.path().join("pkg.tar.gz")).expect("create");
        let err = client
            .download(
                "jlo-bin-v1.0.0",
                "pkg.tar.gz",
                &sha256(b"the real thing"),
                &mut file,
                &InstallUi::hidden("1.0.0"),
            )
            .expect_err("mismatch");
        assert!(format!("{err:#}").contains("checksum mismatch"), "{err:#}");
    }

    /// A release with no published checksum is refused rather than installed,
    /// unlike the bootstrap - see `fetch_checksum`.
    #[test]
    fn a_missing_checksum_stops_the_update() {
        let mut server = mockito::Server::new();
        let _sum = server
            .mock("GET", "/download/jlo-bin-v1.0.0/pkg.tar.gz.sha256")
            .with_status(404)
            .create();

        let client = ReleaseClient::new(server.url());
        let err = client
            .fetch_checksum("jlo-bin-v1.0.0", "pkg.tar.gz")
            .expect_err("404");
        assert!(format!("{err:#}").contains("HTTP 404"), "{err:#}");
    }

    #[test]
    fn a_malformed_checksum_asset_stops_the_update() {
        let mut server = mockito::Server::new();
        let _sum = server
            .mock("GET", "/download/jlo-bin-v1.0.0/pkg.tar.gz.sha256")
            .with_body("\n")
            .create();

        let client = ReleaseClient::new(server.url());
        let err = client
            .fetch_checksum("jlo-bin-v1.0.0", "pkg.tar.gz")
            .expect_err("malformed");
        assert!(
            format!("{err:#}").contains("not a SHA256 checksum file"),
            "{err:#}"
        );
    }
}
