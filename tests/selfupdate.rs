//! End-to-end tests for `jlo selfupdate` against a `mockito` release host.
//!
//! `JLO_RELEASE_API_URL` stands in for `github.com/.../releases`: the
//! `/latest` redirect, the tag's `install.sh`, and the tarball and checksum
//! that script fetches. The script served is the repo's own `install.sh`, run
//! by the real `sh` and `curl`, so the path under test is the one a user takes.
//!
//! Each test builds a real `$JLO_HOME` with a copy of the binary under test at
//! `bin/jlo-bin` and runs it from there: `selfupdate` refuses any other
//! executable.

// Test code: an `unwrap` failure here is a test failure, which is the point.
#![allow(clippy::unwrap_used)]

mod common;

use common::{
    INTERPRETERS, fake_jdk_archive, hermetic, install_fake_jdk, jdk_entry, jlo_bin, offer, shells,
    squote,
};
use sha2::{Digest, Sha256};
use std::fs;
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

/// A tag no real release will ever carry, so "is it newer?" has an
/// unambiguous answer whatever the crate version is.
const TAG: &str = "jlo-bin-v99.9.9";
/// `TAG`'s version, as the hint names it.
const NEWER: &str = "99.9.9";
const VERSION: &str = env!("CARGO_PKG_VERSION");

fn current_tag() -> String {
    format!("jlo-bin-v{VERSION}")
}

/// What the release workflow names the asset for this platform - and what
/// `install.sh` derives from `uname`.
fn package() -> &'static str {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => "jlo-linux-x86_64.tar.gz",
        ("linux", "aarch64") => "jlo-linux-aarch64.tar.gz",
        ("macos", "aarch64") => "jlo-macos-arm64.tar.gz",
        (os, arch) => panic!("no release asset for {os}/{arch}"),
    }
}

/// The repo's `install.sh`, as a release serves it.
fn installer() -> String {
    fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("install.sh")).unwrap()
}

/// A `$JLO_HOME` - also the test's `$HOME` - holding a copy of the binary under
/// test at `bin/jlo-bin`.
struct Install {
    home: tempfile::TempDir,
}

impl Install {
    fn new() -> Self {
        let home = tempfile::tempdir().unwrap();
        let bin = home.path().join("bin");
        fs::create_dir_all(&bin).unwrap();
        let target = bin.join("jlo-bin");
        // Copied by a child `cp`, not `fs::copy`: a write fd on `target` held
        // by this process leaks into any child another test thread forks at
        // that moment, and exec'ing `target` then fails with ETXTBSY.
        let copied = hermetic("cp", home.path())
            .arg(jlo_bin())
            .arg(&target)
            .status()
            .unwrap();
        assert!(copied.success(), "cp of the binary under test failed");
        fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).unwrap();
        Self { home }
    }

    fn path(&self) -> &Path {
        self.home.path()
    }

    fn binary(&self) -> PathBuf {
        self.path().join("bin").join("jlo-bin")
    }

    /// The installed binary, with the release host at `release_url`.
    fn command(&self, release_url: &str) -> Command {
        let mut cmd = hermetic(self.binary(), self.path());
        cmd.env("JLO_HOME", self.path())
            .env("JLO_RELEASE_API_URL", release_url);
        cmd
    }

    fn run(&self, args: &[&str], release_url: &str) -> Output {
        self.command(release_url).args(args).output().unwrap()
    }

    fn selfupdate(&self, release_url: &str) -> Output {
        self.run(&["selfupdate"], release_url)
    }

    fn wrapped_selfupdate(&self, release_url: &str) -> Output {
        self.run(&["__wrapped", "selfupdate"], release_url)
    }

    /// Writes the layout the way an earlier install left it.
    fn write_layout(&self) {
        let out = self.run(&["__install"], "http://127.0.0.1:1");
        assert!(out.status.success(), "{out:?}");
    }
}

