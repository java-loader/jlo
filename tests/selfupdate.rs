//! End-to-end tests for `jlo selfupdate` against a `mockito` release host.
//!
//! There is no spare release to point a real test at, so `JLO_RELEASE_API_URL`
//! (the seam ADR-0002 established for Adoptium) stands in for
//! `github.com/.../releases`. That makes the whole path testable offline: the
//! `/releases/latest` redirect, the checksum, the download, the atomic swap
//! and the `exec` of the binary that was just published.
//!
//! Each test builds a real `$JLO_HOME`: the binary under test is *copied* to
//! `$JLO_HOME/bin/jlo-bin` and run from there, because `selfupdate` refuses an
//! install whose receipt names a different executable - running it straight
//! out of `target/` would only ever exercise that refusal.
//!
//! The tarball those tests serve carries a shell script called `jlo-bin`
//! rather than a second Rust binary. A released jlo has to answer two
//! questions - `--version`, and the hidden install verb - and a script answers
//! both, which lets a test serve a version that is genuinely *newer* than the
//! one under test. Serving the real binary would only ever reproduce the
//! already-current case.

// Test code: an `unwrap` failure here is a test failure, which is the point.
#![allow(clippy::unwrap_used)]

use sha2::{Digest, Sha256};
use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;

/// A version no real release will ever carry, so the "is it newer?" check has
/// an unambiguous answer whatever the crate version happens to be.
const NEWER: &str = "99.9.9";
const TAG: &str = "jlo-bin-v99.9.9";

/// What the release workflow names the asset for this platform.
fn package() -> &'static str {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => "jlo-linux-x86_64.tar.gz",
        ("linux", "aarch64") => "jlo-linux-aarch64.tar.gz",
        ("macos", "aarch64") => "jlo-macos-arm64.tar.gz",
        (os, arch) => panic!("no release asset for {os}/{arch}"),
    }
}

/// A `$JLO_HOME` holding a real copy of the binary under test plus the receipt
/// that install would have written for it.
struct Install {
    home: tempfile::TempDir,
}

impl Install {
    fn new(method: &str) -> Self {
        let home = tempfile::tempdir().unwrap();
        let bin = home.path().join("bin");
        fs::create_dir_all(&bin).unwrap();
        let target = bin.join("jlo-bin");
        fs::copy(assert_cmd::cargo::cargo_bin("jlo-bin"), &target).unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).unwrap();

        let install = Self { home };
        install.write_receipt(method, &Self::version(), &target);
        install
    }

    fn path(&self) -> &Path {
        self.home.path()
    }

    fn binary(&self) -> PathBuf {
        self.home.path().join("bin").join("jlo-bin")
    }

    fn receipt_path(&self) -> PathBuf {
        self.home.path().join("install-receipt.json")
    }

    /// The version of the binary under test, read from the binary itself so
    /// the fixtures never drift from `Cargo.toml`.
    fn version() -> String {
        let out = Command::new(assert_cmd::cargo::cargo_bin("jlo-bin"))
            .arg("--version")
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout)
            .split_whitespace()
            .next_back()
            .unwrap()
            .to_string()
    }

    fn write_receipt(&self, method: &str, version: &str, binary: &Path) {
        fs::write(
            self.receipt_path(),
            format!(
                "{{\n  \"version\": {:?},\n  \"method\": {:?},\n  \
                 \"jlo_home\": {:?},\n  \"binary\": {:?}\n}}\n",
                version,
                method,
                self.home.path().to_string_lossy(),
                binary.to_string_lossy()
            ),
        )
        .unwrap();
    }

    /// Runs `jlo selfupdate` as the installed binary, with the release host
    /// pointed at `base_url`.
    fn selfupdate(&self, base_url: &str) -> std::process::Output {
        self.run(&["selfupdate"], base_url)
    }

    /// The same, as the `jlo` shell function calls it.
    fn wrapped_selfupdate(&self, base_url: &str) -> std::process::Output {
        self.run(&["__wrapped", "selfupdate"], base_url)
    }

    fn run(&self, args: &[&str], base_url: &str) -> std::process::Output {
        Command::new(self.binary())
            .args(args)
            .env("JLO_HOME", self.home.path())
            .env("JLO_RELEASE_API_URL", base_url)
            // Keep the symlink logic out of the developer's real ~/.local/bin.
            .env("HOME", self.home.path())
            .output()
            .unwrap()
    }
}

