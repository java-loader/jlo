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
        .stdout(predicate::str::contains("current"))
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

/// The autoload hook's contract, checked at the binary rather than through a
/// shell: entering a directory whose .jlorc pins an uninstalled version must
/// cost nothing. Empty stdout is the load-bearing half - the hook *sources*
/// this stream, so a decline that emitted a partial export would leave the
/// shell worse off than one that emitted none.
#[test]
fn env_offline_fails_without_touching_the_network() {
    let mut cmd = Command::cargo_bin("jlo-bin").unwrap();
    cmd.args(["env", "--offline", "99"])
        .env("JLO_ADOPTIUM_API_URL", "http://127.0.0.1:1")
        .assert()
        .failure()
        .code(1)
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("no installed JDK matches Java 99"))
        .stderr(predicate::str::contains(
            "Run 'jlo env 99' without --offline to install it.",
        ));
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

/// `install` was missing entirely until it was added as its own verb: the
/// obvious first guess was an `unrecognized subcommand` error, and nothing in
/// the help pointed at `update`. It has to be listed where the other
/// version-taking commands are, or the discoverability problem is unfixed.
#[test]
fn install_is_documented_in_help() {
    let mut cmd = Command::cargo_bin("jlo-bin").unwrap();
    cmd.arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("jlo install [VERSION...]"))
        .stdout(predicate::str::contains("jlo install 25"));
}

