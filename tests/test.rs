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

    // check if .jlorc contains a valid major version
    let content = std::fs::read_to_string(".jlorc").unwrap();
    let lines: Vec<_> = content.lines().collect();
    assert_eq!(
        lines[0],
        "# Java version configured by J'Lo - https://github.com/java-loader/jlo"
    );
    let version: u32 = lines[1].parse().expect("expected numeric version");
    assert!(version >= 8, "expected version >= 8, got {}", version);

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
    assert_eq!(stdout.lines().count(), 1, "stdout: {:?}", stdout);
    assert!(stdout.ends_with('\n'), "stdout must be newline-terminated");
    let java_home = stdout.trim_end();
    assert!(
        std::path::Path::new(java_home)
            .join("bin")
            .join("java")
            .exists(),
        "printed JAVA_HOME must contain bin/java: {}",
        java_home
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
        format!("{}/bin/java", java_home),
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
