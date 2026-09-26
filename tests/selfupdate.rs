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

use common::{INTERPRETERS, hermetic, jlo_bin, shells, squote};
use sha2::{Digest, Sha256};
use std::fs;
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// A tag no real release will ever carry, so "is it newer?" has an
/// unambiguous answer whatever the crate version is.
const TAG: &str = "jlo-bin-v99.9.9";
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
    assert!(fs::read(install.binary()).unwrap() == before);
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
// The released updaters' hand-over
// ---------------------------------------------------------------------------

/// J'Lo 0.4.0's resident wrapper (zsh dialect), comments dropped: it evaluates
/// whatever stdout a successful `selfupdate` prints, marker or not.
const WRAPPER_0_4_0: &str = r#"
jlo() {
  local J arg out
  J="${JLO_HOME-}/bin/jlo-bin"
  case "$1" in
    env|use|selfupdate)
      for arg in "$@"; do
        case "$arg" in
          -h|--help|-V|--version)
            "$J" "$@"
            return
            ;;
        esac
      done
      out="$("$J" "$@")" || return
      eval "$out"
      ;;
    *)
      "$J" "$@"
      ;;
  esac
}
"#;

/// J'Lo 0.5.0's resident wrapper (zsh dialect), comments dropped: it
/// evaluates stdout only when it ends in the marker.
const WRAPPER_0_5_0: &str = r##"
jlo() {
  local J out rc=0
  J="${JLO_HOME-}/bin/jlo-bin"
  case "${1-}" in
    env|use|selfupdate|install|update)
      if ! ( JAVA_HOME='' PATH='' ) 2>/dev/null; then
        echo "jlo: JAVA_HOME or PATH is read-only" >&2
        return 1
      fi
      out="$("$J" __wrapped "$@")" || rc=$?
      case "$out" in
        *"# jlo'end") eval "${out%"# jlo'end"}" || [ "$rc" -ne 0 ] || rc=1 ;;
        *) [ "$rc" -ne 0 ] || printf '%s\n' "$out" >&1 || rc=1 ;;
      esac
      return "$rc"
      ;;
    *)
      "$J" "$@"
      ;;
  esac
}
"##;

/// Runs `jlo selfupdate` through a released `wrapper`, with `updater` - a
/// stand-in for that release's binary, from the point its download verified -
/// at `bin/jlo-bin`. Reports the status and whether the calling shell now has
/// the current wrapper, which alone says "neither `JLO_HOME` nor HOME".
fn released_hand_over(install: &Install, wrapper: &str, updater: &str) -> Output {
    install.write_layout();
    // Written by this process to a file that is never exec'd, then copied by
    // a child `cp` onto a fresh inode and renamed into place: a write fd this
    // process held on the exec'd inode itself could leak into another test
    // thread's fork and fail the exec with ETXTBSY (see `Install::new`).
    let source = install.path().join("updater.src");
    fs::write(&source, updater).unwrap();
    let fresh = install.path().join("bin").join(".jlo-bin.standin");
    let placed = hermetic("/bin/sh", install.path())
        .arg("-c")
        .arg("cp \"$1\" \"$2\" && chmod 755 \"$2\" && mv \"$2\" \"$3\"")
        .arg("sh")
        .arg(&source)
        .arg(&fresh)
        .arg(install.binary())
        .status()
        .unwrap();
    assert!(placed.success());

    hermetic("zsh", install.path())
        .arg("-c")
        .arg(format!(
            r#". "$JLO_HOME/jlo.sh"
            {wrapper}
            jlo selfupdate
            echo "status=$?"
            if typeset -f jlo | grep -q 'neither JLO_HOME nor HOME'; then echo reloaded=yes; else echo reloaded=no; fi"#
        ))
        .env("JLO_HOME", install.path())
        .output()
        .unwrap()
}

/// Both released updaters probed the staged binary with `--version` and
/// refused the hand-over unless the output's last word was the tag's version.
/// The stand-ins below do the same, so a change to the version output that
/// the released updaters would refuse fails here.
fn version_probe(staged: &str) -> String {
    format!(
        "v=$({staged} --version) || exit 71\n\
         [ \"${{v##* }}\" = '{VERSION}' ] || {{ echo \"probe refused: $v\" >&2; exit 73; }}\n"
    )
}

