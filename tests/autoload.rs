//! Tests for the shell integration in `shell/jlo-autoload.{bash,zsh}` and the
//! `shell/jlo-autoload-common.sh` the binary appends to each.
//!
//! The two dialects are separate files: the binary writes both into
//! `$JLO_HOME/bin/` and the generated `autoload.sh` picks one at source time,
//! so neither file ever has to parse under the other shell. These tests source
//! each dialect under the shell it is written for - a POSIX `sh` never reaches
//! either, which `tests/install_sh.rs` asserts on the dispatch itself.
//!
//! The script is sourced by the user's interactive shell, so these tests drive
//! a real `bash` and inspect the hook it registers. `JLO_HOME` and the working
//! directory both point at empty temp dirs so the script's "fresh shell" branch
//! finds neither `.jlorc` nor `default.jlorc` and stays quiet.
//!
//! Set `JLO_TEST_BASH` to exercise a specific interpreter, e.g. a bash >= 5.1
//! that honours an array-valued `PROMPT_COMMAND`:
//!
//! ```text
//! JLO_TEST_BASH=/opt/homebrew/bin/bash cargo test --release --test autoload
//! ```

// Test code: an `unwrap` failure here is a test failure, which is the point.
#![allow(clippy::unwrap_used)]

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use tempfile::tempdir;

/// Separates the `declare -p` line from the element dump.
const MARKER: &str = "--8<--";

/// The dialect as `__install` writes it: its own registration, then the
/// common rest - the same `concat!` `src/install.rs` compiles in, assembled
/// once per test run.
fn autoload_script(dialect: &str) -> String {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    let dir = DIR.get_or_init(|| {
        let shell = Path::new(env!("CARGO_MANIFEST_DIR")).join("shell");
        let common = std::fs::read_to_string(shell.join("jlo-autoload-common.sh")).unwrap();
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("autoload");
        std::fs::create_dir_all(&dir).unwrap();
        for dialect in ["bash", "zsh"] {
            let name = format!("jlo-autoload.{dialect}");
            let head = std::fs::read_to_string(shell.join(&name)).unwrap();
            std::fs::write(dir.join(&name), head + &common).unwrap();
        }
        dir
    });
    dir.join(format!("jlo-autoload.{dialect}"))
        .display()
        .to_string()
}

fn bash_bin() -> String {
    std::env::var("JLO_TEST_BASH").unwrap_or_else(|_| "bash".to_string())
}

/// The state of `PROMPT_COMMAND` after sourcing.
struct PromptCommand {
    /// `declare -p PROMPT_COMMAND` output, or `UNSET`. Carries the variable's
    /// type and export flag, which a plain element dump would hide.
    decl: String,
    /// One entry per command bash will run. A scalar yields exactly one.
    elements: Vec<String>,
}

impl PromptCommand {
    fn is_array(&self) -> bool {
        // `declare -a` / `declare -ax` ... the flags sit between "declare -" and
        // the variable name.
        self.decl
            .split_whitespace()
            .nth(1)
            .is_some_and(|flags| flags.starts_with('-') && flags.contains('a'))
    }

    fn is_exported(&self) -> bool {
        self.decl
            .split_whitespace()
            .nth(1)
            .is_some_and(|flags| flags.starts_with('-') && flags.contains('x'))
    }

    fn hook_elements(&self) -> usize {
        self.elements
            .iter()
            .filter(|e| e.split(';').any(|tok| tok.trim() == "jlo_after_cd"))
            .count()
    }
}