/// The reason `install` is a command of its own rather than an alias on
/// `update`: there is no such thing as installing every major, so the flag
/// that makes sense for `update` must not be a documented spelling here.
#[test]
fn install_has_no_all_flag() {
    let mut cmd = Command::cargo_bin("jlo-bin").unwrap();
    cmd.args(["install", "--all"])
        .env("JLO_ADOPTIUM_API_URL", "http://127.0.0.1:1")
        .assert()
        .failure()
        .code(2)
        .stderr(predicate::str::contains("--all"));

    let mut help = Command::cargo_bin("jlo-bin").unwrap();
    help.args(["install", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--all").not());
}

/// `install` does not touch the shell, so nothing may reach stdout - the
/// wrapper sources what lands there. The argument check fails before the
/// network, hence the unreachable API address.
#[test]
fn install_rejects_an_invalid_version_without_writing_to_stdout() {
    let mut cmd = Command::cargo_bin("jlo-bin").unwrap();
    cmd.args(["install", "abc"])
        .env("JLO_ADOPTIUM_API_URL", "http://127.0.0.1:1")
        .assert()
        .failure()
        .code(1)
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains(
            "no valid Java versions provided to install",
        ));
}

/// A bare `jlo install` resolves the version the same way env/home/exec do.
/// With no .jlorc anywhere above the run directory, no user default and no
/// JDK installed, the cascade runs all the way to its last stage - so what
/// fails here is the request for the latest release, not the resolution.
/// `install` has no --offline flag, so there is nothing to stop it earlier.
#[test]
#[serial]
fn install_without_a_version_falls_through_to_the_latest_release() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();

    let mut cmd = Command::cargo_bin("jlo-bin").unwrap();
    cmd.arg("install")
        .current_dir(project.path())
        .env("HOME", home.path())
        .env("JLO_HOME", home.path().join(".jlo"))
        .env("JLO_ADOPTIUM_API_URL", "http://127.0.0.1:1")
        .assert()
        .failure()
        .code(1)
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains(
            "could not fetch latest JDK version",
        ))
        // Not the old "run jlo init" refusal: a missing config is no longer
        // the end of the road.
        .stderr(predicate::str::contains("jlo init").not());
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
        "current",
        "list",
        "install",
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
fn version_is_not_a_subcommand_anywhere() {
    // `jlo version` was removed from the clap `Command` enum in favour of
    // `-V`/`--version`, and the transition shim that kept it working for
    // v0.2.0's resident shell wrapper is gone too. What must not come back
    // is `version` as a *discoverable* subcommand: `hide = true` would only
    // suppress it from `--help`, not from generated completions or clap's
    // typo-suggestion engine, which is precisely the drift this CLI rewrite
    // exists to eliminate.
    //
    // "version" legitimately appears elsewhere in this output (the
    // `-V, --version` option, and JAVA_HOME/`.jlorc` prose that talks
    // about "a version"), so this can't assert the word is wholly absent.
    // Each check below targets the specific shape a *subcommand* entry
    // would take - `.contains("version")` would false-positive on
    // `--version` and on that prose.

    // The bare token is now an ordinary usage error.
    let mut gone = Command::cargo_bin("jlo-bin").unwrap();
    gone.arg("version").assert().failure();

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

    // A typo must still be a real usage error, and since `version` is not a
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

/// The JDK store lives under `$HOME`, so a `jlo list` test that does not
/// override it lists whatever the developer happens to have installed.
fn jdk_store_in(home: &std::path::Path) -> std::path::PathBuf {
    if cfg!(target_os = "macos") {
        home.join("Library/Java/JavaVirtualMachines")
    } else {
        home.join(".jdks")
    }
}

fn install_fake_jdk(home: &std::path::Path, version: &str) {
    let dir = jdk_store_in(home).join(version);
    std::fs::create_dir_all(dir.join("bin")).unwrap();
    std::fs::write(dir.join(".jlo-managed"), "").unwrap();
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
    let home = tempfile::tempdir().unwrap();

    Command::cargo_bin("jlo-bin")
        .unwrap()
        .arg("list")
        .env("HOME", home.path())
        .env("JLO_ADOPTIUM_API_URL", server.url())
        .assert()
        .success()
        // The major version leads the row, after the gutter column that marks
        // which install `$JAVA_HOME` points at - the major is what
        // `jlo update` takes.
        .stdout(predicate::str::starts_with("    21  21.0.11+10.0.LTS"))
        .stdout(predicate::str::contains("LTS"));
}

/// The case `jlo remove 17.0.11+10` had nowhere to read its argument from:
/// a build older than the catalogue's used to collapse into a parenthetical
/// on the row above it.
#[test]
fn list_remote_gives_a_superseded_build_its_own_row() {
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

    let home = tempfile::tempdir().unwrap();
    install_fake_jdk(home.path(), "21.0.11+10.0.LTS");
    install_fake_jdk(home.path(), "21.0.9+10.0.LTS");
    let active = jdk_store_in(home.path()).join("21.0.9+10.0.LTS");

    Command::cargo_bin("jlo-bin")
        .unwrap()
        .arg("list")
        .env("HOME", home.path())
        .env("JAVA_HOME", &active)
        .env("JLO_ADOPTIUM_API_URL", server.url())
        .assert()
        .success()
        .stdout(
            "    21  21.0.11+10.0.LTS  LTS  installed\n \u{2192}  21  21.0.9+10.0.LTS   LTS  superseded\n",
        )
        // The advice belongs on stderr, so a pipe sees only the rows.
        .stderr(predicate::str::contains("`jlo prune` (1 superseded)"));
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
        predicate::str::contains("export JAVA_HOME='")
            .and(predicate::str::contains("export PATH='")),
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

/// Pull the value out of an `export NAME='...'` line of `jlo env` output,
/// undoing the single-quoting the binary applies.
///
/// The quoting is not cosmetic: the `jlo` shell function evaluates these
/// lines, so a `$(...)` arriving unquoted in `PATH` would run in the user's
/// shell. See `shell_quote` in `src/main.rs`.
fn exported_var(stdout: &str, name: &str) -> Option<String> {
    let prefix = format!("export {name}='");
    stdout
        .lines()
        .find_map(|l| l.strip_prefix(prefix.as_str()))
        .and_then(|l| l.strip_suffix('\''))
        .map(|v| v.replace(r"'\''", "'"))
}

fn exported_path(stdout: &str) -> Option<String> {
    exported_var(stdout, "PATH")
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

/// End to end, through a real shell: the `jlo` function evaluates what the
/// binary writes to stdout, and `PATH` is echoed back out of the caller's own
/// environment. A `$(...)` in it must therefore arrive as eight literal
/// characters rather than as a command the user's shell runs.
///
/// The marker file is the assertion: if it exists, evaluating `jlo env`'s
/// output executed an attacker's command in the user's session.
#[test]
#[serial]
fn env_does_not_let_a_hostile_path_execute_when_evaluated() {
    let temp_dir = tempfile::tempdir().unwrap();
    unsafe {
        std::env::set_var("JLO_HOME", temp_dir.path());
    }
    std::env::set_current_dir(&temp_dir).unwrap();
    std::fs::write(".jlorc", "25").unwrap();

    let marker = temp_dir.path().join("pwned");
    let payload = format!("$(touch '{}')", marker.display());
    let input_path = format!("/usr/bin:/bin:{payload}");

    let mut cmd = Command::cargo_bin("jlo-bin").unwrap();
    let assert = cmd
        .arg("env")
        .env("PATH", &input_path)
        .assert()
        .success()
        .code(0);
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();

    // The payload survives as data ...
    let new_path = exported_path(&stdout).expect("env must export PATH");
    assert!(
        new_path.ends_with(&payload),
        "the PATH entry must be preserved verbatim.\n  expected suffix: {payload}\n  got: {new_path}"
    );

    // ... and evaluating the line does not run it, in any supported shell.
    for sh in ["/bin/bash", "zsh"] {
        if std::process::Command::new(sh)
            .arg("-c")
            .arg("exit 0")
            .output()
            .is_err()
        {
            eprintln!("SKIP env_does_not_let_a_hostile_path_execute_when_evaluated: {sh} missing.");
            continue;
        }
        let _ = std::fs::remove_file(&marker);
        let out = std::process::Command::new(sh)
            .arg("-c")
            .arg(format!("eval {}", shell_single_quote(&stdout)))
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{sh} failed to evaluate the export lines: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            !marker.exists(),
            "{sh}: evaluating jlo env's output executed an injected command"
        );
    }

    std::env::set_current_dir(std::env::temp_dir()).unwrap();
    unsafe {
        std::env::remove_var("JLO_HOME");
    }
    temp_dir.close().unwrap();
}

/// Hand a string to `sh -c` as one literal argument. Mirrors `shell_quote` in
/// `src/main.rs`; kept separate so a bug there cannot hide itself here.
fn shell_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
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
    let java_home =
        exported_var(&stdout, "JAVA_HOME").expect("first env run must export JAVA_HOME");

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

/// `jlo env` writes the environment to stdout, and a user may pipe it - to
/// `head`, to `grep`, to anything that stops reading early. `println!` panics
/// on a closed pipe, which would turn an ordinary pipeline into a crash and a
/// stack-trace-shaped diagnostic. `jlo list` has always handled this; `env`
/// writes to the same channel and must behave the same way.
#[test]
#[serial]
fn env_survives_a_reader_that_stops_early() {
    let home = tempfile::tempdir().unwrap();
    // Mirrors base_dir_for(): the store is derived from $HOME, so a throwaway
    // home is enough to stand a JDK up without installing one.
    let store = if cfg!(target_os = "macos") {
        home.path().join("Library/Java/JavaVirtualMachines")
    } else {
        home.path().join(".jdks")
    };
    std::fs::create_dir_all(store.join("21.0.5+11/bin")).unwrap();

    let bin = assert_cmd::cargo::cargo_bin("jlo-bin");
    let out = std::process::Command::new("/bin/bash")
        .arg("-c")
        .arg(format!(
            "set -o pipefail; {} env --offline 21 | true",
            shell_escape(&bin.display().to_string())
        ))
        .env("HOME", home.path())
        .env("JLO_ADOPTIUM_API_URL", "http://127.0.0.1:1")
        .output()
        .unwrap();

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains("Broken pipe") && !stderr.contains("panicked"),
        "jlo env crashed on a closed pipe: {stderr}"
    );
    assert!(
        out.status.success(),
        "jlo env failed on a closed pipe: status={:?} stderr={stderr}",
        out.status.code()
    );
}

/// POSIX single-quoting, so a cargo target directory with an odd character in
/// it cannot break the `-c` string above.
fn shell_escape(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

// -- jlo current --
//
// One test per case in the command's table, each asserting stdout, stderr and
// the exit code separately: the split between the two streams is the contract
// (`VER=$(jlo current)` must get the answer and nothing else), so a combined
// assertion would pass with them swapped.
//
// Offline by construction - nothing here needs JLO_ADOPTIUM_API_URL - and
// `current_never_touches_the_network` pins that down by pointing the client at
// a dead port.

/// The store directory `JdkStore::discover` derives from `$HOME`. Not
/// configurable, which is why these tests move `$HOME` instead.
fn store_base(home: &std::path::Path) -> std::path::PathBuf {
    if cfg!(target_os = "macos") {
        home.join("Library/Java/JavaVirtualMachines")
    } else {
        home.join(".jdks")
    }
}

/// A temp `$HOME` holding the named JDK installs, plus the project directory
/// the command runs from. The project sits *inside* `$HOME` so the `.jlorc`
/// walk stops there rather than climbing into the real filesystem and finding
/// a config that belongs to something else.
fn store_fixture(versions: &[&str]) -> (tempfile::TempDir, std::path::PathBuf) {
    let home = tempfile::tempdir().unwrap();
    for version in versions {
        let jdk = store_base(home.path()).join(version);
        std::fs::create_dir_all(jdk.join("bin")).unwrap();
        std::fs::write(jdk.join("bin").join("java"), "").unwrap();
        std::fs::File::create(jdk.join(".jlo-managed")).unwrap();
    }

    let project = home.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    (home, project)
}

fn current_fixture(version: &str) -> (tempfile::TempDir, std::path::PathBuf) {
    store_fixture(&[version])
}

/// Every `jlo current` invocation starts from a shell that inherits nothing:
/// `$JLO_HOME` points at a directory with no default.jlorc, so "nothing
/// configured" is genuinely nothing.
fn current_cmd(home: &std::path::Path, project: &std::path::Path) -> Command {
    let mut cmd = Command::cargo_bin("jlo-bin").unwrap();
    cmd.arg("current")
        .current_dir(project)
        .env("HOME", home)
        .env("JLO_HOME", home.join(".jlo"))
        .env_remove("JAVA_HOME");
    cmd
}

/// Case 1. Exit 1, because stdout is empty: exiting 0 would hand
/// `VER=$(jlo current)` an empty string and a success code.
#[test]
fn current_without_java_home_has_no_answer() {
    let (home, project) = current_fixture("25.0.4+101");

    current_cmd(home.path(), &project)
        .assert()
        .failure()
        .code(1)
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("No JDK is active."))
        .stderr(predicate::str::contains("jlo env"));
}

/// Case 2: the active JDK is the one the project pins, and the line says which
/// file that is.
#[test]
fn current_names_the_project_config_when_it_agrees() {
    let (home, project) = current_fixture("25.0.4+101");
    std::fs::write(project.join(".jlorc"), "25\n").unwrap();

    current_cmd(home.path(), &project)
        .env("JAVA_HOME", store_base(home.path()).join("25.0.4+101"))
        .assert()
        .success()
        .code(0)
        .stdout("25.0.4+101  (from ./.jlorc)\n")
        .stderr(predicate::str::is_empty());
}

/// Case 3, the one this command is for: "why am I on the wrong JDK". Exit 0,
/// because the question asked - what is active - has an answer, and exiting 1
/// would make `jlo current` useless in exactly the situation where you most
/// want to read the version.
#[test]
fn current_warns_but_still_answers_when_the_config_disagrees() {
    let (home, project) = current_fixture("25.0.4+101");
    std::fs::write(project.join(".jlorc"), "21\n").unwrap();

    current_cmd(home.path(), &project)
        .env("JAVA_HOME", store_base(home.path()).join("25.0.4+101"))
        .assert()
        .success()
        .code(0)
        .stdout("25.0.4+101  (active)\n")
        .stderr(predicate::str::contains(
            "./.jlorc pins Java 21; run 'jlo env' to switch.",
        ));
}

/// Case 4. Nothing is configured, but the active JDK is the newest installed
/// one - which is stage 3 of the cascade, i.e. exactly what a bare `jlo env`
/// here would resolve to. Saying so is more useful than "nothing pinned",
/// which used to be the answer and said nothing about why this JDK.
#[test]
fn current_names_the_newest_installed_jdk_when_nothing_is_configured() {
    let (home, project) = current_fixture("25.0.4+101");

    current_cmd(home.path(), &project)
        .env("JAVA_HOME", store_base(home.path()).join("25.0.4+101"))
        .assert()
        .success()
        .code(0)
        .stdout("25.0.4+101  (from the newest installed JDK)\n")
        .stderr(predicate::str::is_empty());
}

/// Stage 3 names an *install*, not a major, so the check behind it has to be
/// one too. Nothing configured, 21.0.5+11 active, 21.0.6+7 installed beside
/// it: the majors agree, but a bare `jlo env` here would resolve major 21 and
/// then hand back 21.0.6+7, so calling the active build "the newest installed
/// JDK" would claim more than is true.
#[test]
fn current_does_not_call_an_older_build_of_the_same_major_the_newest_install() {
    let (home, project) = store_fixture(&["21.0.5+11", "21.0.6+7"]);

    current_cmd(home.path(), &project)
        .env("JAVA_HOME", store_base(home.path()).join("21.0.5+11"))
        .assert()
        .success()
        .code(0)
        .stdout("21.0.5+11  (active, nothing pinned)\n")
        .stderr(predicate::str::is_empty());
}

/// "Nothing pinned" survives as the answer for the state it actually
/// describes: nothing configured, and the active JDK is not what the cascade
/// would pick either. No warning - stage 3 is not a pin, so a shell on an
/// older major is not wrong about anything anyone asked for.
#[test]
fn current_says_nothing_is_pinned_when_the_cascade_would_pick_another_major() {
    let (home, project) = store_fixture(&["17.0.11+9", "25.0.4+101"]);

    current_cmd(home.path(), &project)
        .env("JAVA_HOME", store_base(home.path()).join("17.0.11+9"))
        .assert()
        .success()
        .code(0)
        .stdout("17.0.11+9  (active, nothing pinned)\n")
        .stderr(predicate::str::is_empty());
}

/// Case 5: a JDK jlo does not manage. The path is the whole answer - jlo is
/// not managing this, and naming a version would claim knowledge it does not
/// have. No advisory either: whatever is pinned, jlo did not put this here.
#[test]
fn current_reports_a_foreign_java_home_by_path() {
    let (home, project) = current_fixture("25.0.4+101");
    std::fs::write(project.join(".jlorc"), "21\n").unwrap();

    current_cmd(home.path(), &project)
        .env("JAVA_HOME", "/opt/jdk-21")
        .assert()
        .success()
        .code(0)
        .stdout("/opt/jdk-21  ($JAVA_HOME, set outside jlo)\n")
        .stderr(predicate::str::is_empty());
}

/// Case 6, reachable by exactly one route: `jlo remove` on the JDK the current
/// shell is using. Without it this state reports as case 5, which is wrong -
/// the install *was* ours, it is simply gone.
#[test]
fn current_reports_a_removed_install_rather_than_calling_it_foreign() {
    let (home, project) = current_fixture("25.0.4+101");
    let jdk = store_base(home.path()).join("25.0.4+101");
    std::fs::remove_dir_all(&jdk).unwrap();

    current_cmd(home.path(), &project)
        .env("JAVA_HOME", &jdk)
        .assert()
        .failure()
        .code(1)
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains(
            "points at a jlo install that is no longer there",
        ))
        .stderr(predicate::str::contains("jlo env"));
}