/// 0.4.0 staged and probed the new binary, renamed it into place itself, then
/// ran it as `__install --reload --locked`, unwrapped: the reload lines arrive
/// without the marker, and 0.4.0's wrapper evaluates them.
#[test]
fn the_0_4_0_updater_hands_over_and_reloads() {
    if common::skip_missing("the_0_4_0_updater_hands_over_and_reloads", "zsh") {
        return;
    }
    let install = Install::new();
    let updater = format!(
        "#!/bin/sh\n\
         [ \"$1\" = selfupdate ] || exit 64\n\
         stage=\"$JLO_HOME/bin/.jlo-install-$$\"\n\
         mkdir \"$stage\" && cp {real} \"$stage/jlo-bin\" || exit 70\n\
         {probe}\
         mv \"$stage/jlo-bin\" \"$JLO_HOME/bin/jlo-bin\" && rmdir \"$stage\" || exit 70\n\
         exec \"$JLO_HOME/bin/jlo-bin\" __install --reload --locked\n",
        real = squote(jlo_bin()),
        probe = version_probe("\"$stage/jlo-bin\""),
    );
    let out = released_hand_over(&install, WRAPPER_0_4_0, &updater);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let ctx = format!("{stdout:?} {:?}", String::from_utf8_lossy(&out.stderr));
    assert!(stdout.contains("status=0"), "{ctx}");
    assert!(stdout.contains("reloaded=yes"), "{ctx}");

    let direct = install.run(&["__install", "--reload", "--locked"], "http://127.0.0.1:1");
    assert!(direct.status.success());
    assert_reload_lines(
        &String::from_utf8_lossy(&direct.stdout),
        install.path(),
        false,
    );
}

/// 0.5.0 staged the new binary, probed it with `--version` - which must leave
/// it in place - then ran `__wrapped __install --publish-self --reload
/// --locked`: the staged file is published by rename, the reload carries the
/// marker 0.5.0's wrapper needs, and the staging directory goes.
#[test]
fn the_0_5_0_updater_hands_over_and_reloads() {
    if common::skip_missing("the_0_5_0_updater_hands_over_and_reloads", "zsh") {
        return;
    }
    let install = Install::new();
    let updater = format!(
        "#!/bin/sh\n\
         [ \"$1 $2\" = \"__wrapped selfupdate\" ] || exit 64\n\
         stage=\"$JLO_HOME/bin/.jlo-install-$$\"\n\
         mkdir \"$stage\" && cp {real} \"$stage/jlo-bin\" || exit 70\n\
         {probe}\
         [ -x \"$stage/jlo-bin\" ] || exit 72\n\
         set -- $(ls -i \"$stage/jlo-bin\")\n\
         echo \"staged-inode=$1\" >&2\n\
         exec \"$stage/jlo-bin\" __wrapped __install --publish-self --reload --locked\n",
        real = squote(jlo_bin()),
        probe = version_probe("\"$stage/jlo-bin\""),
    );
    let out = released_hand_over(&install, WRAPPER_0_5_0, &updater);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let ctx = format!("{stdout:?} {stderr:?}");
    assert!(stdout.contains("status=0"), "{ctx}");
    assert!(stdout.contains("reloaded=yes"), "{ctx}");

    let staged = stderr
        .lines()
        .find_map(|line| line.strip_prefix("staged-inode="))
        .unwrap_or_else(|| panic!("the staged binary never ran: {ctx}"));
    assert_eq!(
        staged,
        fs::metadata(install.binary()).unwrap().ino().to_string()
    );
    assert!(staging_leftovers(&install).is_empty(), "{ctx}");
}

/// `--reload` is the only thing that puts shell code on the install verb's
/// stdout.
#[test]
fn the_install_verb_prints_the_reload_line_only_with_reload() {
    let install = Install::new();

    let quiet = install.run(&["__install"], "http://127.0.0.1:1");
    assert!(quiet.status.success());
    assert_eq!(
        String::from_utf8_lossy(&quiet.stdout),
        "",
        "the bootstrap install wrote to the environment channel"
    );

    let loud = install.run(&["__install", "--reload"], "http://127.0.0.1:1");
    assert!(loud.status.success());
    assert_reload_lines(
        &String::from_utf8_lossy(&loud.stdout),
        install.path(),
        false,
    );
}
