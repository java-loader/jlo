// Test code: `unwrap` failures are test failures, and `env::set_var` needs
// `unsafe` under edition 2024 despite the serial_test guard.
#![allow(unsafe_code, clippy::unwrap_used)]

use assert_cmd::Command;
use predicates::prelude::*;
use serial_test::serial;

#[test]
fn bare_invocation_prints_help() {
    let mut cmd = Command::cargo_bin("jlo-bin").unwrap();
    cmd.assert()
        .success()
        .code(0)
        .stdout(predicate::str::contains("Usage: jlo"))
        .stdout(predicate::str::contains("env"))
        .stdout(predicate::str::contains("home"))
        .stdout(predicate::str::contains("exec"))
        .stdout(predicate::str::contains("list"))
        .stdout(predicate::str::contains("update"))
        .stdout(predicate::str::contains("prune"))
        .stdout(predicate::str::contains("remove"))
        .stdout(predicate::str::contains("init"))
        .stdout(predicate::str::contains("selfupdate"))
        .stdout(predicate::str::contains("completions"));
    // No assertion for "version" here: since the `version` subcommand was
    // removed, that substring would still match vacuously against the
    // `-V, --version` line in the Options block, making it a false pass
    // rather than real coverage of the subcommand list.
}

#[test]
fn help_flags_print_help() {
    for flag in ["-h", "--help"] {
        let mut cmd = Command::cargo_bin("jlo-bin").unwrap();
        cmd.arg(flag)
            .assert()
            .success()
            .code(0)
            .stdout(predicate::str::contains("Usage: jlo"));
    }
}

#[test]
fn clean_is_gone_entirely() {
    // `clean` was renamed to `prune`, with no alias left behind: one name
    // for one command, everywhere. Typing the old one is an ordinary usage
    // error - not a hidden path that still works.
    let mut old_name = Command::cargo_bin("jlo-bin").unwrap();
    old_name
        .arg("clean")
        .assert()
        .failure()
        .stderr(predicate::str::contains("unrecognized subcommand 'clean'"));

    // And it appears nowhere in the overview - not in the Commands: list,
    // and not in any line of prose still pointing at the old name.
    let mut help = Command::cargo_bin("jlo-bin").unwrap();
    help.arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("prune"))
        .stdout(predicate::str::contains("clean").not());
}

#[test]
fn ls_alias_is_documented_in_help() {
    // Visible, unlike `clean`: `ls` is a second name worth discovering, the
    // same call `env`/`use` makes.
    let mut cmd = Command::cargo_bin("jlo-bin").unwrap();
    cmd.arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::is_match(r"(?m)^\s*list\s+.*\[alias: ls\]").unwrap());
}

#[test]
fn ls_lists_the_same_thing_as_list() {
    // `--offline` is unaffected by the alias, and is the form that does not
    // need the network.
    let mut aliased = Command::cargo_bin("jlo-bin").unwrap();
    let aliased = aliased
        .args(["ls", "--offline"])
        .env("JLO_ADOPTIUM_API_URL", "http://127.0.0.1:1")
        .assert()
        .success();

    let mut spelled_out = Command::cargo_bin("jlo-bin").unwrap();
    let spelled_out = spelled_out
        .args(["list", "--offline"])
        .env("JLO_ADOPTIUM_API_URL", "http://127.0.0.1:1")
        .assert()
        .success();

    assert_eq!(aliased.get_output().stdout, spelled_out.get_output().stdout);
}

#[test]
fn home_offline_fails_without_touching_the_network() {
    // The whole point of the flag: an agent or a CI step can ask "is this
    // JDK here?" without risking a several-hundred-megabyte answer. The API
    // URL points at a port nothing listens on, so a network attempt would
    // surface as a connection error rather than this message - and major 99
    // does not exist, so no real install can satisfy it either.
    let mut cmd = Command::cargo_bin("jlo-bin").unwrap();
    cmd.args(["home", "--offline", "99"])
        .env("JLO_ADOPTIUM_API_URL", "http://127.0.0.1:1")
        .assert()
        .failure()
        .code(1)
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("no installed JDK matches Java 99"))
        .stderr(predicate::str::contains("without --offline"));
}

#[test]
fn remove_refuses_a_version_that_is_not_installed() {
    // Major 99 is not a real release, so this exercises the refusal without
    // depending on - or touching - whatever the host has installed.
    let mut cmd = Command::cargo_bin("jlo-bin").unwrap();
    cmd.args(["remove", "99"])
        .env("JLO_ADOPTIUM_API_URL", "http://127.0.0.1:1")
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains("no installed JDK matches '99'"))
        .stderr(predicate::str::contains("Nothing was removed"))
        .stderr(predicate::str::contains("jlo list --offline"));
}

