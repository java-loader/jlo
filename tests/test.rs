use assert_cmd::Command;
use predicates::prelude::*;
use serial_test::serial;

#[test]
fn missing_arguments() {
    let mut cmd = Command::cargo_bin("jlo-bin").unwrap();
    cmd.assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains(r"Arguments missing."))
        .stdout("");
}

#[test]
fn version() {
    let mut cmd = Command::cargo_bin("jlo-bin").unwrap();
    cmd.arg("version")
        .assert()
        .success()
        .code(0)
        .stdout(predicate::str::is_match(r"^\d+\.\d+\.\d+\n$").unwrap());
}

#[test]
fn unknown_command() {
    let mut cmd = Command::cargo_bin("jlo-bin").unwrap();
    cmd.arg("nosuchcmd")
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains("Unknown command: nosuchcmd"));
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
        .stdout(predicate::str::contains("Java 21"));

    let content = std::fs::read_to_string(".jlorc").unwrap();
    let lines: Vec<_> = content.lines().collect();
    assert_eq!(lines[1], "21");

    std::env::set_current_dir(std::env::temp_dir()).unwrap();
    temp_dir.close().unwrap();
}

#[test]
fn default_missing_arg() {
    let mut cmd = Command::cargo_bin("jlo-bin").unwrap();
    cmd.arg("default")
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains("Missing argument"));
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
        .stdout(predicate::str::is_match(r"Created config file '.jlorc'").unwrap())
        .stderr("");

    // check if .jlorc contains "25"
    let content = std::fs::read_to_string(".jlorc").unwrap();
    let lines: Vec<_> = content.lines().collect();
    assert_eq!(
        lines,
        [
            "# Java version configured by J'Lo - https://github.com/java-loader/jlo",
            "25"
        ]
    );

    // run init again to check for existing file error
    let mut cmd = Command::cargo_bin("jlo-bin").unwrap();
    cmd.arg("init")
        .assert()
        .failure()
        .code(1)
        .stderr(
            predicate::str::is_match(
                r"Error: Could not create config file: File '.jlorc' already exists!",
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
