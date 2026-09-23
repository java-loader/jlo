//! Helpers shared by the integration test crates. Each crate uses a different
//! subset, so every item carries its own `dead_code` allow.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Every interpreter the shell code must work under.
///
/// `/bin/bash` is an absolute path on purpose: on macOS it is the system bash
/// 3.2.57, the only shell in the supported range whose `source` cannot read
/// the `/dev/fd/N` of a process substitution. A `bash` taken from `PATH` is
/// usually a Homebrew 5.x and would not cover it.
#[allow(dead_code)]
pub(crate) const INTERPRETERS: &[&str] = &["/bin/bash", "zsh"];

/// The bash the single-interpreter cases run, overridable with `JLO_TEST_BASH`.
#[allow(dead_code)]
pub(crate) fn bash_bin() -> String {
    std::env::var("JLO_TEST_BASH").unwrap_or_else(|_| "bash".to_string())
}

/// Returns true when the caller should skip this interpreter. Prints loudly: a
/// silently skipped shell is indistinguishable from a passing one.
#[allow(dead_code)]
#[must_use]
pub(crate) fn skip_missing(test: &str, sh: &str) -> bool {
    if Command::new(sh).arg("-c").arg("exit 0").output().is_ok() {
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

/// A flat, jlo-managed JDK named `version` in the store under `home`.
#[allow(dead_code)]
pub(crate) fn install_fake_jdk(home: &Path, version: &str) -> PathBuf {
    let jdk = jdk_store_in(home).join(version);
    std::fs::create_dir_all(jdk.join("bin")).unwrap();
    std::fs::write(jdk.join("bin").join("java"), "").unwrap();
    std::fs::write(jdk.join(".jlo-managed"), "").unwrap();
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