/// The release tarball as the workflow builds it: one entry, `jlo-bin`.
fn tarball(binary: &[u8]) -> Vec<u8> {
    let mut header = tar::Header::new_gnu();
    header.set_path("jlo-bin").unwrap();
    header.set_size(binary.len() as u64);
    header.set_mode(0o755);
    header.set_cksum();
    let mut tar = tar::Builder::new(Vec::new());
    tar.append(&header, binary).unwrap();
    let raw = tar.into_inner().unwrap();
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    std::io::Write::write_all(&mut gz, &raw).unwrap();
    gz.finish().unwrap()
}

/// A release host whose `/latest` redirects to one tag. Every mock states how
/// often it is hit, and [`Release::assert`] checks all of them.
struct Release {
    server: mockito::ServerGuard,
    mocks: Vec<mockito::Mock>,
}

impl Release {
    fn at(tag: &str, lookups: usize) -> Self {
        let mut server = mockito::Server::new();
        let location = format!("{}/tag/{tag}", server.url());
        let latest = server
            .mock("GET", "/latest")
            .with_status(302)
            .with_header("location", &location)
            .expect(lookups)
            .create();
        Self {
            server,
            mocks: vec![latest],
        }
    }

    /// `script` as `tag`'s `install.sh`.
    fn script(mut self, tag: &str, script: &str, hits: usize) -> Self {
        let mock = self
            .server
            .mock("GET", format!("/download/{tag}/install.sh").as_str())
            .with_body(script)
            .expect(hits)
            .create();
        self.mocks.push(mock);
        self
    }

    /// `tag`'s `install.sh` answering 404.
    fn no_script(mut self, tag: &str) -> Self {
        let mock = self
            .server
            .mock("GET", format!("/download/{tag}/install.sh").as_str())
            .with_status(404)
            .expect(1)
            .create();
        self.mocks.push(mock);
        self
    }

    /// The binary under test as `tag`'s tarball, with its checksum.
    fn package(mut self, tag: &str, hits: usize) -> Self {
        let archive = tarball(&fs::read(jlo_bin()).unwrap());
        let sum = hex::encode(Sha256::digest(&archive));
        let base = format!("/download/{tag}/{}", package());
        let tarball = self
            .server
            .mock("GET", base.as_str())
            .with_body(archive)
            .expect(hits)
            .create();
        let checksum = self
            .server
            .mock("GET", format!("{base}.sha256").as_str())
            .with_body(format!("{sum}  {}\n", package()))
            .expect(hits)
            .create();
        self.mocks.extend([tarball, checksum]);
        self
    }

    /// The real installer and package for `tag`, each fetched `hits` times.
    fn serving(tag: &str, lookups: usize, hits: usize) -> Self {
        Self::at(tag, lookups)
            .script(tag, &installer(), hits)
            .package(tag, hits)
    }

    fn url(&self) -> String {
        self.server.url()
    }

    fn assert(&self) {
        for mock in &self.mocks {
            mock.assert();
        }
    }
}

/// The staging directories under `bin/`, by the name the install verb sweeps.
fn staging_leftovers(install: &Install) -> Vec<PathBuf> {
    fs::read_dir(install.path().join("bin"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with(".jlo-install-"))
        })
        .collect()
}

/// stdout must be the reload block and nothing else: a substring check would
/// let any amount of pollution into what the wrapper evaluates.
fn assert_reload_lines(stdout: &str, home: &Path, wrapped: bool) {
    let home = home.to_string_lossy();
    let joiner = if wrapped { " &&" } else { "" };
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), if wrapped { 4 } else { 3 }, "{stdout:?}");
    assert_eq!(lines[0], format!(". '{home}/jlo.sh'{joiner}"));
    assert!(
        lines[1].contains("_JLO_AUTOLOAD")
            && lines[1].ends_with(&format!("autoload.sh'; fi{joiner}")),
        "{stdout:?}"
    );
    assert!(
        lines[2].contains("_JLO_COMPLETIONS") && lines[2].ends_with("completions.sh'; fi"),
        "{stdout:?}"
    );
    if wrapped {
        assert_eq!(lines[3], "# jlo'end", "{stdout:?}");
    }
}

