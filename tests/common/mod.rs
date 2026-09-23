//! Helpers shared by the integration test crates. Each crate uses a different
//! subset, so every item carries its own `dead_code` allow.

use std::path::Path;
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