/// A `jlo-bin` that stands in for a released one.
///
/// It answers `--version` with a version the test chose - which is the whole
/// point, because a release carrying the *real* binary could only ever
/// reproduce the already-current case - and then hands the install verb
/// straight to the real binary under test. So the layout the update publishes
/// is generated by the genuine generator, not faked, and the reload line on
/// stdout is the genuine one.
///
/// Before delegating it probes the lock, which is the only way to observe
/// invariant 5 from outside: `selfupdate` holds an exclusive `flock` and the
/// fd has to survive the `exec` that produced this process. std opens every
/// file `O_CLOEXEC`, so without the explicit `fcntl` the lock would be gone
/// here - silently, and exactly while the install is half-published.
fn fake_release_binary(version: &str) -> String {
    let real = assert_cmd::cargo::cargo_bin("jlo-bin");
    format!(
        "#!/bin/sh\n\
         case \"$1\" in\n\
         \x20 --version) echo 'jlo-bin {version}' ;;\n\
         \x20 __install|__wrapped)\n\
         \x20   python3 -c 'import fcntl, os, sys\n\
         f = open(os.environ[\"JLO_HOME\"] + \"/.selfupdate.lock\", \"w\")\n\
         try:\n\
        \x20    fcntl.flock(f, fcntl.LOCK_EX | fcntl.LOCK_NB)\n\
        \x20    print(\"lock=free\", file=sys.stderr)\n\
         except BlockingIOError:\n\
        \x20    print(\"lock=held\", file=sys.stderr)\n\
         '\n\
         \x20   echo \"delegating {version}\" >&2\n\
         \x20   exec {real:?} \"$@\"\n\
         \x20   ;;\n\
         \x20 *) echo \"unexpected argv: $*\" >&2; exit 64 ;;\n\
         esac\n",
        real = real.display().to_string(),
    )
}

/// The release tarball, built exactly the way the workflow builds it: one
/// entry named `jlo-bin` at the root, gzipped.
fn tarball(body: &str) -> Vec<u8> {
    let mut header = tar::Header::new_gnu();
    header.set_path("jlo-bin").unwrap();
    header.set_size(body.len() as u64);
    header.set_mode(0o755);
    header.set_cksum();

    let mut tar = tar::Builder::new(Vec::new());
    tar.append(&header, body.as_bytes()).unwrap();
    let raw = tar.into_inner().unwrap();

    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    std::io::Write::write_all(&mut gz, &raw).unwrap();
    gz.finish().unwrap()
}

/// A release host serving `tag` with the given tarball bytes and checksum.
struct Release {
    server: mockito::ServerGuard,
    _mocks: Vec<mockito::Mock>,
}

impl Release {
    fn serving(tag: &str, archive: &[u8], checksum: &str) -> Self {
        let mut server = mockito::Server::new();
        let url = server.url();
        let mocks = vec![
            server
                .mock("GET", "/latest")
                .with_status(302)
                .with_header("location", &format!("{url}/tag/{tag}"))
                .create(),
            server
                .mock(
                    "GET",
                    format!("/download/{tag}/{}.sha256", package()).as_str(),
                )
                .with_body(format!("{checksum}  {}\n", package()))
                .create(),
            server
                .mock("GET", format!("/download/{tag}/{}", package()).as_str())
                .with_body(archive)
                .create(),
        ];
        Self {
            server,
            _mocks: mocks,
        }
    }

    fn good(tag: &str, version: &str) -> Self {
        let archive = tarball(&fake_release_binary(version));
        let checksum = hex::encode(Sha256::digest(&archive));
        Self::serving(tag, &archive, &checksum)
    }

    fn url(&self) -> String {
        self.server.url()
    }
}

// ---------------------------------------------------------------------------