// ---------------------------------------------------------------------------
// A newer release
// ---------------------------------------------------------------------------

/// The whole path: the tag from the redirect, that tag's `install.sh`, and the
/// tarball and checksum from that same tag - `/latest/download` is not served,
/// so a script that resolved `latest` on its own would fail. The script's
/// stdout goes to stderr; stdout carries the reload block alone.
#[test]
fn a_newer_release_runs_its_own_installer_pinned_to_the_tag() {
    let install = Install::new();
    let before = fs::metadata(install.binary()).unwrap().ino();
    let script = format!("echo noise-on-stdout\n{}", installer());
    let release = Release::at(TAG, 1).script(TAG, &script, 1).package(TAG, 1);

    let out = install.selfupdate(&release.url());
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stdout:?} {stderr:?}");

    assert_reload_lines(&stdout, install.path(), false);
    assert!(stderr.contains("noise-on-stdout"), "{stderr:?}");
    assert!(
        stderr.contains("installed to"),
        "the installer's report is missing: {stderr:?}"
    );
    assert_ne!(
        fs::metadata(install.binary()).unwrap().ino(),
        before,
        "the installer did not publish a new binary"
    );
    assert!(staging_leftovers(&install).is_empty());
    release.assert();
}

/// Wrapped, the reload block ends in the marker the wrapper looks for.
#[test]
fn a_wrapped_update_ends_its_reload_with_the_marker() {
    let install = Install::new();
    let release = Release::serving(TAG, 1, 1);

    let out = install.wrapped_selfupdate(&release.url());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{:?}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_reload_lines(&stdout, install.path(), true);
    release.assert();
}

/// A failing installer is the command's failure, and nothing is reloaded -
/// for the wrapper there is no payload at all, so nothing is evaluated.
#[test]
fn a_failing_installer_fails_the_update_and_reloads_nothing() {
    let install = Install::new();
    let before = fs::read(install.binary()).unwrap();
    let release = Release::at(TAG, 2).script(TAG, "echo half-way >&2\nexit 3\n", 2);

    for out in [
        install.selfupdate(&release.url()),
        install.wrapped_selfupdate(&release.url()),
    ] {
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(1), "{stderr:?}");
        assert_eq!(String::from_utf8_lossy(&out.stdout), "", "{stderr:?}");
        assert!(stderr.contains("half-way"), "{stderr:?}");
        assert!(
            stderr.contains("install.sh"),
            "no way out named: {stderr:?}"
        );
    }
    assert_eq!(fs::read(install.binary()).unwrap(), before);
    release.assert();
}

/// An HTTP error on the script is an error, never an empty script run.
#[test]
fn an_http_error_on_the_installer_runs_nothing() {
    let install = Install::new();
    let release = Release::at(TAG, 1).no_script(TAG).package(TAG, 0);

    let out = install.selfupdate(&release.url());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{stderr:?}");
    assert!(stderr.contains("HTTP 404"), "{stderr:?}");
    assert!(
        stderr.contains("Run the installer directly"),
        "no way out named: {stderr:?}"
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout), "");
    release.assert();
}

/// The tag format is a contract with release-please; an unknown one is an
/// error, not a guess about what is on the other end.
#[test]
fn an_unrecognised_tag_stops_the_update() {
    let install = Install::new();
    let release = Release::serving("v99.9.9", 1, 0);

    let out = install.selfupdate(&release.url());
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("unrecognised release tag"));
    release.assert();
}

// ---------------------------------------------------------------------------
// Already current: the repair
// ---------------------------------------------------------------------------

