//! Helpers shared by the integration test crates. Each crate uses a different
//! subset, so every item carries its own `dead_code` allow.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Where every URL jlo would fetch from points unless a test says otherwise: a
/// port nothing listens on, so a forgotten mock fails fast instead of
/// reaching the real service.
const DEAD_URL: &str = "http://127.0.0.1:1";

/// The `PATH` every spawned process gets: the system directories alone, enough
/// for `sh`, `bash`, `zsh`, `cp`, `tar` and `python3`. A tool found only on the
/// developer's `PATH` is one CI does not have.
#[allow(dead_code)]
pub(crate) const HERMETIC_PATH: &str = "/usr/bin:/bin";

/// The binary under test.
#[allow(dead_code)]
pub(crate) fn jlo_bin() -> PathBuf {
    assert_cmd::cargo::cargo_bin("jlo-bin")
}

/// `program` run with nothing inherited from whoever runs the tests: jlo reads
/// `JAVA_HOME`, `JLO_HOME`, `TERM`, the colour variables and more, and a shell
/// reads `BASH_ENV`, `ENV` and `ZDOTDIR` before the test body runs. A test that
/// forgets one passes on its author's machine and fails where the environment
/// differs. `home` is required because the store, the `.jlorc` walk and zsh's
/// startup files derive from it, and no default can be trusted not to be the
/// real one. A bare `program` is looked up on [`HERMETIC_PATH`].
#[allow(dead_code)]
pub(crate) fn hermetic(program: impl AsRef<OsStr>, home: &Path) -> Command {
    let mut cmd = Command::new(program);
    cmd.env_clear()
        .env("HOME", home)
        .env("PATH", HERMETIC_PATH)
        .env("JLO_ADOPTIUM_API_URL", DEAD_URL)
        .env("JLO_RELEASE_API_URL", DEAD_URL);
    cmd
}

/// `jlo-bin` under [`hermetic`]. A test adds only what it is about.
#[allow(dead_code)]
pub(crate) fn jlo(home: &Path) -> assert_cmd::Command {
    assert_cmd::Command::from_std(hermetic(jlo_bin(), home))
}

/// [`jlo`] with Adoptium left at its compiled-in address: the one opt-in to
/// the real API, for the tests that exist to notice the fixtures drifting
/// from it.
#[allow(dead_code)]
pub(crate) fn jlo_online(home: &Path) -> assert_cmd::Command {
    let mut cmd = hermetic(jlo_bin(), home);
    cmd.env_remove("JLO_ADOPTIUM_API_URL");
    assert_cmd::Command::from_std(cmd)
}

/// Every interpreter the shell code must work under.
///
/// `/bin/bash` is an absolute path on purpose: on macOS it is the system bash
/// 3.2.57, the only shell in the supported range whose `source` cannot read
/// the `/dev/fd/N` of a process substitution. A `bash` taken from `PATH` is
/// usually a Homebrew 5.x and would not cover it.
#[allow(dead_code)]
pub(crate) const INTERPRETERS: &[&str] = &["/bin/bash", "zsh"];

/// The bash the single-interpreter cases run: the system one, found on
/// [`HERMETIC_PATH`]. `JLO_TEST_BASH` overrides it, as an absolute path.
#[allow(dead_code)]
pub(crate) fn bash_bin() -> String {
    std::env::var("JLO_TEST_BASH").unwrap_or_else(|_| "bash".to_string())
}

/// Returns true when the caller should skip this interpreter. Prints loudly: a
/// silently skipped shell is indistinguishable from a passing one. Probed the
/// way the tests run it, under [`hermetic`], so a shell only the host `PATH`
/// has is skipped rather than failing to spawn.
#[allow(dead_code)]
#[must_use]
pub(crate) fn skip_missing(test: &str, sh: &str) -> bool {
    let home = tempfile::tempdir().unwrap();
    if hermetic(sh, home.path())
        .arg("-c")
        .arg("exit 0")
        .output()
        .is_ok()
    {
        return false;
    }
    eprintln!("SKIP {test}: {sh} is not installed here.");
    true
}

/// The `candidates` that are installed here, announcing each one skipped.
#[allow(dead_code)]
pub(crate) fn shells<S: AsRef<str>>(
    test: &str,
    candidates: impl IntoIterator<Item = S>,
) -> impl Iterator<Item = S> {
    candidates
        .into_iter()
        .filter(move |sh| !skip_missing(test, sh.as_ref()))
}