/// The whole path: resolve the tag from the redirect, verify the checksum,
/// swap the binary by `rename`, and `exec` the *new* one so it generates its
/// own files. The reload line on stdout is what the wrapper evals.
#[test]
fn a_newer_release_is_verified_published_and_reloaded() {
    let install = Install::new("installer");
    let release = Release::good(TAG, NEWER);

    let out = install.selfupdate(&release.url());
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stdout:?} {stderr:?}");

    // The exec'd binary is the one that wrote the files, which is the whole
    // reason for the exec: the running process carries the *old* templates.
    assert!(
        stderr.contains(&format!("delegating {NEWER}")),
        "the new binary never ran the install verb: {stderr:?}"
    );
    // Invariant 5: the lock is on the open file description and the fd has to
    // survive the exec. `O_CLOEXEC` - which std sets on every file - would
    // drop it silently, right here, with the install half-published.
    assert!(
        stderr.contains("lock=held"),
        "the update lock did not survive the exec: {stderr:?}"
    );

    // stdout is the environment channel (ADR-0001) and carries *only* the
    // reload block: three lines, nothing else. A substring check would let
    // any amount of pollution through.
    let home = install.path().to_string_lossy();
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(
        lines.len(),
        3,
        "stdout is not just the reload block: {stdout:?}"
    );
    assert_eq!(lines[0], format!(". '{home}/jlo.sh'"));
    assert!(
        lines[1].contains("_JLO_AUTOLOAD") && lines[1].ends_with("autoload.sh'; fi"),
        "{stdout:?}"
    );
    assert!(
        lines[2].contains("_JLO_COMPLETIONS") && lines[2].ends_with("completions.sh'; fi"),
        "{stdout:?}"
    );

    let published = fs::read_to_string(install.binary()).unwrap();
    assert!(
        published.contains(&format!("jlo-bin {NEWER}")),
        "the binary was not replaced"
    );
    // The generator really ran, rather than a fixture printing a line that
    // looks like it did.
    assert!(
        install.path().join("jlo.sh").is_file(),
        "the exec'd binary did not generate the layout"
    );
    // Staged beside the target and removed again; nothing left in bin/.
    let leftovers: Vec<String> = fs::read_dir(install.path().join("bin"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with(".jlo-update-"))
        .collect();
    assert!(leftovers.is_empty(), "staging left behind: {leftovers:?}");
}

/// Invariant 3, asserted rather than assumed: the staging directory is a
/// *sibling* of the target file, because `rename` is atomic only within one
/// filesystem and `bin/` can itself be a mount point or a symlink.
///
/// Observed by making `bin/` unwritable: staging under it must fail and name
/// the path it tried. A version that staged in `std::env::temp_dir()` would
/// get past this point and fail later, somewhere else.
#[test]
fn staging_happens_beside_the_target_not_in_the_temp_dir() {
    let install = Install::new("installer");
    let release = Release::good(TAG, NEWER);
    let bin = install.path().join("bin");

    fs::set_permissions(&bin, fs::Permissions::from_mode(0o555)).unwrap();
    let out = install.selfupdate(&release.url());
    // Restore before any assertion, so a failure still leaves a removable
    // temp directory behind.
    fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "an unwritable bin/ was not noticed");
    assert!(
        stderr.contains("could not create") && stderr.contains("/bin/.jlo-update-"),
        "the update did not stage beside the target: {stderr:?}"
    );
}

/// Nothing to reload when there is nothing to do: for the wrapper, the
/// marker alone, so "nothing to do" is not mistaken for "cut short"; for
/// anyone else, nothing at all.
#[test]
fn an_up_to_date_install_prints_no_reload() {
    let install = Install::new("installer");
    let version = Install::version();
    let release = Release::good(&format!("jlo-bin-v{version}"), &version);

    for (out, expected) in [
        (install.selfupdate(&release.url()), ""),
        (install.wrapped_selfupdate(&release.url()), "# jlo'end\n"),
    ] {
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success(), "{stderr:?}");
        assert_eq!(String::from_utf8_lossy(&out.stdout), expected);
        assert!(
            stderr.contains("already the latest version"),
            "no status line: {stderr:?}"
        );
    }
}

/// Wrapped mode crosses the `exec`: the new binary writes the reload block,
/// so only it can end it with the marker - `&&`-joined, so a reload that
/// cannot source `jlo.sh` fails the call.
#[test]
fn a_wrapped_update_forwards_the_mode_to_the_new_binary() {
    let install = Install::new("installer");
    let release = Release::good(TAG, NEWER);

    let out = install.wrapped_selfupdate(&release.url());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{:?}",
        String::from_utf8_lossy(&out.stderr)
    );

    let home = install.path().to_string_lossy();
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), 4, "{stdout:?}");
    assert_eq!(lines[0], format!(". '{home}/jlo.sh' &&"));
    assert!(lines[1].ends_with("autoload.sh'; fi &&"), "{stdout:?}");
    assert!(lines[2].ends_with("completions.sh'; fi"), "{stdout:?}");
    assert_eq!(lines[3], "# jlo'end");
}

