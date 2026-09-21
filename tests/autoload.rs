//! Tests for the shell integration in `jlo-autoload.sh`.
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
use std::path::Path;
use std::process::Command;
use tempfile::tempdir;

/// Separates the `declare -p` line from the element dump.
const MARKER: &str = "--8<--";

fn autoload_script() -> String {
    format!("{}/jlo-autoload.sh", env!("CARGO_MANIFEST_DIR"))
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

/// Run `prologue`, then source `jlo-autoload.sh` `times` times in one bash
/// session, and report the resulting `PROMPT_COMMAND`.
fn source_script(prologue: &str, times: usize) -> PromptCommand {
    let cwd = tempdir().unwrap();
    let jlo_home = tempdir().unwrap();
    let script = autoload_script();

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
    let script = autoload_script();

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
// Walk-up lookup: the hook must see a project's .jlorc from a subdirectory
// ---------------------------------------------------------------------------

/// Source the script from an empty directory (so the fresh-shell branch stays
/// quiet), `cd` to `start`, and report what `jlo_find_jlorc` decides.
///
/// `HOME` is the temp tree rather than the real one: the search stops there,
/// and a test that walked up into the developer's actual home would depend on
/// whatever lives in it.
fn finds_jlorc(home: &Path, start: &Path) -> bool {
    let neutral = tempdir().unwrap();
    let jlo_home = tempdir().unwrap();
    let script = autoload_script();

    let body = format!(
        // `type` under `set -e`: without it a missing function would exit
        // non-zero and read as an honest "not found".
        "set -e\n. '{script}'\ntype jlo_find_jlorc >/dev/null\ncd '{}'\n\
         jlo_find_jlorc && echo FOUND || echo NONE\n",
        start.display()
    );

    let out = Command::new(bash_bin())
        .arg("-c")
        .arg(&body)
        .current_dir(neutral.path())
        .env("HOME", home)
        .env("JLO_HOME", jlo_home.path())
        .output()
        .unwrap_or_else(|e| panic!("failed to run {}: {e}", bash_bin()));

    assert!(
        out.status.success(),
        "bash failed ({}): {}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );

    let stdout = String::from_utf8(out.stdout).unwrap();
    match stdout.trim() {
        "FOUND" => true,
        "NONE" => false,
        other => panic!("unexpected output {other:?}"),
    }
}

/// `tempdir()` hands back `/var/...` on macOS while `$PWD` after `cd` shows
/// `/private/var/...`; the `$HOME` boundary is a string comparison.
fn canon(p: &Path) -> std::path::PathBuf {
    p.canonicalize().unwrap()
}

#[test]
fn find_walks_up_from_subdirectory() {
    let home = tempdir().unwrap();
    let home = canon(home.path());
    let project = home.join("project");
    let deep = project.join("src").join("main");
    std::fs::create_dir_all(&deep).unwrap();
    std::fs::write(project.join(".jlorc"), "21\n").unwrap();

    assert!(finds_jlorc(&home, &deep));
}

#[test]
fn find_stops_at_vcs_root() {
    let home = tempdir().unwrap();
    let home = canon(home.path());
    let outer = home.join("outer");
    let repo = outer.join("repo");
    let deep = repo.join("src");
    std::fs::create_dir_all(&deep).unwrap();
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    std::fs::write(outer.join(".jlorc"), "17\n").unwrap();

    assert!(!finds_jlorc(&home, &deep));
}

#[test]
fn find_stops_at_home() {
    let outside = tempdir().unwrap();
    let outside = canon(outside.path());
    let home = outside.join("home");
    let project = home.join("project");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(outside.join(".jlorc"), "17\n").unwrap();

    assert!(!finds_jlorc(&home, &project));
}

#[test]
fn find_reports_none_when_absent() {
    let home = tempdir().unwrap();
    let home = canon(home.path());
    let project = home.join("project");
    std::fs::create_dir_all(&project).unwrap();

    assert!(!finds_jlorc(&home, &project));
}

/// The behaviour that matters: `cd` into a subdirectory of a project must set
/// the env, not leave it to the user default. A stub `jlo` records the call.
///
/// `--offline` is part of the assertion, not incidental: without it a `cd`
/// into a project pinning an uninstalled JDK stalls the shell on a
/// several-hundred-megabyte download.
#[test]
fn hook_runs_jlo_env_from_a_project_subdirectory() {
    let home = tempdir().unwrap();
    let home = canon(home.path());
    let project = home.join("project");
    let deep = project.join("src");
    std::fs::create_dir_all(&deep).unwrap();
    std::fs::write(project.join(".jlorc"), "21\n").unwrap();

    let neutral = tempdir().unwrap();
    let jlo_home = tempdir().unwrap();
    let script = autoload_script();
    let body = format!(
        "set -e\njlo() {{ echo \"jlo $*\"; }}\n. '{script}'\ncd '{}'\njlo_after_cd\n",
        deep.display()
    );

    let out = Command::new(bash_bin())
        .arg("-c")
        .arg(&body)
        .current_dir(neutral.path())
        .env("HOME", &home)
        .env("JLO_HOME", jlo_home.path())
        .output()
        .unwrap();

    assert!(
        out.status.success(),
        "bash failed ({}): {}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        String::from_utf8(out.stdout).unwrap().trim(),
        "jlo env --offline"
    );
}

/// Run `body` under bash with `HOME`, `JLO_HOME` and the working directory
/// pinned to the given temp trees, and hand back stdout. The stub `jlo`
/// defined by the callers below records what the script asked for.
fn run_bash(body: &str, home: &Path, jlo_home: &Path, cwd: &Path) -> String {
    let out = Command::new(bash_bin())
        .arg("-c")
        .arg(body)
        .current_dir(cwd)
        .env("HOME", home)
        .env("JLO_HOME", jlo_home)
        .env_remove("PROMPT_COMMAND")
        .output()
        .unwrap_or_else(|e| panic!("failed to run {}: {e}", bash_bin()));

    assert!(
        out.status.success(),
        "bash failed ({}): {}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

/// The fresh-shell branch at the tail of the script. A new terminal is the
/// worst possible place to start a download, so it too must ask offline.
#[test]
fn fresh_shell_applies_the_user_default_offline() {
    let home = tempdir().unwrap();
    let home = canon(home.path());
    let neutral = home.join("neutral");
    std::fs::create_dir_all(&neutral).unwrap();

    let jlo_home = tempdir().unwrap();
    std::fs::write(jlo_home.path().join("default.jlorc"), "21\n").unwrap();

    let script = autoload_script();
    let body = format!("set -e\njlo() {{ echo \"jlo $*\"; }}\n. '{script}'\n");

    let stdout = run_bash(&body, &home, jlo_home.path(), &neutral);
    assert_eq!(stdout.trim(), "jlo env --offline");
}

/// With neither a `.jlorc` above the cwd nor a `default.jlorc`, the fresh-shell
/// branch must not run the binary at all - there is nothing for it to resolve.
#[test]
fn fresh_shell_stays_quiet_without_any_config() {
    let home = tempdir().unwrap();
    let home = canon(home.path());
    let neutral = home.join("neutral");
    std::fs::create_dir_all(&neutral).unwrap();
    let jlo_home = tempdir().unwrap();

    let script = autoload_script();
    let body = format!("set -e\njlo() {{ echo \"jlo $*\"; }}\n. '{script}'\n");

    let stdout = run_bash(&body, &home, jlo_home.path(), &neutral);
    assert_eq!(stdout.trim(), "");
}

/// bash runs the hook from `PROMPT_COMMAND`, i.e. before every prompt, but the
/// body is guarded on `$PWD` changing. Three prompts in one directory are one
/// call - which is what keeps the "not installed" line from repeating under
/// every command the user runs there.
#[test]
fn hook_runs_once_per_directory_not_once_per_prompt() {
    let home = tempdir().unwrap();
    let home = canon(home.path());
    let project = home.join("project");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(project.join(".jlorc"), "21\n").unwrap();

    let neutral = tempdir().unwrap();
    let jlo_home = tempdir().unwrap();
    let script = autoload_script();
    let body = format!(
        "set -e\njlo() {{ echo \"jlo $*\"; }}\n. '{script}'\ncd '{}'\n\
         jlo_after_cd\njlo_after_cd\njlo_after_cd\n",
        project.display()
    );

    let stdout = run_bash(&body, &home, jlo_home.path(), neutral.path());
    assert_eq!(stdout.trim(), "jlo env --offline");
}

/// The hook runs between the user's command and their prompt. A version the
/// store cannot satisfy makes `jlo env --offline` exit non-zero, and that must
/// not become the status their prompt reports.
#[test]
fn hook_reports_success_even_when_jlo_env_fails() {
    let home = tempdir().unwrap();
    let home = canon(home.path());
    let project = home.join("project");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(project.join(".jlorc"), "99\n").unwrap();

    let neutral = tempdir().unwrap();
    let jlo_home = tempdir().unwrap();
    let script = autoload_script();
    let body = format!(
        // `set -e` is deliberately absent: the point is the status, and the
        // stub fails on purpose.
        "jlo() {{ return 1; }}\n. '{script}'\ncd '{}'\n\
         jlo_after_cd\necho \"status=$?\"\n",
        project.display()
    );

    let stdout = run_bash(&body, &home, jlo_home.path(), neutral.path());
    assert_eq!(stdout.trim(), "status=0");
}
