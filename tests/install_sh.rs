//! Tests for the profile snippet `install.sh` generates.
//!
//! The installer's user-facing contract is the block it tells people to paste
//! into a profile. That block used to be twelve lines that restated every
//! internal path, so an installer change meant every existing user had a stale
//! profile - and a re-install could emit a block with no `JLO_HOME` export at
//! all, leaving every `source` line below it a silent no-op.
//!
//! Now the installer generates three entry files under `$JLO_HOME` and the
//! profile only sources them. These tests run the real `install.sh` against a
//! temporary `HOME`, with `curl` stubbed out so nothing is downloaded, and then
//! source the generated files from a real shell.

// Test code: an `unwrap` failure here is a test failure, which is the point.
#![allow(clippy::unwrap_used)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const INTERPRETERS: &[&str] = &["/bin/bash", "zsh"];

fn manifest() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

#[must_use]
fn skip_missing(test: &str, sh: &str) -> bool {
    if Command::new(sh).arg("-c").arg("exit 0").output().is_ok() {
        return false;
    }
    eprintln!("SKIP {test}: {sh} is not installed here.");
    true
}

/// A release tarball with the same shape as the real one: the binary under
/// test plus the two shell scripts, all at the archive root.
fn release_tarball(dir: &Path) -> PathBuf {
    let stage = dir.join("stage");
    std::fs::create_dir_all(&stage).unwrap();
    std::fs::copy(
        assert_cmd::cargo::cargo_bin("jlo-bin"),
        stage.join("jlo-bin"),
    )
    .unwrap();
    for script in ["jlo-init.sh", "jlo-autoload.sh"] {
        std::fs::copy(manifest().join(script), stage.join(script)).unwrap();
    }
    let tarball = dir.join("jlo.tar.gz");
    let ok = Command::new("tar")
        .arg("-czf")
        .arg(&tarball)
        .arg("-C")
        .arg(&stage)
        .args(["jlo-bin", "jlo-init.sh", "jlo-autoload.sh"])
        .status()
        .unwrap()
        .success();
    assert!(ok, "could not build the stub release tarball");
    tarball
}

/// A `curl` that serves the stub tarball instead of reaching GitHub. Placed
/// first on `PATH` so `install.sh` itself needs no test-only branch.
fn stub_curl(dir: &Path, tarball: &Path) -> PathBuf {
    let bin = dir.join("stubbin");
    std::fs::create_dir_all(&bin).unwrap();
    let curl = bin.join("curl");
    std::fs::write(
        &curl,
        format!(
            "#!/bin/sh\n\
             # Ignores every flag but -o, which is all install.sh passes.\n\
             while [ $# -gt 0 ]; do\n\
             \x20 case \"$1\" in -o) shift; cp '{}' \"$1\" ; exit 0 ;; esac\n\
             \x20 shift\n\
             done\n\
             exit 1\n",
            tarball.display()
        ),
    )
    .unwrap();
    let mut perms = std::fs::metadata(&curl).unwrap().permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
    std::fs::set_permissions(&curl, perms).unwrap();
    bin
}