/// End to end through the shell function: the wrapped update's reload,
/// written by the binary on the far side of the `exec`, is evaluated.
/// `jlo.sh` unsets `_jlo_d` as its last act, so a sentinel left in it
/// survives unless the reload sourced `jlo.sh` again.
#[test]
fn a_wrapped_update_reloads_the_calling_shell() {
    for (sh, dialect) in [("/bin/bash", "bash"), ("zsh", "zsh")] {
        if Command::new(sh).arg("-c").arg("exit 0").output().is_err() {
            eprintln!("SKIP a_wrapped_update_reloads_the_calling_shell: no {sh}");
            continue;
        }
        let install = Install::new("installer");
        let release = Release::good(TAG, NEWER);
        let layout = Command::new(install.binary())
            .arg("__install")
            .env("JLO_HOME", install.path())
            .env("HOME", install.path())
            .output()
            .unwrap();
        assert!(layout.status.success(), "{layout:?}");

        let out = Command::new(sh)
            .arg("-c")
            .arg(
                r#". "$JLO_HOME/jlo.sh"
                _jlo_d=sentinel
                jlo selfupdate
                echo "status=$?"
                echo "reloaded=${_jlo_d-yes}""#,
            )
            .env("JLO_HOME", install.path())
            .env("HOME", install.path())
            .env("JLO_RELEASE_API_URL", release.url())
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        let ctx = format!(
            "{dialect}: {stdout:?} {:?}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(stdout.contains("status=0"), "{ctx}");
        assert!(stdout.contains("reloaded=yes"), "{ctx}");
    }
}

/// A corrupt or tampered download must never be unpacked, and the binary the
/// user is running must survive it untouched.
#[test]
fn a_checksum_mismatch_leaves_the_binary_alone() {
    let install = Install::new("installer");
    let before = fs::read(install.binary()).unwrap();
    let archive = tarball(&fake_release_binary(NEWER));
    let release = Release::serving(TAG, &archive, &"0".repeat(64));

    let out = install.selfupdate(&release.url());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "a bad checksum was accepted");
    assert!(
        stderr.contains("checksum mismatch"),
        "the error does not name the checksum: {stderr:?}"
    );
    assert_eq!(fs::read(install.binary()).unwrap(), before);
}

/// The release the tag promises and the binary inside it must be the same
/// version; a release whose assets disagree with its tag is refused.
#[test]
fn a_release_whose_binary_disagrees_with_its_tag_is_refused() {
    let install = Install::new("installer");
    let before = fs::read(install.binary()).unwrap();
    let release = Release::good(TAG, "98.0.0");

    let out = install.selfupdate(&release.url());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "a mismatched release was accepted");
    assert!(
        stderr.contains("contains J'Lo 98.0.0"),
        "the error does not name what it found: {stderr:?}"
    );
    assert_eq!(fs::read(install.binary()).unwrap(), before);
}

/// The tag format is a contract with release-please. A tag it does not
/// recognise is an error, not a guess about which version is on the other end.
#[test]
fn an_unrecognised_tag_stops_the_update() {
    let install = Install::new("installer");
    let release = Release::good("v99.9.9", NEWER);

    let out = install.selfupdate(&release.url());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success());
    assert!(stderr.contains("unrecognised release tag"), "{stderr:?}");
}

/// A local build is exactly the case where somebody does not want a release
/// written over their work, so `selfupdate` stops and says why.
#[test]
fn a_local_build_is_not_silently_replaced_by_a_release() {
    let install = Install::new("local");
    let release = Release::good(TAG, NEWER);

    let out = install.selfupdate(&release.url());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "a local build was overwritten");
    assert!(
        stderr.contains("local build"),
        "the refusal does not say why: {stderr:?}"
    );
    assert!(
        stderr.contains("install-local.sh"),
        "the refusal does not name the way out: {stderr:?}"
    );
}

/// The receipt's whole point: a future Homebrew formula owns its install, and
/// jlo must not fight it.
#[test]
fn a_package_manager_install_is_refused() {
    let install = Install::new("package-manager");
    let release = Release::good(TAG, NEWER);

    let out = install.selfupdate(&release.url());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success());
    assert!(stderr.contains("package manager"), "{stderr:?}");
}