#[test]
fn remove_names_every_version_that_missed_in_one_message() {
    // None of 97, 98, 99 is a real release. All three have to be named at
    // once: since the rule is that none of them ran, reporting only the
    // first would send the user through one rerun per typo.
    let mut cmd = Command::cargo_bin("jlo-bin").unwrap();
    cmd.args(["remove", "97", "98", "99"])
        .env("JLO_ADOPTIUM_API_URL", "http://127.0.0.1:1")
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains(
            "no installed JDK matches '97', '98' or '99'",
        ))
        .stderr(predicate::str::contains("Nothing was removed"));
}

#[test]
fn remove_requires_at_least_one_version() {
    // A bare `jlo remove` must be a usage error, not a no-op that exits 0.
    let mut cmd = Command::cargo_bin("jlo-bin").unwrap();
    cmd.arg("remove")
        .assert()
        .failure()
        .stderr(predicate::str::contains("Usage: jlo remove"));
}

#[test]
fn help_mentions_version_resolution_and_examples() {
    let mut cmd = Command::cargo_bin("jlo-bin").unwrap();
    cmd.arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains(".jlorc"))
        .stdout(predicate::str::contains("default.jlorc"))
        .stdout(predicate::str::contains("Examples:"))
        .stdout(predicate::str::contains("jlo exec 21 -- ./gradlew build"));
}

#[test]
fn use_alias_is_documented_in_help() {
    // `jlo-init.sh` has always accepted `use` as an alias for `env`; the
    // binary silently rejected it until this alias was added. A *hidden*
    // alias would only half-fix that, so it must show up in `jlo --help`,
    // not just work when typed.
    let mut cmd = Command::cargo_bin("jlo-bin").unwrap();
    cmd.arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("[alias: use]"));
}