/// The command answers from disk and the environment alone. The API URL points
/// at a port nothing listens on, so a request would surface as a connection
/// error instead of the answer.
#[test]
fn current_never_touches_the_network() {
    let (home, project) = current_fixture("25.0.4+101");

    current_cmd(home.path(), &project)
        .env("JAVA_HOME", store_base(home.path()).join("25.0.4+101"))
        .env("JLO_ADOPTIUM_API_URL", "http://127.0.0.1:1")
        .assert()
        .success()
        .code(0)
        .stdout("25.0.4+101  (from the newest installed JDK)\n");
}

/// No `--offline` flag: a flag that would always be on is noise, so the fact
/// is documented instead. And no version argument - `current` means the active
/// one; asking about an arbitrary version is `jlo home`'s job.
#[test]
fn current_takes_no_flags_or_version() {
    for extra in ["--offline", "21"] {
        let mut cmd = Command::cargo_bin("jlo-bin").unwrap();
        cmd.args(["current", extra]).assert().failure().code(2);
    }
}

// -- jlo env --verbose --
//
// `setup` prints nothing to stderr on purpose - the autoload hook calls it on
// every new shell and every cd - so the report is opt-in. These tests pin the
// two halves of the contract: it says where the version came from, and it says
// so on a run that changed nothing.