/// Guessing is how a package-manager install gets clobbered, so a receipt that
/// cannot be parsed stops the update and names the file.
#[test]
fn a_malformed_receipt_stops_the_update() {
    let install = Install::new("installer");
    fs::write(install.receipt_path(), "{ not json").unwrap();
    let release = Release::good(TAG, NEWER);

    let out = install.selfupdate(&release.url());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success());
    assert!(
        stderr.contains("install-receipt.json") && stderr.contains("not a valid install receipt"),
        "{stderr:?}"
    );
}

/// A receipt naming a path this executable does not occupy means jlo was moved
/// or copied; updating would write a release over somebody else's install.
#[test]
fn a_receipt_naming_another_binary_stops_the_update() {
    let install = Install::new("installer");
    install.write_receipt(
        "installer",
        &Install::version(),
        Path::new("/nowhere/jlo-bin"),
    );
    let release = Release::good(TAG, NEWER);

    let out = install.selfupdate(&release.url());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success());
    assert!(
        stderr.contains("/nowhere/jlo-bin"),
        "the refusal does not name the receipt's path: {stderr:?}"
    );
}

/// A missing receipt is an install that predates them, not a broken one: the
/// update proceeds, and the install verb writes one.
#[test]
fn a_missing_receipt_does_not_stop_the_update() {
    let install = Install::new("installer");
    fs::remove_file(install.receipt_path()).unwrap();
    let release = Release::good(TAG, NEWER);

    let out = install.selfupdate(&release.url());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stderr:?}");
    assert!(
        fs::read_to_string(install.binary())
            .unwrap()
            .contains(&format!("jlo-bin {NEWER}")),
        "the binary was not replaced"
    );
}

/// Two updates must not interleave their writes. The lock is exclusive and
/// fails fast rather than waiting on something the user cannot see.
///
/// The holder is a `python3` one-liner rather than `flock(1)`, which is a
/// util-linux tool and absent on macOS - and a test that silently skips on
/// the developer's own machine is not a test. It takes the lock from its own
/// open file description, which is the only way to observe the `flock` from
/// outside this process.
#[test]
fn a_held_lock_stops_a_second_update() {
    let install = Install::new("installer");
    let release = Release::good(TAG, NEWER);

    let lock = install.path().join(".selfupdate.lock");
    let ready = install.path().join("lock-held");
    let holder = Command::new("python3")
        .arg("-c")
        .arg(
            "import fcntl, pathlib, sys, time\n\
             f = open(sys.argv[1], 'w')\n\
             fcntl.flock(f, fcntl.LOCK_EX)\n\
             pathlib.Path(sys.argv[2]).write_text('held')\n\
             time.sleep(30)\n",
        )
        .arg(&lock)
        .arg(&ready)
        .spawn();
    let Ok(mut holder) = holder else {
        eprintln!("SKIP a_held_lock_stops_a_second_update: python3 is not installed here.");
        return;
    };

    let mut held = false;
    for _ in 0..100 {
        if ready.exists() {
            held = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }

    let out = held.then(|| install.selfupdate(&release.url()));
    let _ = holder.kill();
    let _ = holder.wait();
    assert!(held, "the holder never took the lock");

    let out = out.unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "the lock was ignored: {stderr:?}");
    assert!(
        stderr.contains("already running"),
        "the refusal does not name the reason: {stderr:?}"
    );
}

/// `--reload` is the only thing that puts shell code on the install verb's
/// stdout, and it re-sources only what this shell had already enabled.
#[test]
fn the_install_verb_prints_the_reload_line_only_with_reload() {
    let install = Install::new("installer");

    let quiet = Command::new(install.binary())
        .arg("__install")
        .env("JLO_HOME", install.path())
        .env("HOME", install.path())
        .output()
        .unwrap();
    assert!(quiet.status.success());
    assert_eq!(
        String::from_utf8_lossy(&quiet.stdout),
        "",
        "the bootstrap install wrote to the environment channel"
    );

    let loud = Command::new(install.binary())
        .args(["__install", "--reload"])
        .env("JLO_HOME", install.path())
        .env("HOME", install.path())
        .output()
        .unwrap();
    assert!(loud.status.success());
    let stdout = String::from_utf8_lossy(&loud.stdout);
    let home = install.path().to_string_lossy();
    assert!(
        stdout.contains(&format!(". '{home}/jlo.sh'")),
        "no unconditional reload of jlo.sh: {stdout:?}"
    );
    assert!(
        stdout.contains("_JLO_AUTOLOAD") && stdout.contains("_JLO_COMPLETIONS"),
        "the optional stubs are not gated on their markers: {stdout:?}"
    );
}
