//! Tests for the shell wrapper in `jlo-init.sh`.
//!
//! The script is sourced by the user's interactive shell and its `env` branch
//! evaluates the binary's stdout. Anything printed there is executed, so help
//! output must never reach it. These tests drive real shells with `JLO_HOME`
//! pointing at the built binary, or at a stub standing in for it.
//!
//! Most cases run under every interpreter in [`INTERPRETERS`] rather than a
//! single one: the bug that motivated the capture-then-eval form was visible
//! only under macOS's bash 3.2, so a suite that tests whichever `bash` happens
//! to be first on `PATH` can miss it entirely.
//!
//! Set `JLO_TEST_BASH` to point the single-interpreter cases at a specific
//! bash.

// Test code: an `unwrap` failure here is a test failure, which is the point.
#![allow(clippy::unwrap_used)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// Every interpreter the wrapper must work under.
///
/// `/bin/bash` is an absolute path on purpose: on macOS it is the system bash
/// 3.2.57, the only shell in the supported range whose `source` cannot read
/// the `/dev/fd/N` of a process substitution. A `bash` taken from `PATH` is
/// usually a Homebrew 5.x and would not cover it.
const INTERPRETERS: &[&str] = &["/bin/bash", "zsh"];

fn bash_bin() -> String {
    std::env::var("JLO_TEST_BASH").unwrap_or_else(|_| "bash".to_string())
}

/// Returns true when the caller should skip this interpreter. Prints loudly: a
/// silently skipped shell is indistinguishable from a passing one.
#[must_use]
fn skip_missing(test: &str, sh: &str) -> bool {
    if Command::new(sh).arg("-c").arg("exit 0").output().is_ok() {
        return false;
    }
    eprintln!("SKIP {test}: {sh} is not installed here.");
    true
}

/// A `JLO_HOME` whose `bin/` holds `jlo-init.sh` and a `jlo-bin` symlink to the
/// binary under test, which is what the wrapper expects to find.
fn jlo_home() -> tempfile::TempDir {
    let home = init_sh_home();
    let target = assert_cmd::cargo::cargo_bin("jlo-bin");
    std::os::unix::fs::symlink(target, home.path().join("bin").join("jlo-bin")).unwrap();
    home
}

/// The same layout, but with `jlo-bin` replaced by a `/bin/sh` script. Lets a
/// test pin exactly what the binary writes to stdout, to stderr and exits
/// with, without a network round trip or a JDK on disk.
fn jlo_home_with_stub(body: &str) -> tempfile::TempDir {
    let home = init_sh_home();
    let stub = home.path().join("bin").join("jlo-bin");
    std::fs::write(&stub, format!("#!/bin/sh\n{body}\n")).unwrap();
    let mut perms = std::fs::metadata(&stub).unwrap().permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
    std::fs::set_permissions(&stub, perms).unwrap();
    home
}

fn init_sh_home() -> tempfile::TempDir {
    let home = tempfile::tempdir().unwrap();
    let bin = home.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    std::fs::copy(manifest.join("jlo-init.sh"), bin.join("jlo-init.sh")).unwrap();
    home
}

/// Source `jlo-init.sh` from `home` under `sh`, then run `body`.
fn run_in(sh: &str, home: &Path, body: &str) -> Output {
    let script = format!(
        r#"
        export JLO_HOME="{}"
        . "$JLO_HOME/bin/jlo-init.sh"
        {body}
        "#,
        home.display()
    );

    Command::new(sh)
        .arg("-c")
        .arg(script)
        .env("JLO_ADOPTIUM_API_URL", "http://127.0.0.1:1")
        .output()
        .unwrap_or_else(|e| panic!("failed to run {sh}: {e}"))
}

/// Source `jlo-init.sh` and run `jlo $args` against the real binary.
fn run_wrapper(args: &str) -> Output {
    let home = jlo_home();
    run_in(&bash_bin(), home.path(), &format!("jlo {args}"))
}

