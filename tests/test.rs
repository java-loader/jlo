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
        .stdout(predicate::str::contains("remove"))
        .stdout(predicate::str::contains("init"))
        .stdout(predicate::str::contains("selfupdate"))
        .stdout(predicate::str::contains("completions"));
    // No assertion for "version" here: since the `version` subcommand was
    // removed, that substring would still match vacuously against the
    // `-V, --version` line in the Options block, making it a false pass
    // rather than real coverage of the subcommand list.
}

/// The names jlo used to have, and the one it hides.
///
/// `clean` became `prune`, `prune` became `jlo remove --superseded`,
/// `default` folded into `jlo init --global`, `version` became `-V`, and
/// `env --verbose` gave way to `jlo current`. None was kept as an alias.
/// `sing` is the other half of the same rule: it still works, and it is
/// likewise not discoverable.
///
/// The four channels below are the whole of "not discoverable", and they are
/// four because `hide = true` only ever suppressed the first: the `--help`
/// listing, the two generated completion scripts, and clap's typo-suggestion
/// engine. A name leaking into a completion script is the drift the CLI
/// rewrite exists to eliminate, so this is one test over a list rather than
/// one test per name.
#[test]
fn removed_and_hidden_names_are_nowhere_to_be_found() {
    // A bare substring would false-positive: "version" appears in the
    // `-V, --version` option line and throughout the .jlorc prose, and "sing"
    // sits inside "using", "missing", "parsing". Match the whole word, and
    // for the help listing the shape a *subcommand entry* takes.
    fn absent(
        word: &str,
    ) -> predicates::boolean::NotPredicate<predicates::str::RegexPredicate, str> {
        predicate::str::is_match(format!(r"(?i)\b{word}\b"))
            .unwrap()
            .not()
    }

    let bash = Command::cargo_bin("jlo-bin")
        .unwrap()
        .args(["completions", "bash"])
        .assert()
        .success();
    let bash = String::from_utf8(bash.get_output().stdout.clone()).unwrap();
    let zsh = Command::cargo_bin("jlo-bin")
        .unwrap()
        .args(["completions", "zsh"])
        .assert()
        .success();
    let zsh = String::from_utf8(zsh.get_output().stdout.clone()).unwrap();

    for name in ["clean", "prune", "default", "version", "sing"] {
        // 1. Not a line in the Commands: listing. `version` is the reason
        //    this is anchored rather than a `contains`.
        Command::cargo_bin("jlo-bin")
            .unwrap()
            .arg("--help")
            .assert()
            .success()
            .stdout(
                predicate::str::is_match(format!(r"(?m)^\s*{name}(\s|$)"))
                    .unwrap()
                    .not(),
            );

        // 2. and 3. Not a case in either generated completion script - the
        //    shape every real subcommand takes there.
        assert!(
            !bash.contains(&format!("jlo,{name})"))
                && !bash.contains(&format!("jlo__subcmd__{name}")),
            "'{name}' leaked into the bash completion script"
        );
        assert!(
            !zsh.lines()
                .any(|line| line.starts_with(&format!("'{name}:"))),
            "'{name}' leaked into the zsh completion script"
        );
    }

    // 4. Typing one is an ordinary usage error, and clap cannot suggest what
    //    it does not know about.
    for (typo, name) in [("vrsion", "version"), ("sng", "sing"), ("prnue", "prune")] {
        Command::cargo_bin("jlo-bin")
            .unwrap()
            .arg(typo)
            .assert()
            .failure()
            .stderr(absent(name));
    }

    // Removed means gone, not hidden-but-working.
    for name in ["clean", "prune", "default", "version"] {
        Command::cargo_bin("jlo-bin")
            .unwrap()
            .arg(name)
            .assert()
            .failure();
    }

    // The easter egg is hidden, not removed.
    Command::cargo_bin("jlo-bin")
        .unwrap()
        .arg("sing")
        .assert()
        .success()
        .code(0)
        .stderr(predicate::str::contains("There are no Easter Eggs"));

    // `env --verbose` went the same way, and has no subcommand entry to check.
    Command::cargo_bin("jlo-bin")
        .unwrap()
        .args(["env", "--verbose"])
        .assert()
        .failure()
        .code(2);
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

/// The rule-based half of `remove` had no command-level test at all while it
/// was `jlo prune`: every one of its 12 tests called `JdkStore::prune`
/// directly, so the wiring and the report were unproven. This covers all
/// three things the report says in one run - what went, what stayed, and the
/// unmanaged install it left alone.
#[test]
fn remove_superseded_deletes_only_the_older_managed_builds() {
    let home = tempfile::tempdir().unwrap();
    install_fake_jdk(home.path(), "21.0.11+10");
    install_fake_jdk(home.path(), "21.0.9+10");
    // No `.jlo-managed` marker: the rule skips it rather than refusing, since
    // nobody named it.
    let unmanaged = jdk_store_in(home.path()).join("17.0.11+10");
    std::fs::create_dir_all(unmanaged.join("bin")).unwrap();

    Command::cargo_bin("jlo-bin")
        .unwrap()
        .args(["remove", "--superseded"])
        .env("HOME", home.path())
        .env("JLO_ADOPTIUM_API_URL", "http://127.0.0.1:1")
        .env_remove("JAVA_HOME")
        .assert()
        .success()
        // ADR-0001: deletion is not the environment channel.
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("21.0.9+10"))
        .stderr(predicate::str::contains("Removed 1 JDK"))
        .stderr(predicate::str::contains("left 1 install alone"));

    let store = jdk_store_in(home.path());
    assert!(store.join("21.0.11+10").exists(), "the newest minor stays");
    assert!(!store.join("21.0.9+10").exists(), "the older minor goes");
    assert!(unmanaged.exists(), "an unmanaged install is never deleted");
}

