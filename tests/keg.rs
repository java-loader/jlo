//! The Homebrew keg: `jlo-bin __install --keg <dir> --binary <path>`, as the
//! formula's `install` step runs it, and the files it writes sourced into
//! real shells the way the caveats' profile line sources them.
//!
//! The formula runs in brew's sandbox, where `HOME` is a scratch directory, and
//! a user's shell sources the result later with its own `HOME` - so these
//! tests generate under one `HOME` and source under another.

// Test code: an `unwrap` failure here is a test failure, which is the point.
#![allow(clippy::unwrap_used)]

mod common;

use common::{INTERPRETERS, chmod, hermetic, jlo_bin, shells};
use std::path::{Path, PathBuf};
use std::process::Output;

/// Stands in for the keg's `jlo-bin`: the eval verb gets a marked export, every
/// other verb echoes the argv it received.
const ARGV_STUB: &str = r##"#!/bin/sh
case "$1" in
  __wrapped) echo 'export JLO_PROBE=reached'; echo "# jlo'end" ;;
  *) printf 'argv:'; printf ' [%s]' "$@"; echo ;;
esac
"##;

/// A keg generated into `<root>/share` with the binary at
/// `<root>/libexec/jlo-bin` (the stub), under a `HOME` of its own.
struct Keg {
    root: tempfile::TempDir,
}

impl Keg {
    fn share(&self) -> PathBuf {
        self.root.path().join("share")
    }

    fn binary(&self) -> PathBuf {
        self.root.path().join("libexec").join("jlo-bin")
    }

    /// The `HOME` brew's sandbox gives the formula.
    fn build_home(&self) -> PathBuf {
        self.root.path().join("brew_home")
    }
}

fn install_keg(args: &[&str], build_home: &Path) -> Output {
    hermetic(jlo_bin(), build_home)
        .arg("__install")
        .args(args)
        .output()
        .unwrap()
}

fn keg() -> Keg {
    let keg = Keg {
        root: tempfile::tempdir().unwrap(),
    };
    std::fs::create_dir_all(keg.build_home()).unwrap();
    std::fs::create_dir_all(keg.binary().parent().unwrap()).unwrap();
    std::fs::write(keg.binary(), ARGV_STUB).unwrap();
    chmod(&keg.binary(), 0o755);
    let out = install_keg(
        &[
            "--keg",
            keg.share().to_str().unwrap(),
            "--binary",
            keg.binary().to_str().unwrap(),
        ],
        &keg.build_home(),
    );
    assert!(out.status.success(), "keg install failed: {out:?}");
    keg
}

/// Every file under `dir`, relative, sorted.
fn files(dir: &Path) -> Vec<String> {
    let mut found = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(next) = stack.pop() {
        for entry in std::fs::read_dir(&next).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path);
            } else {
                found.push(
                    path.strip_prefix(dir)
                        .unwrap()
                        .to_string_lossy()
                        .into_owned(),
                );
            }
        }
    }
    found.sort();
    found
}

/// The keg holds the two entry files and their two implementation files -
/// no completions, which brew installs itself - and nothing is written under
/// the `HOME` the formula runs with, or printed.
#[test]
fn a_keg_holds_the_shell_files_and_nothing_goes_to_home() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("brew_home");
    std::fs::create_dir_all(&home).unwrap();
    let share = root.path().join("share");
    let out = install_keg(
        &[
            "--binary",
            "/opt/homebrew/opt/jlo/libexec/jlo-bin",
            "--keg",
            share.to_str().unwrap(),
        ],
        &home,
    );
    assert!(out.status.success(), "{out:?}");
    assert!(out.stdout.is_empty(), "stdout: {:?}", out.stdout);
    assert!(out.stderr.is_empty(), "stderr: {:?}", out.stderr);
    assert_eq!(
        files(&share),
        [
            "autoload.sh",
            "bin/jlo-autoload.sh",
            "bin/jlo-init.sh",
            "jlo.sh"
        ]
    );
    assert!(files(&home).is_empty(), "wrote to HOME: {:?}", files(&home));
    assert!(
        std::fs::read_to_string(share.join("jlo.sh"))
            .unwrap()
            .contains("_JLO_BIN='/opt/homebrew/opt/jlo/libexec/jlo-bin'"),
        "jlo.sh does not name the opt path"
    );
}