#[test]
fn init_help_states_the_default_when_version_is_omitted() {
    // `[VERSION]` alone signals "optional" too subtly for the compact `-h`
    // tier, and doesn't say what filling it in falls back to; that has to
    // be in the argument's own help line, not just `long_about`.
    let mut cmd = Command::cargo_bin("jlo-bin").unwrap();
    cmd.args(["init", "-h"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Default: latest release"));
}

#[test]
fn version_flag_prints_the_crate_version() {
    let mut flag = Command::cargo_bin("jlo-bin").unwrap();
    let flag_out = flag
        .arg("-V")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let flag_out = String::from_utf8(flag_out).unwrap();
    assert!(flag_out.contains(env!("CARGO_PKG_VERSION")));

    let mut long_flag = Command::cargo_bin("jlo-bin").unwrap();
    long_flag
        .arg("--version")
        .assert()
        .success()
        .stdout(predicate::str::contains(env!("CARGO_PKG_VERSION")));
}

#[test]
fn unknown_command_is_a_usage_error() {
    let mut cmd = Command::cargo_bin("jlo-bin").unwrap();
    cmd.arg("nosuchcmd")
        .assert()
        .failure()
        .code(2)
        .stderr(predicate::str::contains("nosuchcmd"));
}

#[test]
fn unknown_command_suggests_a_real_one() {
    let mut cmd = Command::cargo_bin("jlo-bin").unwrap();
    cmd.arg("enb")
        .assert()
        .failure()
        .stderr(predicate::str::contains("env"));
}

#[test]
fn the_default_subcommand_is_gone() {
    // `jlo default <v>` folded into `jlo init --global <v>`: both only ever
    // wrote a .jlorc, so one command with a scope flag replaces two.
    let mut cmd = Command::cargo_bin("jlo-bin").unwrap();
    cmd.args(["default", "21"])
        .assert()
        .failure()
        .code(2)
        .stderr(predicate::str::contains("default"));
}

#[test]
fn init_help_mentions_global_and_force() {
    let mut cmd = Command::cargo_bin("jlo-bin").unwrap();
    cmd.args(["init", "-h"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--global"))
        .stdout(predicate::str::contains("--force"));
}

#[test]
fn init_global_writes_the_default_config() {
    let home = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();

    Command::cargo_bin("jlo-bin")
        .unwrap()
        .args(["init", "--global", "21"])
        .current_dir(cwd.path())
        .env("JLO_HOME", home.path())
        .assert()
        .success()
        .code(0)
        .stderr(predicate::str::contains("default.jlorc"))
        .stdout("");

    let content = std::fs::read_to_string(home.path().join("default.jlorc")).unwrap();
    assert_eq!(content.lines().nth(1), Some("21"));
    // --global must not also touch the project.
    assert!(!cwd.path().join(".jlorc").exists());
}

#[test]
fn init_force_overwrites_an_existing_config() {
    // The whole point of folding `default` into `init`: setting a version
    // you already set has to be idempotent, not an error.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join(".jlorc"), "17\n").unwrap();

    Command::cargo_bin("jlo-bin")
        .unwrap()
        .args(["init", "--force", "21"])
        .current_dir(dir.path())
        .assert()
        .success()
        .code(0)
        .stderr(predicate::str::contains("Updated config file"))
        .stdout("");

    let content = std::fs::read_to_string(dir.path().join(".jlorc")).unwrap();
    assert_eq!(content.lines().nth(1), Some("21"));
}

#[test]
fn init_without_force_hints_at_force() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join(".jlorc"), "17\n").unwrap();

    Command::cargo_bin("jlo-bin")
        .unwrap()
        .args(["init", "21"])
        .current_dir(dir.path())
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains("already exists"))
        .stderr(predicate::str::contains("--force"));

    // The existing file is left alone.
    let content = std::fs::read_to_string(dir.path().join(".jlorc")).unwrap();
    assert_eq!(content.lines().next(), Some("17"));
}

#[test]
fn every_subcommand_has_help() {
    for sub in [
        "env",
        "home",
        "exec",
        "list",
        "update",
        "prune",
        "remove",
        "init",
        "selfupdate",
        "completions",
    ] {
        let mut cmd = Command::cargo_bin("jlo-bin").unwrap();
        cmd.args([sub, "--help"])
            .assert()
            .success()
            .code(0)
            .stdout(predicate::str::contains("Usage: jlo"));
    }
}

#[test]
fn exec_help_is_not_passed_to_the_child() {
    let mut cmd = Command::cargo_bin("jlo-bin").unwrap();
    cmd.args(["exec", "--help"])
        .assert()
        .success()
        .code(0)
        .stdout(predicate::str::contains("Usage: jlo exec"));
}

#[test]
fn exec_usage_line_shows_the_mandatory_separator() {
    // clap's default rendering for a `trailing_var_arg` positional
    // (`Usage: jlo exec [ARGS]...`) reads as free-form optional args and
    // hides that `--` is mandatory. `override_usage` must make `-h` agree
    // with the usage line `cmd_exec`'s own parse-error path already prints
    // (main.rs): "Usage: jlo exec [VERSION] -- <COMMAND> [ARGS]...".
    let mut cmd = Command::cargo_bin("jlo-bin").unwrap();
    cmd.args(["exec", "-h"])
        .assert()
        .success()
        .code(0)
        .stdout(predicate::str::contains(
            "Usage: jlo exec [VERSION] -- <COMMAND> [ARGS]...",
        ));
}

#[test]
fn exec_help_still_shows_after_an_explicit_version() {
    // Once a version has bound to the `args` positional, clap's own
    // `-h`/`--help` interception no longer fires on its own (it only
    // catches the flag before any value is captured); `cmd_exec` must
    // still recognise it.
    let mut cmd = Command::cargo_bin("jlo-bin").unwrap();
    cmd.args(["exec", "21", "--help"])
        .assert()
        .success()
        .code(0)
        .stdout(predicate::str::contains("Usage: jlo exec"));
}

#[test]
fn exec_passes_hyphen_args_through_to_the_child() {
    // `--help` after `--` belongs to the child, not to jlo. `true` ignores it
    // and exits 0; jlo's own help would mention "Usage: jlo exec".
    let mut cmd = Command::cargo_bin("jlo-bin").unwrap();
    cmd.args(["exec", "--", "true", "--help"])
        .env("JLO_ADOPTIUM_API_URL", "http://127.0.0.1:1")
        .assert()
        .stdout(predicate::str::contains("Usage: jlo exec").not());
}

#[test]
fn sing_is_hidden_everywhere_but_still_works() {
    // "sing" as a bare substring shows up inside ordinary words the help/
    // completion text may legitimately contain (using, missing, parsing,
    // ...), so check the whole word instead - case-insensitive since clap
    // and shells don't care about case for this. `Not<RegexPredicate>`
    // isn't `Clone`, so build a fresh one per assertion.
    fn not_the_word_sing() -> predicates::boolean::NotPredicate<predicates::str::RegexPredicate, str>
    {
        predicate::str::is_match(r"(?i)\bsing\b").unwrap().not()
    }

    // `hide = true` only ever suppressed the `--help` listing; `sing` is no
    // longer a clap subcommand at all, so this covers all three leaks in
    // one command family: help, completions (both shells), and clap's own
    // typo-suggestion engine.
    let mut help = Command::cargo_bin("jlo-bin").unwrap();
    help.arg("--help")
        .assert()
        .success()
        .stdout(not_the_word_sing());

    let mut bash = Command::cargo_bin("jlo-bin").unwrap();
    bash.args(["completions", "bash"])
        .assert()
        .success()
        .stdout(not_the_word_sing());

    let mut zsh = Command::cargo_bin("jlo-bin").unwrap();
    zsh.args(["completions", "zsh"])
        .assert()
        .success()
        .stdout(not_the_word_sing());

    // A typo must still be a real usage error - it must not silently
    // succeed - and the suggestion it prints must not name the hidden
    // command either.
    let mut typo = Command::cargo_bin("jlo-bin").unwrap();
    typo.arg("sng")
        .assert()
        .failure()
        .stderr(not_the_word_sing());

    // The easter egg itself must still work when invoked directly.
    let mut sing = Command::cargo_bin("jlo-bin").unwrap();
    sing.arg("sing")
        .assert()
        .success()
        .code(0)
        .stderr(predicate::str::contains("There are no Easter Eggs"));
}

#[test]
fn version() {
    let mut cmd = Command::cargo_bin("jlo-bin").unwrap();
    cmd.arg("--version")
        .assert()
        .success()
        .code(0)
        .stdout(predicate::str::is_match(r"^jlo \d+\.\d+\.\d+\n$").unwrap());
}

#[test]
fn version_subcommand_is_a_transition_shim_hidden_from_discovery() {
    // `jlo version` was removed from the clap `Command` enum in favour of
    // `-V`/`--version`, in the same commit that added the `sing` easter
    // egg's raw-argv interception - `hide = true` would only suppress it
    // from `--help`, not from generated completions or clap's typo
    // suggestions. But unlike `sing`, `version` isn't gone: a transition
    // shim (see the comment in `main.rs`, just before `Cli::parse()`)
    // keeps it *working* for one release, because the OLD `jlo` shell
    // function - already resident in a user's shell at `selfupdate` time -
    // calls `"$J" version` itself, against the newly-installed binary.
    //
    // "version" legitimately appears elsewhere in this output (the
    // `-V, --version` option, and JAVA_HOME/`.jlorc` prose that talks
    // about "a version"), so unlike the `sing` test this can't assert the
    // word is wholly absent. Each check below targets the specific shape a
    // *subcommand* entry would take, distinguishing it from those
    // legitimate occurrences - `.contains("version")` would false-positive
    // on `--version` and on that prose.

    // The shim prints the bare crate version - not clap's `jlo 0.2.0` form
    // - so the old wrapper's `echo -n "..."; "$J" --version` output still
    // reads as a clean version string.
    let mut shim = Command::cargo_bin("jlo-bin").unwrap();
    shim.arg("version")
        .assert()
        .success()
        .code(0)
        .stdout(format!("{}\n", env!("CARGO_PKG_VERSION")));

    // No "  version" line in the Commands: list (as opposed to the
    // "  -V, --version  Print version" Options line, which starts with
    // "-V," not "version").
    let mut help = Command::cargo_bin("jlo-bin").unwrap();
    help.arg("--help").assert().success().stdout(
        predicate::str::is_match(r"(?m)^\s*version(\s|$)")
            .unwrap()
            .not(),
    );

    // No `jlo,version)`/`jlo__subcmd__version` case in the generated bash
    // completion script - that's the shape every *real* subcommand takes
    // there (see e.g. `jlo,home)` / `cmd="jlo__subcmd__home"`).
    let mut bash = Command::cargo_bin("jlo-bin").unwrap();
    bash.args(["completions", "bash"])
        .assert()
        .success()
        .stdout(
            predicate::str::is_match(r"jlo,version\)|jlo__subcmd__version")
                .unwrap()
                .not(),
        );

    // No `'version:...'` entry in the generated zsh completion script -
    // that's the shape every *real* subcommand takes there (see e.g.
    // `'home:Print the JAVA_HOME path for a version' \`, where "home" is
    // the command and "version" only appears inside its description).
    let mut zsh = Command::cargo_bin("jlo-bin").unwrap();
    zsh.args(["completions", "zsh"])
        .assert()
        .success()
        .stdout(predicate::str::is_match(r"(?m)^'version:").unwrap().not());

    // A typo must still be a real usage error, and since the shim isn't a
    // clap subcommand, clap's "did you mean" engine can't offer it either.
    let mut typo = Command::cargo_bin("jlo-bin").unwrap();
    typo.arg("vrsion")
        .assert()
        .failure()
        .stderr(predicate::str::is_match(r"(?i)\bversion\b").unwrap().not());
}

#[test]
fn bare_invocation_matches_help_flag_byte_for_byte() {
    // `jlo` (exploration) and `jlo --help` (usage error convention) must
    // print the identical overview - only the exit-code handling in `main`
    // differs. A stray extra `println!()` in either `cli::print_help` call
    // site would desync them by one trailing newline.
    let bare = Command::cargo_bin("jlo-bin").unwrap().assert().success();
    let bare_stdout = bare.get_output().stdout.clone();

    let help = Command::cargo_bin("jlo-bin")
        .unwrap()
        .arg("--help")
        .assert()
        .success();
    let help_stdout = help.get_output().stdout.clone();

    assert_eq!(bare_stdout, help_stdout);
}

#[test]
fn list_offline_succeeds_without_network() {
    let mut cmd = Command::cargo_bin("jlo-bin").unwrap();
    // Either JDKs are installed (one version per line on stdout) or none are
    // (a note on stderr) - both are success, and neither needs the network.
    cmd.args(["list", "--offline"])
        .env("JLO_ADOPTIUM_API_URL", "http://127.0.0.1:1")
        .assert()
        .success()
        .code(0);
}

#[test]
fn list_rejects_unknown_option() {
    let mut cmd = Command::cargo_bin("jlo-bin").unwrap();
    cmd.args(["list", "--nope"])
        .assert()
        .failure()
        .code(2)
        .stderr(predicate::str::contains("--nope"));
}

#[test]
fn list_remote_shows_available_versions() {
    let mut server = mockito::Server::new();
    let _r = server
        .mock("GET", "/v3/info/available_releases")
        .with_body(r#"{"available_releases":[21],"available_lts_releases":[21]}"#)
        .create();
    let _a = server
        .mock(
            "GET",
            mockito::Matcher::Regex(r"^/v3/assets/latest/21/hotspot".to_string()),
        )
        .match_query(mockito::Matcher::Any)
        .with_body(include_str!("fixtures/assets_latest.json"))
        .create();

    Command::cargo_bin("jlo-bin")
        .unwrap()
        .arg("list")
        .env("JLO_ADOPTIUM_API_URL", server.url())
        .assert()
        .success()
        // The major version leads the line - that is what `jlo update` takes.
        .stdout(predicate::str::starts_with("21  21.0.11+10.0.LTS"))
        .stdout(predicate::str::contains("LTS"));
}

#[test]
fn list_remote_network_failure_points_at_offline() {
    Command::cargo_bin("jlo-bin")
        .unwrap()
        .arg("list")
        .env("JLO_ADOPTIUM_API_URL", "http://127.0.0.1:1")
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains("jlo list --offline"));
}

#[test]
fn selfupdate_not_supported() {
    let mut cmd = Command::cargo_bin("jlo-bin").unwrap();
    cmd.arg("selfupdate")
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains(
            "handled by the jlo shell function",
        ));
}