/// The version's provenance is the point, so the line names the file it came
/// from rather than just the JDK.
#[test]
fn env_verbose_names_the_config_the_version_came_from() {
    let (home, project) = current_fixture("25.0.4+101");
    std::fs::write(project.join(".jlorc"), "25\n").unwrap();

    Command::cargo_bin("jlo-bin")
        .unwrap()
        .args(["env", "--offline", "--verbose"])
        .current_dir(&project)
        .env("HOME", home.path())
        .env("JLO_HOME", home.path().join(".jlo"))
        .env_remove("JAVA_HOME")
        .env("PATH", "/usr/bin")
        .assert()
        .success()
        .code(0)
        .stdout(predicate::str::contains("export JAVA_HOME="))
        .stderr("25.0.4+101  (from ./.jlorc)\n");
}

/// The original complaint: `setup` only writes when something changed, so
/// "already correct" and "did nothing" looked identical. The line has to
/// print here too, and say which of the two it was.
#[test]
fn env_verbose_reports_a_run_that_changed_nothing() {
    let (home, project) = current_fixture("25.0.4+101");
    std::fs::write(project.join(".jlorc"), "25\n").unwrap();
    let jdk = store_base(home.path()).join("25.0.4+101");

    Command::cargo_bin("jlo-bin")
        .unwrap()
        .args(["env", "--offline", "--verbose"])
        .current_dir(&project)
        .env("HOME", home.path())
        .env("JLO_HOME", home.path().join(".jlo"))
        .env("JAVA_HOME", &jdk)
        .env("PATH", format!("{}:/usr/bin", jdk.join("bin").display()))
        .assert()
        .success()
        .code(0)
        // Nothing to export - the shell is already on it.
        .stdout(predicate::str::is_empty())
        .stderr("25.0.4+101  (from ./.jlorc, already active)\n");
}

