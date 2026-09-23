//! Tests for the shell wrapper in `shell/jlo-init-common.sh`.
//!
//! The wrapper ships as one file per dialect: the binary writes the one source
//! into `$JLO_HOME/bin/` under both dialect names and the generated `jlo.sh`
//! picks one at source time. Each test therefore sources the file belonging to
//! the interpreter it runs, which is exactly what a real shell does.
//!
//! The script is sourced by the user's interactive shell and its eval branch
//! evaluates the binary's stdout - but only a payload ending in the marker
//! the binary writes in wrapped mode, so help text, an older binary's output
//! and a cut-short payload never are. These tests drive real shells with
//! `JLO_HOME` pointing at the built binary, or at a stub standing in for it.
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

mod common;

use common::{INTERPRETERS, bash_bin, chmod, fake_jdk_archive, shells};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// Which wrapper file this interpreter would be handed by `jlo.sh`.
fn dialect(sh: &str) -> &'static str {
    if Path::new(sh).file_name().is_some_and(|n| n == "zsh") {
        "zsh"
    } else {
        "bash"
    }
}

/// A `JLO_HOME` whose `bin/` holds both wrapper dialects and a `jlo-bin`
/// symlink to the binary under test, which is what the wrapper expects to find.
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
    chmod(&stub, 0o755);
    home
}

fn init_sh_home() -> tempfile::TempDir {
    let home = tempfile::tempdir().unwrap();
    let bin = home.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    for name in WRAPPERS {
        std::fs::copy(shell_source(), bin.join(name)).unwrap();
    }
    home
}

/// The two wrapper dialects, named as they are under `$JLO_HOME/bin/`.
const WRAPPERS: &[&str] = &["jlo-init.bash", "jlo-init.zsh"];

/// A stub line printing the marker a payload has to end in to be evaluated.
const MARK: &str = "echo \"# jlo'end\"";

/// The one source both dialect files are written from.
fn shell_source() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("shell")
        .join("jlo-init-common.sh")
}

/// Source the wrapper dialect belonging to `sh` from `home`, then run `body`.
fn run_in(sh: &str, home: &Path, body: &str) -> Output {
    let script = format!(
        r#"
        export JLO_HOME="{}"
        . "$JLO_HOME/bin/jlo-init.{}"
        {body}
        "#,
        home.display(),
        dialect(sh),
    );

    Command::new(sh)
        .arg("-c")
        .arg(script)
        .env("JLO_ADOPTIUM_API_URL", "http://127.0.0.1:1")
        .output()
        .unwrap_or_else(|e| panic!("failed to run {sh}: {e}"))
}

/// Source the bash wrapper and run `jlo $args` against the real binary.
fn run_wrapper(args: &str) -> Output {
    let home = jlo_home();
    run_in(&bash_bin(), home.path(), &format!("jlo {args}"))
}