#[test]
#[serial]
fn init_with_version() {
    let temp_dir = tempfile::tempdir().unwrap();
    std::env::set_current_dir(&temp_dir).unwrap();

    let mut cmd = Command::cargo_bin("jlo-bin").unwrap();
    cmd.args(["init", "21"])
        .assert()
        .success()
        .code(0)
        .stderr(predicate::str::contains("Java 21"))
        .stdout("");

    let content = std::fs::read_to_string(".jlorc").unwrap();
    let lines: Vec<_> = content.lines().collect();
    assert_eq!(lines[1], "21");

    std::env::set_current_dir(std::env::temp_dir()).unwrap();
    temp_dir.close().unwrap();
}

#[test]
#[serial]
fn init() {
    // create a temp dir and switch to it
    let temp_dir = tempfile::tempdir().unwrap();
    std::env::set_current_dir(&temp_dir).unwrap();

    // run init
    let mut cmd = Command::cargo_bin("jlo-bin").unwrap();
    cmd.arg("init")
        .assert()
        .success()
        .code(0)
        // Status goes to stderr; stdout stays reserved for eval-able shell
        // output, so `jlo init` contributes nothing to it.
        .stderr(predicate::str::is_match(r"Created config file '.jlorc'").unwrap())
        .stdout("");

    // check if .jlorc contains a valid major version
    let content = std::fs::read_to_string(".jlorc").unwrap();
    let lines: Vec<_> = content.lines().collect();
    assert_eq!(
        lines[0],
        "# Java version configured by J'Lo - https://github.com/java-loader/jlo"
    );
    let version: u32 = lines[1].parse().expect("expected numeric version");
    assert!(version >= 8, "expected version >= 8, got {version}");

    // run init again to check for existing file error
    let mut cmd = Command::cargo_bin("jlo-bin").unwrap();
    cmd.arg("init")
        .assert()
        .failure()
        .code(1)
        .stderr(
            predicate::str::is_match(
                r"Error: could not create config file: file '.jlorc' already exists",
            )
            .unwrap(),
        )
        .stdout("");

    // leave temp dir and clean up
    std::env::set_current_dir(std::env::temp_dir()).unwrap();
    temp_dir.close().unwrap();
}