/// Nothing downloaded; the shell files come back and the reload is printed,
/// because the stale piece may be the wrapper resident in the calling shell.
#[test]
fn a_current_install_rewrites_its_shell_files_and_reloads() {
    let install = Install::new();
    install.write_layout();
    let release = Release::serving(&current_tag(), 2, 0);

    for wrapped in [false, true] {
        fs::remove_file(install.path().join("bin").join("jlo-init.sh")).unwrap();
        let out = if wrapped {
            install.wrapped_selfupdate(&release.url())
        } else {
            install.selfupdate(&release.url())
        };
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success(), "{stderr:?}");
        assert!(stderr.contains("already the latest version"), "{stderr:?}");
        assert_reload_lines(
            &String::from_utf8_lossy(&out.stdout),
            install.path(),
            wrapped,
        );
        assert!(install.path().join("bin").join("jlo-init.sh").is_file());
    }
    release.assert();
}

/// A published release older than this binary - a local build installed into
/// `$JLO_HOME/bin` - is not an update: no download, no downgrade.
#[test]
fn an_older_latest_release_is_not_a_downgrade() {
    let install = Install::new();
    let release = Release::serving("jlo-bin-v0.0.1", 1, 0);

    let out = install.selfupdate(&release.url());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stderr:?}");
    assert!(stderr.contains("already the latest version"), "{stderr:?}");
    release.assert();
}

/// A required file that cannot be written fails the repair, and nothing is
/// reloaded; once the path is writable again, a retry succeeds.
#[test]
fn a_failed_required_write_fails_the_repair_and_reloads_nothing() {
    let install = Install::new();
    install.write_layout();
    let release = Release::serving(&current_tag(), 3, 0);
    let jlo_sh = install.path().join("jlo.sh");
    // A non-empty directory where the file belongs: the rename onto it fails.
    fs::remove_file(&jlo_sh).unwrap();
    fs::create_dir(&jlo_sh).unwrap();
    fs::write(jlo_sh.join("keep"), "").unwrap();

    for out in [
        install.selfupdate(&release.url()),
        install.wrapped_selfupdate(&release.url()),
    ] {
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(1), "{stderr:?}");
        assert_eq!(
            String::from_utf8_lossy(&out.stdout),
            "",
            "reloaded anyway: {stderr:?}"
        );
        assert!(stderr.contains("jlo.sh"), "{stderr:?}");
    }

    fs::remove_dir_all(&jlo_sh).unwrap();
    let out = install.selfupdate(&release.url());
    assert!(
        out.status.success(),
        "{:?}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(jlo_sh.is_file());
    release.assert();
}

/// The repair follows the install verb's symlink rules: an owned link keeps
/// its inode, a missing one comes back, anything else is left alone.
#[test]
fn the_repair_keeps_the_symlink_rules() {
    let install = Install::new();
    install.write_layout();
    let release = Release::serving(&current_tag(), 3, 0);
    let link = install.path().join(".local").join("bin").join("jlo");
    let identity = |p: &Path| {
        let m = fs::symlink_metadata(p).unwrap();
        (m.ino(), m.dev())
    };

    let owned = identity(&link);
    assert!(install.selfupdate(&release.url()).status.success());
    assert_eq!(identity(&link), owned, "an owned link was recreated");

    fs::remove_file(&link).unwrap();
    assert!(install.selfupdate(&release.url()).status.success());
    assert_eq!(fs::read_link(&link).unwrap(), install.binary());

    fs::remove_file(&link).unwrap();
    fs::write(&link, "not ours\n").unwrap();
    let out = install.selfupdate(&release.url());
    assert!(out.status.success());
    assert_eq!(fs::read_to_string(&link).unwrap(), "not ours\n");
    assert!(String::from_utf8_lossy(&out.stderr).contains("not managed by J'Lo"));
    release.assert();
}

// ---------------------------------------------------------------------------
// Not ours
// ---------------------------------------------------------------------------

/// A J'Lo that is not `$JLO_HOME/bin/jlo-bin` - a build in `target/`, a
/// Homebrew keg - is refused before anything is asked or written.
#[test]
fn a_jlo_that_is_not_the_installed_one_is_refused() {
    let home = tempfile::tempdir().unwrap();
    let jlo_home = tempfile::tempdir().unwrap();
    let release = Release::serving(TAG, 0, 0);

    let out = hermetic(jlo_bin(), home.path())
        .arg("selfupdate")
        .env("JLO_HOME", jlo_home.path())
        .env("JLO_RELEASE_API_URL", release.url())
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{stderr:?}");
    assert!(stderr.contains("not the one installed at"), "{stderr:?}");
    assert!(
        stderr.contains("brew upgrade"),
        "no way out named: {stderr:?}"
    );
    assert_eq!(fs::read_dir(home.path()).unwrap().count(), 0);
    assert_eq!(fs::read_dir(jlo_home.path()).unwrap().count(), 0);
    release.assert();
}