/// Run `prologue`, then source the bash dialect `times` times in one bash
/// session, and report the resulting `PROMPT_COMMAND`.
fn source_script(prologue: &str, times: usize) -> PromptCommand {
    let cwd = tempdir().unwrap();
    let jlo_home = tempdir().unwrap();
    let script = autoload_script("bash");

    // `set -e` so a failure inside the sourced script fails the test instead of
    // being masked by the trailing printf's exit status.
    let mut body = String::from("set -e\n");
    body.push_str(prologue);
    body.push('\n');
    for _ in 0..times {
        writeln!(body, ". '{script}'").unwrap();
    }
    body.push_str("declare -p PROMPT_COMMAND 2>/dev/null || echo UNSET\n");
    writeln!(body, "printf '%s\\n' '{MARKER}'").unwrap();
    // Unset expands to zero words, so nothing is printed.
    body.push_str("printf '%s\\0' \"${PROMPT_COMMAND[@]}\"\n");

    let out = Command::new(bash_bin())
        .arg("-c")
        .arg(&body)
        .current_dir(cwd.path())
        .env("JLO_HOME", jlo_home.path())
        .env_remove("PROMPT_COMMAND")
        .output()
        .unwrap_or_else(|e| panic!("failed to run {}: {e}", bash_bin()));

    assert!(
        out.status.success(),
        "bash failed ({}): {}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );

    let stdout = String::from_utf8(out.stdout).unwrap();
    let (decl, dump) = stdout
        .split_once(&format!("{MARKER}\n"))
        .unwrap_or_else(|| panic!("marker missing in bash output: {stdout:?}"));

    PromptCommand {
        decl: decl.trim().to_string(),
        elements: dump
            .split('\0')
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect(),
    }
}

/// bash only honours an array-valued `PROMPT_COMMAND` from 5.1 on; before that
/// only element 0 ever runs, so array-shaped expectations are meaningless.
fn array_prompt_command_supported() -> bool {
    Command::new(bash_bin())
        .arg("-c")
        .arg("(( BASH_VERSINFO[0] > 5 || (BASH_VERSINFO[0] == 5 && BASH_VERSINFO[1] >= 1) ))")
        .output()
        .is_ok_and(|o| o.status.success())
}

/// Returns true when the caller should bail out. Prints loudly: a silently
/// skipped test is indistinguishable from a passing one.
#[must_use]
fn skip_without_array_support(test: &str) -> bool {
    if array_prompt_command_supported() {
        return false;
    }
    eprintln!(
        "SKIP {test}: {} predates 5.1 and ignores array PROMPT_COMMAND. \
         Re-run with JLO_TEST_BASH=<bash>=5.1> to cover it.",
        bash_bin()
    );
    true
}

// ---------------------------------------------------------------------------
// Scalar PROMPT_COMMAND
// ---------------------------------------------------------------------------

#[test]
fn registers_hook_when_prompt_command_is_unset() {
    let pc = source_script("unset PROMPT_COMMAND", 1);
    assert_eq!(pc.elements, vec!["jlo_after_cd"]);
}

#[test]
fn registers_hook_when_prompt_command_is_empty() {
    let pc = source_script("PROMPT_COMMAND=", 1);
    assert_eq!(pc.elements, vec!["jlo_after_cd"]);
}

#[test]
fn prepends_hook_before_existing_command() {
    let pc = source_script("PROMPT_COMMAND='__user_hook'", 1);
    assert_eq!(pc.elements, vec!["jlo_after_cd;__user_hook"]);
}

#[test]
fn does_not_duplicate_hook_when_resourced() {
    let pc = source_script("PROMPT_COMMAND='__user_hook'", 3);
    assert_eq!(pc.elements, vec!["jlo_after_cd;__user_hook"]);
    assert_eq!(pc.hook_elements(), 1);
}

/// An exported `PROMPT_COMMAND` must stay an exported scalar - it must not be
/// silently converted to an array.
#[test]
fn keeps_exported_scalar_exported_and_scalar() {
    let pc = source_script("export PROMPT_COMMAND='__user_hook'", 1);
    assert!(pc.is_exported(), "export lost; declare was {:?}", pc.decl);
    assert!(!pc.is_array(), "became an array; declare was {:?}", pc.decl);
    assert_eq!(pc.elements, vec!["jlo_after_cd;__user_hook"]);
}

// ---------------------------------------------------------------------------
// Registration must recognise *our* hook, not merely its name
// ---------------------------------------------------------------------------

/// `echo jlo_after_cd` mentions the hook but does not run it, so J'Lo must
/// still register. A substring test would wrongly treat this as registered.
#[test]
fn registers_when_hook_name_only_appears_as_an_argument() {
    let pc = source_script("PROMPT_COMMAND='echo jlo_after_cd'", 1);
    assert_eq!(pc.elements, vec!["jlo_after_cd;echo jlo_after_cd"]);
}

/// A different function whose name merely contains ours is not our hook.
#[test]
fn registers_alongside_similarly_named_hook() {
    let pc = source_script("PROMPT_COMMAND='other_jlo_after_cd_hook'", 1);
    assert_eq!(pc.elements, vec!["jlo_after_cd;other_jlo_after_cd_hook"]);
}

// ---------------------------------------------------------------------------
// Array PROMPT_COMMAND (bash >= 5.1)
// ---------------------------------------------------------------------------

#[test]
fn array_keeps_every_user_element_intact() {
    if skip_without_array_support("array_keeps_every_user_element_intact") {
        return;
    }
    let pc = source_script("PROMPT_COMMAND=(__first __second)", 1);
    assert!(pc.is_array(), "declare was {:?}", pc.decl);
    assert_eq!(pc.elements, vec!["jlo_after_cd", "__first", "__second"]);
}

#[test]
fn array_does_not_duplicate_hook_when_resourced() {
    if skip_without_array_support("array_does_not_duplicate_hook_when_resourced") {
        return;
    }
    let pc = source_script("PROMPT_COMMAND=(__first __second)", 3);
    assert_eq!(pc.elements, vec!["jlo_after_cd", "__first", "__second"]);
}

/// Another integration may prepend its own element after J'Lo registered, so
/// the hook is not necessarily at index 0 when the file is sourced again.
#[test]
fn array_finds_hook_outside_element_zero() {
    if skip_without_array_support("array_finds_hook_outside_element_zero") {
        return;
    }
    let pc = source_script("PROMPT_COMMAND=(__other jlo_after_cd)", 1);
    assert_eq!(pc.elements, vec!["__other", "jlo_after_cd"]);
    assert_eq!(pc.hook_elements(), 1);
}

#[test]
fn empty_array_gets_the_hook() {
    if skip_without_array_support("empty_array_gets_the_hook") {
        return;
    }
    let pc = source_script("PROMPT_COMMAND=()", 1);
    assert_eq!(pc.elements, vec!["jlo_after_cd"]);
}

// ---------------------------------------------------------------------------
// zsh uses add-zsh-hook and must not touch PROMPT_COMMAND at all
// ---------------------------------------------------------------------------

#[test]
fn zsh_registers_chpwd_hook_once_and_leaves_prompt_command_alone() {
    let cwd = tempdir().unwrap();
    let jlo_home = tempdir().unwrap();
    let script = autoload_script("zsh");

    let body = format!(
        "set -e\n. '{script}'\n. '{script}'\n. '{script}'\n\
         print -r -- \"hooks=$chpwd_functions\"\nprint -r -- \"pc=${{PROMPT_COMMAND-}}\"\n"
    );

    let out = Command::new("zsh")
        .arg("-c")
        .arg(&body)
        .current_dir(cwd.path())
        .env("JLO_HOME", jlo_home.path())
        .env_remove("PROMPT_COMMAND")
        .output()
        .expect("failed to run zsh");

    assert!(
        out.status.success(),
        "zsh failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(
        stdout.contains("hooks=jlo_after_cd\n"),
        "expected exactly one chpwd hook, got {stdout:?}"
    );
    assert!(
        stdout.contains("pc=\n"),
        "zsh must not touch PROMPT_COMMAND, got {stdout:?}"
    );
}

// ---------------------------------------------------------------------------
// Behaviour shared by both dialects
// ---------------------------------------------------------------------------
//
// Everything from here down runs under *both* shells. The two dialect files
// each carry their own copy of `jlo_find_jlorc` and `jlo_after_cd`, so a rule
// that is asserted in only one of them - `--offline`, the `$PWD` guard, the
// `return 0`, the `|| :` at the tail - is a rule that can silently disappear
// from the other. These tests run each dialect under the shell that will
// actually source it.

/// The shells that source a dialect, paired with the file they get.
fn hook_shells() -> Vec<(String, &'static str)> {
    vec![(bash_bin(), "bash"), ("zsh".to_string(), "zsh")]
}

#[must_use]
fn skip_missing(test: &str, sh: &str) -> bool {
    if Command::new(sh).arg("-c").arg("exit 0").output().is_ok() {
        return false;
    }
    eprintln!("SKIP {test}: {sh} is not installed here.");
    true
}

/// Run `body` under `sh` with `HOME`, `JLO_HOME` and the working directory
/// pinned to the given temp trees, and hand back stdout. The stub `jlo`
/// defined by the callers below records what the script asked for.
fn run_sh(sh: &str, body: &str, home: &Path, jlo_home: &Path, cwd: &Path) -> String {
    let out = Command::new(sh)
        .arg("-c")
        .arg(body)
        .current_dir(cwd)
        .env("HOME", home)
        .env("JLO_HOME", jlo_home)
        .env_remove("PROMPT_COMMAND")
        .output()
        .unwrap_or_else(|e| panic!("failed to run {sh}: {e}"));

    assert!(
        out.status.success(),
        "{sh} failed ({}): {}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

// ---------------------------------------------------------------------------
// Walk-up lookup: the hook must see a project's .jlorc from a subdirectory
// ---------------------------------------------------------------------------

/// Source the dialect from an empty directory (so the fresh-shell branch stays
/// quiet), `cd` to `start`, and report what `jlo_find_jlorc` decides.
///
/// `HOME` is the temp tree rather than the real one: the search stops there,
/// and a test that walked up into the developer's actual home would depend on
/// whatever lives in it. The inert `jlo` stub is there for zsh, where the `cd`
/// below fires the chpwd hook for real - without it an undefined `jlo` would
/// abort the script under `set -e` before the assertion ran.
fn finds_jlorc(sh: &str, dialect: &str, home: &Path, start: &Path) -> bool {
    let neutral = tempdir().unwrap();
    let jlo_home = tempdir().unwrap();
    let script = autoload_script(dialect);

    let body = format!(
        // `type` under `set -e`: without it a missing function would exit
        // non-zero and read as an honest "not found".
        "set -e\njlo() {{ :; }}\n. '{script}'\ntype jlo_find_jlorc >/dev/null\ncd '{}'\n\
         jlo_find_jlorc && echo FOUND || echo NONE\n",
        start.display()
    );

    let stdout = run_sh(sh, &body, home, jlo_home.path(), neutral.path());
    match stdout.trim() {
        "FOUND" => true,
        "NONE" => false,
        other => panic!("{sh}: unexpected output {other:?}"),
    }
}

/// `tempdir()` hands back `/var/...` on macOS while `$PWD` after `cd` shows
/// `/private/var/...`; the `$HOME` boundary is a string comparison.
fn canon(p: &Path) -> std::path::PathBuf {
    p.canonicalize().unwrap()
}

/// Builds a `HOME` tree, then asserts what every dialect makes of it.
fn assert_lookup(test: &str, build: impl Fn(&Path) -> std::path::PathBuf, expected: bool) {
    for (sh, dialect) in hook_shells() {
        if skip_missing(test, &sh) {
            continue;
        }
        let home = tempdir().unwrap();
        let home = canon(home.path());
        let start = build(&home);
        assert_eq!(
            finds_jlorc(&sh, dialect, &home, &start),
            expected,
            "{sh} ({dialect}) disagrees about {start:?}"
        );
    }
}

#[test]
fn find_walks_up_from_subdirectory() {
    assert_lookup(
        "find_walks_up_from_subdirectory",
        |home| {
            let project = home.join("project");
            let deep = project.join("src").join("main");
            std::fs::create_dir_all(&deep).unwrap();
            std::fs::write(project.join(".jlorc"), "21\n").unwrap();
            deep
        },
        true,
    );
}

/// The hook reacts to the *presence* of a `.jlorc`, never to its contents -
/// which is why widening the grammar costs nothing on the shell side. A hook
/// that parsed the value would need this rule in two languages, and this test
/// is what makes such a change announce itself.
#[test]
fn find_does_not_read_the_pinned_value() {
    assert_lookup(
        "find_does_not_read_the_pinned_value",
        |home| {
            let project = home.join("project");
            std::fs::create_dir_all(&project).unwrap();
            std::fs::write(project.join(".jlorc"), "28-ea\n").unwrap();
            project
        },
        true,
    );
}

#[test]
fn find_stops_at_vcs_root() {
    assert_lookup(
        "find_stops_at_vcs_root",
        |home| {
            let outer = home.join("outer");
            let repo = outer.join("repo");
            let deep = repo.join("src");
            std::fs::create_dir_all(&deep).unwrap();
            std::fs::create_dir_all(repo.join(".git")).unwrap();
            std::fs::write(outer.join(".jlorc"), "17\n").unwrap();
            deep
        },
        false,
    );
}

/// The same rule as `find_project_config_stops_at_a_worktree_whose_git_is_a_file`
/// in `src/conf.rs`, in the other implementation. Two tests for one rule are
/// justified here for the reason ADR-0004 gives: the walk exists twice, in
/// Rust and in shell, and the shell half is `[ -e ]` rather than `[ -d ]`.
/// Nothing but this notices if one dialect drifts to `-d`.
#[test]
fn find_stops_at_a_worktree_whose_git_is_a_file() {
    assert_lookup(
        "find_stops_at_a_worktree_whose_git_is_a_file",
        |home| {
            let outer = home.join("outer");
            let worktree = outer.join("worktree");
            let deep = worktree.join("src");
            std::fs::create_dir_all(&deep).unwrap();
            std::fs::write(
                worktree.join(".git"),
                "gitdir: /elsewhere/.git/worktrees/w\n",
            )
            .unwrap();
            std::fs::write(outer.join(".jlorc"), "17\n").unwrap();
            deep
        },
        false,
    );
}

/// `$HOME` is the other boundary. Built the other way round - the `.jlorc`
/// sits *above* the home directory - so `home` here is a subdirectory of the
/// temp tree rather than the tree itself.
#[test]
fn find_stops_at_home() {
    for (sh, dialect) in hook_shells() {
        if skip_missing("find_stops_at_home", &sh) {
            continue;
        }
        let outside = tempdir().unwrap();
        let outside = canon(outside.path());
        let home = outside.join("home");
        let project = home.join("project");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(outside.join(".jlorc"), "17\n").unwrap();

        assert!(
            !finds_jlorc(&sh, dialect, &home, &project),
            "{sh} ({dialect}) walked past $HOME"
        );
    }
}

/// The same boundary, with a `HOME` carrying a trailing slash. The Rust side
/// compares `Path`s, which are component-wise and so already ignore it; a
/// plain string compare here never matched, so the hook walked past home and
/// fired `jlo env --offline` on a `.jlorc` the binary would then refuse to
/// read - the two halves of one rule disagreeing about where home is.
#[test]
fn find_stops_at_home_with_a_trailing_slash() {
    for (sh, dialect) in hook_shells() {
        if skip_missing("find_stops_at_home_with_a_trailing_slash", &sh) {
            continue;
        }
        let outside = tempdir().unwrap();
        let outside = canon(outside.path());
        let home = outside.join("home");
        let project = home.join("project");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(outside.join(".jlorc"), "17\n").unwrap();

        let home_with_slash = std::path::PathBuf::from(format!("{}/", home.display()));
        assert!(
            !finds_jlorc(&sh, dialect, &home_with_slash, &project),
            "{sh} ({dialect}) walked past a $HOME spelled with a trailing slash"
        );
    }
}

#[test]
fn find_reports_none_when_absent() {
    assert_lookup(
        "find_reports_none_when_absent",
        |home| {
            let project = home.join("project");
            std::fs::create_dir_all(&project).unwrap();
            project
        },
        false,
    );
}

// ---------------------------------------------------------------------------
// What the hook actually asks the binary for
// ---------------------------------------------------------------------------

/// The behaviour that matters: `cd` into a subdirectory of a project must set
/// the env, not leave it to the user default. A stub `jlo` records the call.
///
/// `--offline` is part of the assertion, not incidental: without it a `cd`
/// into a project pinning an uninstalled JDK stalls the shell on a
/// several-hundred-megabyte download.
#[test]
fn hook_runs_jlo_env_from_a_project_subdirectory() {
    for (sh, dialect) in hook_shells() {
        if skip_missing("hook_runs_jlo_env_from_a_project_subdirectory", &sh) {
            continue;
        }
        let home = tempdir().unwrap();
        let home = canon(home.path());
        let project = home.join("project");
        let deep = project.join("src");
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::write(project.join(".jlorc"), "21\n").unwrap();

        let neutral = tempdir().unwrap();
        let jlo_home = tempdir().unwrap();
        let script = autoload_script(dialect);
        // zsh runs the hook on the `cd` itself; the explicit call after it is
        // what bash needs, and the `$PWD` guard makes it a no-op for zsh. One
        // line of output either way is the assertion.
        let body = format!(
            "set -e\njlo() {{ echo \"jlo $*\"; }}\n. '{script}'\ncd '{}'\njlo_after_cd\n",
            deep.display()
        );

        let stdout = run_sh(&sh, &body, &home, jlo_home.path(), neutral.path());
        assert_eq!(stdout.trim(), "jlo env --offline", "{sh} ({dialect})");
    }
}

/// The fresh-shell branch at the tail of the script. A new terminal is the
/// worst possible place to start a download, so it too must ask offline.
#[test]
fn fresh_shell_applies_the_user_default_offline() {
    for (sh, dialect) in hook_shells() {
        if skip_missing("fresh_shell_applies_the_user_default_offline", &sh) {
            continue;
        }
        let home = tempdir().unwrap();
        let home = canon(home.path());
        let neutral = home.join("neutral");
        std::fs::create_dir_all(&neutral).unwrap();

        let jlo_home = tempdir().unwrap();
        std::fs::write(jlo_home.path().join("default.jlorc"), "21\n").unwrap();

        let script = autoload_script(dialect);
        let body = format!("set -e\njlo() {{ echo \"jlo $*\"; }}\n. '{script}'\n");

        let stdout = run_sh(&sh, &body, &home, jlo_home.path(), &neutral);
        assert_eq!(stdout.trim(), "jlo env --offline", "{sh} ({dialect})");
    }
}

/// With neither a `.jlorc` above the cwd nor a `default.jlorc`, the fresh-shell
/// branch must not run the binary at all - there is nothing for it to resolve.
#[test]
fn fresh_shell_stays_quiet_without_any_config() {
    for (sh, dialect) in hook_shells() {
        if skip_missing("fresh_shell_stays_quiet_without_any_config", &sh) {
            continue;
        }
        let home = tempdir().unwrap();
        let home = canon(home.path());
        let neutral = home.join("neutral");
        std::fs::create_dir_all(&neutral).unwrap();
        let jlo_home = tempdir().unwrap();

        let script = autoload_script(dialect);
        let body = format!("set -e\njlo() {{ echo \"jlo $*\"; }}\n. '{script}'\n");

        let stdout = run_sh(&sh, &body, &home, jlo_home.path(), &neutral);
        assert_eq!(stdout.trim(), "", "{sh} ({dialect}) ran jlo for nothing");
    }
}

/// bash runs the hook from `PROMPT_COMMAND`, i.e. before every prompt, but the
/// body is guarded on `$PWD` changing. Three prompts in one directory are one
/// call - which is what keeps the "not installed" line from repeating under
/// every command the user runs there. zsh only fires on an actual `cd`, but
/// carries the same guard so a hand call behaves identically.
#[test]
fn hook_runs_once_per_directory_not_once_per_prompt() {
    for (sh, dialect) in hook_shells() {
        if skip_missing("hook_runs_once_per_directory_not_once_per_prompt", &sh) {
            continue;
        }
        let home = tempdir().unwrap();
        let home = canon(home.path());
        let project = home.join("project");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join(".jlorc"), "21\n").unwrap();

        let neutral = tempdir().unwrap();
        let jlo_home = tempdir().unwrap();
        let script = autoload_script(dialect);
        let body = format!(
            "set -e\njlo() {{ echo \"jlo $*\"; }}\n. '{script}'\ncd '{}'\n\
             jlo_after_cd\njlo_after_cd\njlo_after_cd\n",
            project.display()
        );

        let stdout = run_sh(&sh, &body, &home, jlo_home.path(), neutral.path());
        assert_eq!(stdout.trim(), "jlo env --offline", "{sh} ({dialect})");
    }
}

/// The hook runs between the user's command and their prompt. A version the
/// store cannot satisfy makes `jlo env --offline` exit non-zero, and that must
/// not become the status their prompt reports.
#[test]
fn hook_reports_success_even_when_jlo_env_fails() {
    for (sh, dialect) in hook_shells() {
        if skip_missing("hook_reports_success_even_when_jlo_env_fails", &sh) {
            continue;
        }
        let home = tempdir().unwrap();
        let home = canon(home.path());
        let project = home.join("project");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join(".jlorc"), "99\n").unwrap();

        let neutral = tempdir().unwrap();
        let jlo_home = tempdir().unwrap();
        let script = autoload_script(dialect);
        // Both with and without `set -e`. Without it the failure shows up as
        // a status the prompt would report; with it, as a shell that is simply
        // gone - POSIX exempts every command of an AND-OR list from `set -e`
        // except the last, so `jlo_find_jlorc && jlo env --offline` took the
        // whole shell down before the `return 0` could run. A profile that cds
        // after sourcing the hook is enough to reach it.
        for prologue in ["", "set -e\n"] {
            let body = format!(
                "{prologue}jlo() {{ return 1; }}\n. '{script}'\ncd '{}'\n\
                 _JLO_LAST_DIR=\njlo_after_cd\necho \"status=$?\"\n",
                project.display()
            );

            let stdout = run_sh(&sh, &body, &home, jlo_home.path(), neutral.path());
            assert_eq!(
                stdout.trim(),
                "status=0",
                "{sh} ({dialect}) with prologue {prologue:?}"
            );
        }
    }
}

/// The fresh-shell branch runs while the profile is still being sourced. Since
/// `jlo env` propagates the binary's status, an unsatisfiable `--offline`
/// lookup there would abort a profile running under `set -e` - taking the rest
/// of the user's shell setup with it.
#[test]
fn fresh_shell_survives_a_failing_jlo_env_under_set_e() {
    for (sh, dialect) in hook_shells() {
        if skip_missing("fresh_shell_survives_a_failing_jlo_env_under_set_e", &sh) {
            continue;
        }
        let home = tempdir().unwrap();
        let home = canon(home.path());
        let project = home.join("project");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join(".jlorc"), "99\n").unwrap();

        let jlo_home = tempdir().unwrap();
        let script = autoload_script(dialect);
        let body =
            format!("set -eu\njlo() {{ return 1; }}\n. '{script}'\necho 'profile-continued'\n");

        let stdout = run_sh(&sh, &body, &home, jlo_home.path(), &project);
        assert_eq!(
            stdout.trim(),
            "profile-continued",
            "{sh} ({dialect}) aborted the profile"
        );
    }
}