#[test]
#[serial]
fn home() {
    // create a temp dir and set its path to JLO_HOME
    let temp_dir = tempfile::tempdir().unwrap();
    unsafe {
        std::env::set_var("JLO_HOME", temp_dir.path());
    }

    // switch to temp dir and create .jlorc with "25"
    std::env::set_current_dir(&temp_dir).unwrap();
    std::fs::write(".jlorc", "25").unwrap();

    // run home (version resolved from .jlorc)
    let mut cmd = Command::cargo_bin("jlo-bin").unwrap();
    let assert = cmd.arg("home").assert().success().code(0);
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();

    // stdout must be exactly one line: the JAVA_HOME path, nothing else
    assert_eq!(stdout.lines().count(), 1, "stdout: {stdout:?}");
    assert!(stdout.ends_with('\n'), "stdout must be newline-terminated");
    let java_home = stdout.trim_end();
    assert!(
        std::path::Path::new(java_home)
            .join("bin")
            .join("java")
            .exists(),
        "printed JAVA_HOME must contain bin/java: {java_home}"
    );

    // explicit version argument resolves the same way
    let mut cmd = Command::cargo_bin("jlo-bin").unwrap();
    let assert = cmd.args(["home", "25"]).assert().success().code(0);
    let stdout_explicit = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert_eq!(stdout_explicit, stdout);

    // leave temp dir and clean up
    std::env::set_current_dir(std::env::temp_dir()).unwrap();
    unsafe {
        std::env::remove_var("JLO_HOME");
    }
    temp_dir.close().unwrap();
}