// ---------------------------------------------------------------------------
// The calling shell
// ---------------------------------------------------------------------------

/// Through the shell function, both branches reload the calling shell. `jlo.sh`
/// defines `jlo`, so the update runs from a renamed copy while a stand-in holds
/// the name: the stand-in survives unless the reload sourced `jlo.sh` again.
#[test]
fn selfupdate_reloads_the_calling_shell_whether_it_upgrades_or_repairs() {
    for sh in shells("selfupdate_reloads_the_calling_shell", INTERPRETERS) {
        for (tag, hits) in [(TAG.to_string(), 1), (current_tag(), 0)] {
            let install = Install::new();
            install.write_layout();
            let release = Release::serving(&tag, 1, hits);

            let out = hermetic(sh, install.path())
                .arg("-c")
                .arg(
                    r#". "$JLO_HOME/jlo.sh"
                    eval "_jlo_old$(typeset -f jlo | sed '1s/^jlo//')"
                    jlo() { echo stale; }
                    rm "$JLO_HOME/bin/jlo-init.sh"
                    _jlo_old selfupdate
                    echo "status=$?"
                    if typeset -f jlo | grep -q stale; then echo reloaded=no; else echo reloaded=yes; fi"#,
                )
                .env("JLO_HOME", install.path())
                .env("JLO_RELEASE_API_URL", release.url())
                .output()
                .unwrap();
            let stdout = String::from_utf8_lossy(&out.stdout);
            let ctx = format!(
                "{sh} {tag}: {stdout:?} {:?}",
                String::from_utf8_lossy(&out.stderr)
            );
            assert!(stdout.contains("status=0"), "{ctx}");
            assert!(stdout.contains("reloaded=yes"), "{ctx}");
            assert!(
                install.path().join("bin").join("jlo-init.sh").is_file(),
                "{ctx}"
            );
            release.assert();
        }
    }
}

// ---------------------------------------------------------------------------
// The reload line
// ---------------------------------------------------------------------------

/// The reload line `jlo selfupdate` prints, evaluated in a shell that sourced
/// exactly `enabled` of the optional stubs. Reports which of the three files
/// the eval actually re-sourced.
///
/// Sourcing is observed by shadowing `.` once the opt-in sourcing is done, so
/// each file the eval'd line sources is echoed rather than run.
fn reload_in(sh: &str, install: &Install, release_url: &str, enabled: &[&str]) -> Output {
    let mut sources = String::new();
    for name in std::iter::once("jlo.sh").chain(enabled.iter().copied()) {
        sources.push_str(". ");
        sources.push_str(&squote(install.path().join(name)));
        sources.push('\n');
    }
    let reload = install.selfupdate(release_url);
    assert!(reload.status.success(), "{reload:?}");
    let line = String::from_utf8_lossy(&reload.stdout).into_owned();

    // `.` is shadowed *after* the opt-in sourcing above, so it only records
    // what the eval'd reload line does.
    hermetic(sh, install.path())
        .arg("-c")
        .arg(format!(
            "{sources}\
             . () {{ echo \"sourced=$1\"; }}\n\
             eval {}\n\
             echo \"status=$?\"\n",
            squote(Path::new(&line))
        ))
        .output()
        .unwrap()
}