// ---------------------------------------------------------------------------
// Help must be printed, never evaluated
// ---------------------------------------------------------------------------

#[test]
fn env_help_is_printed_not_sourced() {
    let out = run_wrapper("env --help");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);

    // The help text reached the terminal ...
    assert!(
        stdout.contains("Usage: jlo env"),
        "expected help on stdout, got stdout={stdout:?} stderr={stderr:?}"
    );
    // ... and the shell did not try to execute it.
    assert!(
        !stderr.contains("command not found"),
        "help was sourced instead of printed: {stderr:?}"
    );
    assert!(out.status.success(), "wrapper failed: {stderr:?}");
}

#[test]
fn env_short_help_is_printed_not_sourced() {
    let out = run_wrapper("env -h");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("Usage: jlo env"), "got {stdout:?}");
}

#[test]
fn use_alias_help_is_printed_not_sourced() {
    let out = run_wrapper("use --help");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stdout.contains("Usage: jlo env"),
        "got {stdout:?} {stderr:?}"
    );
    assert!(!stderr.contains("command not found"), "{stderr:?}");
}

#[test]
fn top_level_help_still_works_through_the_wrapper() {
    let out = run_wrapper("--help");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("Usage: jlo"), "got {stdout:?}");
}

#[test]
fn exec_does_not_intercept_child_help_flags() {
    // `--help` here belongs to `echo`, not to jlo. If the wrapper hijacked it,
    // jlo's own exec help would appear instead.
    let out = run_wrapper("exec -- echo --help");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        !stdout.contains("Usage: jlo exec"),
        "wrapper intercepted a child's --help: {stdout:?}"
    );
}

// ---------------------------------------------------------------------------
// The exports must actually land in the calling shell, under every shell
// ---------------------------------------------------------------------------