/// An explicit argument is a provenance too, and `-v` is the short form.
#[test]
fn env_verbose_names_the_command_line_as_a_source() {
    let (home, project) = current_fixture("25.0.4+101");

    Command::cargo_bin("jlo-bin")
        .unwrap()
        .args(["env", "--offline", "-v", "25"])
        .current_dir(&project)
        .env("HOME", home.path())
        .env("JLO_HOME", home.path().join(".jlo"))
        .env_remove("JAVA_HOME")
        .env("PATH", "/usr/bin")
        .assert()
        .success()
        .code(0)
        .stderr("25.0.4+101  (from the command line)\n");
}

/// Without the flag the path stays silent, which is what keeps the autoload
/// hook from printing a line on every new shell and every cd.
#[test]
fn env_without_verbose_still_says_nothing() {
    let (home, project) = current_fixture("25.0.4+101");
    std::fs::write(project.join(".jlorc"), "25\n").unwrap();

    Command::cargo_bin("jlo-bin")
        .unwrap()
        .args(["env", "--offline"])
        .current_dir(&project)
        .env("HOME", home.path())
        .env("JLO_HOME", home.path().join(".jlo"))
        .env_remove("JAVA_HOME")
        .env("PATH", "/usr/bin")
        .assert()
        .success()
        .code(0)
        .stdout(predicate::str::contains("export JAVA_HOME="))
        .stderr(predicate::str::is_empty());
}