/// Runs the real installer against a throwaway `HOME`. `jlo_home` overrides
/// the install directory the way a user exporting `JLO_HOME` would.
fn install(jlo_home: Option<&str>) -> (tempfile::TempDir, Output) {
    let dir = tempfile::tempdir().unwrap();
    let tarball = release_tarball(dir.path());
    let stubbin = stub_curl(dir.path(), &tarball);
    let home = dir.path().join("home");
    std::fs::create_dir_all(&home).unwrap();

    let path = format!(
        "{}:{}",
        stubbin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let mut cmd = Command::new("/bin/sh");
    cmd.arg(manifest().join("install.sh"))
        .env("HOME", &home)
        .env("PATH", path)
        .env_remove("JLO_HOME");
    if let Some(h) = jlo_home {
        cmd.env("JLO_HOME", h.replace("$HOME", &home.display().to_string()));
    }
    let out = cmd.output().unwrap();
    assert!(
        out.status.success(),
        "install.sh failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    (dir, out)
}

/// POSIX single-quoting, so a path containing an apostrophe can be embedded in
/// the `-c` string these tests build.
fn squote(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', "'\\''"))
}

/// Sources `script` in a fresh interactive-style shell and runs `body`.
fn source_and_run(sh: &str, home: &Path, script: &Path, body: &str) -> Output {
    Command::new(sh)
        .arg("-c")
        .arg(format!(". {}\n{body}", squote(script)))
        .env("HOME", home)
        .env_remove("JLO_HOME")
        .output()
        .unwrap()
}

// ---------------------------------------------------------------------------

/// The one line a user cannot skip has to be enough on its own: it defines the
/// `jlo` function and exports the `JLO_HOME` the rest of the layout hangs off.
#[test]
fn sourcing_jlo_sh_alone_defines_the_wrapper_and_exports_jlo_home() {
    let (dir, _) = install(None);
    let home = dir.path().join("home");
    let entry = home.join(".jlo").join("jlo.sh");
    assert!(entry.is_file(), "install.sh did not generate {entry:?}");

    for sh in INTERPRETERS {
        if skip_missing(
            "sourcing_jlo_sh_alone_defines_the_wrapper_and_exports_jlo_home",
            sh,
        ) {
            continue;
        }
        // Read JLO_HOME back from a *child* process: a plain assignment would
        // satisfy an in-shell echo, but the binary and jlo-autoload.sh both
        // read it out of the environment.
        let out = source_and_run(
            sh,
            &home,
            &entry,
            "type jlo\n/bin/sh -c 'echo \"home=[$JLO_HOME]\"'",
        );
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            stdout.contains("function"),
            "{sh}: jlo is not a shell function after sourcing jlo.sh: {stdout:?}"
        );
        assert!(
            stdout.contains(&format!("home=[{}]", home.join(".jlo").display())),
            "{sh}: JLO_HOME not exported correctly: {stdout:?}"
        );
    }
}

/// The bug that motivated this: a re-install runs with `JLO_HOME` already
/// exported by the very profile block about to be replaced. The generated entry
/// file must set it unconditionally rather than assume a surviving profile line.
#[test]
fn a_reinstall_with_jlo_home_already_exported_still_exports_it() {
    let (dir, out) = install(Some("$HOME/.jlo"));
    let home = dir.path().join("home");
    let entry = home.join(".jlo").join("jlo.sh");
    let body = std::fs::read_to_string(&entry).unwrap();
    assert!(
        body.contains("export JLO_HOME="),
        "generated jlo.sh has no JLO_HOME export: {body}"
    );
    let printed = String::from_utf8_lossy(&out.stdout);
    assert!(
        !printed.contains("keep your existing export"),
        "installer still tells the user to keep an export it did not print: {printed}"
    );
}

/// A custom install directory stays supported: it is baked into the generated
/// files and into the path the installer tells the user to source.
#[test]
fn a_custom_jlo_home_is_baked_into_the_generated_files() {
    let (dir, out) = install(Some("$HOME/custom-jlo"));
    let home = dir.path().join("home");
    let custom = home.join("custom-jlo");
    let entry = custom.join("jlo.sh");
    assert!(entry.is_file(), "install.sh did not generate {entry:?}");

    let sh = "/bin/bash";
    if !skip_missing("a_custom_jlo_home_is_baked_into_the_generated_files", sh) {
        let out = source_and_run(sh, &home, &entry, "echo \"home=[$JLO_HOME]\"\ntype jlo");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            stdout.contains(&format!("home=[{}]", custom.display())),
            "custom JLO_HOME not baked in: {stdout:?}"
        );
        assert!(stdout.contains("function"), "no jlo function: {stdout:?}");
    }
    let printed = String::from_utf8_lossy(&out.stdout);
    assert!(
        printed.contains(&format!("{}/jlo.sh", custom.display())),
        "installer did not print the custom path to source: {printed}"
    );
}