#[allow(dead_code)]
pub(crate) fn chmod(path: &Path, mode: u32) {
    let mut perms = std::fs::metadata(path).unwrap().permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut perms, mode);
    std::fs::set_permissions(path, perms).unwrap();
}

/// A tar.gz holding one JDK root with a `bin/java`, as Adoptium ships it -
/// enough for the whole download, verify, extract and move pipeline.
#[allow(dead_code)]
pub(crate) fn fake_jdk_archive(root: &str) -> Vec<u8> {
    let mut builder = tar::Builder::new(flate2::write::GzEncoder::new(
        Vec::new(),
        flate2::Compression::fast(),
    ));
    let mut header = tar::Header::new_gnu();
    header.set_size(0);
    header.set_mode(0o755);
    header.set_cksum();
    builder
        .append_data(&mut header, format!("{root}/bin/java"), std::io::empty())
        .unwrap();
    builder.into_inner().unwrap().finish().unwrap()
}

/// The not-yet-created mock of Adoptium's latest-build lookup for `major`,
/// for the caller to give a body or status (and an `expect`) and `create`.
#[allow(dead_code)]
pub(crate) fn latest(server: &mut mockito::ServerGuard, major: &str) -> mockito::Mock {
    server
        .mock(
            "GET",
            mockito::Matcher::Regex(format!(r"^/v3/assets/latest/{major}/hotspot")),
        )
        .match_query(mockito::Matcher::Any)
}

/// [`latest`] answering with one build, `version`, whose package is served at
/// `/jdk-{major}.tar.gz` on the same server and hashes to `checksum`.
#[allow(dead_code)]
pub(crate) fn offer(
    server: &mut mockito::ServerGuard,
    major: &str,
    version: &str,
    checksum: &str,
) -> mockito::Mock {
    let link = format!("{}/jdk-{major}.tar.gz", server.url());
    latest(server, major).with_body(format!(
        r#"[{{"version":{{"semver":"{version}"}},"binary":{{"package":{{"name":"jdk.tar.gz","link":"{link}","checksum":"{checksum}"}}}}}}]"#
    ))
}

/// The JDK store `JdkStore::discover` derives from `$HOME`. It is not
/// configurable, which is why the tests move `$HOME` instead.
#[allow(dead_code)]
pub(crate) fn jdk_store_in(home: &Path) -> PathBuf {
    if cfg!(target_os = "macos") {
        home.join("Library/Java/JavaVirtualMachines")
    } else {
        home.join(".jdks")
    }
}

/// The store entry under `home` holding `version`, named as jlo names it.
#[allow(dead_code)]
pub(crate) fn jdk_entry(home: &Path, version: &str) -> PathBuf {
    jdk_store_in(home).join(format!("jlo-temurin-{version}"))
}

/// A flat JDK holding `version` in the store under `home`.
#[allow(dead_code)]
pub(crate) fn install_fake_jdk(home: &Path, version: &str) -> PathBuf {
    let jdk = jdk_entry(home, version);
    std::fs::create_dir_all(jdk.join("bin")).unwrap();
    std::fs::write(jdk.join("bin").join("java"), "").unwrap();
    jdk
}

/// POSIX single-quoting, so a value can be embedded in a `sh -c` string as one
/// literal word. Mirrors `shell_quote` in `src/shellenv.rs`; kept separate so a
/// bug there cannot hide itself here.
#[allow(dead_code)]
pub(crate) fn squote(value: impl AsRef<Path>) -> String {
    format!(
        "'{}'",
        value.as_ref().display().to_string().replace('\'', r"'\''")
    )
}

/// Run `program args` with stdout and stderr on a pseudo-terminal, the way a
/// user's shell runs it; both come back merged, in `stdout`. python3 because
/// `script` takes different flags on macOS and Linux - and not `pty.spawn`,
/// which relays stdin and did not return under the test harness.
#[allow(dead_code)]
pub(crate) fn on_a_terminal(home: &Path, program: &Path, args: &[&str]) -> std::process::Output {
    const RUN: &str = "\
import os, subprocess, sys
leader, follower = os.openpty()
child = subprocess.Popen(sys.argv[1:], stdin=subprocess.DEVNULL, stdout=follower, stderr=follower)
os.close(follower)
while True:
    try:
        chunk = os.read(leader, 4096)
    except OSError:
        break
    if not chunk:
        break
    sys.stdout.buffer.write(chunk)
sys.exit(child.wait())
";
    hermetic("python3", home)
        .arg("-c")
        .arg(RUN)
        .arg(program)
        .args(args)
        .output()
        .unwrap()
}