/// The rule takes no version, and clap has to say so rather than silently
/// ignoring one: `jlo remove --superseded 21` reads as "only the superseded
/// 21s", which is not what it would do.
#[test]
fn remove_superseded_refuses_a_version_alongside_it() {
    let mut cmd = Command::cargo_bin("jlo-bin").unwrap();
    cmd.args(["remove", "--superseded", "21"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("cannot be used with"));
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
    // An empty store, not the developer's: "no JDKs installed" is a success
    // with a note on stderr, and reading the real store would make the
    // outcome depend on what happens to be on the machine.
    let home = tempfile::tempdir().unwrap();

    Command::cargo_bin("jlo-bin")
        .unwrap()
        .args(["list", "--offline"])
        .env("HOME", home.path())
        .env("JLO_ADOPTIUM_API_URL", "http://127.0.0.1:1")
        .assert()
        .success()
        .code(0)
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("No JDKs installed"));
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
        .stderr(predicate::str::contains(
            "`jlo remove --superseded` (1 superseded)",
        ));
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

// -- the three tests that really talk to Adoptium --
//
// ADR-0002: the fixtures can drift silently from the live API, and these are
// the only thing that would notice. Everything else runs against mockito or a
// dead port.
//
// They install a real ~200 MB JDK, so `$HOME` has to move: `JdkStore::base()`
// is $HOME-derived (ADR-0005), and without this they wrote into the
// developer's own ~/Library/Java/JavaVirtualMachines and read back whatever
// was already there - `cargo test --release` mutated the machine, and the
// result depended on what had been installed beforehand.
//
// The replacement is one directory under `target/`, not a fresh temp dir per
// test: all three share it, so a run downloads at most once, and CI can cache
// the path across runs. It deliberately survives between runs on a developer
// machine too - the second `cargo test` is then offline for these.

/// The `$HOME` the real-network tests install into.
fn network_test_home() -> std::path::PathBuf {
    let home = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("network-home");
    std::fs::create_dir_all(&home).unwrap();
    home
}

#[test]
#[serial]
fn init_with_version() {
    let temp_dir = tempfile::tempdir().unwrap();
    std::env::set_current_dir(&temp_dir).unwrap();

    let mut cmd = Command::cargo_bin("jlo-bin").unwrap();
    cmd.args(["init", "21"])
        // An explicit version never asks Adoptium anything.
        .env("JLO_ADOPTIUM_API_URL", "http://127.0.0.1:1")
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

/// A bare `jlo init` pins the latest release, which is the one thing it asks
/// Adoptium for - so mockito answers, rather than this being a fourth
/// real-network test. ADR-0002 names three, and now there are three.
#[test]
#[serial]
fn init() {
    let mut server = mockito::Server::new();
    let _r = server
        .mock("GET", "/v3/info/available_releases")
        .with_body(include_str!("fixtures/available_releases.json"))
        .create();

    // create a temp dir and switch to it
    let temp_dir = tempfile::tempdir().unwrap();
    std::env::set_current_dir(&temp_dir).unwrap();

    // run init
    let mut cmd = Command::cargo_bin("jlo-bin").unwrap();
    cmd.arg("init")
        .env("JLO_ADOPTIUM_API_URL", server.url())
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
        .env("JLO_ADOPTIUM_API_URL", server.url())
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
    // The JDK store is $HOME-derived (ADR-0005), so every command below is
    // given one under `target/` rather than the developer's own.
    let home = network_test_home();
    unsafe {
        std::env::set_var("JLO_HOME", temp_dir.path());
    }

    // switch to temp dir and create .jlorc with "25"
    std::env::set_current_dir(&temp_dir).unwrap();
    std::fs::write(".jlorc", "25").unwrap();

    // run home (version resolved from .jlorc)
    let mut cmd = Command::cargo_bin("jlo-bin").unwrap();
    cmd.env("HOME", &home);
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
    cmd.env("HOME", &home);
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
    // The JDK store is $HOME-derived (ADR-0005), so every command below is
    // given one under `target/` rather than the developer's own.
    let home = network_test_home();
    unsafe {
        std::env::set_var("JLO_HOME", temp_dir.path());
    }

    // switch to temp dir and create .jlorc with "25"
    std::env::set_current_dir(&temp_dir).unwrap();
    std::fs::write(".jlorc", "25").unwrap();

    // exit code of the child propagates through exec
    let mut cmd = Command::cargo_bin("jlo-bin").unwrap();
    cmd.env("HOME", &home);
    cmd.args(["exec", "25", "--", "sh", "-c", "exit 7"])
        .assert()
        .failure()
        .code(7);

    // JAVA_HOME is set in the child and its bin is first on PATH
    let mut cmd = Command::cargo_bin("jlo-bin").unwrap();
    cmd.env("HOME", &home);
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
    cmd.env("HOME", &home);
    cmd.args(["exec", "--", "java", "-version"])
        .assert()
        .success()
        .code(0)
        .stderr(predicate::str::contains("openjdk version \"25"));

    // missing '--' is a usage error
    let mut cmd = Command::cargo_bin("jlo-bin").unwrap();
    cmd.env("HOME", &home);
    cmd.args(["exec", "25", "java", "-version"])
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains("expected '--'"));

    // a command that cannot be launched exits 127 with an error on stderr
    let mut cmd = Command::cargo_bin("jlo-bin").unwrap();
    cmd.env("HOME", &home);
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
    cmd.env("HOME", &home);
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
    cmd.env("HOME", &home);
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
    // The JDK store is $HOME-derived (ADR-0005), so every command below is
    // given one under `target/` rather than the developer's own.
    let home = network_test_home();
    unsafe {
        std::env::set_var("JLO_HOME", temp_dir.path());
    }

    // switch to temp dir and create .jlorc with "25"
    std::env::set_current_dir(&temp_dir).unwrap();
    std::fs::write(".jlorc", "25").unwrap();

    // run env
    let mut cmd = Command::cargo_bin("jlo-bin").unwrap();
    cmd.env("HOME", &home);
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

/// Stage 3 is GA-only, so a shell on a pre-release is never "the newest
/// installed JDK" - a bare `jlo env` here resolves 21 and hands back
/// 21.0.5+11, not the beta. Nothing configured, the beta active, a released
/// build installed beside it.
#[test]
fn current_does_not_call_a_pre_release_the_newest_install() {
    let (home, project) = store_fixture(&["21.0.5+11", "28.0.0-beta+16.0.ea"]);

    current_cmd(home.path(), &project)
        .env(
            "JAVA_HOME",
            store_base(home.path()).join("28.0.0-beta+16.0.ea"),
        )
        .assert()
        .success()
        .code(0)
        .stdout("28.0.0-beta+16.0.ea  (active, nothing pinned)\n")
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

/// The other way to be inside the store and absent from the listing, and the
/// one the test above must not swallow: a vendor-named directory, which is
/// what the IDE's own JDK downloads land as. The store is shared with
/// `IntelliJ` by design, and `is_jdk_version_dir` deliberately keeps a
/// non-semver name out of `jlo list` - so "unlistable" alone cannot mean
/// "gone", and only an existence check tells the two apart.
/// `jlo list --offline` already calls this JDK foreign, and `jlo current` has
/// to agree with it.
#[test]
fn current_calls_a_vendor_named_jdk_in_the_store_foreign_rather_than_missing() {
    let (home, project) = current_fixture("25.0.4+101");
    let vendor = store_base(home.path()).join("temurin-17.0.9");
    std::fs::create_dir_all(vendor.join("bin")).unwrap();

    current_cmd(home.path(), &project)
        .env("JAVA_HOME", &vendor)
        .assert()
        .success()
        .code(0)
        .stdout(predicate::str::contains("($JAVA_HOME, set outside jlo)"))
        .stderr(predicate::str::is_empty());
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

// -- jlo env's output channel --

/// ADR-0001: on success `env` writes export statements to stdout and nothing
/// anywhere else. `--verbose` used to add a stderr line here; it is gone,
/// because `jlo current` answers the same question from the live `JAVA_HOME`
/// and can therefore also report drift. What must not come back is a second
/// writer on this path - the autoload hook runs it on every new shell and
/// every cd.
#[test]
fn env_writes_exports_to_stdout_and_nothing_to_stderr() {
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

// -- the pre-bundle macOS layout --
//
// `store_fixture` builds a flat install carrying the in-directory marker,
// which is exactly what jlo wrote before it kept the macOS bundle. Both tests
// pass the same fixture and the same `--offline`, and differ only in the verb:
// that is the whole of the rule, so they are worth having as a pair.

/// A person asking where a JDK is gets told when that JDK is one
/// `/usr/libexec/java_home` cannot see. On stderr, so `JH=$(jlo home 25)`
/// still gets only the path.
#[test]
#[serial]
#[cfg(target_os = "macos")]
fn home_warns_that_a_pre_bundle_install_is_invisible_to_java_home() {
    let (home, project) = current_fixture("25.0.4+101");

    Command::cargo_bin("jlo-bin")
        .unwrap()
        .args(["home", "--offline", "25"])
        .current_dir(&project)
        .env("HOME", home.path())
        .env("JLO_HOME", home.path().join(".jlo"))
        .env("JLO_ADOPTIUM_API_URL", "http://127.0.0.1:1")
        .assert()
        .success()
        .stdout(predicate::str::contains("25.0.4+101"))
        .stderr(predicate::str::contains("java_home"))
        .stderr(predicate::str::contains("jlo install 25"));
}

/// The other funnel. `home` without `--offline` resolves through
/// `resolve_java_home`, and an installed JDK is answered before the client is
/// ever used - which is why a dead API URL is enough here. Without this the
/// online path could lose the warning with every test still green.
#[test]
#[serial]
#[cfg(target_os = "macos")]
fn the_online_funnel_warns_about_a_pre_bundle_install_too() {
    let (home, project) = current_fixture("25.0.4+101");

    Command::cargo_bin("jlo-bin")
        .unwrap()
        .args(["home", "25"])
        .current_dir(&project)
        .env("HOME", home.path())
        .env("JLO_HOME", home.path().join(".jlo"))
        .env("JLO_ADOPTIUM_API_URL", "http://127.0.0.1:1")
        .assert()
        .success()
        .stdout(predicate::str::contains("25.0.4+101"))
        .stderr(predicate::str::contains("java_home"));
}

/// The same install, the same flag, the other verb - and silence. `env
/// --offline` is how the autoload hook runs, on every new shell and every
/// `cd`; a warning there would print forever and train the user to ignore it.
#[test]
#[serial]
#[cfg(target_os = "macos")]
fn env_offline_stays_silent_about_a_pre_bundle_install() {
    let (home, project) = current_fixture("25.0.4+101");

    Command::cargo_bin("jlo-bin")
        .unwrap()
        .args(["env", "--offline", "25"])
        .current_dir(&project)
        .env("HOME", home.path())
        .env("JLO_HOME", home.path().join(".jlo"))
        .env_remove("JAVA_HOME")
        .env("PATH", "/usr/bin")
        .env("JLO_ADOPTIUM_API_URL", "http://127.0.0.1:1")
        .assert()
        .success()
        .stdout(predicate::str::contains("export JAVA_HOME="))
        .stderr(predicate::str::is_empty());
}

/// The flag is gone from `env` as well as from `home`, and gone means a usage
/// error rather than a silently accepted no-op.
#[test]
fn neither_env_nor_home_has_a_verbose_flag() {
    for verb in ["env", "home"] {
        Command::cargo_bin("jlo-bin")
            .unwrap()
            .args([verb, "--verbose", "25"])
            .assert()
            .failure()
            .code(2);
    }
}

// -- every rule survives without colour --
//
// ADR-0007: `NO_COLOR`, `CLICOLOR=0` and `TERM=dumb` are handled by `console`
// itself, so no rule in that ADR may depend on colour *alone* to be
// understood. None of the three appeared in any test file, and neither did
// the plainer version of the same question: whether anything writes an escape
// sequence by hand, which `console` would not strip whatever the environment
// says.

/// The status words are words, `!` and the tick are distinct characters, and
/// the active marker is distinguished by *position* - so a run with colour
/// turned off every way it can be turned off still carries every distinction,
/// and carries no escape bytes at all.
#[test]
fn a_listing_reads_the_same_with_colour_off() {
    let home = tempfile::tempdir().unwrap();
    install_fake_jdk(home.path(), "21.0.11+10");
    install_fake_jdk(home.path(), "21.0.9+10");
    let active = jdk_store_in(home.path()).join("21.0.9+10");

    let assert = Command::cargo_bin("jlo-bin")
        .unwrap()
        .args(["list", "--offline"])
        .env("HOME", home.path())
        .env("JAVA_HOME", &active)
        .env("JLO_ADOPTIUM_API_URL", "http://127.0.0.1:1")
        .env("NO_COLOR", "1")
        .env("CLICOLOR", "0")
        .env("TERM", "dumb")
        .assert()
        .success();

    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();

    // Not one escape byte, on either stream. `console` answers for its own
    // `style()` calls; a hand-written "\x1b[31m" anywhere is what this sees.
    for (name, stream) in [("stdout", &stdout), ("stderr", &stderr)] {
        assert!(
            !stream.contains('\u{1b}'),
            "{name} carries an escape sequence with colour off: {stream:?}"
        );
    }

    // The distinctions survive as text: a status word per row, and the active
    // row marked by the gutter column rather than by a colour.
    assert_eq!(
        stdout, "    21  21.0.11+10  installed\n \u{2192}  21  21.0.9+10   superseded\n",
        "with colour off the rows must still say which is which"
    );
    assert!(
        stderr.contains("`jlo remove --superseded` (1 superseded)"),
        "the tip must still name its command: {stderr:?}"
    );
}

/// The other half: a destructive run says what it did in characters, not in
/// green. The tick and the `!` are the closed marker vocabulary of rule 5.
#[test]
fn a_deletion_report_reads_the_same_with_colour_off() {
    let home = tempfile::tempdir().unwrap();
    install_fake_jdk(home.path(), "21.0.11+10");
    install_fake_jdk(home.path(), "21.0.9+10");

    let assert = Command::cargo_bin("jlo-bin")
        .unwrap()
        .args(["remove", "--superseded"])
        .env("HOME", home.path())
        .env("JLO_ADOPTIUM_API_URL", "http://127.0.0.1:1")
        .env_remove("JAVA_HOME")
        .env("NO_COLOR", "1")
        .env("CLICOLOR", "0")
        .env("TERM", "dumb")
        .assert()
        .success();

    let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    assert!(!stderr.contains('\u{1b}'), "{stderr:?}");
    assert!(stderr.contains("\u{2713} Removed 1 JDK"), "{stderr:?}");
}

// -- the cascade is wired into every verb that resolves a version --
//
// The rule itself - which of the four stages wins, and where `--offline`
// stops - is unit-tested in `src/main.rs` (`cascade_*`) against the same
// cases, without a process spawn or a temp store. What a unit test cannot
// reach is the wiring: each verb has to call the cascade rather than grow a
// lookup of its own. That is what this section covers, one case per verb.
//
// Stage 3 (the newest installed JDK) is the probe, because it is the last
// stage every verb can reach without the network - and every command below
// points `JLO_ADOPTIUM_API_URL` at a port nothing listens on, which turns
// "no network access" into an assertion rather than a claim.

/// A command run against a fixture store, with the network wired to fail.
fn cascade_cmd(home: &std::path::Path, project: &std::path::Path, args: &[&str]) -> Command {
    let mut cmd = Command::cargo_bin("jlo-bin").unwrap();
    cmd.args(args)
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

/// Stage 4, reached only when nothing is configured *and* nothing is
/// installed. The download is what fails here, which is the point: the old
/// behaviour refused to resolve at all and sent the user to `jlo init`.
#[test]
fn a_bare_command_reaches_for_the_latest_release_when_nothing_is_installed() {
    let (home, project) = store_fixture(&[]);

    cascade_cmd(home.path(), &project, &["home"])
        .assert()
        .failure()
        .code(1)
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains(
            "could not fetch latest JDK version",
        ))
        .stderr(predicate::str::contains("jlo init").not());
}

#[test]
fn home_resolves_the_newest_installed_jdk() {
    let (home, project) = store_fixture(&["17.0.11+9", "21.0.5+11"]);

    cascade_cmd(home.path(), &project, &["home"])
        .assert()
        .success()
        .code(0)
        .stdout(installed_path(home.path(), "21.0.5+11"));
}

/// The same answer, as exports. stdout is the environment channel here - the
/// jlo shell function evaluates this stream - so the assertion is the whole
/// of it, not a substring.
#[test]
fn env_resolves_the_newest_installed_jdk_and_exports_only_that() {
    let (home, project) = store_fixture(&["21.0.5+11"]);

    cascade_cmd(home.path(), &project, &["env", "--offline"])
        .assert()
        .success()
        .code(0)
        .stdout(
            predicate::str::is_match(
                r"^export JAVA_HOME='[^']*21\.0\.5\+11'\nexport PATH='[^']*'\n$",
            )
            .unwrap(),
        )
        .stderr(predicate::str::is_empty());
}

/// `exec` had no cascade coverage at all: the version is optional there too,
/// and the child is where the answer shows up.
#[cfg(unix)]
#[test]
fn exec_resolves_the_newest_installed_jdk_for_the_child() {
    let (home, project) = store_fixture(&["21.0.5+11"]);

    cascade_cmd(
        home.path(),
        &project,
        &["exec", "--", "sh", "-c", "echo \"$JAVA_HOME\""],
    )
    .assert()
    .success()
    .code(0)
    .stdout(installed_path(home.path(), "21.0.5+11"));
}

/// The whole of decision B, from the outside: a store holding only a
/// pre-release answers a bare `jlo env --offline` with a failure, not with the
/// beta.
#[test]
#[serial]
fn bare_env_offline_ignores_an_ea_install() {
    let (home, project) = store_fixture(&["28.0.0-beta+16.0.ea"]);

    cascade_cmd(home.path(), &project, &["env", "--offline"])
        .assert()
        .failure()
        .stdout("");
}

/// `update` and `install` resolve through the same cascade, and both then ask
/// Adoptium about whatever it produced. With the network wired to fail, the
/// version named in the failure is the assertion: reaching Adoptium at all
/// means stage 3 answered.
#[test]
fn update_and_install_resolve_the_newest_installed_jdk() {
    for verb in ["update", "install"] {
        let (home, project) = store_fixture(&["21.0.5+11"]);

        cascade_cmd(home.path(), &project, &[verb])
            .assert()
            .failure()
            .code(1)
            .stdout(predicate::str::is_empty())
            // Not "no valid Java versions provided": the cascade produced 21,
            // and it is the request about 21 that failed.
            .stderr(predicate::str::contains("Adoptium"));
    }
}

// -- the success paths of update and remove --
//
// Both were covered only by their refusals. `update` needs Adoptium to answer,
// so it runs against mockito with the captured asset response; `remove` never
// touches the network.

/// A major already on its latest build is reported and left alone - the
/// branch that decides not to download.
#[test]
fn update_leaves_a_major_already_on_its_latest_build_alone() {
    let mut server = mockito::Server::new();
    let _a = server
        .mock(
            "GET",
            mockito::Matcher::Regex(r"^/v3/assets/latest/21/hotspot".to_string()),
        )
        .match_query(mockito::Matcher::Any)
        .with_body(include_str!("fixtures/assets_latest.json"))
        .create();

    let home = tempfile::tempdir().unwrap();
    // The exact semver the fixture names, so `find_exact` matches.
    install_fake_jdk(home.path(), "21.0.11+10.0.LTS");

    Command::cargo_bin("jlo-bin")
        .unwrap()
        .args(["update", "21"])
        .env("HOME", home.path())
        .env("JLO_ADOPTIUM_API_URL", server.url())
        .env_remove("JAVA_HOME")
        .assert()
        .success()
        .code(0)
        // Nothing was downloaded, so nothing is superseded and no hint follows.
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("21.0.11+10.0.LTS"))
        .stderr(predicate::str::contains("superseded").not());
}

/// Naming a major removes every build of it, and says which ones went.
/// A deletion that could not be made must not exit 0. Both selectors printed
/// each failure and then a green tick over it - `Removed 0 JDKs` for a named
/// target, and for `--superseded` the flatly false `Nothing to remove (only
/// the newest of each major is installed)` about a store still holding every
/// one of them. A script chaining `jlo remove 21 && ...` reads the status, not
/// the lines.
///
/// The failure is made by taking write permission off the store directory,
/// which is what an install under a root-owned or read-only prefix looks
/// like; the permissions go back so the temp directory can be cleaned up.
#[test]
fn remove_reports_a_failed_deletion_as_a_failure() {
    for args in [vec!["remove", "21"], vec!["remove", "--superseded"]] {
        let home = tempfile::tempdir().unwrap();
        install_fake_jdk(home.path(), "21.0.11+10");
        install_fake_jdk(home.path(), "21.0.9+10");

        let store = jdk_store_in(home.path());
        let restore = std::fs::metadata(&store).unwrap().permissions();
        let mut readonly = restore.clone();
        std::os::unix::fs::PermissionsExt::set_mode(&mut readonly, 0o555);
        std::fs::set_permissions(&store, readonly).unwrap();

        let assertion = Command::cargo_bin("jlo-bin")
            .unwrap()
            .args(&args)
            .env("HOME", home.path())
            .env("JLO_ADOPTIUM_API_URL", "http://127.0.0.1:1")
            .env_remove("JAVA_HOME")
            .assert()
            .failure()
            .code(1)
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("could not be removed"));

        // The tick is reserved for a run that did what it was asked, so
        // neither of the two success lines may appear over a failure.
        assertion
            .stderr(predicate::str::contains("Removed").not())
            .stderr(predicate::str::contains("Nothing to remove").not());

        std::fs::set_permissions(&store, restore).unwrap();
        assert!(
            store.join("21.0.9+10").exists(),
            "{args:?}: nothing was actually deleted"
        );
    }
}

#[test]
fn remove_deletes_every_build_of_the_major_named() {
    let home = tempfile::tempdir().unwrap();
    install_fake_jdk(home.path(), "21.0.11+10");
    install_fake_jdk(home.path(), "21.0.9+10");
    install_fake_jdk(home.path(), "17.0.11+10");

    Command::cargo_bin("jlo-bin")
        .unwrap()
        .args(["remove", "21"])
        .env("HOME", home.path())
        .env("JLO_ADOPTIUM_API_URL", "http://127.0.0.1:1")
        .env_remove("JAVA_HOME")
        .assert()
        .success()
        .code(0)
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("Removed 2 JDKs"));

    let store = jdk_store_in(home.path());
    assert!(!store.join("21.0.11+10").exists());
    assert!(!store.join("21.0.9+10").exists());
    assert!(
        store.join("17.0.11+10").exists(),
        "another major is untouched"
    );
}