/// Not added to `home`: its stdout is a bare path for `$(jlo home)`, and it
/// has no no-op case to explain.
#[test]
fn home_has_no_verbose_flag() {
    Command::cargo_bin("jlo-bin")
        .unwrap()
        .args(["home", "--verbose", "25"])
        .assert()
        .failure()
        .code(2);
}

// -- the version-resolution cascade, end to end --
//
// Four stages: the nearest `.jlorc`, `$JLO_HOME/default.jlorc`, the newest
// JDK already installed, then the latest release, downloaded. `jlo home` is
// the probe throughout: it resolves exactly as `env` and `exec` do, and prints
// the answer as one line on stdout instead of exports a shell has to source.
//
// Every command below points `JLO_ADOPTIUM_API_URL` at a port nothing listens
// on. That turns "no network access" into an assertion rather than a claim: a
// run that reaches Adoptium fails with a connection error instead of printing
// a path.

/// `jlo home` against a fixture store, with the network wired to fail.
fn cascade_cmd(home: &std::path::Path, project: &std::path::Path, args: &[&str]) -> Command {
    let mut cmd = Command::cargo_bin("jlo-bin").unwrap();
    cmd.arg("home")
        .args(args)
        .current_dir(project)
        .env("HOME", home)
        .env("JLO_HOME", home.join(".jlo"))
        .env("JLO_ADOPTIUM_API_URL", "http://127.0.0.1:1")
        .env_remove("JAVA_HOME");
    cmd
}

