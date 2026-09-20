//! Tests for the shell wrapper in `jlo-init.sh`.
//!
//! The script is sourced by the user's interactive shell and its `env` branch
//! *sources* the binary's stdout. Anything printed there is executed, so help
//! output must never reach it. These tests drive a real `bash` with `JLO_HOME`
//! pointing at the built binary.
//!
//! Set `JLO_TEST_BASH` to exercise a specific interpreter.

// Test code: an `unwrap` failure here is a test failure, which is the point.
#![allow(clippy::unwrap_used)]

use std::path::PathBuf;
use std::process::Command;

fn bash_bin() -> String {
    std::env::var("JLO_TEST_BASH").unwrap_or_else(|_| "bash".to_string())
}

/// A `JLO_HOME` whose `bin/` holds `jlo-init.sh` and a `jlo-bin` symlink to the
/// binary under test, which is what the wrapper expects to find.
fn jlo_home() -> tempfile::TempDir {
    let home = tempfile::tempdir().unwrap();
    let bin = home.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();

    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    std::fs::copy(manifest.join("jlo-init.sh"), bin.join("jlo-init.sh")).unwrap();

    let target = assert_cmd::cargo::cargo_bin("jlo-bin");
    std::os::unix::fs::symlink(target, bin.join("jlo-bin")).unwrap();

    home
}

/// Source `jlo-init.sh` and run `jlo $args` in a non-interactive bash.
fn run_wrapper(args: &str) -> std::process::Output {
    let home = jlo_home();
    let script = format!(
        r#"
        export JLO_HOME="{}"
        source "$JLO_HOME/bin/jlo-init.sh"
        jlo {args}
        "#,
        home.path().display()
    );

    Command::new(bash_bin())
        .arg("-c")
        .arg(script)
        .env("JLO_ADOPTIUM_API_URL", "http://127.0.0.1:1")
        .output()
        .unwrap()
}

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
    // ... and bash did not try to execute it.
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