/// The two optional lines are opt-in, so a profile may contain them in any
/// order - or contain the autoload line while the user removes the required
/// one. Sourcing autoload without the wrapper must be inert, not an error.
#[test]
fn autoload_is_inert_without_the_required_entry() {
    let (dir, _) = install(None);
    let home = dir.path().join("home");
    let autoload = home.join(".jlo").join("autoload.sh");
    assert!(
        autoload.is_file(),
        "install.sh did not generate {autoload:?}"
    );

    for sh in INTERPRETERS {
        if skip_missing("autoload_is_inert_without_the_required_entry", sh) {
            continue;
        }
        let out = source_and_run(sh, &home, &autoload, "echo \"status=$?\"");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            stdout.contains("status=0"),
            "{sh}: autoload.sh without jlo.sh did not exit clean: {stdout:?} \
             stderr={:?}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

/// Every generated file is sourced from a profile we do not control, so each
/// must at least parse everywhere - including under a POSIX `sh`.
#[test]
fn generated_entries_parse_under_every_supported_shell() {
    let (dir, _) = install(None);
    let jlo = dir.path().join("home").join(".jlo");
    for name in ["jlo.sh", "autoload.sh", "completions.sh"] {
        let script = jlo.join(name);
        assert!(script.is_file(), "install.sh did not generate {script:?}");
        for sh in INTERPRETERS.iter().chain(["/bin/sh"].iter()) {
            if skip_missing("generated_entries_parse_under_every_supported_shell", sh) {
                continue;
            }
            let out = Command::new(sh).arg("-n").arg(&script).output().unwrap();
            assert!(
                out.status.success(),
                "{name} does not parse under {sh}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }
}

/// The printed instructions are the whole manual step. Keep them to the three
/// source lines - if this count grows, the regression is user-visible.
#[test]
fn the_printed_snippet_is_three_source_lines() {
    let (dir, out) = install(None);
    let home = dir.path().join("home");
    let printed = String::from_utf8_lossy(&out.stdout);
    assert!(home.join(".jlo").is_dir());
    // A default install prints the unexpanded "$HOME/.jlo" so the same profile
    // line works on another machine; only a custom JLO_HOME is spelled out.
    for name in ["jlo.sh", "autoload.sh", "completions.sh"] {
        assert!(
            printed.contains(&format!("\"$HOME/.jlo/{name}\"")),
            "installer never mentions {name} in portable form: {printed}"
        );
    }
    assert!(
        !printed.contains(&format!("{}/jlo.sh", home.join(".jlo").display())),
        "installer hardcoded the expanded home path: {printed}"
    );
    let sourcing = printed
        .lines()
        .filter(|l| l.trim_start().starts_with("[ -s "))
        .count();
    assert_eq!(
        sourcing, 3,
        "expected exactly three source lines:\n{printed}"
    );
}

/// Paths are pasted into the generated files as shell literals, so a character
/// that ends a quoted string early turns every one of them into a syntax error,
/// which the user discovers only when their profile breaks. An apostrophe is the
/// one that actually closes the quote; `$` and a backtick must survive as data
/// rather than being expanded when the file is sourced.
#[test]
fn a_jlo_home_with_shell_metacharacters_still_generates_valid_files() {
    let (dir, out) = install(Some("$HOME/o'brien $x `id`"));
    let home = dir.path().join("home");
    let custom = home.join("o'brien $x `id`");

    for name in ["jlo.sh", "autoload.sh", "completions.sh"] {
        let script = custom.join(name);
        assert!(script.is_file(), "install.sh did not generate {script:?}");
        for sh in INTERPRETERS.iter().chain(["/bin/sh"].iter()) {
            if skip_missing(
                "a_jlo_home_with_shell_metacharacters_still_generates_valid_files",
                sh,
            ) {
                continue;
            }
            let parsed = Command::new(sh).arg("-n").arg(&script).output().unwrap();
            assert!(
                parsed.status.success(),
                "{name} does not parse under {sh}: {}",
                String::from_utf8_lossy(&parsed.stderr)
            );
        }
    }

    let sh = "/bin/bash";
    if !skip_missing(
        "a_jlo_home_with_shell_metacharacters_still_generates_valid_files",
        sh,
    ) {
        let ran = source_and_run(
            sh,
            &home,
            &custom.join("jlo.sh"),
            "/bin/sh -c 'echo \"home=[$JLO_HOME]\"'",
        );
        let stdout = String::from_utf8_lossy(&ran.stdout);
        assert!(
            stdout.contains(&format!("home=[{}]", custom.display())),
            "metacharacters were expanded instead of preserved: {stdout:?}"
        );
    }

    // The printed lines are pasted into a profile, so they need the same
    // treatment. Rather than inspect the quoting, run the required line the way
    // a profile would and check it actually loaded the wrapper.
    let printed = String::from_utf8_lossy(&out.stdout);
    let required = printed
        .lines()
        .find(|l| l.trim_start().starts_with("[ -s ") && l.contains("jlo.sh"))
        .expect("installer printed no line for jlo.sh");
    let sh = "/bin/bash";
    if !skip_missing(
        "a_jlo_home_with_shell_metacharacters_still_generates_valid_files",
        sh,
    ) {
        let ran = Command::new(sh)
            .arg("-c")
            .arg(format!("{required}\ntype jlo"))
            .env("HOME", &home)
            .env_remove("JLO_HOME")
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&ran.stdout);
        assert!(
            stdout.contains("function"),
            "the printed profile line did not load the wrapper: {stdout:?} \
             line={required:?} stderr={:?}",
            String::from_utf8_lossy(&ran.stderr)
        );
    }
}

/// `jlo.sh` is the one file the printed instructions cannot work without. If it
/// could not be written, the installer must fail rather than print a snippet
/// pointing at nothing.
#[test]
fn a_failure_to_write_the_required_entry_fails_the_install() {
    let dir = tempfile::tempdir().unwrap();
    let tarball = release_tarball(dir.path());
    let stubbin = stub_curl(dir.path(), &tarball);
    let home = dir.path().join("home");
    std::fs::create_dir_all(&home).unwrap();

    // A JLO_HOME whose own directory is read-only: bin/ and the binary land
    // there first, then the entry file cannot be created next to them.
    let jlo_home = home.join("ro-jlo");
    std::fs::create_dir_all(jlo_home.join("bin")).unwrap();

    let path = format!(
        "{}:{}",
        stubbin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let script = dir.path().join("run.sh");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\n\
             /bin/sh '{}' > \"$HOME/out.txt\" 2>\"$HOME/err.txt\" &\n\
             installer=$!\n\
             # Seal the directory once the installer has populated bin/.\n\
             while [ ! -x '{}/bin/jlo-bin' ]; do sleep 0.05; done\n\
             chmod a-w '{}'\n\
             wait $installer\n",
            manifest().join("install.sh").display(),
            jlo_home.display(),
            jlo_home.display(),
        ),
    )
    .unwrap();

    let out = Command::new("/bin/sh")
        .arg(&script)
        .env("HOME", &home)
        .env("PATH", path)
        .env("JLO_HOME", &jlo_home)
        .output()
        .unwrap();

    // Restore write permission so the tempdir can be cleaned up.
    let mut perms = std::fs::metadata(&jlo_home).unwrap().permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
    std::fs::set_permissions(&jlo_home, perms).unwrap();

    let stdout = std::fs::read_to_string(home.join("out.txt")).unwrap_or_default();
    if jlo_home.join("jlo.sh").is_file() {
        eprintln!(
            "SKIP a_failure_to_write_the_required_entry_fails_the_install: could not make the write fail here."
        );
        return;
    }
    assert!(
        !out.status.success(),
        "install.sh reported success without writing jlo.sh: {stdout}"
    );
    assert!(
        !stdout.contains("Successfully installed"),
        "install.sh announced success without writing jlo.sh: {stdout}"
    );
}