fn installed_path(home: &std::path::Path, version: &str) -> String {
    format!("{}\n", store_base(home).join(version).display())
}

/// Stage 3. Nothing is configured, so the newest JDK on disk answers - and
/// answers without a round trip, which the unreachable API proves.
#[test]
fn home_falls_back_to_the_newest_installed_jdk() {
    let (home, project) = store_fixture(&["17.0.11+9", "21.0.5+11"]);

    cascade_cmd(home.path(), &project, &[])
        .assert()
        .success()
        .code(0)
        .stdout(installed_path(home.path(), "21.0.5+11"));
}

/// The case this cascade is most easily got wrong in. A machine holding only
/// an outdated major resolves to *that* major: stage 3 never asks Adoptium
/// whether something newer exists, because putting a network round trip on
/// every bare `jlo env` to answer a question `jlo update` already answers
/// would be the wrong trade. Nothing is downloaded, and 25 does not appear.
#[test]
fn home_keeps_an_outdated_install_rather_than_downloading_a_newer_major() {
    let (home, project) = store_fixture(&["17.0.11+9"]);

    cascade_cmd(home.path(), &project, &[])
        .assert()
        .success()
        .code(0)
        .stdout(installed_path(home.path(), "17.0.11+9"));
}

/// Stage 4, reached only when nothing is configured *and* nothing is
/// installed. The download is what fails here, which is the point: the old
/// behaviour refused to resolve at all and sent the user to `jlo init`.
#[test]
fn home_reaches_for_the_latest_release_when_nothing_is_installed() {
    let (home, project) = store_fixture(&[]);

    cascade_cmd(home.path(), &project, &[])
        .assert()
        .failure()
        .code(1)
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains(
            "could not fetch latest JDK version",
        ))
        .stderr(predicate::str::contains("jlo init").not());
}