#[cfg(unix)]
#[test]
#[serial]
fn exec() {
    // create a temp dir and set its path to JLO_HOME
    let temp_dir = tempfile::tempdir().unwrap();
    unsafe {
        std::env::set_var("JLO_HOME", temp_dir.path());
    }

    // switch to temp dir and create .jlorc with "25"
    std::env::set_current_dir(&temp_dir).unwrap();
    std::fs::write(".jlorc", "25").unwrap();

    // exit code of the child propagates through exec
    let mut cmd = Command::cargo_bin("jlo-bin").unwrap();
    cmd.args(["exec", "25", "--", "sh", "-c", "exit 7"])
        .assert()
        .failure()
        .code(7);

    // JAVA_HOME is set in the child and its bin is first on PATH
    let mut cmd = Command::cargo_bin("jlo-bin").unwrap();
    let assert = cmd
        .args([
            "exec",
            "25",
            "--",
            "sh",
            "-c",
            "echo \"$JAVA_HOME\"; command -v java",
        ])
        .assert()
        .success()
        .code(0);
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let mut lines = stdout.lines();
    let java_home = lines.next().unwrap();
    let java_bin = lines.next().unwrap();
    assert!(!java_home.is_empty(), "JAVA_HOME must be set in child");
    assert_eq!(
        java_bin,
        format!("{java_home}/bin/java"),
        "java must resolve to the JDK from JAVA_HOME"
    );

    // version resolved from .jlorc when omitted (no explicit version before --)
    let mut cmd = Command::cargo_bin("jlo-bin").unwrap();
    cmd.args(["exec", "--", "java", "-version"])
        .assert()
        .success()
        .code(0)
        .stderr(predicate::str::contains("openjdk version \"25"));

    // missing '--' is a usage error
    let mut cmd = Command::cargo_bin("jlo-bin").unwrap();
    cmd.args(["exec", "25", "java", "-version"])
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains("expected '--'"));

    // a command that cannot be launched exits 127 with an error on stderr
    let mut cmd = Command::cargo_bin("jlo-bin").unwrap();
    cmd.args(["exec", "25", "--", "definitely-not-a-real-command-xyz"])
        .assert()
        .failure()
        .code(127)
        .stderr(predicate::str::contains("could not execute"));

    // Only the *first* `--` is the separator; a second, user-typed `--`
    // belongs to the command and must survive - whether or not a version
    // precedes it. This must behave identically either way: both try to
    // execute a program literally named "--" (regression test: the
    // version-omitted path used to silently drop the second `--` because
    // clap eats the leading separator before `cmd_exec` ever sees it).
    let mut cmd = Command::cargo_bin("jlo-bin").unwrap();
    cmd.args([
        "exec",
        "25",
        "--",
        "--",
        "definitely-not-a-real-command-xyz",
    ])
    .assert()
    .failure()
    .code(127)
    .stderr(predicate::str::contains("could not execute '--'"));

    let mut cmd = Command::cargo_bin("jlo-bin").unwrap();
    cmd.args(["exec", "--", "--", "definitely-not-a-real-command-xyz"])
        .assert()
        .failure()
        .code(127)
        .stderr(predicate::str::contains("could not execute '--'"));

    // leave temp dir and clean up
    std::env::set_current_dir(std::env::temp_dir()).unwrap();
    unsafe {
        std::env::remove_var("JLO_HOME");
    }
    temp_dir.close().unwrap();
}