/// The reload always re-sources `jlo.sh` - that is the resident wrapper being
/// replaced - and never enables an optional stub the user had not enabled.
#[test]
fn the_reload_line_re_sources_only_what_this_shell_had_enabled() {
    let install = Install::new();
    install.write_layout();
    let interpreters = shells(
        "the_reload_line_re_sources_only_what_this_shell_had_enabled",
        INTERPRETERS,
    )
    .collect::<Vec<_>>();
    let release = Release::at(&current_tag(), 2 * interpreters.len());

    for sh in interpreters {
        let bare = reload_in(sh, &install, &release.url(), &[]);
        let stdout = String::from_utf8_lossy(&bare.stdout);
        assert!(
            stdout.contains("sourced=") && stdout.contains("jlo.sh"),
            "{sh}: the reload did not re-source jlo.sh: {stdout:?}"
        );
        assert!(
            !stdout.contains("autoload.sh") && !stdout.contains("completions.sh"),
            "{sh}: the reload enabled a stub the user had not: {stdout:?}"
        );
        // A false `[ -n ... ]` must not become the status of the whole eval.
        assert!(
            stdout.contains("status=0"),
            "{sh}: a successful reload reported failure: {stdout:?}"
        );

        let opted_in = reload_in(
            sh,
            &install,
            &release.url(),
            &["autoload.sh", "completions.sh"],
        );
        let stdout = String::from_utf8_lossy(&opted_in.stdout);
        for name in ["jlo.sh", "autoload.sh", "completions.sh"] {
            assert!(
                stdout.contains(name),
                "{sh}: the reload skipped {name} although this shell had it: {stdout:?}"
            );
        }
        assert!(
            stdout.contains("status=0"),
            "{sh}: a successful reload reported failure: {stdout:?}"
        );
    }
    release.assert();
}

/// The install verb never puts shell code on stdout: `install.sh`'s stdout
/// is evaluated by nothing, and only `selfupdate` hands the wrapper a reload.
#[test]
fn the_install_verb_writes_nothing_to_stdout() {
    let install = Install::new();
    let out = install.run(&["__install"], "http://127.0.0.1:1");
    assert!(out.status.success());
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "",
        "the bootstrap install wrote to the environment channel"
    );
}

// ---------------------------------------------------------------------------
// The hint on `jlo update`
// ---------------------------------------------------------------------------

/// Adoptium offering 21.0.9+10, and the tarball for it.
fn adoptium_offering_21() -> (mockito::ServerGuard, Vec<mockito::Mock>) {
    let mut server = mockito::Server::new();
    let archive = fake_jdk_archive("jdk-21.0.9+10");
    let sum = hex::encode(Sha256::digest(&archive));
    let meta = offer(&mut server, "21", "21.0.9+10", &sum).create();
    let package = server
        .mock("GET", "/jdk-21.tar.gz")
        .with_body(archive)
        .create();
    (server, vec![meta, package])
}

/// `jlo update` with nothing to do: 21.0.9+10 installed, and the newest.
fn update_with_nothing_to_do(install: &Install, release_url: &str) -> Output {
    install_fake_jdk(install.path(), "21.0.9+10");
    let (adoptium, _mocks) = adoptium_offering_21();
    install
        .command(release_url)
        .arg("update")
        .env("JLO_ADOPTIUM_API_URL", adoptium.url())
        .output()
        .unwrap()
}

#[test]
fn update_names_a_newer_jlo() {
    let install = Install::new();
    let release = Release::at(TAG, 1);

    let out = update_with_nothing_to_do(&install, &release.url());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stderr:?}");
    assert!(
        stderr.contains(&format!("J'Lo {NEWER} is available")),
        "{stderr:?}"
    );
    assert!(stderr.contains("jlo selfupdate"), "{stderr:?}");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "");
    release.assert();
}

/// Current, or the lookup failing any way at all: nothing extra is said.
#[test]
fn update_says_nothing_extra_when_current_or_the_lookup_fails() {
    let current = Release::at(&current_tag(), 1);
    // `hermetic`'s default release URL is a port nothing listens on.
    for url in [current.url(), "http://127.0.0.1:1".to_string()] {
        let install = Install::new();
        let out = update_with_nothing_to_do(&install, &url);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success(), "{url}: {stderr:?}");
        assert!(!stderr.contains("is available"), "{url}: {stderr:?}");
        assert!(!stderr.contains("Error"), "{url}: {stderr:?}");
        assert!(!stderr.contains("could not reach"), "{url}: {stderr:?}");
    }
    current.assert();
}