/// The regression this file exists for. `. <(jlo-bin env)` silently set
/// nothing under bash 3.2 - the shell opened the `/dev/fd/N` pipe, read no
/// seekable content, and returned 0 - so `jlo env` was a no-op for every macOS
/// user on the system bash. Capture-then-eval works in every supported shell.
#[test]
fn env_exports_reach_the_calling_shell() {
    for sh in INTERPRETERS {
        if skip_missing("env_exports_reach_the_calling_shell", sh) {
            continue;
        }
        let home = jlo_home_with_stub("echo 'export JLO_PROBE=reached'");
        let out = run_in(sh, home.path(), "jlo env\necho \"probe=[${JLO_PROBE-}]\"");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            stdout.contains("probe=[reached]"),
            "{sh}: the binary's exports never reached the shell: \
             stdout={stdout:?} stderr={:?}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

#[test]
fn use_alias_exports_reach_the_calling_shell() {
    for sh in INTERPRETERS {
        if skip_missing("use_alias_exports_reach_the_calling_shell", sh) {
            continue;
        }
        let home = jlo_home_with_stub("echo 'export JLO_PROBE=reached'");
        let out = run_in(
            sh,
            home.path(),
            "jlo use 25\necho \"probe=[${JLO_PROBE-}]\"",
        );
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(stdout.contains("probe=[reached]"), "{sh}: got {stdout:?}");
    }
}

/// `. <(cmd)` returns 0 whatever `cmd` did - in *every* bash and zsh, not just
/// 3.2 - so the wrapper could not tell a successful `jlo env` from a failed
/// one. The capture form propagates it.
#[test]
fn env_propagates_the_binarys_failure_status() {
    for sh in INTERPRETERS {
        if skip_missing("env_propagates_the_binarys_failure_status", sh) {
            continue;
        }
        let home = jlo_home_with_stub("exit 3");
        let out = run_in(sh, home.path(), "jlo env\necho \"status=$?\"");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(stdout.contains("status=3"), "{sh}: got {stdout:?}");
    }
}

#[test]
fn env_reports_success_when_the_binary_succeeds() {
    for sh in INTERPRETERS {
        if skip_missing("env_reports_success_when_the_binary_succeeds", sh) {
            continue;
        }
        let home = jlo_home_with_stub("echo 'export JLO_PROBE=reached'");
        let out = run_in(sh, home.path(), "jlo env\necho \"status=$?\"");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(stdout.contains("status=0"), "{sh}: got {stdout:?}");
    }
}

/// A failing run must leave the environment exactly as it was. The binary
/// emits nothing to stdout when it fails, but the wrapper must not evaluate a
/// half-written line even if it someday did.
#[test]
fn env_does_not_evaluate_output_of_a_failing_run() {
    for sh in INTERPRETERS {
        if skip_missing("env_does_not_evaluate_output_of_a_failing_run", sh) {
            continue;
        }
        let home = jlo_home_with_stub("echo 'export JLO_PROBE=leaked'\nexit 3");
        let out = run_in(sh, home.path(), "jlo env\necho \"probe=[${JLO_PROBE-}]\"");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(stdout.contains("probe=[]"), "{sh}: got {stdout:?}");
    }
}

/// stdout is the environment channel; diagnostics travel on stderr and must
/// still reach the terminal rather than being swallowed by the capture.
#[test]
fn env_lets_the_binarys_stderr_through() {
    for sh in INTERPRETERS {
        if skip_missing("env_lets_the_binarys_stderr_through", sh) {
            continue;
        }
        let home = jlo_home_with_stub("echo 'jlo: nope' >&2\nexit 1");
        let out = run_in(sh, home.path(), "jlo env --offline");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.contains("jlo: nope"), "{sh}: got stderr {stderr:?}");
    }
}

/// The wrapper's scratch variables are `local`, so a `jlo env` must not clobber
/// or introduce anything in the user's namespace.
#[test]
fn env_does_not_leak_helper_variables() {
    for sh in INTERPRETERS {
        if skip_missing("env_does_not_leak_helper_variables", sh) {
            continue;
        }
        let home = jlo_home_with_stub("echo 'export JLO_PROBE=reached'");
        let out = run_in(
            sh,
            home.path(),
            "out=mine\nJ=mine\narg=mine\njlo env\n\
             echo \"out=[${out-UNSET}] J=[${J-UNSET}] arg=[${arg-UNSET}]\"",
        );
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            stdout.contains("out=[mine] J=[mine] arg=[mine]"),
            "{sh}: the wrapper leaked into the caller's namespace: {stdout:?}"
        );
    }
}

/// End to end against the real binary: an offline request the store cannot
/// satisfy must fail loudly rather than reporting success.
#[test]
fn real_binary_offline_miss_fails_through_the_wrapper() {
    let home = jlo_home();
    let out = run_in(
        &bash_bin(),
        home.path(),
        "jlo env --offline 99\necho \"status=$?\"",
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        !stdout.contains("status=0"),
        "an unsatisfiable offline request reported success: {stdout:?} \
         stderr={:?}",
        String::from_utf8_lossy(&out.stderr)
    );
}

// ---------------------------------------------------------------------------
// Parseability: the shipped files are sourced from profiles we do not control
// ---------------------------------------------------------------------------

/// `jlo-init.sh` is bash/zsh syntax by design, but it must at least parse
/// wherever it is sourced from - the profile line the installer prints is not
/// guarded by a shell test. The process substitution it used to contain was a
/// hard syntax error under a POSIX `sh`.
#[test]
fn shipped_scripts_parse_under_every_supported_shell() {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    for script in ["jlo-init.sh", "jlo-autoload.sh", "install.sh"] {
        for sh in INTERPRETERS.iter().chain(["/bin/sh"].iter()) {
            if skip_missing("shipped_scripts_parse_under_every_supported_shell", sh) {
                continue;
            }
            let out = Command::new(sh)
                .arg("-n")
                .arg(manifest.join(script))
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{script} does not parse under {sh}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }
}