#[test]
#[serial]
fn env() {
    // create a temp dir and set its path to JLO_HOME
    let temp_dir = tempfile::tempdir().unwrap();
    unsafe {
        std::env::set_var("JLO_HOME", temp_dir.path());
    }

    // switch to temp dir and create .jlorc with "25"
    std::env::set_current_dir(&temp_dir).unwrap();
    std::fs::write(".jlorc", "25").unwrap();

    // run env
    let mut cmd = Command::cargo_bin("jlo-bin").unwrap();
    cmd.arg("env").assert().success().code(0).stdout(
        predicate::str::contains("export JAVA_HOME=").and(predicate::str::contains("export PATH=")),
    );

    // leave temp dir and clean up
    std::env::set_current_dir(std::env::temp_dir()).unwrap();
    unsafe {
        std::env::remove_var("JLO_HOME");
    }
    temp_dir.close().unwrap();
}

#[test]
fn update_reports_api_http_error() {
    let mut server = mockito::Server::new();
    let _m = server
        .mock(
            "GET",
            mockito::Matcher::Regex(r"^/v3/assets/latest/10014/hotspot".to_string()),
        )
        .match_query(mockito::Matcher::Any)
        .with_status(500)
        .create();

    Command::cargo_bin("jlo-bin")
        .unwrap()
        .args(["update", "10014"])
        .env("JLO_ADOPTIUM_API_URL", server.url())
        .assert()
        .failure()
        .stderr(predicate::str::contains("HTTP 500"));
}

#[test]
fn init_uses_latest_version_from_api() {
    let mut server = mockito::Server::new();
    let _m = server
        .mock("GET", "/v3/info/available_releases")
        .with_body(include_str!("fixtures/available_releases.json"))
        .create();

    let temp_dir = tempfile::tempdir().unwrap();
    Command::cargo_bin("jlo-bin")
        .unwrap()
        .arg("init")
        .current_dir(temp_dir.path())
        .env("JLO_ADOPTIUM_API_URL", server.url())
        .assert()
        .success();

    let content = std::fs::read_to_string(temp_dir.path().join(".jlorc")).unwrap();
    let lines: Vec<_> = content.lines().collect();
    assert_eq!(lines[1], "26");
}

#[test]
fn init_reports_api_http_error() {
    let mut server = mockito::Server::new();
    // valid body — the status alone must fail the command
    let _m = server
        .mock("GET", "/v3/info/available_releases")
        .with_status(500)
        .with_body(include_str!("fixtures/available_releases.json"))
        .create();

    let temp_dir = tempfile::tempdir().unwrap();
    Command::cargo_bin("jlo-bin")
        .unwrap()
        .arg("init")
        .current_dir(temp_dir.path())
        .env("JLO_ADOPTIUM_API_URL", server.url())
        .assert()
        .failure()
        .stderr(predicate::str::contains("HTTP 500"));
}

/// Where `jlo` installs JDKs, mirroring `JdkStore::discover()` in store.rs.
/// IntelliJ-compatible: `~/Library/Java/JavaVirtualMachines` on macOS, `~/.jdks` elsewhere.
fn jdk_base() -> std::path::PathBuf {
    let home = std::env::home_dir().unwrap();
    if cfg!(target_os = "macos") {
        home.join("Library/Java/JavaVirtualMachines")
    } else {
        home.join(".jdks")
    }
}

/// Pull the value out of the `export PATH="..."` line of `jlo env` output.
fn exported_path(stdout: &str) -> Option<String> {
    stdout
        .lines()
        .find_map(|l| l.strip_prefix("export PATH=\""))
        .and_then(|l| l.strip_suffix('"'))
        .map(str::to_string)
}