/// A release host that accepts the connection and never answers must not hold
/// the prompt: the lookup gives up after its timeout.
#[test]
fn a_hanging_lookup_is_cut_off() {
    // Bound and never accepted from: the kernel completes the handshake from
    // the backlog, the request is sent, and no answer ever comes.
    let silent = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", silent.local_addr().unwrap());
    let install = Install::new();
    install_fake_jdk(install.path(), "21.0.9+10");
    let (adoptium, _mocks) = adoptium_offering_21();

    let started = Instant::now();
    let mut child = install
        .command(&url)
        .arg("update")
        .env("JLO_ADOPTIUM_API_URL", adoptium.url())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    // Waits for the child itself rather than trusting it to end: a missing
    // timeout must fail this test, not hang the suite.
    while child.try_wait().unwrap().is_none() {
        if started.elapsed() > Duration::from_secs(30) {
            let _ = child.kill();
            panic!("jlo update still waiting on the release host after 30 s");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let elapsed = started.elapsed();
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "{:?}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(elapsed < Duration::from_secs(10), "took {elapsed:?}");
    drop(silent);
}

/// An update that fails before doing anything keeps its status and its one
/// error report; the hint is an extra line on stderr, not a second failure.
#[test]
fn an_early_failure_keeps_its_status_and_single_error() {
    let install = Install::new();
    let release = Release::at(TAG, 1);

    let out = install.run(&["update"], &release.url());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{stderr:?}");
    assert_eq!(stderr.matches("Error:").count(), 1, "{stderr:?}");
    assert!(stderr.contains("no installed JDKs to update"), "{stderr:?}");
    assert!(
        stderr.contains("is available"),
        "a newer J'Lo may be the fix: {stderr:?}"
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout), "");
    release.assert();
}

/// A wrapped update that downloaded 21.0.9+10 but could not remove the live
/// 21.0.5+11 it replaces: exit 1 after the payload is written. The payload is
/// exactly the update's own, and the wrapper still evaluates it.
#[test]
fn a_partly_failed_wrapped_update_keeps_its_payload_status_and_single_error() {
    /// The store with the live 21.0.5+11 undeletable, and its path.
    fn stuck_old_build(install: &Install) -> PathBuf {
        let old = install_fake_jdk(install.path(), "21.0.5+11");
        fs::set_permissions(old.join("bin"), fs::Permissions::from_mode(0o555)).unwrap();
        old
    }
    /// The removal renames the build aside before deleting it, so the stuck
    /// `bin` may be under a staging directory beside `old` by now.
    fn unstick(old: &Path) {
        let (store, name) = (old.parent().unwrap(), old.file_name().unwrap());
        let aside = fs::read_dir(store)
            .into_iter()
            .flatten()
            .flatten()
            .map(|entry| entry.path().join(name));
        for dir in std::iter::once(old.to_path_buf()).chain(aside) {
            let _ = fs::set_permissions(dir.join("bin"), fs::Permissions::from_mode(0o755));
        }
    }

    // Straight at the binary: stdout is the update's payload and nothing else.
    let install = Install::new();
    let old = stuck_old_build(&install);
    let release = Release::at(TAG, 2);
    let (adoptium, _mocks) = adoptium_offering_21();
    let out = install
        .command(&release.url())
        .args(["__wrapped", "update", "21"])
        .env("JLO_ADOPTIUM_API_URL", adoptium.url())
        .env("JAVA_HOME", &old)
        .output()
        .unwrap();
    unstick(&old);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let new = jdk_entry(install.path(), "21.0.9+10");
    assert_eq!(out.status.code(), Some(1), "{stderr:?}");
    assert!(stderr.contains("could not be removed"), "{stderr:?}");
    assert_eq!(stderr.matches("Error:").count(), 1, "{stderr:?}");
    assert!(stderr.contains("is available"), "{stderr:?}");
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), 3, "{stdout:?}");
    // Exactly the update's own payload: the hermetic PATH with the new
    // build's bin in front, nothing truncated, nothing appended.
    assert_eq!(lines[0], format!("export JAVA_HOME='{}' &&", new.display()));
    assert_eq!(
        lines[1],
        format!("export PATH='{}/bin:/usr/bin:/bin'", new.display())
    );
    assert_eq!(lines[2], "# jlo'end", "{stdout:?}");

    // Through the wrapper: the shell follows the new build despite the status.
    let install = Install::new();
    install.write_layout();
    let old = stuck_old_build(&install);
    let (adoptium, _mocks) = adoptium_offering_21();
    let out = hermetic(common::bash_bin(), install.path())
        .arg("-c")
        .arg(
            r#". "$JLO_HOME/jlo.sh"
            jlo update 21
            echo "status=$?"
            echo "java_home=$JAVA_HOME"
            echo "path=$PATH""#,
        )
        .env("JLO_HOME", install.path())
        .env("JLO_RELEASE_API_URL", release.url())
        .env("JLO_ADOPTIUM_API_URL", adoptium.url())
        .env("JAVA_HOME", &old)
        .output()
        .unwrap();
    unstick(&old);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let new = jdk_entry(install.path(), "21.0.9+10");
    assert!(stdout.contains("status=1"), "{stdout:?}");
    assert!(
        stdout.contains(&format!("java_home={}", new.display())),
        "{stdout:?}"
    );
    assert!(
        stdout.contains(&format!("path={}/bin:/usr/bin:/bin\n", new.display())),
        "{stdout:?}"
    );
    release.assert();
}