/// Source the keg's `jlo.sh` (and `autoload.sh`) with the user's `HOME`, then
/// run `body`.
fn source_keg(sh: &str, keg: &Keg, home: &Path, env: &[(&str, &str)], body: &str) -> Output {
    let share = keg.share();
    hermetic(sh, home)
        .arg("-c")
        .arg(format!(
            ". '{jlo}'\n. '{autoload}'\n{body}",
            jlo = share.join("jlo.sh").display(),
            autoload = share.join("autoload.sh").display(),
        ))
        .envs(env.iter().copied())
        .current_dir(home)
        .output()
        .unwrap()
}

/// What the caveats promise: the profile line makes `jlo` work. `JLO_HOME`
/// comes from the user's `HOME` when the line is sourced - not from the
/// sandbox the files were generated in - and is exported for the binary; the
/// wrapper runs the keg's binary, for a pass-through verb and for the eval
/// branch. A `JLO_HOME` the user set is kept.
#[test]
fn the_keg_profile_line_makes_jlo_work() {
    let keg = keg();
    for sh in shells("the_keg_profile_line_makes_jlo_work", INTERPRETERS) {
        let user = tempfile::tempdir().unwrap();
        let home = user.path();
        let custom = home.join("custom");
        let custom_str = custom.to_str().unwrap();
        for (env, expected) in [
            (vec![], home.join(".jlo")),
            (vec![("JLO_HOME", custom_str)], custom.clone()),
        ] {
            let out = source_keg(
                sh,
                &keg,
                home,
                &env,
                "sh -c 'echo \"home=$JLO_HOME\"'\njlo exec -- a\njlo env\n\
                 echo \"probe=[${JLO_PROBE-}]\"",
            );
            let stdout = String::from_utf8_lossy(&out.stdout);
            let stderr = String::from_utf8_lossy(&out.stderr);
            assert!(
                stdout.contains(&format!("home={}\n", expected.display())),
                "{sh} {env:?}: JLO_HOME not exported as {expected:?}: \
                 stdout={stdout:?} stderr={stderr:?}"
            );
            assert!(
                stdout.contains("argv: [exec] [--] [a]"),
                "{sh} {env:?}: exec did not reach the keg's binary: \
                 stdout={stdout:?} stderr={stderr:?}"
            );
            assert!(
                stdout.contains("probe=[reached]"),
                "{sh} {env:?}: env's export never reached the shell: \
                 stdout={stdout:?} stderr={stderr:?}"
            );
        }
    }
}

/// A profile under `set -u` in a shell without `HOME`: sourcing the keg's
/// lines must not abort it, and `JLO_HOME` stays unset rather than becoming
/// `/.jlo`.
#[test]
fn the_keg_lines_do_not_abort_a_nounset_shell_without_home() {
    let keg = keg();
    for sh in shells(
        "the_keg_lines_do_not_abort_a_nounset_shell_without_home",
        INTERPRETERS,
    ) {
        let user = tempfile::tempdir().unwrap();
        let share = keg.share();
        let out = hermetic(sh, user.path())
            .arg("-c")
            .arg(format!(
                "set -u\nunset HOME JLO_HOME\n. '{}'\n. '{}'\n\
                 echo \"survived jlo_home=[${{JLO_HOME-unset}}]\"",
                share.join("jlo.sh").display(),
                share.join("autoload.sh").display(),
            ))
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            stdout.contains("survived jlo_home=[unset]"),
            "{sh}: stdout={stdout:?} stderr={:?}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

/// The formula is the only caller, so a wrong call is a bug in it: refused
/// with 1, before anything is written.
#[test]
fn a_malformed_keg_call_writes_nothing() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("brew_home");
    std::fs::create_dir_all(&home).unwrap();
    let share = root.path().join("share");
    let share = share.to_str().unwrap();
    for args in [
        vec!["--keg", share],
        vec!["--keg", share, "--binary"],
        vec!["--keg", "share", "--binary", "/opt/jlo-bin"],
        vec!["--keg", share, "--binary", "jlo-bin"],
        vec!["--keg", share, "--keg", share, "--binary", "/opt/jlo-bin"],
        vec!["--keg", share, "--binary", "/opt/jlo-bin", "--publish-self"],
    ] {
        let out = install_keg(&args, &home);
        assert_eq!(out.status.code(), Some(1), "{args:?}: {out:?}");
        assert!(
            !Path::new(share).exists(),
            "{args:?} wrote {:?}",
            files(Path::new(share))
        );
        assert!(files(&home).is_empty(), "{args:?} wrote to HOME");
    }
}