// ---------------------------------------------------------------------------
// Outside the eval branch, stdout is passed through
// ---------------------------------------------------------------------------

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
    for sh in shells("env_exports_reach_the_calling_shell", INTERPRETERS) {
        let home = jlo_home_with_stub(&format!("echo 'export JLO_PROBE=reached'\n{MARK}"));
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
    for sh in shells("use_alias_exports_reach_the_calling_shell", INTERPRETERS) {
        let home = jlo_home_with_stub(&format!("echo 'export JLO_PROBE=reached'\n{MARK}"));
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
    for sh in shells("env_propagates_the_binarys_failure_status", INTERPRETERS) {
        let home = jlo_home_with_stub("exit 3");
        let out = run_in(sh, home.path(), "jlo env\necho \"status=$?\"");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(stdout.contains("status=3"), "{sh}: got {stdout:?}");
    }
}

#[test]
fn env_reports_success_when_the_binary_succeeds() {
    for sh in shells("env_reports_success_when_the_binary_succeeds", INTERPRETERS) {
        let home = jlo_home_with_stub(&format!("echo 'export JLO_PROBE=reached'\n{MARK}"));
        let out = run_in(sh, home.path(), "jlo env\necho \"status=$?\"");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(stdout.contains("status=0"), "{sh}: got {stdout:?}");
    }
}

/// stdout is the environment channel; diagnostics travel on stderr and must
/// still reach the terminal rather than being swallowed by the capture.
#[test]
fn env_lets_the_binarys_stderr_through() {
    for sh in shells("env_lets_the_binarys_stderr_through", INTERPRETERS) {
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
    for sh in shells("env_does_not_leak_helper_variables", INTERPRETERS) {
        let home = jlo_home_with_stub(&format!("echo 'export JLO_PROBE=reached'\n{MARK}"));
        let out = run_in(
            sh,
            home.path(),
            "out=mine\nJ=mine\nrc=mine\njlo env\n\
             echo \"out=[${out-UNSET}] J=[${J-UNSET}] rc=[${rc-UNSET}]\"",
        );
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            stdout.contains("out=[mine] J=[mine] rc=[mine]"),
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

/// The one source is written out under both dialect names, so it has to
/// parse under both shells - but never under a POSIX sh. The Rust compiler
/// never looks at these bytes, so this is the cheapest half of the net that
/// catches a typo in them.
#[test]
fn each_wrapper_dialect_parses_under_its_own_shell() {
    for sh in shells(
        "each_wrapper_dialect_parses_under_its_own_shell",
        INTERPRETERS,
    ) {
        let out = Command::new(sh)
            .arg("-n")
            .arg(shell_source())
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "the wrapper does not parse under {sh}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

/// `install.sh` keeps the dual-parse requirement the wrappers shed: it is
/// fetched over the network and piped into whatever shell the user typed.
#[test]
fn the_installer_parses_under_every_supported_shell() {
    let installer = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("install.sh");
    for sh in shells(
        "the_installer_parses_under_every_supported_shell",
        INTERPRETERS.iter().chain(["/bin/sh"].iter()),
    ) {
        let out = Command::new(sh).arg("-n").arg(&installer).output().unwrap();
        assert!(
            out.status.success(),
            "install.sh does not parse under {sh}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

// ---------------------------------------------------------------------------
// selfupdate: the reload line, and a failure that must not be evaluated
// ---------------------------------------------------------------------------

/// Sources the wrapper, runs `jlo selfupdate`, and reports both the status and
/// whether the eval actually happened.
///
/// No `curl` stub any more: the update is the binary's job from 0.4.0 on, so
/// what this file still owns is the branch around it - capture stdout,
/// propagate a failure without evaluating anything, and eval the reload line
/// on success.
fn run_selfupdate(sh: &str, home: &Path, args: &str) -> Output {
    let script = format!(
        r#"
        export JLO_HOME="{}"
        . "$JLO_HOME/bin/jlo-init.{}"
        jlo selfupdate {args}
        echo "status=$?"
        echo "reloaded=${{JLO_TEST_RELOADED-no}}"
        "#,
        home.display(),
        dialect(sh),
    );
    Command::new(sh).arg("-c").arg(script).output().unwrap()
}

/// The point of moving `selfupdate` onto the `env`/`use` branch: the binary
/// prints the reload line on stdout and the wrapper evals it, so the shell
/// that ran the update ends up with the function the new binary generated.
#[test]
fn selfupdate_evals_the_reload_line() {
    for sh in shells("selfupdate_evals_the_reload_line", INTERPRETERS) {
        let home = jlo_home_with_stub(&format!("echo 'JLO_TEST_RELOADED=yes'\n{MARK}"));
        let out = run_selfupdate(sh, home.path(), "");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            stdout.contains("status=0"),
            "{sh}: a successful update did not report success: {stdout:?}"
        );
        assert!(
            stdout.contains("reloaded=yes"),
            "{sh}: the reload line was never evaluated: {stdout:?}"
        );
    }
}

/// "Already current" prints the marker alone, so the eval is a no-op - and
/// must not turn a successful update into a non-zero status.
#[test]
fn selfupdate_with_an_empty_stdout_still_succeeds() {
    for sh in shells(
        "selfupdate_with_an_empty_stdout_still_succeeds",
        INTERPRETERS,
    ) {
        let home = jlo_home_with_stub(&format!(
            "echo \"J'Lo 0.4.0 is already the latest version.\" >&2\n{MARK}"
        ));
        let out = run_selfupdate(sh, home.path(), "");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            stdout.contains("status=0"),
            "{sh}: an up-to-date install reported failure: {stdout:?}"
        );
        assert!(
            stdout.contains("reloaded=no"),
            "{sh}: something was evaluated from an empty stdout: {stdout:?}"
        );
    }
}

/// The bug that started this: a failed update must not look like a successful
/// one, and an unmarked reload line is never executed.
#[test]
fn selfupdate_propagates_a_failure_without_evaluating() {
    for sh in shells(
        "selfupdate_propagates_a_failure_without_evaluating",
        INTERPRETERS,
    ) {
        let home = jlo_home_with_stub("echo 'JLO_TEST_RELOADED=yes'\necho boom >&2\nexit 3");
        let out = run_selfupdate(sh, home.path(), "");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            stdout.contains("status=3"),
            "{sh}: the binary's status never reached the caller: {stdout:?}"
        );
        assert!(
            stdout.contains("reloaded=no"),
            "{sh}: a failed update was evaluated anyway: {stdout:?}"
        );
    }
}

/// `--help` and `--version` go to stdout, unmarked: printed, not evaluated.
#[test]
fn selfupdate_help_is_printed_not_evaluated() {
    for sh in shells("selfupdate_help_is_printed_not_evaluated", INTERPRETERS) {
        let home = jlo_home_with_stub("echo 'JLO_TEST_RELOADED=yes'");
        let out = run_selfupdate(sh, home.path(), "--help");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            stdout.contains("reloaded=no"),
            "{sh}: help output reached the eval: {stdout:?}"
        );
        assert!(
            stdout.contains("JLO_TEST_RELOADED=yes"),
            "{sh}: help output never reached the terminal: {stdout:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// The marker: only a payload ending in it is evaluated, whatever the status
// ---------------------------------------------------------------------------

/// Runs `jlo {args}` and reports the status and whether `JLO_PROBE` was set.
fn probe(sh: &str, home: &Path, args: &str) -> String {
    let out = run_in(
        sh,
        home,
        &format!("jlo {args}\necho \"status=$?\"\necho \"probe=[${{JLO_PROBE-}}]\""),
    );
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

/// `install` and `update` write the exports that move the shell before they
/// delete the build it is on, so a later name failing must not stop them
/// being evaluated - and the binary's own status reaches the caller, not a
/// generic 1, whether the eval succeeds or fails.
#[test]
fn a_marked_payload_is_evaluated_even_when_the_run_fails() {
    for sh in shells(
        "a_marked_payload_is_evaluated_even_when_the_run_fails",
        INTERPRETERS,
    ) {
        for (first, moved) in [("true", "moved"), ("false", "")] {
            let home = jlo_home_with_stub(&format!(
                "echo '{first} &&'\necho 'export JLO_PROBE=moved'\n{MARK}\necho boom >&2\nexit 7"
            ));
            for verb in ["update", "install 21"] {
                let out = probe(sh, home.path(), verb);
                assert!(out.contains("status=7"), "{sh} {verb} {first}: {out:?}");
                assert!(
                    out.contains(&format!("probe=[{moved}]")),
                    "{sh} {verb} {first}: {out:?}"
                );
            }
        }
    }
}

/// Source the wrapper into an interactive `sh`, switch comments off (bash's
/// `interactive_comments`, zsh's `interactivecomments`), then run `body`
/// under `set -eu`, which exits on the first failing `jlo`. [`run_in`]'s
/// shells are not interactive, and those always honour `#`. `+m`: no job
/// control, so the shell leaves the terminal running the tests alone.
fn run_interactive_without_comments(sh: &str, home: &Path, body: &str) -> Output {
    let (flags, comments_off): (&[&str], _) = if dialect(sh) == "zsh" {
        (&["-f", "+m", "-i"], "unsetopt interactivecomments")
    } else {
        (&["--norc", "+m", "-i"], "shopt -u interactive_comments")
    };
    let script = format!(
        r#"
        export JLO_HOME="{}"
        . "$JLO_HOME/bin/jlo-init.{}"
        {comments_off}
        set -eu
        {body}
        "#,
        home.display(),
        dialect(sh),
    );
    Command::new(sh)
        .args(flags)
        .arg("-c")
        .arg(script)
        .output()
        .unwrap_or_else(|e| panic!("failed to run {sh}: {e}"))
}

/// Without comments, the marker line is code - an unterminated quote - so
/// the wrapper must not hand it to `eval`, or every successful `jlo env`
/// fails after applying its exports and a `set -e` shell exits.
#[test]
fn a_marked_payload_is_evaluated_without_interactive_comments() {
    for sh in shells(
        "a_marked_payload_is_evaluated_without_interactive_comments",
        INTERPRETERS,
    ) {
        for (stub, reached) in [
            (
                format!("echo 'export JLO_PROBE=reached'\n{MARK}"),
                "reached",
            ),
            (MARK.to_string(), ""),
        ] {
            let home = jlo_home_with_stub(&stub);
            let out = run_interactive_without_comments(
                sh,
                home.path(),
                "jlo env\necho \"status=$? probe=[${JLO_PROBE-}]\"",
            );
            let stdout = String::from_utf8_lossy(&out.stdout);
            assert!(
                stdout.contains(&format!("status=0 probe=[{reached}]")),
                "{sh} {stub:?}: {stdout:?} {:?}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }
}

/// Help and version text is on stdout without the marker: printed, never
/// evaluated, status 0 - the argument scan that used to guard this is gone.
#[test]
fn help_through_the_eval_branch_is_printed_not_evaluated() {
    let home = jlo_home();
    for sh in shells(
        "help_through_the_eval_branch_is_printed_not_evaluated",
        INTERPRETERS,
    ) {
        for args in [
            "env --help",
            "env -h",
            "use -h",
            "update -hh",
            "install --help",
            "selfupdate -h",
        ] {
            let out = run_in(sh, home.path(), &format!("jlo {args}\necho \"status=$?\""));
            let stdout = String::from_utf8_lossy(&out.stdout);
            let stderr = String::from_utf8_lossy(&out.stderr);
            assert!(
                stdout.contains("Usage: jlo") && stdout.contains("status=0"),
                "{sh} {args}: {stdout:?} {stderr:?}"
            );
            assert!(
                stderr.is_empty(),
                "{sh} {args}: help was evaluated: {stderr:?}"
            );
        }
    }
}

/// A usage error goes to stderr with clap's status; stdout is empty, so
/// nothing is evaluated or printed.
#[test]
fn an_invalid_flag_fails_without_evaluating() {
    let home = jlo_home();
    for sh in shells("an_invalid_flag_fails_without_evaluating", INTERPRETERS) {
        let out = run_in(sh, home.path(), "jlo env --bogus\necho \"status=$?\"");
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_eq!(stdout, "status=2\n", "{sh}: {stderr:?}");
        assert!(stderr.contains("--bogus"), "{sh}: {stderr:?}");
    }
}

/// A payload cut short never ends in the marker - not even when a value
/// carries text that looks like it, since a quoted `'` is spelled `'\''`.
/// Cut just before the marker or inside a value, whatever the status, none
/// of it is evaluated.
#[test]
fn a_truncated_payload_is_not_evaluated() {
    for sh in shells("a_truncated_payload_is_not_evaluated", INTERPRETERS) {
        for cut in [
            r"export JLO_PROBE='x # jlo'\''end'",
            r"export JLO_PROBE='x # jlo'\''end' &&\nexport PATH='/cut",
        ] {
            for status in [0, 1] {
                let home = jlo_home_with_stub(&format!("printf '%b' \"{cut}\"\nexit {status}"));
                let out = probe(sh, home.path(), "env");
                assert!(
                    out.contains("probe=[]"),
                    "{sh} {cut} exit {status}: {out:?}"
                );
            }
        }
    }
}

/// A newer wrapper over an older binary: the prefix is an unknown
/// subcommand there, rejected before any JDK command runs.
#[test]
fn a_binary_that_rejects_the_prefix_is_not_evaluated() {
    for sh in shells(
        "a_binary_that_rejects_the_prefix_is_not_evaluated",
        INTERPRETERS,
    ) {
        let home = jlo_home_with_stub(&format!(
            "if [ \"$1\" = __wrapped ]; then echo \"error: unrecognized subcommand '__wrapped'\" >&2; exit 2; fi\n\
             echo 'export JLO_PROBE=old'\n{MARK}"
        ));
        let out = probe(sh, home.path(), "update");
        assert!(out.contains("status=2"), "{sh}: {out:?}");
        assert!(out.contains("probe=[]"), "{sh}: {out:?}");
    }
}

/// The payload is `&&`-joined, so a first statement that fails - a reload
/// `.` that cannot open its file - stops the rest and fails the call, even
/// though the binary succeeded.
#[test]
fn a_failing_first_statement_fails_the_call() {
    for sh in shells("a_failing_first_statement_fails_the_call", INTERPRETERS) {
        let home = jlo_home_with_stub(&format!(
            "echo \". '/nonexistent/jlo.sh' &&\"\necho 'export JLO_PROBE=reached'\n{MARK}"
        ));
        let out = probe(sh, home.path(), "selfupdate");
        assert!(!out.contains("status=0"), "{sh}: {out:?}");
        assert!(out.contains("probe=[]"), "{sh}: {out:?}");
    }
}

/// Unmarked output is printed; when that printing fails, the call fails.
#[test]
fn a_failed_print_of_unmarked_output_fails_the_call() {
    for sh in shells(
        "a_failed_print_of_unmarked_output_fails_the_call",
        INTERPRETERS,
    ) {
        let home = jlo_home_with_stub("echo 'Usage: jlo env'");
        let out = run_in(
            sh,
            home.path(),
            "if jlo env --help >&-; then echo status=0; else echo status=failed; fi",
        );
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            stdout.contains("status=failed"),
            "{sh}: {stdout:?} {:?}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

/// A shell that cannot assign `JAVA_HOME` or `PATH` could not follow a
/// deletion, so it is refused before the binary runs.
#[test]
fn a_read_only_environment_is_refused_before_the_binary_runs() {
    for sh in shells(
        "a_read_only_environment_is_refused_before_the_binary_runs",
        INTERPRETERS,
    ) {
        for var in ["JAVA_HOME", "PATH"] {
            let home = jlo_home_with_stub(&format!(
                "touch \"$JLO_HOME/called\"\necho 'export JLO_PROBE=reached'\n{MARK}"
            ));
            let out = run_in(
                sh,
                home.path(),
                &format!("readonly {var}\njlo update\necho \"status=$?\""),
            );
            let stdout = String::from_utf8_lossy(&out.stdout);
            let stderr = String::from_utf8_lossy(&out.stderr);
            assert!(stdout.contains("status=1"), "{sh} {var}: {stdout:?}");
            assert!(
                stderr.contains("JAVA_HOME or PATH is read-only"),
                "{sh} {var}: {stderr:?}"
            );
            assert!(
                !home.path().join("called").exists(),
                "{sh} {var}: the binary ran"
            );
        }
    }
}

/// The stub above pins the wrapper's half; this is the whole chain, real
/// binary included. Names go sorted: 17, which the shell is on, is replaced;
/// 21 is looked up fine but its download fails. The run fails, and the shell
/// must still end up on the new 17 - `JAVA_HOME` and `PATH` alike - because
/// the old 17 is gone and a failure later in the run does not bring it back.
#[test]
fn a_later_failure_still_moves_the_shell_off_the_replaced_build() {
    use sha2::Digest;

    for sh in shells(
        "a_later_failure_still_moves_the_shell_off_the_replaced_build",
        INTERPRETERS,
    ) {
        for verb in ["update", "install 17 21"] {
            let mut server = mockito::Server::new();
            let archive = fake_jdk_archive("jdk-17.0.9+10");
            let checksum = hex::encode(sha2::Sha256::digest(&archive));
            let mut latest = |major: &str, semver: &str, checksum: &str| {
                server
                    .mock(
                        "GET",
                        mockito::Matcher::Regex(format!(r"^/v3/assets/latest/{major}/hotspot")),
                    )
                    .match_query(mockito::Matcher::Any)
                    .with_body(format!(
                        r#"[{{"version":{{"semver":"{semver}"}},"binary":{{"package":{{"name":"jdk.tar.gz","link":"{}/jdk-{major}.tar.gz","checksum":"{checksum}"}}}}}}]"#,
                        server.url()
                    ))
                    .create()
            };
            let asked_17 = latest("17", "17.0.9+10", &checksum);
            let asked_21 = latest("21", "21.0.9+10", "00");
            let _pkg_17 = server
                .mock("GET", "/jdk-17.tar.gz")
                .with_body(archive)
                .create();
            let pkg_21 = server
                .mock("GET", "/jdk-21.tar.gz")
                .with_status(500)
                .create();

            let jlo = jlo_home();
            let home = tempfile::tempdir().unwrap();
            let store = if cfg!(target_os = "macos") {
                home.path().join("Library/Java/JavaVirtualMachines")
            } else {
                home.path().join(".jdks")
            };
            for version in ["17.0.5+8", "21.0.5+11"] {
                let jdk = store.join(version);
                std::fs::create_dir_all(jdk.join("bin")).unwrap();
                std::fs::write(jdk.join(".jlo-managed"), "").unwrap();
            }
            let old = store.join("17.0.5+8");
            let new = store.join("17.0.9+10");

            let out = run_in(
                sh,
                jlo.path(),
                &format!(
                    r#"
                    export HOME="{home}"
                    export JLO_ADOPTIUM_API_URL="{api}"
                    export JAVA_HOME="{old}"
                    export PATH="{old}/bin:$PATH"
                    jlo {verb}
                    echo "status=$?"
                    echo "java_home=$JAVA_HOME"
                    echo "path=$PATH"
                    "#,
                    home = home.path().display(),
                    api = server.url(),
                    old = old.display(),
                ),
            );
            let stdout = String::from_utf8_lossy(&out.stdout);
            let stderr = String::from_utf8_lossy(&out.stderr);
            let ctx = format!("{sh} {verb}: {stdout:?} / {stderr:?}");

            asked_17.assert();
            asked_21.assert();
            pkg_21.assert();
            assert!(stdout.contains("status=1"), "{ctx}");
            assert!(!old.exists(), "the replacement was undone: {ctx}");
            assert!(new.join("bin/java").exists(), "{ctx}");
            assert!(
                store.join("21.0.5+11").exists() && !store.join("21.0.9+10").exists(),
                "the failed name's install was touched: {ctx}"
            );
            assert!(
                stdout.contains(&format!("java_home={}\n", new.display())),
                "JAVA_HOME left on the deleted build: {ctx}"
            );
            let path = stdout
                .lines()
                .find_map(|line| line.strip_prefix("path="))
                .unwrap();
            assert!(
                path.starts_with(&format!("{}/bin:", new.display())),
                "PATH does not lead with the new build: {ctx}"
            );
            assert!(
                !path.contains(&format!("{}/bin", old.display())),
                "PATH still holds the deleted build: {ctx}"
            );
        }
    }
}

/// A profile may well run under `set -u`, and `jlo` with no arguments is the
/// ordinary way to ask for help. `case "$1"` aborted the shell there on an
/// unbound parameter before the binary was reached - the same failure
/// `tests/autoload.rs` already pins for the other file, and one the Rust
/// compiler never sees.
#[test]
fn a_bare_jlo_survives_set_u() {
    for sh in shells("a_bare_jlo_survives_set_u", INTERPRETERS) {
        let home = jlo_home_with_stub("echo reached-the-binary");
        let out = run_in(sh, home.path(), "set -u\njlo");
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stdout.contains("reached-the-binary"),
            "{sh}: the wrapper never reached the binary: {stdout:?} / {stderr:?}"
        );
        assert!(
            !stderr.contains("unbound") && !stderr.contains("parameter not set"),
            "{sh}: set -u aborted the wrapper: {stderr:?}"
        );
    }
}