/// `update` is the one verb that asks. Every other verb - above all `env`,
/// which the cd hook runs - leaves the release host alone. The verbs that can
/// succeed here must, so a hint wrongly added to the tail of a shared path
/// (`install_names`, which `install` shares with `update`) is reached.
#[test]
fn no_other_verb_asks_for_the_release() {
    let install = Install::new();
    install.write_layout();
    let jdk = install_fake_jdk(install.path(), "21.0.9+10");
    let (adoptium, _mocks) = adoptium_offering_21();
    let release = Release::at(TAG, 0);
    let run = |args: &[&str]| {
        install
            .command(&release.url())
            .args(args)
            .env("JLO_ADOPTIUM_API_URL", adoptium.url())
            .env("JAVA_HOME", &jdk)
            .current_dir(install.path())
            .output()
            .unwrap()
    };
    for args in [
        &["env", "21"][..],
        &["env", "--offline", "21"],
        &["__wrapped", "env", "21"],
        &["home", "21"],
        &["home", "--offline", "21"],
        &["exec", "21", "--", "/bin/sh", "-c", "true"],
        &["current"],
        &["list", "--offline"],
        &["install", "21"],
        &["__wrapped", "install", "21"],
        &["init", "21"],
        &["--version"],
    ] {
        let out = run(args);
        assert!(
            out.status.success(),
            "{args:?}: {:?}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    // Their outcome is beside the point: only that they never ask.
    for args in [&["list"][..], &["remove", "17"]] {
        run(args);
    }
    release.assert();
}

/// A J'Lo `selfupdate` may not replace - a build in `target/`, Homebrew's -
/// has no newer release to be told about, so it never asks.
#[test]
fn an_unmanaged_jlo_never_asks() {
    let home = tempfile::tempdir().unwrap();
    install_fake_jdk(home.path(), "21.0.9+10");
    let (adoptium, _mocks) = adoptium_offering_21();
    let release = Release::at(TAG, 0);

    let out = hermetic(jlo_bin(), home.path())
        .arg("update")
        .env("JLO_ADOPTIUM_API_URL", adoptium.url())
        .env("JLO_RELEASE_API_URL", release.url())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{:?}",
        String::from_utf8_lossy(&out.stderr)
    );
    release.assert();
}