#[test]
#[serial]
fn env_removes_stale_jdk_bin_entries() {
    let temp_dir = tempfile::tempdir().unwrap();
    unsafe {
        std::env::set_var("JLO_HOME", temp_dir.path());
    }
    std::env::set_current_dir(&temp_dir).unwrap();
    std::fs::write(".jlorc", "25").unwrap();

    // A JDK bin dir jlo prepended on an earlier run, for a version we are no longer using.
    let stale = jdk_base().join("99.0.1").join("bin");
    let input_path = format!("{}:/usr/bin:/bin", stale.display());

    let mut cmd = Command::cargo_bin("jlo-bin").unwrap();
    let assert = cmd
        .arg("env")
        .env("PATH", &input_path)
        .assert()
        .success()
        .code(0);
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let new_path = exported_path(&stdout).expect("env must export PATH");

    assert!(
        !new_path.split(':').any(|p| p == stale.to_str().unwrap()),
        "stale JDK bin must be removed from PATH.\n  stale: {}\n  got:   {}",
        stale.display(),
        new_path
    );

    std::env::set_current_dir(std::env::temp_dir()).unwrap();
    unsafe {
        std::env::remove_var("JLO_HOME");
    }
    temp_dir.close().unwrap();
}

#[test]
#[serial]
fn env_without_jlo_home_keeps_unrelated_home_path_entries() {
    let temp_dir = tempfile::tempdir().unwrap();
    unsafe {
        std::env::remove_var("JLO_HOME");
    }
    std::env::set_current_dir(&temp_dir).unwrap();
    std::fs::write(".jlorc", "25").unwrap();

    // User-local PATH entries that have nothing to do with jlo.
    let home = std::env::home_dir().unwrap();
    let cargo_bin = home.join(".cargo").join("bin");
    let user_bin = home.join("bin");
    let input_path = format!(
        "{}:{}:/usr/bin:/bin",
        cargo_bin.display(),
        user_bin.display()
    );

    let mut cmd = Command::cargo_bin("jlo-bin").unwrap();
    let assert = cmd
        .arg("env")
        .env("PATH", &input_path)
        .assert()
        .success()
        .code(0);
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let new_path = exported_path(&stdout).expect("env must export PATH");

    for keep in [&cargo_bin, &user_bin] {
        assert!(
            new_path.split(':').any(|p| p == keep.to_str().unwrap()),
            "PATH entry under $HOME must survive.\n  expected: {}\n  got:      {}",
            keep.display(),
            new_path
        );
    }

    std::env::set_current_dir(std::env::temp_dir()).unwrap();
    temp_dir.close().unwrap();
}

#[test]
#[serial]
fn env_is_idempotent() {
    let temp_dir = tempfile::tempdir().unwrap();
    unsafe {
        std::env::set_var("JLO_HOME", temp_dir.path());
    }
    std::env::set_current_dir(&temp_dir).unwrap();
    std::fs::write(".jlorc", "25").unwrap();

    let mut cmd = Command::cargo_bin("jlo-bin").unwrap();
    let assert = cmd
        .arg("env")
        .env("PATH", "/usr/bin:/bin")
        .assert()
        .success()
        .code(0);
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let first_path = exported_path(&stdout).expect("first env run must export PATH");
    let java_home = stdout
        .lines()
        .find_map(|l| l.strip_prefix("export JAVA_HOME=\""))
        .and_then(|l| l.strip_suffix('"'))
        .expect("first env run must export JAVA_HOME")
        .to_string();

    // Second run with the environment the first run produced: nothing left to change.
    let mut cmd = Command::cargo_bin("jlo-bin").unwrap();
    let assert = cmd
        .arg("env")
        .env("PATH", &first_path)
        .env("JAVA_HOME", &java_home)
        .assert()
        .success()
        .code(0);
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();

    assert_eq!(
        stdout, "",
        "re-running env in an already-configured shell must emit nothing, got: {stdout:?}"
    );

    std::env::set_current_dir(std::env::temp_dir()).unwrap();
    unsafe {
        std::env::remove_var("JLO_HOME");
    }
    temp_dir.close().unwrap();
}

#[test]
fn completions_emit_a_script_per_shell() {
    for (shell, needle) in [
        ("bash", "complete"),
        ("zsh", "compdef"),
        ("fish", "complete"),
    ] {
        let mut cmd = Command::cargo_bin("jlo-bin").unwrap();
        cmd.args(["completions", shell])
            .assert()
            .success()
            .code(0)
            .stdout(predicate::str::contains(needle))
            .stdout(predicate::str::contains("jlo"));
    }
}

#[test]
fn completions_reject_an_unknown_shell() {
    let mut cmd = Command::cargo_bin("jlo-bin").unwrap();
    cmd.args(["completions", "nosuchshell"])
        .assert()
        .failure()
        .code(2);
}

#[test]
fn completions_do_not_hit_the_network() {
    let mut cmd = Command::cargo_bin("jlo-bin").unwrap();
    cmd.args(["completions", "bash"])
        .env("JLO_ADOPTIUM_API_URL", "http://127.0.0.1:1")
        .assert()
        .success();
}