/// A profile may well run under `set -u`; the script must not abort it by
/// touching `PROMPT_COMMAND`, `JLO_HOME` or `_JLO_LAST_DIR` before they exist.
/// Checked in both dialects - a `set -u` failure in the zsh half would
/// otherwise only surface on a zsh user's next login.
///
/// `JLO_HOME` is unset in the second pass on purpose. The fresh-shell branch
/// reads it, and a shell sourcing `autoload.sh` without `jlo.sh` - or before
/// it - has never seen it. Leaving it set in every case made the `${JLO_HOME-}`
/// default untested: a bare `$JLO_HOME` would have passed just as well.
#[test]
fn sources_and_runs_the_hook_under_set_u() {
    let home = tempdir().unwrap();
    let home = canon(home.path());
    let neutral = home.join("neutral");
    std::fs::create_dir_all(&neutral).unwrap();
    let jlo_home = tempdir().unwrap();

    for (sh, dialect) in hook_shells() {
        if skip_missing("sources_and_runs_the_hook_under_set_u", &sh) {
            continue;
        }
        let script = autoload_script(dialect);
        for prologue in ["", "unset JLO_HOME\n"] {
            let body = format!(
                "set -u\n{prologue}jlo() {{ return 1; }}\n. '{script}'\n\
                 jlo_after_cd\necho \"rc=$?\"\n"
            );
            let stdout = run_sh(&sh, &body, &home, jlo_home.path(), &neutral);
            assert_eq!(
                stdout.trim(),
                "rc=0",
                "{sh} ({dialect}) with prologue {prologue:?}: the hook must not \
                 leak a failure into $?"
            );
        }
    }
}