/// The same machine with `--offline`: the cascade stops one stage short and
/// reports the refusal it has always reported. This is why the autoload hook,
/// which calls `jlo env --offline` on every `cd`, can never start a download.
#[test]
fn home_offline_refuses_instead_of_downloading() {
    let (home, project) = store_fixture(&[]);

    // The whole sentence, not two substrings of it: the decision was that
    // this message stays exactly as it was when a missing config was the
    // ordinary outcome, so the wording is the thing under test.
    cascade_cmd(home.path(), &project, &["--offline"])
        .assert()
        .failure()
        .code(1)
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains(
            "No '.jlorc' found in the current directory or its parents, and no default \
             config file. Please run 'jlo init' to create a configuration file.",
        ));
}

/// `--offline` stops *after* stage 3, not before it: what is already on disk
/// costs no network, so it is still an answer.
#[test]
fn home_offline_still_uses_the_newest_installed_jdk() {
    let (home, project) = store_fixture(&["21.0.5+11"]);

    cascade_cmd(home.path(), &project, &["--offline"])
        .assert()
        .success()
        .code(0)
        .stdout(installed_path(home.path(), "21.0.5+11"));
}

/// Stage 2 beats stage 3. `jlo init --global` is how a user asks for a stable
/// answer on neutral ground, and a JDK installed for some other project must
/// not quietly override it - which is the one sharp edge of resolving to
/// "whatever is newest here".
#[test]
fn a_default_config_beats_the_newest_installed_jdk() {
    let (home, project) = store_fixture(&["17.0.11+9", "25.0.4+101"]);
    let jlo_home = home.path().join(".jlo");
    std::fs::create_dir_all(&jlo_home).unwrap();
    std::fs::write(jlo_home.join("default.jlorc"), "17\n").unwrap();

    cascade_cmd(home.path(), &project, &[])
        .assert()
        .success()
        .code(0)
        .stdout(installed_path(home.path(), "17.0.11+9"));
}

/// Stage 1 beats stage 2, which beats stage 3 - the whole order in one run.
#[test]
fn a_project_config_beats_both_the_default_and_the_newest_install() {
    let (home, project) = store_fixture(&["17.0.11+9", "21.0.5+11", "25.0.4+101"]);
    let jlo_home = home.path().join(".jlo");
    std::fs::create_dir_all(&jlo_home).unwrap();
    std::fs::write(jlo_home.join("default.jlorc"), "17\n").unwrap();
    std::fs::write(project.join(".jlorc"), "21\n").unwrap();

    cascade_cmd(home.path(), &project, &[])
        .assert()
        .success()
        .code(0)
        .stdout(installed_path(home.path(), "21.0.5+11"));
}

/// `jlo env --verbose` names the stage too, through the same formatter
/// `jlo current` uses. The exports still go to stdout; the report is the
/// stderr line.
#[test]
fn env_verbose_names_the_newest_installed_jdk_as_the_source() {
    let (home, project) = store_fixture(&["21.0.5+11"]);

    let mut cmd = Command::cargo_bin("jlo-bin").unwrap();
    cmd.args(["env", "--offline", "--verbose"])
        .current_dir(&project)
        .env("HOME", home.path())
        .env("JLO_HOME", home.path().join(".jlo"))
        .env("JLO_ADOPTIUM_API_URL", "http://127.0.0.1:1")
        .env_remove("JAVA_HOME")
        .assert()
        .success()
        .code(0)
        // stdout is the environment channel: the report must not leak into
        // the stream the jlo shell function evaluates.
        .stdout(
            predicate::str::is_match(
                r"^export JAVA_HOME='[^']*21\.0\.5\+11'\nexport PATH='[^']*'\n$",
            )
            .unwrap(),
        )
        .stderr("21.0.5+11  (from the newest installed JDK)\n");
}
