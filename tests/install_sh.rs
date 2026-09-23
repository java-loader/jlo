//! Tests for the layout an install produces, and for what it prints.
//!
//! `install.sh` is a bootstrap now: it downloads one file, verifies it,
//! unpacks it and hands over to the binary, which owns everything under
//! `$JLO_HOME` - the three entry stubs, the two wrapper dialects, the
//! completions, the symlink and the receipt. These tests run the real
//! `install.sh` against a temporary `HOME` with `curl` stubbed out, then
//! source the generated files from real shells.
//!
//! The Rust compiler never checks the generated shell code, so sourcing it
//! here is the only thing that does.

// Test code: an `unwrap` failure here is a test failure, which is the point.
#![allow(clippy::unwrap_used)]

mod common;

use common::{INTERPRETERS, chmod, shells, skip_missing};
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// A `tar` that unquotes backslash escapes in its arguments, the way GNU tar
/// does by default and bsdtar does not.
///
/// Without it this whole class of bug is invisible on a macOS dev machine and
/// only surfaces on Linux CI: a `JLO_HOME` holding a literal `\n` is turned
/// into a real newline before tar looks for it, so extraction fails on a path
/// that does exist. `install.sh` must therefore never hand tar a
/// user-supplied path - not as `-C`, and not as the argument of `-f`.
fn stub_gnu_tar(bin: &Path) {
    let tar = bin.join("tar");
    std::fs::write(
        &tar,
        "#!/bin/sh\n\
         # Refuse any argument carrying a backslash escape, which real GNU tar\n\
         # would silently reinterpret instead.\n\
         for a in \"$@\"; do\n\
         \x20 case \"$a\" in *'\\n'*) echo \"tar: unquoted $a\" >&2; exit 2 ;; esac\n\
         done\n\
         exec /usr/bin/tar \"$@\"\n",
    )
    .unwrap();
    chmod(&tar, 0o755);
}

/// Runs a binary this thread has just written, waiting out `ETXTBSY`.
///
/// These tests run as parallel threads of one process. `Command` forks, and a
/// fork taken by *another* thread while this one is between `open` and `close`
/// on the file inherits that writable descriptor - so `execve` of the file
/// fails with "Text file busy" even though this thread closed it long ago.
/// `O_CLOEXEC` does not help: the kernel checks for writers before it clears
/// the child's descriptors.
///
/// Nothing here can close somebody else's inherited copy, so waiting is the
/// only answer. Linux only; on macOS the exec succeeds the first time.
fn run_staged(command: &mut Command) -> Output {
    for _ in 0..100 {
        match command.output() {
            Ok(out) => return out,
            Err(e) if e.kind() == std::io::ErrorKind::ExecutableFileBusy => {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            Err(e) => panic!("could not run {command:?}: {e}"),
        }
    }
    panic!("{command:?} reported ETXTBSY for two seconds");
}

fn manifest() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// A release tarball with the same shape as the real one: the binary under
/// test, and nothing else. The shell code travels *inside* it now, so there is
/// one file to download, verify and swap instead of three that can fall out of
/// step with each other.
fn release_tarball(dir: &Path) -> PathBuf {
    let stage = dir.join("stage");
    std::fs::create_dir_all(&stage).unwrap();
    std::fs::copy(
        assert_cmd::cargo::cargo_bin("jlo-bin"),
        stage.join("jlo-bin"),
    )
    .unwrap();
    let tarball = dir.join("jlo.tar.gz");
    let ok = Command::new("tar")
        .arg("-czf")
        .arg(&tarball)
        .arg("-C")
        .arg(&stage)
        .arg("jlo-bin")
        .status()
        .unwrap()
        .success();
    assert!(ok, "could not build the stub release tarball");
    tarball
}

/// What the stubbed `curl` serves when the installer asks for the `.sha256`
/// published beside the tarball.
#[derive(Clone, Copy)]
enum Checksum {
    /// Computed at run time by the same tool `install.sh` uses, so the happy
    /// path cannot pass by both sides being wrong in the same way.
    Correct,
    Wrong,
    /// A release that predates published checksums: `curl -f` fails.
    Missing,
}

/// A `curl` that serves the stub tarball instead of reaching GitHub. Placed
/// first on `PATH` so `install.sh` itself needs no test-only branch.
fn stub_curl(dir: &Path, tarball: &Path, checksum: Checksum) -> PathBuf {
    let bin = dir.join("stubbin");
    std::fs::create_dir_all(&bin).unwrap();
    let curl = bin.join("curl");
    let serve_sum = match checksum {
        // Either tool, as install.sh itself accepts: a GNU userland without
        // perl has no `shasum`, and an empty sum is a refused install.
        Checksum::Correct => format!(
            "{{ shasum -a 256 '{t}' 2>/dev/null || sha256sum '{t}'; }} | cut -d ' ' -f 1 > \"$out\"",
            t = tarball.display()
        ),
        Checksum::Wrong => format!("echo '{}' > \"$out\"", "0".repeat(64)),
        Checksum::Missing => "exit 22".to_string(),
    };
    std::fs::write(
        &curl,
        format!(
            "#!/bin/sh\n\
             # Ignores every flag but -o, which is all install.sh passes.\n\
             out=\n\
             for a in \"$@\"; do\n\
             \x20 case \"$a\" in *://*) echo \"$a\" >> '{log}' ;; esac\n\
             done\n\
             while [ $# -gt 0 ]; do\n\
             \x20 case \"$1\" in -o) shift; out=\"$1\" ;; esac\n\
             \x20 shift\n\
             done\n\
             [ -n \"$out\" ] || exit 1\n\
             case \"$out\" in\n\
             \x20 *.sha256) {serve_sum} ;;\n\
             \x20 *) cp '{tar}' \"$out\" ;;\n\
             esac\n",
            tar = tarball.display(),
            log = dir.join("curl-urls").display(),
        ),
    )
    .unwrap();
    chmod(&curl, 0o755);
    bin
}

/// Runs the real installer against a throwaway `HOME`, without judging the
/// outcome. `jlo_home` overrides the install directory the way a user
/// exporting `JLO_HOME` would.
///
/// `SHELL` is pinned so the profile the installer names is deterministic - it
/// reads the *login* shell from there, which is the right signal for a file
/// the user will edit, and the wrong one for the dialect dispatch.
fn run_installer(jlo_home: Option<&str>, checksum: Checksum) -> (tempfile::TempDir, Output) {
    let dir = tempfile::tempdir().unwrap();
    let tarball = release_tarball(dir.path());
    let stubbin = stub_curl(dir.path(), &tarball, checksum);
    // Every install test runs against it: install.sh must never hand tar a
    // path it could reinterpret, whatever the test is otherwise about.
    stub_gnu_tar(&stubbin);
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
        .env("SHELL", "/bin/zsh")
        .env_remove("JLO_HOME");
    if let Some(h) = jlo_home {
        cmd.env("JLO_HOME", h.replace("$HOME", &home.display().to_string()));
    }
    let out = cmd.output().unwrap();
    (dir, out)
}

fn install(jlo_home: Option<&str>) -> (tempfile::TempDir, Output) {
    let (dir, out) = run_installer(jlo_home, Checksum::Correct);
    assert!(
        out.status.success(),
        "install.sh failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    (dir, out)
}

/// The installer's human output is on stderr: stdout is the environment
/// channel (ADR-0001), and from 0.4.0 `selfupdate` prints a `. jlo.sh` line
/// there for the shell wrapper to eval.
fn printed(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
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

    for sh in shells(
        "sourcing_jlo_sh_alone_defines_the_wrapper_and_exports_jlo_home",
        INTERPRETERS,
    ) {
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
    let printed = printed(&out);
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
    let printed = printed(&out);
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

    for sh in shells("autoload_is_inert_without_the_required_entry", INTERPRETERS) {
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
        for sh in shells(
            "generated_entries_parse_under_every_supported_shell",
            INTERPRETERS.iter().chain(["/bin/sh"].iter()),
        ) {
            let out = Command::new(sh).arg("-n").arg(&script).output().unwrap();
            assert!(
                out.status.success(),
                "{name} does not parse under {sh}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }
}

/// The printed instructions are the whole manual step: one heredoc the user
/// pastes, and one line that loads jlo into the shell they are sitting in. If
/// that shape grows, the regression is user-visible.
#[test]
fn the_printed_snippet_is_one_heredoc_and_one_source_line() {
    let (dir, out) = install(None);
    let home = dir.path().join("home");
    let printed = printed(&out);
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
    let block = heredoc_block(&printed).expect("installer printed no heredoc");
    // The delimiter must be quoted, or `$HOME` is expanded into the profile
    // and the portable form above is defeated at the moment it is written.
    assert!(
        block[0].contains("<<'EOF'"),
        "the heredoc delimiter is not quoted: {:?}",
        block[0]
    );
    assert_eq!(
        block.last().map(String::as_str),
        Some("EOF"),
        "an ordinary install should still use the plain EOF terminator: {block:#?}"
    );
    // A blank line first: `>>` appends at the exact end of the file, and a
    // profile whose last line has no newline would otherwise get jlo's first
    // line welded onto it.
    assert_eq!(
        block[1], "",
        "the heredoc does not open with a blank line: {block:#?}"
    );
    let body: Vec<&String> = block[2..block.len() - 1].iter().collect();
    assert_eq!(body.len(), 3, "expected three profile lines: {block:#?}");
    assert_eq!(
        printed
            .lines()
            .filter(|l| l.trim_start().starts_with("cat >>"))
            .count(),
        1,
        "expected exactly one heredoc:\n{printed}"
    );
    // The second half of the manual step, and the only other command: the
    // installer runs in a subshell and cannot load jlo into the parent itself.
    assert_eq!(
        printed
            .lines()
            .map(visible)
            .filter(|l| l.starts_with(". \"") || l.starts_with(". '"))
            .count(),
        1,
        "expected exactly one line that loads jlo into this shell:\n{printed}"
    );
}

/// A directory name may contain a newline, so a `JLO_HOME` can put a bare
/// `EOF` on a line of its own *inside* the block. Quoting does not help
/// there, because in a heredoc body the quotes are data, so the heredoc would
/// end in the middle of a path, append half a statement to the profile and
/// hand the rest to the shell. The terminator is picked against the body for
/// exactly this.
#[test]
fn a_jlo_home_that_spells_the_terminator_does_not_end_the_heredoc() {
    let (dir, _) = install(None);
    let home = dir.path().join("home");
    let hostile = home.join("jlo\nEOF\nx");
    std::fs::create_dir_all(hostile.join("bin")).unwrap();
    std::fs::copy(
        home.join(".jlo").join("bin").join("jlo-bin"),
        hostile.join("bin").join("jlo-bin"),
    )
    .unwrap();

    let out = run_staged(
        Command::new(hostile.join("bin").join("jlo-bin"))
            .arg("__install")
            .env("HOME", &home)
            .env("JLO_HOME", &hostile)
            .env("SHELL", "/bin/zsh"),
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let printed = printed(&out);
    let opener = printed
        .lines()
        .map(visible)
        .find(|l| l.starts_with("cat >>"))
        .expect("installer printed no heredoc");
    assert!(
        !opener.contains("<<'EOF'"),
        "the terminator collides with a line of the body: {opener:?}"
    );

    // The real check: the block the user would paste has to parse, and it has
    // to write the profile lines rather than spill into the shell.
    let block = heredoc_block(&printed).expect("installer printed no heredoc");
    for (i, sh) in shells(
        "a_jlo_home_that_spells_the_terminator_does_not_end_the_heredoc",
        INTERPRETERS,
    )
    .enumerate()
    {
        let profile = home.join(format!(".eof-profile{i}"));
        let script =
            block
                .join("\n")
                .replacen(">> ~/.zshrc", &format!(">> {}", squote(&profile)), 1);
        let ran = Command::new(sh)
            .arg("-c")
            .arg(&script)
            .env("HOME", &home)
            .output()
            .unwrap();
        assert!(
            ran.status.success(),
            "{sh}: the printed block did not run: {} script={script:?}",
            String::from_utf8_lossy(&ran.stderr)
        );
        let written = std::fs::read_to_string(&profile).unwrap();
        assert!(
            written.contains("/jlo.sh"),
            "{sh}: the heredoc ended early - the profile holds {written:?}"
        );
    }
}

/// The heredoc as the user would select it: from `cat >>` to the terminator,
/// with the escape bytes stripped so the lines are the ones that reach the
/// profile.
fn heredoc_block(printed: &str) -> Option<Vec<String>> {
    let lines: Vec<String> = printed.lines().map(visible).collect();
    let start = lines.iter().position(|l| l.starts_with("cat >>"))?;
    // Read the delimiter off the opener rather than assuming `EOF`: it is
    // chosen against the body, so a hostile JLO_HOME moves it. Assuming it
    // here would cut the block at the very line the choice exists to survive.
    let quoted = lines[start].rsplit_once("<<'")?.1;
    let delimiter = quoted.strip_suffix('\'')?.to_string();
    let end = start + 1 + lines[start + 1..].iter().position(|l| *l == delimiter)?;
    Some(lines[start..=end].to_vec())
}

/// The half of the output that makes the difference between "installed" and
/// "usable": one line makes jlo permanent, the next makes it effective in the
/// shell the user is already sitting in. The installer cannot do that second
/// part itself - it runs in a subshell under `curl | bash`.
#[test]
fn the_installer_prints_a_line_that_activates_the_current_shell() {
    let (_dir, out) = install(None);
    let printed = printed(&out);
    assert!(
        printed.contains("\n. \"$HOME/.jlo/jlo.sh\""),
        "installer printed no line to source jlo right now: {printed}"
    );
    assert!(
        !printed.to_lowercase().contains("restart your terminal"),
        "installer still tells the user to restart their terminal: {printed}"
    );
}

/// Every line the installer offers for copying starts at column 0.
///
/// Indentation is invisible in review and fatal in use: double-click and
/// shift-select take the leading spaces with them, so an indented command is
/// one that gets pasted broken. The block was indented four spaces for four
/// releases and nobody noticed.
///
/// Colour is forced on for half of this, because that is the shape the check
/// is easiest to get wrong in: with escape bytes in front of it, a command
/// line no longer *starts* with the command, and a naive predicate stops
/// matching exactly the lines it is meant to police. The legacy branch is
/// exercised too - it prints a second block of commands that a fresh install
/// never reaches.
#[test]
fn no_copyable_line_is_indented() {
    let (dir, out) = install(None);
    let home = dir.path().join("home");
    assert_no_indented_commands(&printed(&out), 4, "fresh install");

    let coloured = Command::new(home.join(".jlo").join("bin").join("jlo-bin"))
        .arg("__install")
        .env("HOME", &home)
        .env("JLO_HOME", home.join(".jlo"))
        .env("SHELL", "/bin/zsh")
        .env("CLICOLOR_FORCE", "1")
        .output()
        .unwrap();
    let painted = printed(&coloured);
    assert!(
        painted.contains('\u{1b}'),
        "CLICOLOR_FORCE produced no escapes, so this run proves nothing: {painted}"
    );
    assert_no_indented_commands(&painted, 4, "coloured re-install");

    // The v0.2.0 block, which sends the installer down the legacy notice
    // instead of the activation block.
    std::fs::write(
        home.join(".zshrc"),
        "export JLO_HOME=\"$HOME/.jlo\"\n\
         [[ -s \"$JLO_HOME/bin/jlo-init.sh\" ]] && source \"$JLO_HOME/bin/jlo-init.sh\"\n",
    )
    .unwrap();
    assert_no_indented_commands(&printed(&reinstall_over(&home)), 1, "legacy notice");
}

/// Strip SGR escapes so the check sees the line the *user* sees. `trim_start`
/// is not enough: `\x1b[38;5;12mprintf ...` is an indented command as far as a
/// terminal is concerned only if the spaces come first, and it is not a
/// command as far as `starts_with` is concerned at all.
fn visible(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        if chars.next() != Some('[') {
            continue;
        }
        for c in chars.by_ref() {
            if c.is_ascii_alphabetic() {
                break;
            }
        }
    }
    out
}

/// `expected` guards against the vacuous pass: a predicate that matches
/// nothing satisfies "no indented commands" perfectly.
fn assert_no_indented_commands(printed: &str, expected: usize, what: &str) {
    let runnable = |l: &str| {
        let t = l.trim_start();
        t.starts_with("cat >> ")
            || t.starts_with(". \"")
            || t.starts_with(". '")
            || t.starts_with("export ")
            || t.starts_with("[ -s ")
            || t.starts_with("ln -s ")
            || t.trim_end() == "EOF"
    };
    let commands: Vec<String> = printed
        .lines()
        .map(visible)
        .filter(|l| runnable(l))
        .collect();
    assert!(
        commands.len() >= expected,
        "{what}: found {} command lines, expected at least {expected}. \
         The predicate has drifted from the output:\n{printed}",
        commands.len()
    );
    let indented: Vec<&String> = commands
        .iter()
        .filter(|l| l.starts_with(char::is_whitespace))
        .collect();
    assert!(
        indented.is_empty(),
        "{what}: these copyable lines are indented: {indented:#?}\n\nfull output:\n{printed}"
    );
}

/// The profile file is chosen from `$SHELL` - the login shell, which is what
/// the user will actually edit - and stays *visible* in the printed command,
/// so a wrong guess is a one-word fix rather than a line written silently into
/// the wrong file.
#[test]
fn the_profile_path_follows_the_login_shell() {
    let (dir, out) = run_installer(None, Checksum::Correct);
    assert!(out.status.success());
    drop(dir);
    assert!(
        printed(&out).contains(">> ~/.zshrc"),
        "installer did not name the zsh profile: {}",
        printed(&out)
    );
}

/// The three re-install cases, in the order a user meets them.
///
/// The receipt alone cannot decide: someone who abandoned the first install
/// halfway has a receipt and no profile line, and would otherwise be told
/// nothing at all on the upgrade that was supposed to fix it.
#[test]
fn a_reinstall_says_nothing_when_the_profile_already_sources_jlo() {
    let (dir, first) = install(None);
    let home = dir.path().join("home");
    assert!(
        printed(&first).contains("To activate"),
        "the first install withheld the instructions"
    );

    // The user runs the line the installer printed.
    std::fs::write(
        home.join(".zshrc"),
        "[ -s \"$HOME/.jlo/jlo.sh\" ] && . \"$HOME/.jlo/jlo.sh\"\n",
    )
    .unwrap();

    let second = reinstall_over(&home);
    let printed = printed(&second);
    assert!(
        printed.contains("installed to ~/.jlo"),
        "the upgrade said nothing at all: {printed}"
    );
    assert!(
        !printed.contains("To activate"),
        "the upgrade repeated first-install instructions: {printed}"
    );
}

#[test]
fn a_reinstall_repeats_the_instructions_when_the_profile_line_is_missing() {
    let (dir, _) = install(None);
    let home = dir.path().join("home");
    // A receipt is there, but the user never added the line.
    let second = reinstall_over(&home);
    let printed = printed(&second);
    assert!(
        printed.contains("To activate"),
        "a half-finished install got no instructions on the retry: {printed}"
    );
    assert!(
        printed.contains("never added"),
        "the installer did not point out the missing profile line: {printed}"
    );
}

/// Runs the install verb again over an existing `$JLO_HOME`, the way a second
/// `install.sh` run would once the tarball is unpacked.
fn reinstall_over(home: &Path) -> Output {
    Command::new(home.join(".jlo").join("bin").join("jlo-bin"))
        .arg("__install")
        .env("HOME", home)
        .env("JLO_HOME", home.join(".jlo"))
        .env("SHELL", "/bin/zsh")
        .output()
        .unwrap()
}

/// Paths are pasted into the generated files as shell literals, so a character
/// that ends a quoted string early turns every one of them into a syntax error,
/// which the user discovers only when their profile breaks. An apostrophe is the
/// one that actually closes the quote; `$` and a backtick must survive as data
/// rather than being expanded when the file is sourced.
///
/// The backslash is in here for the *printed* half rather than the generated
/// one: `echo` expands `\n` in zsh, in dash and in any bash built with
/// `xpg_echo`, so the append command the user is told to run would have written
/// a line break into their profile and split the line in two.
#[test]
fn a_jlo_home_with_shell_metacharacters_still_generates_valid_files() {
    let (dir, out) = install(Some("$HOME/o'brien $x `id` a\\nb"));
    let home = dir.path().join("home");
    let custom = home.join("o'brien $x `id` a\\nb");

    for name in [
        "jlo.sh",
        "autoload.sh",
        "completions.sh",
        "bin/jlo-init.sh",
        "bin/jlo-autoload.sh",
    ] {
        let script = custom.join(name);
        assert!(script.is_file(), "install.sh did not generate {script:?}");
        for sh in shells(
            "a_jlo_home_with_shell_metacharacters_still_generates_valid_files",
            INTERPRETERS.iter().chain(["/bin/sh"].iter()),
        ) {
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
            // printf, not echo: the value under test contains a backslash,
            // and echo would expand it here in the test harness itself.
            "/bin/sh -c 'printf \"home=[%s]\\n\" \"$JLO_HOME\"'",
        );
        let stdout = String::from_utf8_lossy(&ran.stdout);
        assert!(
            stdout.contains(&format!("home=[{}]", custom.display())),
            "metacharacters were expanded instead of preserved: {stdout:?}"
        );
    }

    // What the installer prints is a command the *user* runs, so the path is
    // quoted twice over: once inside the profile line, and once by the shell
    // reading the heredoc. Rather than inspect either, run the block the
    // installer printed and then source what it produced.
    let printed = printed(&out);
    let block = heredoc_block(&printed).expect("installer printed no heredoc");
    let append = block.join("\n");
    // Under both shells. A quoted heredoc is literal everywhere, which is the
    // property being asserted: the backslash in this JLO_HOME must arrive in
    // the profile unchanged.
    for (i, sh) in shells(
        "a_jlo_home_with_shell_metacharacters_still_generates_valid_files",
        INTERPRETERS,
    )
    .enumerate()
    {
        // A profile of its own per shell, so the second run appends to an
        // empty file rather than to the first run's line.
        let profile = home.join(format!(".profile{i}"));
        let appended = append.replacen(">> ~/.zshrc", &format!(">> {}", squote(&profile)), 1);
        assert_ne!(appended, append, "could not redirect the printed command");
        // The heredoc body ends up in the profile verbatim, blank line
        // included; only the entry line has to load the wrapper.
        let ran = Command::new(sh)
            .arg("-c")
            .arg(format!("{appended}\n. {}\ntype jlo", squote(&profile)))
            .env("HOME", &home)
            .env_remove("JLO_HOME")
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&ran.stdout);
        assert!(
            stdout.contains("function"),
            "{sh}: the printed command did not produce a profile line that loads \
             the wrapper: {stdout:?} line={appended:?} stderr={:?}",
            String::from_utf8_lossy(&ran.stderr)
        );
        let written = std::fs::read_to_string(&profile).unwrap();
        assert_eq!(
            written.lines().filter(|l| !l.trim().is_empty()).count(),
            3,
            "{sh}: the heredoc wrote {written:?} into the profile"
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
    let stubbin = stub_curl(dir.path(), &tarball, Checksum::Correct);
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
             # Seal the directory once the installer has populated bin/ -\n\
             # or stop waiting if it died first, which the asserts report,\n\
             # or if it is still running after two minutes: a hang must\n\
             # fail the test, not stall the suite.\n\
             waited=0\n\
             while [ ! -x '{}/bin/jlo-bin' ] && kill -0 $installer 2>/dev/null; do\n\
             \x20 waited=$((waited + 1))\n\
             \x20 if [ $waited -ge 2400 ]; then\n\
             \x20   echo 'timed out: install.sh never populated bin/' >&2\n\
             \x20   kill $installer\n\
             \x20   exit 124\n\
             \x20 fi\n\
             \x20 sleep 0.05\n\
             done\n\
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
    chmod(&jlo_home, 0o755);

    let said = std::fs::read_to_string(home.join("err.txt")).unwrap_or_default();
    assert_ne!(
        out.status.code(),
        Some(124),
        "{}{said}",
        String::from_utf8_lossy(&out.stderr)
    );
    if jlo_home.join("jlo.sh").is_file() {
        eprintln!(
            "SKIP a_failure_to_write_the_required_entry_fails_the_install: could not make the write fail here."
        );
        return;
    }
    assert!(
        !out.status.success(),
        "install.sh reported success without writing jlo.sh: {said}"
    );
    assert!(
        !said.contains("To activate"),
        "install.sh printed activation instructions for a layout it never wrote: {said}"
    );
    assert!(
        !jlo_home.join("install-receipt.json").is_file(),
        "the receipt was committed although the install never finished"
    );
}

// ---------------------------------------------------------------------------
// Completions: zsh autoloads, bash cannot
// ---------------------------------------------------------------------------

/// Sources the generated `completions.sh` in a zsh that has run `pre` first,
/// then reports what the shell ended up with: which function completes `jlo`,
/// and whether that function has been read yet. `-f` on purpose - the
/// developer's own dotfiles must not decide whether this passes.
fn zsh_completion_state(home: &Path, pre: &str) -> Output {
    let entry = squote(&home.join(".jlo").join("completions.sh"));
    let script = format!(
        r#"{pre}
. {entry}
print -r -- "comps=[${{_comps[jlo]-}}]"
print -r -- "whence=[$(whence -v _jlo 2>&1)]"
"#
    );
    Command::new("zsh")
        .args(["-f", "-c", &script])
        .env("HOME", home)
        .env_remove("JLO_HOME")
        .output()
        .unwrap()
}

/// The point of generating `_jlo` into a directory of its own. Sourcing the
/// 11 KB completion into every interactive zsh undoes the reason completions
/// are generated at install time at all - the subprocess was avoided, the parse
/// was not. `$fpath` plus `autoload` defers that parse to the first Tab press.
#[test]
fn zsh_completions_are_autoloaded_from_fpath_not_sourced() {
    let (dir, _) = install(None);
    let home = dir.path().join("home");
    if skip_missing(
        "zsh_completions_are_autoloaded_from_fpath_not_sourced",
        "zsh",
    ) {
        return;
    }
    let out = zsh_completion_state(&home, "");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("comps=[_jlo]"),
        "zsh does not know how to complete 'jlo': {stdout:?} stderr={:?}",
        String::from_utf8_lossy(&out.stderr)
    );
    // Still a stub, not a body: nothing has read the file yet.
    assert!(
        stdout.contains("whence=[_jlo is an autoload shell function"),
        "_jlo was loaded eagerly instead of autoloaded: {stdout:?}"
    );
}

/// The ordering trap. `compinit` scans `$fpath` once, so a user whose framework
/// (oh-my-zsh and friends) ran it before the jlo line would otherwise get a
/// completion directory nobody ever reads. The fallback registers after the
/// fact - and has to `autoload` first, because `compdef _jlo jlo` alone records
/// the mapping without making `_jlo` loadable and completion comes up empty.
#[test]
fn zsh_completions_survive_a_framework_that_ran_compinit_first() {
    let (dir, _) = install(None);
    let home = dir.path().join("home");
    if skip_missing(
        "zsh_completions_survive_a_framework_that_ran_compinit_first",
        "zsh",
    ) {
        return;
    }
    let out = zsh_completion_state(
        &home,
        "autoload -Uz compinit && compinit -i -d \"${TMPDIR:-/tmp}/jlo-zcompdump.$$\"",
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("comps=[_jlo]"),
        "a pre-existing compinit left 'jlo' with no completion: {stdout:?} \
         stderr={:?}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        stdout.contains("whence=[_jlo is an autoload shell function"),
        "_jlo is registered but not loadable - completion would be empty: {stdout:?}"
    );
}

/// bash has no autoload: `complete -F` must name a function that exists at
/// registration time, so its completion stays eager. Asserted rather than
/// assumed, so the zsh change above cannot quietly take bash with it.
#[test]
fn bash_completions_stay_eager() {
    let (dir, _) = install(None);
    let home = dir.path().join("home");
    let sh = "/bin/bash";
    if skip_missing("bash_completions_stay_eager", sh) {
        return;
    }
    let out = source_and_run(
        sh,
        &home,
        &home.join(".jlo").join("completions.sh"),
        "complete -p jlo",
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("-F _jlo jlo"),
        "bash has no completion for 'jlo': {stdout:?} stderr={:?}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// `install.sh` is fetched over the network and piped into a shell, and a
/// connection dropped mid-transfer leaves a *truncated but non-empty* body that
/// `curl -f` cannot catch. Every statement therefore lives inside `main`, which
/// only the last line calls: a half-downloaded copy either fails to parse or
/// parses and does nothing, but never runs half an install.
#[test]
fn a_truncated_installer_does_nothing() {
    let full = std::fs::read_to_string(manifest().join("install.sh")).unwrap();
    let lines: Vec<&str> = full.lines().collect();
    let main_start = lines
        .iter()
        .position(|l| *l == "main() {")
        .expect("install.sh no longer wraps its body in main()");
    // The line that closes `main`. A copy cut before it leaves an unterminated
    // function, which is a parse error; one cut after it is a complete
    // function nobody calls.
    let main_end = main_start
        + lines[main_start..]
            .iter()
            .position(|l| *l == "}")
            .expect("install.sh's main() is never closed");

    for fraction in [3, 10, 25, 50, 75, 90, 99] {
        let dir = tempfile::tempdir().unwrap();
        let tarball = release_tarball(dir.path());
        let stubbin = stub_curl(dir.path(), &tarball, Checksum::Correct);
        let home = dir.path().join("home");
        std::fs::create_dir_all(&home).unwrap();

        let cut = lines.len() * fraction / 100;
        let script = dir.path().join("truncated.sh");
        std::fs::write(&script, lines[..cut].join("\n")).unwrap();

        let out = Command::new("/bin/sh")
            .arg(&script)
            .env("HOME", &home)
            .env(
                "PATH",
                format!(
                    "{}:{}",
                    stubbin.display(),
                    std::env::var("PATH").unwrap_or_default()
                ),
            )
            .env_remove("JLO_HOME")
            .output()
            .unwrap();

        let said = printed(&out);
        assert!(
            !home.join(".jlo").exists(),
            "a {fraction}% copy of install.sh installed something anyway"
        );
        assert!(
            !said.contains("installed to"),
            "a {fraction}% copy of install.sh announced an install: {said}"
        );
        // Cut inside main, the function never closes: that is a parse error,
        // and the caller sees it. Cut above main there is nothing but comments
        // and `set -eu`; cut past its closing brace there is a function and no
        // call. Both of those legitimately succeed at doing nothing.
        if cut > main_start && cut <= main_end {
            assert!(
                !out.status.success(),
                "a {fraction}% copy of install.sh reported success: {said}"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// The layout the binary owns
// ---------------------------------------------------------------------------

/// The tarball carries one file. Everything else under `$JLO_HOME` is written
/// by the binary, from sources compiled into it - which is what makes the
/// files on disk match the binary that wrote them by construction.
#[test]
fn the_binary_writes_the_whole_layout() {
    let (dir, _) = install(None);
    let jlo = dir.path().join("home").join(".jlo");
    for rel in [
        "jlo.sh",
        "autoload.sh",
        "completions.sh",
        "install-receipt.json",
        "bin/jlo-bin",
        "bin/jlo-init.zsh",
        "bin/jlo-init.bash",
        "bin/jlo-autoload.zsh",
        "bin/jlo-autoload.bash",
        "completions/jlo.bash",
        "completions/_jlo",
    ] {
        assert!(jlo.join(rel).is_file(), "install did not write {rel}");
    }
    // These two used to travel in the tarball as dual-parse wrappers, and are
    // generated shims now: the paths every released profile block sources,
    // redirecting to the dialect files beside them. Deleting them was the bug
    // - leaving them shipped was the older one.
    for rel in ["bin/jlo-init.sh", "bin/jlo-autoload.sh"] {
        let body = std::fs::read_to_string(jlo.join(rel)).unwrap();
        assert!(
            body.contains("Compatibility shim"),
            "{rel} is not the generated shim: {body}"
        );
        assert!(
            !body.contains("jlo() {"),
            "the shipped dual-parse wrapper is still being installed as {rel}"
        );
    }
}

/// The symlink target name is load-bearing: `jlo` on PATH is only ever
/// refreshed when it already points at exactly this path, so renaming the
/// binary would leave every existing user's symlink dangling.
#[test]
fn the_symlink_points_at_jlo_bin_in_the_install_directory() {
    let (dir, _) = install(None);
    let home = dir.path().join("home");
    let link = home.join(".local").join("bin").join("jlo");
    assert!(link.symlink_metadata().is_ok(), "no symlink at {link:?}");
    assert_eq!(
        std::fs::read_link(&link).unwrap(),
        home.join(".jlo").join("bin").join("jlo-bin")
    );
}

/// An unrelated `jlo` on PATH is somebody else's file. The installer warns and
/// leaves it alone rather than replacing it.
#[test]
fn an_unmanaged_jlo_on_path_is_left_untouched() {
    let dir = tempfile::tempdir().unwrap();
    let tarball = release_tarball(dir.path());
    let stubbin = stub_curl(dir.path(), &tarball, Checksum::Correct);
    let home = dir.path().join("home");
    let local_bin = home.join(".local").join("bin");
    std::fs::create_dir_all(&local_bin).unwrap();
    std::fs::write(local_bin.join("jlo"), "#!/bin/sh\necho not ours\n").unwrap();

    let out = Command::new("/bin/sh")
        .arg(manifest().join("install.sh"))
        .env("HOME", &home)
        .env("SHELL", "/bin/zsh")
        .env(
            "PATH",
            format!(
                "{}:{}",
                stubbin.display(),
                std::env::var("PATH").unwrap_or_default()
            ),
        )
        .env_remove("JLO_HOME")
        .output()
        .unwrap();

    assert!(out.status.success(), "{}", printed(&out));
    assert_eq!(
        std::fs::read_to_string(local_bin.join("jlo")).unwrap(),
        "#!/bin/sh\necho not ours\n",
        "the installer overwrote a file it does not own"
    );
    assert!(
        printed(&out).contains("not managed by J'Lo"),
        "the installer replaced nothing but also said nothing: {}",
        printed(&out)
    );
}

// ---------------------------------------------------------------------------
// The dialect dispatch in the generated jlo.sh
// ---------------------------------------------------------------------------

/// Sources `jlo.sh` under `sh` and reports which wrapper file it loaded, or
/// `NONE` when it loaded none.
fn dispatched_dialect(sh: &str, home: &Path, prologue: &str) -> String {
    let entry = squote(&home.join(".jlo").join("jlo.sh"));
    let out = Command::new(sh)
        .arg("-c")
        .arg(format!(
            "{prologue}\n. {entry}\n\
             if typeset -f jlo >/dev/null 2>&1; then\n\
             \x20 case \"$JLO_PROBE\" in *) :;; esac\n\
             fi\n\
             echo \"dialect=[${{_JLO_TEST_DIALECT-NONE}}]\"\n"
        ))
        .env("HOME", home)
        .env_remove("JLO_HOME")
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Only the shell knows which shell it is, so `jlo.sh` decides at source time.
/// The builtin test is the whole point: `ZSH_VERSION` is not exported *by
/// default*, but nothing stops a user from exporting it, and a bash child then
/// inherits it and sets `BASH_VERSION` itself. A plain concatenation of the two
/// names a file that does not exist, and jlo silently fails to initialise.
#[test]
fn the_dialect_dispatch_picks_the_running_shell_and_resists_a_spoof() {
    let (dir, _) = install(None);
    let home = dir.path().join("home");
    let bin = home.join(".jlo").join("bin");

    // Mark each wrapper so the sourcing shell can say which one it read.
    for dialect in ["zsh", "bash"] {
        let file = bin.join(format!("jlo-init.{dialect}"));
        let body = std::fs::read_to_string(&file).unwrap();
        std::fs::write(&file, format!("{body}_JLO_TEST_DIALECT={dialect}\n")).unwrap();
    }

    for (sh, expected, prologue) in [
        ("zsh", "zsh", ""),
        ("/bin/bash", "bash", ""),
        // The measured attack on the naive form.
        ("/bin/bash", "bash", "export ZSH_VERSION=5.9"),
    ] {
        if skip_missing("the_dialect_dispatch_picks_the_running_shell", sh) {
            continue;
        }
        let stdout = dispatched_dialect(sh, &home, prologue);
        assert!(
            stdout.contains(&format!("dialect=[{expected}]")),
            "{sh} (prologue {prologue:?}) loaded the wrong wrapper: {stdout:?}"
        );
    }
}

/// A shell that is neither must load nothing at all - quietly, and without
/// aborting the profile it is being sourced from.
#[test]
fn the_dialect_dispatch_is_a_clean_no_op_under_a_non_bash_sh() {
    let (dir, _) = install(None);
    let home = dir.path().join("home");
    let sh = "/bin/dash";
    if Command::new(sh).arg("-c").arg("exit 0").output().is_err() {
        eprintln!("SKIP the_dialect_dispatch_is_a_clean_no_op_under_a_non_bash_sh: no {sh}.");
        return;
    }
    let out = Command::new(sh)
        .arg("-c")
        .arg(format!(
            "set -eu\n. {}\necho \"rc=$?\"\n",
            squote(&home.join(".jlo").join("jlo.sh"))
        ))
        .env("HOME", &home)
        .env_remove("JLO_HOME")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{sh} aborted on jlo.sh: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "rc=0");
}

// ---------------------------------------------------------------------------
// Checksum verification
// ---------------------------------------------------------------------------

/// A checksum that is present and wrong is fatal, and nothing is installed.
#[test]
fn a_checksum_mismatch_aborts_the_install() {
    let (dir, out) = run_installer(None, Checksum::Wrong);
    let home = dir.path().join("home");
    assert!(
        !out.status.success(),
        "a corrupt download installed anyway: {}",
        printed(&out)
    );
    assert!(
        printed(&out).contains("Checksum mismatch"),
        "the installer did not say why it stopped: {}",
        printed(&out)
    );
    assert!(
        !home.join(".jlo").join("jlo.sh").exists(),
        "the installer wrote the layout despite a bad checksum"
    );
}

/// A *missing* checksum is a release that predates them, not an attack: it
/// shares an origin with the tarball either way, so failing closed here would
/// strand users on a download TLS already protected.
#[test]
fn a_missing_checksum_warns_but_installs() {
    let (dir, out) = run_installer(None, Checksum::Missing);
    assert!(
        out.status.success(),
        "a release without a published checksum could not be installed: {}",
        printed(&out)
    );
    assert!(
        printed(&out).contains("no published checksum"),
        "the installer skipped verification silently: {}",
        printed(&out)
    );
    assert!(
        dir.path()
            .join("home")
            .join(".jlo")
            .join("jlo.sh")
            .is_file()
    );
}

// ---------------------------------------------------------------------------
// The install verb is hidden
// ---------------------------------------------------------------------------

/// Whether `haystack` offers `token` as a word of its own rather than as the
/// tail of a longer identifier.
///
/// A plain `contains` was enough until `jlo install` existed. `clap_complete`
/// names its generated dispatch states after the subcommand path, so the
/// visible verb produces `jlo__subcmd__install` - which ends in the hidden
/// verb's spelling without offering it. The boundary check is what separates
/// the two.
fn offers_word(haystack: &str, token: &str) -> bool {
    let is_word = |c: char| c.is_alphanumeric() || c == '_';
    haystack.match_indices(token).any(|(at, _)| {
        let before = haystack[..at].chars().next_back();
        let after = haystack[at + token.len()..].chars().next();
        !before.is_some_and(is_word) && !after.is_some_and(is_word)
    })
}

/// `hide = true` would only drop it from `--help`: `clap_complete` still emits
/// hidden subcommands into generated completion scripts, and clap's "did you
/// mean" engine still offers them for typos. Intercepting the raw token before
/// `Cli::parse()` keeps it out of all three.
#[test]
fn the_install_verb_appears_in_no_generated_surface() {
    let (dir, _) = install(None);
    let jlo = dir.path().join("home").join(".jlo");
    let binary = jlo.join("bin").join("jlo-bin");

    for name in ["completions/jlo.bash", "completions/_jlo"] {
        let body = std::fs::read_to_string(jlo.join(name)).unwrap();
        assert!(
            !offers_word(&body, "__install"),
            "{name} offers the hidden install verb"
        );
    }

    let help = Command::new(&binary).arg("--help").output().unwrap();
    assert!(
        !offers_word(&String::from_utf8_lossy(&help.stdout), "__install"),
        "--help lists the hidden install verb"
    );

    let typo = Command::new(&binary).arg("__instal").output().unwrap();
    let said = String::from_utf8_lossy(&typo.stderr);
    assert!(
        !offers_word(&said, "__install"),
        "clap suggested the hidden install verb for a typo: {said}"
    );
}

// ---------------------------------------------------------------------------
// The receipt, and what a mismatched one heals
// ---------------------------------------------------------------------------

#[test]
fn the_receipt_records_the_version_and_the_paths() {
    let (dir, _) = install(None);
    let home = dir.path().join("home");
    let receipt = std::fs::read_to_string(home.join(".jlo").join("install-receipt.json")).unwrap();
    for expected in [
        "\"version\"",
        "\"method\": \"installer\"",
        &format!(
            "\"binary\": \"{}\"",
            home.join(".jlo/bin/jlo-bin").display()
        ),
        &format!("\"symlink\": \"{}\"", home.join(".local/bin/jlo").display()),
    ] {
        assert!(
            receipt.contains(expected),
            "receipt is missing {expected}:\n{receipt}"
        );
    }
}

/// A receipt whose version does not match the binary is the known-incomplete
/// state - the binary landed, the generated files did not. There is
/// deliberately no `--repair` verb and no version guard in the wrapper: any
/// invocation regenerates the files, with no network and nothing for the user
/// to learn.
#[test]
fn a_stale_receipt_makes_the_next_invocation_rewrite_the_files() {
    let (dir, _) = install(None);
    let home = dir.path().join("home");
    let jlo = home.join(".jlo");
    let receipt = jlo.join("install-receipt.json");

    // An interrupted publish: the receipt is from an older version and the
    // generated files never landed.
    let body = std::fs::read_to_string(&receipt).unwrap();
    let stale = body.replacen(
        &format!("\"version\": \"{}\"", env!("CARGO_PKG_VERSION")),
        "\"version\": \"0.0.1-stale\"",
        1,
    );
    assert_ne!(stale, body, "could not make the receipt stale");
    std::fs::write(&receipt, stale).unwrap();
    std::fs::remove_file(jlo.join("jlo.sh")).unwrap();
    std::fs::remove_file(jlo.join("bin").join("jlo-init.zsh")).unwrap();

    // Any command at all, and one that needs no network.
    let out = Command::new(jlo.join("bin").join("jlo-bin"))
        .arg("--version")
        .env("HOME", &home)
        .env("JLO_HOME", &jlo)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    assert!(jlo.join("jlo.sh").is_file(), "jlo.sh was not regenerated");
    assert!(
        jlo.join("bin").join("jlo-init.zsh").is_file(),
        "the zsh wrapper was not regenerated"
    );
    assert!(
        std::fs::read_to_string(&receipt)
            .unwrap()
            .contains(&format!("\"version\": \"{}\"", env!("CARGO_PKG_VERSION"))),
        "the receipt still disagrees with the binary"
    );
}

/// The guard that keeps the self-heal from reaching into an install this
/// binary is not. Without it every `cargo test` run - and every developer
/// build executed from `target/` - would rewrite the real `~/.jlo`.
#[test]
fn a_receipt_naming_a_different_binary_is_left_alone() {
    let (dir, _) = install(None);
    let home = dir.path().join("home");
    let jlo = home.join(".jlo");
    let receipt = jlo.join("install-receipt.json");

    let body = std::fs::read_to_string(&receipt).unwrap();
    let foreign = body
        .replacen(
            &format!("\"version\": \"{}\"", env!("CARGO_PKG_VERSION")),
            "\"version\": \"0.0.1-stale\"",
            1,
        )
        .replacen(
            &format!("\"binary\": \"{}\"", jlo.join("bin/jlo-bin").display()),
            "\"binary\": \"/somewhere/else/jlo-bin\"",
            1,
        );
    std::fs::write(&receipt, &foreign).unwrap();
    std::fs::remove_file(jlo.join("jlo.sh")).unwrap();

    let out = Command::new(jlo.join("bin").join("jlo-bin"))
        .arg("--version")
        .env("HOME", &home)
        .env("JLO_HOME", &jlo)
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(
        !jlo.join("jlo.sh").exists(),
        "the binary rewrote an install the receipt says belongs to another path"
    );
    assert_eq!(
        std::fs::read_to_string(&receipt).unwrap(),
        foreign,
        "the receipt of a foreign install was overwritten"
    );
}

/// The detection reads the profile, and a commented-out line is not an active
/// one. It is also exactly how a user turns J'Lo off, so counting it would be
/// the one way this check can fail *closed*: withholding the instructions from
/// someone whose shell does not in fact load jlo.
#[test]
fn a_commented_out_profile_line_does_not_count_as_activated() {
    let (dir, _) = install(None);
    let home = dir.path().join("home");
    for disabled in [
        // The line the installer printed, switched off.
        "# [ -s \"$HOME/.jlo/jlo.sh\" ] && . \"$HOME/.jlo/jlo.sh\"\n",
        // Indented, as a profile with a conditional block would have it.
        "    #[ -s \"$HOME/.jlo/jlo.sh\" ] && . \"$HOME/.jlo/jlo.sh\"\n",
        // Trailing on a line that does run, but does not source anything.
        ": # . \"$HOME/.jlo/jlo.sh\"\n",
    ] {
        std::fs::write(home.join(".zshrc"), disabled).unwrap();
        let out = reinstall_over(&home);
        assert!(
            printed(&out).contains("To activate"),
            "{disabled:?} passed for an active install: {}",
            printed(&out)
        );
    }

    // And the live line still counts, so the stripping did not simply break
    // the check in the other direction.
    std::fs::write(
        home.join(".zshrc"),
        "[ -s \"$HOME/.jlo/jlo.sh\" ] && . \"$HOME/.jlo/jlo.sh\"  # jlo\n",
    )
    .unwrap();
    let out = reinstall_over(&home);
    assert!(
        !printed(&out).contains("To activate"),
        "a live profile line was not recognised: {}",
        printed(&out)
    );
}

/// An optional stub that cannot be written is a warning, not a failure, and
/// the receipt is still written afterwards.
///
/// That is the one thing the receipt deliberately does not promise.
/// Suppressing it here would overload the state that already means something
/// else (a missing receipt is an install from before receipts existed, not a
/// broken one), and would put every later invocation, including the one the cd
/// hook makes, into a rewrite for as long as the underlying write kept
/// failing. What the user gets instead is the warning, by name, and the
/// repair.
#[test]
fn an_optional_stub_that_cannot_be_written_warns_and_names_the_repair() {
    let (dir, _) = install(None);
    let home = dir.path().join("home");
    let jlo = home.join(".jlo");
    let receipt = jlo.join("install-receipt.json");
    // Start from a receipt that does *not* match the binary, so the assertion
    // below is that the receipt advanced - not merely that one still exists.
    let before = std::fs::read_to_string(&receipt).unwrap();
    std::fs::write(
        &receipt,
        before.replacen(
            &format!("\"version\": \"{}\"", env!("CARGO_PKG_VERSION")),
            "\"version\": \"0.0.1-stale\"",
            1,
        ),
    )
    .unwrap();
    // A directory where a file belongs: the rename onto it cannot succeed.
    std::fs::remove_file(jlo.join("autoload.sh")).unwrap();
    std::fs::create_dir(jlo.join("autoload.sh")).unwrap();

    let out = reinstall_over(&home);
    assert!(
        out.status.success(),
        "an optional stub took the whole install down: {}",
        printed(&out)
    );
    assert!(
        printed(&out).contains("cd autoloading is unavailable"),
        "the failure was swallowed silently: {}",
        printed(&out)
    );
    assert!(
        printed(&out).contains("Re-run the installer"),
        "the warning did not say how to recover: {}",
        printed(&out)
    );
    assert!(
        std::fs::read_to_string(&receipt)
            .unwrap()
            .contains(&format!("\"version\": \"{}\"", env!("CARGO_PKG_VERSION"))),
        "a required file landed but the receipt still names the old version"
    );
}

/// A checksum file that is the right length but not hex is a file we could not
/// read, not a digest. It must not reach the "no verification tool here"
/// branch, which exists for a missing `shasum` and would let it through.
#[test]
fn a_malformed_checksum_file_aborts_the_install() {
    // Two shapes, and the second is the one the obvious validation misses: 64
    // characters counting the newline, and hex either side of it, so a
    // length-plus-alphabet check that never looks at line structure waves it
    // through - and with no shasum on the box it would install unverified.
    for malformed in [
        "z".repeat(64),
        format!("{}\n{}", "a".repeat(32), "a".repeat(31)),
    ] {
        a_malformed_checksum_file_aborts_the_install_with(&malformed);
    }
}

fn a_malformed_checksum_file_aborts_the_install_with(malformed: &str) {
    let dir = tempfile::tempdir().unwrap();
    let tarball = release_tarball(dir.path());
    let stubbin = dir.path().join("stubbin");
    std::fs::create_dir_all(&stubbin).unwrap();
    let curl = stubbin.join("curl");
    std::fs::write(
        &curl,
        format!(
            "#!/bin/sh\n\
             out=\n\
             while [ $# -gt 0 ]; do\n\
             \x20 case \"$1\" in -o) shift; out=\"$1\" ;; esac\n\
             \x20 shift\n\
             done\n\
             [ -n \"$out\" ] || exit 1\n\
             case \"$out\" in\n\
             \x20 *.sha256) printf '%s' '{}' > \"$out\" ;;\n\
             \x20 *) cp '{}' \"$out\" ;;\n\
             esac\n",
            malformed,
            tarball.display()
        ),
    )
    .unwrap();
    chmod(&curl, 0o755);

    let home = dir.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let out = Command::new("/bin/sh")
        .arg(manifest().join("install.sh"))
        .env("HOME", &home)
        .env("SHELL", "/bin/zsh")
        .env(
            "PATH",
            format!(
                "{}:{}",
                stubbin.display(),
                std::env::var("PATH").unwrap_or_default()
            ),
        )
        .env_remove("JLO_HOME")
        .output()
        .unwrap();

    assert!(
        !out.status.success(),
        "the checksum file {malformed:?} was accepted: {}",
        printed(&out)
    );
    assert!(
        printed(&out).contains("not a SHA256"),
        "the installer did not say what was wrong: {}",
        printed(&out)
    );
    assert!(
        !home.join(".jlo").join("jlo.sh").exists(),
        "the layout was written despite an unusable checksum"
    );
}

/// A symlink that is already correct is left exactly as it is - not removed
/// and recreated. Between those two syscalls `jlo` is missing from PATH, and
/// something else can take the name.
#[test]
fn an_already_correct_symlink_is_not_recreated() {
    let (dir, _) = install(None);
    let home = dir.path().join("home");
    let link = home.join(".local").join("bin").join("jlo");
    let before = std::fs::symlink_metadata(&link).unwrap();

    let out = reinstall_over(&home);
    assert!(out.status.success(), "{}", printed(&out));

    let after = std::fs::symlink_metadata(&link).unwrap();
    assert_eq!(
        (before.ino(), before.dev()),
        (after.ino(), after.dev()),
        "the installer replaced a symlink that was already right"
    );
}

/// The other half of the ownership check. The binary path can line up while
/// the receipt describes a different `$JLO_HOME` - a copied install, or one
/// reached through a `JLO_HOME` pointing elsewhere - and rewriting the
/// original's scripts from there is exactly the clobber the guard exists for.
#[test]
fn a_receipt_naming_a_different_jlo_home_is_left_alone() {
    let (dir, _) = install(None);
    let home = dir.path().join("home");
    let jlo = home.join(".jlo");
    let receipt = jlo.join("install-receipt.json");

    let body = std::fs::read_to_string(&receipt).unwrap();
    let foreign = body
        .replacen(
            &format!("\"version\": \"{}\"", env!("CARGO_PKG_VERSION")),
            "\"version\": \"0.0.1-stale\"",
            1,
        )
        .replacen(
            &format!("\"jlo_home\": \"{}\"", jlo.display()),
            "\"jlo_home\": \"/somewhere/else\"",
            1,
        );
    assert!(foreign.contains("/somewhere/else"), "receipt shape changed");
    std::fs::write(&receipt, &foreign).unwrap();
    std::fs::remove_file(jlo.join("jlo.sh")).unwrap();

    let out = Command::new(jlo.join("bin").join("jlo-bin"))
        .arg("--version")
        .env("HOME", &home)
        .env("JLO_HOME", &jlo)
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(
        !jlo.join("jlo.sh").exists(),
        "the binary rewrote a layout whose receipt describes another JLO_HOME"
    );
}

// ---------------------------------------------------------------------------
// The post-update reload line
// ---------------------------------------------------------------------------

/// The reload line `jlo selfupdate` prints, evaluated in a shell that sourced
/// exactly `enabled` of the optional stubs. Reports which of the three files
/// the eval actually re-sourced.
///
/// Sourcing is observed by re-defining the files' own effects rather than by
/// spying on `.`: each stub is marked by having the shell record it in a
/// variable the moment it runs.
fn reload_in(sh: &str, home: &Path, jlo: &Path, enabled: &[&str]) -> Output {
    let mut sources = String::new();
    for name in std::iter::once("jlo.sh").chain(enabled.iter().copied()) {
        sources.push('.');
        sources.push(' ');
        sources.push_str(&squote(&jlo.join(name)));
        sources.push('\n');
    }
    let reload = Command::new(jlo.join("bin").join("jlo-bin"))
        .args(["__install", "--reload"])
        .env("HOME", home)
        .env("JLO_HOME", jlo)
        .output()
        .unwrap();
    assert!(reload.status.success());
    let line = String::from_utf8_lossy(&reload.stdout).into_owned();

    // `.` is shadowed *after* the opt-in sourcing above, so it only records
    // what the eval'd reload line does.
    Command::new(sh)
        .arg("-c")
        .arg(format!(
            "{sources}\
             . () {{ echo \"sourced=$1\"; }}\n\
             eval {}\n\
             echo \"status=$?\"\n",
            squote(Path::new(&line))
        ))
        .env("HOME", home)
        .env_remove("JLO_HOME")
        .output()
        .unwrap()
}

/// The reload always re-sources `jlo.sh` - that is the resident wrapper being
/// replaced - and never enables an optional stub the user had not enabled.
#[test]
fn the_reload_line_re_sources_only_what_this_shell_had_enabled() {
    let (dir, _) = install(None);
    let home = dir.path().join("home");
    let jlo = home.join(".jlo");

    for sh in shells(
        "the_reload_line_re_sources_only_what_this_shell_had_enabled",
        INTERPRETERS,
    ) {
        let bare = reload_in(sh, &home, &jlo, &[]);
        let stdout = String::from_utf8_lossy(&bare.stdout);
        assert!(
            stdout.contains("sourced=") && stdout.contains("jlo.sh"),
            "{sh}: the reload did not re-source jlo.sh: {stdout:?}"
        );
        assert!(
            !stdout.contains("autoload.sh") && !stdout.contains("completions.sh"),
            "{sh}: the reload enabled a stub the user had not: {stdout:?}"
        );
        // A false `[ -n ... ]` must not become the status of the whole eval.
        assert!(
            stdout.contains("status=0"),
            "{sh}: a successful reload reported failure: {stdout:?}"
        );

        let opted_in = reload_in(sh, &home, &jlo, &["autoload.sh", "completions.sh"]);
        let stdout = String::from_utf8_lossy(&opted_in.stdout);
        for name in ["jlo.sh", "autoload.sh", "completions.sh"] {
            assert!(
                stdout.contains(name),
                "{sh}: the reload skipped {name} although this shell had it: {stdout:?}"
            );
        }
        assert!(
            stdout.contains("status=0"),
            "{sh}: a successful reload reported failure: {stdout:?}"
        );
    }
}

/// Every stub `__install` writes, paired with the file it loads under `sh`
/// (both relative to `$JLO_HOME`), and whether it needs `jlo.sh` sourced
/// first - the autoload stubs are inert without the wrapper, by design.
fn stub_targets(sh: &str) -> [(&'static str, String, bool); 5] {
    let d = if sh.ends_with("zsh") { "zsh" } else { "bash" };
    let completion = if d == "zsh" {
        "completions/_jlo"
    } else {
        "completions/jlo.bash"
    };
    [
        ("jlo.sh", format!("bin/jlo-init.{d}"), false),
        ("bin/jlo-init.sh", format!("bin/jlo-init.{d}"), false),
        ("autoload.sh", format!("bin/jlo-autoload.{d}"), true),
        ("bin/jlo-autoload.sh", format!("bin/jlo-autoload.{d}"), true),
        ("completions.sh", completion.to_string(), false),
    ]
}

#[derive(Clone, Copy, Debug)]
enum Breakage {
    Missing,
    Unreadable,
    /// Present and readable, but its own `.` fails.
    FailsToLoad,
}

/// A stub's exit status is the reload's only signal. Trailing cleanup (an
/// `unset`, a marker assignment) must not turn a load that did not happen into
/// a success, and the marker the reload reads must not claim it did.
#[test]
fn a_stub_whose_target_cannot_be_loaded_fails_when_sourced() {
    let (dir, _) = install(None);
    let home = dir.path().join("home");
    let jlo = home.join(".jlo");

    for sh in shells(
        "a_stub_whose_target_cannot_be_loaded_fails_when_sourced",
        INTERPRETERS,
    ) {
        for (stub, target, needs_wrapper) in stub_targets(sh) {
            let target = jlo.join(target);
            for how in [
                Breakage::Missing,
                Breakage::Unreadable,
                Breakage::FailsToLoad,
            ] {
                // zsh's `_jlo` is autoloaded on the first Tab, never sourced.
                if matches!(how, Breakage::FailsToLoad) && target.ends_with("_jlo") {
                    continue;
                }
                let original = std::fs::read(&target).unwrap();
                let mode = std::fs::metadata(&target).unwrap().mode();
                match how {
                    Breakage::Missing => std::fs::remove_file(&target).unwrap(),
                    Breakage::Unreadable => {
                        chmod(&target, 0o000);
                    }
                    Breakage::FailsToLoad => std::fs::write(&target, "return 7\n").unwrap(),
                }
                // Root reads a mode-000 file anyway, so there is nothing to test.
                let untestable =
                    matches!(how, Breakage::Unreadable) && std::fs::File::open(&target).is_ok();

                let out = if untestable {
                    eprintln!("SKIP {stub} with an unreadable target: this user reads it anyway.");
                    None
                } else {
                    let pre = if needs_wrapper {
                        format!(". {}\n", squote(&jlo.join("jlo.sh")))
                    } else {
                        String::new()
                    };
                    Some(
                        Command::new(sh)
                            .arg("-c")
                            .arg(format!(
                                "{pre}. {}\n\
                                 echo \"status=$?\"\n\
                                 echo \"markers=[${{_JLO_AUTOLOAD-}}${{_JLO_COMPLETIONS-}}]\"",
                                squote(&jlo.join(stub))
                            ))
                            .env("HOME", &home)
                            .env_remove("JLO_HOME")
                            .output()
                            .unwrap(),
                    )
                };

                let _ = std::fs::remove_file(&target);
                std::fs::write(&target, original).unwrap();
                chmod(&target, mode);

                let Some(out) = out else { continue };
                let stdout = String::from_utf8_lossy(&out.stdout);
                assert!(
                    stdout.contains("status=") && !stdout.contains("status=0"),
                    "{sh}: {stub} reported success with its target {how:?}: {stdout:?} \
                     stderr={:?}",
                    String::from_utf8_lossy(&out.stderr)
                );
                assert!(
                    stdout.contains("markers=[]"),
                    "{sh}: {stub} set its marker although nothing loaded ({how:?}): {stdout:?}"
                );
            }

            // A `set -e` profile must still finish starting up: there the
            // failure is dropped rather than turned into a dead login shell.
            let original = std::fs::read(&target).unwrap();
            std::fs::remove_file(&target).unwrap();
            let pre = if needs_wrapper {
                format!(". {}\n", squote(&jlo.join("jlo.sh")))
            } else {
                String::new()
            };
            let out = Command::new(sh)
                .arg("-c")
                .arg(format!(
                    "set -e\n{pre}. {}\necho survived",
                    squote(&jlo.join(stub))
                ))
                .env("HOME", &home)
                .env_remove("JLO_HOME")
                .output()
                .unwrap();
            std::fs::write(&target, original).unwrap();
            let stdout = String::from_utf8_lossy(&out.stdout);
            assert!(
                stdout.contains("survived"),
                "{sh}: {stub} with a missing target aborted a set -e shell: {stdout:?}"
            );
        }
    }
}

/// The end-to-end form of the above: `jlo selfupdate` through the resident
/// wrapper, whose reload re-sources a stub that can no longer load what it
/// points at. The `&&` in the payload only helps if the stub reports it.
#[test]
fn a_reload_that_cannot_load_a_stub_fails_the_selfupdate() {
    let (dir, _) = install(None);
    let home = dir.path().join("home");
    let jlo = home.join(".jlo");
    let binary = jlo.join("bin").join("jlo-bin");

    // The payload the freshly published binary prints, replayed by a stand-in
    // so the run needs neither a network nor a second release.
    let payload = run_staged(
        Command::new(&binary)
            .args(["__wrapped", "__install", "--reload"])
            .env("HOME", &home)
            .env("JLO_HOME", &jlo),
    );
    assert!(payload.status.success());
    let replay = dir.path().join("payload");
    std::fs::write(&replay, &payload.stdout).unwrap();
    std::fs::remove_file(&binary).unwrap();
    std::fs::write(&binary, format!("#!/bin/sh\ncat {}\n", squote(&replay))).unwrap();
    chmod(&binary, 0o755);

    for sh in shells(
        "a_reload_that_cannot_load_a_stub_fails_the_selfupdate",
        INTERPRETERS,
    ) {
        // `errexit` too: the stubs drop a failure under it when a profile
        // sources them, and must not when the reload does.
        let run = |options: &str, breakage: &str| {
            let out = Command::new(sh)
                .arg("-c")
                .arg(format!(
                    "set {options}\n. {jlo_sh}\n. {autoload}\n. {completions}\n\
                     {breakage}\n\
                     if jlo selfupdate; then echo status=0; else echo \"status=$?\"; fi",
                    jlo_sh = squote(&jlo.join("jlo.sh")),
                    autoload = squote(&jlo.join("autoload.sh")),
                    completions = squote(&jlo.join("completions.sh")),
                ))
                .env("HOME", &home)
                .env_remove("JLO_HOME")
                .output()
                .unwrap();
            (
                String::from_utf8_lossy(&out.stdout).into_owned(),
                String::from_utf8_lossy(&out.stderr).into_owned(),
            )
        };
        // Without this the failures below could be the stand-in's.
        for options in ["+e", "-eu"] {
            let (stdout, stderr) = run(options, ":");
            assert!(
                stdout.contains("status=0"),
                "{sh} (set {options}): an intact reload failed: {stdout:?} stderr={stderr:?}"
            );
        }

        // Only the three entry files: the reload never sources the shims.
        for (stub, target, _) in stub_targets(sh)
            .into_iter()
            .filter(|(stub, _, _)| !stub.starts_with("bin/"))
        {
            let target = jlo.join(target);
            let away = target.with_extension("away");
            for options in ["+e", "-eu"] {
                let (stdout, stderr) = run(
                    options,
                    &format!("mv {} {}", squote(&target), squote(&away)),
                );
                std::fs::rename(&away, &target).unwrap();
                assert!(
                    stdout.contains("status=") && !stdout.contains("status=0"),
                    "{sh} (set {options}): a reload that could not load {stub}'s target \
                     reported success: {stdout:?} stderr={stderr:?}"
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// The 0.2.0/0.3.0 compatibility shims
// ---------------------------------------------------------------------------

/// The migration path from every version that was ever released.
///
/// 0.2.0 and 0.3.0 are the only tags there are, and both print a profile block
/// that sources `bin/jlo-init.sh` and `bin/jlo-autoload.sh` directly - the
/// generated entry files landed after 0.3.0 was tagged, so no released
/// installer knows `jlo.sh` exists. Writing the dialect files beside those two
/// and leaving them alone left every existing user loading the *old* wrapper
/// permanently, `curl | bash` selfupdate and all, with nothing looking broken.
///
/// So the install verb generates them too. The foreign contents below stand in
/// for the 0.3.0 originals: the test is that none of it survives, in both
/// shells, with `JLO_HOME` deliberately unexported - the shim bakes the path,
/// which is what makes it right for an install that is not `$HOME/.jlo`.
#[test]
fn the_old_profile_paths_load_the_new_wrapper() {
    let (dir, _) = install(None);
    let home = dir.path().join("home");
    let bin = home.join(".jlo").join("bin");

    std::fs::write(
        bin.join("jlo-init.sh"),
        "jlo() { echo 'the 0.3.0 wrapper'; }\n",
    )
    .unwrap();
    std::fs::write(
        bin.join("jlo-autoload.sh"),
        "jlo_after_cd() { echo 'the 0.3.0 hook'; }\n",
    )
    .unwrap();

    let out = reinstall_over(&home);
    assert!(
        out.status.success(),
        "the upgrade failed: {}",
        printed(&out)
    );

    for sh in shells("the_old_profile_paths_load_the_new_wrapper", INTERPRETERS) {
        // The old block's own two lines, verbatim.
        let out = Command::new(sh)
            .arg("-c")
            .arg(format!(
                "[ -s {init} ] && . {init}\n\
                 [ -s {auto} ] && . {auto}\n\
                 typeset -f jlo\n\
                 typeset -f jlo_after_cd\n\
                 echo \"marker=[${{_JLO_AUTOLOAD-}}]\"\n\
                 /bin/sh -c 'echo \"home=[$JLO_HOME]\"'",
                init = squote(&bin.join("jlo-init.sh")),
                auto = squote(&bin.join("jlo-autoload.sh")),
            ))
            .env("HOME", &home)
            .env_remove("JLO_HOME")
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            !stdout.contains("0.3.0 wrapper"),
            "{sh}: the old wrapper is still what the old path loads: {stdout:?}"
        );
        assert!(
            stdout.contains("jlo-bin"),
            "{sh}: the old path did not load the generated wrapper: {stdout:?} \
             stderr={:?}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            !stdout.contains("0.3.0 hook"),
            "{sh}: the old cd hook is still resident: {stdout:?}"
        );
        assert!(
            stdout.contains("--offline"),
            "{sh}: the old path did not load the generated cd hook: {stdout:?}"
        );
        // The shim marks the hook the way autoload.sh does, so a later
        // 'jlo selfupdate' re-sources for this shell exactly what it had.
        assert!(
            stdout.contains("marker=[1]"),
            "{sh}: the autoload shim left no marker for the reload line: {stdout:?}"
        );
        // Read back from a *child*: the wrapper and the hook both read
        // JLO_HOME out of the environment.
        assert!(
            stdout.contains(&format!("home=[{}]", home.join(".jlo").display())),
            "{sh}: the shim did not export the baked JLO_HOME: {stdout:?}"
        );
    }
}

/// The shims make the old block keep working, which is exactly why the user
/// has to be told about it once: it is the only moment J'Lo can name the form
/// that replaces it while both still work.
///
/// Nothing is written. The installer only ever reads the profile - a block the
/// user pasted is theirs to remove.
#[test]
fn a_profile_with_the_old_block_is_told_what_replaces_it() {
    let (dir, _) = install(None);
    let home = dir.path().join("home");
    let profile = home.join(".zshrc");
    // The v0.2.0 block, verbatim. v0.3.0 prints the same three lines and adds
    // a completions block of its own.
    let block = "export JLO_HOME=\"$HOME/.jlo\"\n\
                 [[ -s \"$JLO_HOME/bin/jlo-init.sh\" ]] && source \"$JLO_HOME/bin/jlo-init.sh\"\n\
                 [[ -s \"$JLO_HOME/bin/jlo-autoload.sh\" ]] && source \"$JLO_HOME/bin/jlo-autoload.sh\"\n";
    std::fs::write(&profile, block).unwrap();

    let printed = printed(&reinstall_over(&home));
    assert!(
        printed.contains("pre-0.4.0 J'Lo block"),
        "the installer never mentioned the old block: {printed}"
    );
    for name in ["jlo.sh", "autoload.sh"] {
        assert!(
            printed.contains(&format!("\"$HOME/.jlo/{name}\"")),
            "the installer did not print the line replacing {name}: {printed}"
        );
    }
    // v0.2.0's block has no completions line, so offering one back would be
    // handing the user something they never had.
    assert!(
        !printed.contains("completions.sh"),
        "the installer offered back a line the old block never had: {printed}"
    );
    // This profile does load J'Lo, so the "you never added the line" warning
    // would be simply untrue here.
    assert!(
        !printed.contains("never added"),
        "a profile that loads J'Lo was told it does not: {printed}"
    );
    assert_eq!(
        std::fs::read_to_string(&profile).unwrap(),
        block,
        "the installer wrote to the user's profile"
    );
}

/// v0.3.0's block sources the completion scripts out of `completions/` under
/// its own `$BASH_VERSION`/`$ZSH_VERSION` test, so that user gets the line
/// that replaces it too - and only that user.
#[test]
fn the_0_3_0_block_is_also_offered_the_completions_line() {
    let (dir, _) = install(None);
    let home = dir.path().join("home");
    std::fs::write(
        home.join(".zshrc"),
        "export JLO_HOME=\"$HOME/.jlo\"\n\
         [[ -s \"$JLO_HOME/bin/jlo-init.sh\" ]] && source \"$JLO_HOME/bin/jlo-init.sh\"\n\
         [[ -s \"$JLO_HOME/bin/jlo-autoload.sh\" ]] && source \"$JLO_HOME/bin/jlo-autoload.sh\"\n\
         if [ -n \"$ZSH_VERSION\" ]; then\n\
         \x20 [[ -s \"$JLO_HOME/completions/_jlo\" ]] && source \"$JLO_HOME/completions/_jlo\"\n\
         fi\n",
    )
    .unwrap();

    let printed = printed(&reinstall_over(&home));
    assert!(
        printed.contains("\"$HOME/.jlo/completions.sh\""),
        "the 0.3.0 block was not offered the completions line: {printed}"
    );
}

/// The lock file is the one thing under `$JLO_HOME` with no purpose once the
/// run that took it is over.
#[test]
fn an_install_leaves_no_lock_file_behind() {
    let (dir, _) = install(None);
    let jlo = dir.path().join("home").join(".jlo");
    let lock = jlo.join(".selfupdate.lock");
    assert!(!lock.exists(), "the installer left {lock:?} behind");
    reinstall_over(jlo.parent().unwrap());
    assert!(!lock.exists(), "the upgrade left {lock:?} behind");
}

/// A directory name may contain a newline, and the two shims are the only
/// generated files that put a path in a **comment** rather than inside single
/// quotes. One leading `#` would end at the first newline and leave the rest
/// of the path standing as shell code with an unmatched quote: a file that
/// does not parse, sourced from the user's profile on every shell start.
///
/// The install verb is run directly here rather than through `install.sh`,
/// which is not the code under test and has its own quoting to answer for.
#[test]
fn a_jlo_home_containing_a_newline_still_generates_files_that_parse() {
    let (dir, _) = install(None);
    let home = dir.path().join("home");
    let odd = home.join("two\nlines");
    std::fs::create_dir_all(odd.join("bin")).unwrap();
    std::fs::copy(
        home.join(".jlo").join("bin").join("jlo-bin"),
        odd.join("bin").join("jlo-bin"),
    )
    .unwrap();

    let out = Command::new(odd.join("bin").join("jlo-bin"))
        .arg("__install")
        .env("HOME", &home)
        .env("JLO_HOME", &odd)
        .env("SHELL", "/bin/zsh")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "the install verb failed: {}",
        printed(&out)
    );

    for name in [
        "jlo.sh",
        "autoload.sh",
        "completions.sh",
        "bin/jlo-init.sh",
        "bin/jlo-autoload.sh",
    ] {
        let script = odd.join(name);
        assert!(
            script.is_file(),
            "the install verb did not write {script:?}"
        );
        for sh in shells(
            "a_jlo_home_containing_a_newline_still_generates_files_that_parse",
            INTERPRETERS.iter().chain(["/bin/sh"].iter()),
        ) {
            let parsed = Command::new(sh).arg("-n").arg(&script).output().unwrap();
            assert!(
                parsed.status.success(),
                "{name} does not parse under {sh}: {}",
                String::from_utf8_lossy(&parsed.stderr)
            );
        }
    }
}

/// The old block is not always in the file the login shell reads. The
/// released installers said "e.g., ~/.bashrc, ~/.zshrc", and bash on macOS is
/// the motivating case: the login shell reads `~/.bash_profile`, so a block
/// pasted into `~/.bashrc` and sourced from there is active and invisible to a
/// one-file check. Spelled here with zsh as the login shell, which puts the
/// block outside the candidate on every platform - on Linux, bash's own
/// candidate *is* `~/.bashrc`.
///
/// Both halves matter. The scan has to find it, and finding it must not
/// replace the activation instructions: this profile may load nothing at all,
/// and sending the user away with no install and an edit to make in a file
/// their shell never opens is the one failure this check exists to prevent.
#[test]
fn the_old_block_outside_the_login_profile_is_named_but_replaces_nothing() {
    let (dir, _) = install(None);
    let home = dir.path().join("home");
    std::fs::write(
        home.join(".bashrc"),
        "[[ -s \"$JLO_HOME/bin/jlo-init.sh\" ]] && source \"$JLO_HOME/bin/jlo-init.sh\"\n",
    )
    .unwrap();

    // SHELL is zsh, so the login profile is ~/.zshrc - which is not there.
    let printed = printed(&reinstall_over(&home));
    assert!(
        printed.contains("pre-0.4.0 J'Lo block"),
        "the block outside the login profile went unnoticed: {printed}"
    );
    assert!(
        printed.contains(".bashrc"),
        "the notice did not name the file the block is in: {printed}"
    );
    assert!(
        printed.contains("To activate"),
        "a block in a file the login shell does not read suppressed the \
         instructions: {printed}"
    );
}

// ---------------------------------------------------------------------------
// Staged publication
// ---------------------------------------------------------------------------

/// The installer must not write `bin/jlo-bin` itself.
///
/// It unpacks into a staging directory beside the destination and hands the
/// staged binary to the install verb, which renames it into place *under the
/// publication lock*. Writing it directly is how two publishers fail to
/// serialise: only one of them is inside the gate, and on Linux overwriting a
/// running executable is `ETXTBSY` besides.
///
/// Beside the destination, not under `$TMPDIR`: `rename` is atomic only
/// within one filesystem.
#[test]
fn publish_self_renames_the_staged_binary_into_the_layout() {
    let (dir, _) = install(None);
    let home = dir.path().join("home");
    let jlo_home = home.join(".jlo");
    let binary = jlo_home.join("bin").join("jlo-bin");

    let stage = jlo_home.join("bin").join(".jlo-install-test");
    std::fs::create_dir_all(&stage).unwrap();
    let staged = stage.join("jlo-bin");
    std::fs::copy(&binary, &staged).unwrap();
    // Identity, not just contents: a `copy` would leave the destination
    // looking right while giving up the atomic replacement, and would put
    // `ETXTBSY` back on the table against a binary that is still running.
    let staged_inode = std::fs::metadata(&staged).unwrap().ino();

    // A sentinel at the destination: if the publish is a no-op the assertions
    // below cannot tell a working install from an untouched one.
    std::fs::write(&binary, "not a binary\n").unwrap();

    let out = run_staged(
        Command::new(&staged)
            .args(["__install", "--publish-self"])
            .env("HOME", &home)
            .env("JLO_HOME", &jlo_home)
            .env("SHELL", "/bin/zsh"),
    );
    assert!(
        out.status.success(),
        "__install --publish-self failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    assert!(
        !staged.exists(),
        "the staged binary is still at {staged:?}; it was copied, not renamed"
    );
    assert_eq!(
        std::fs::metadata(&binary).unwrap().ino(),
        staged_inode,
        "{binary:?} is not the file that was staged, so it was copied rather \
         than renamed into place"
    );
    let version = Command::new(&binary).arg("--version").output().unwrap();
    assert!(
        version.status.success(),
        "{binary:?} is not runnable after the publish: {}",
        String::from_utf8_lossy(&version.stderr)
    );
}

/// `install-local.sh` and the self-heal both run the binary that is *already*
/// published. Renaming it onto itself would be a no-op at best, so the verb
/// has to recognise that case rather than trip over it.
#[test]
fn publish_self_is_a_no_op_when_the_binary_is_already_in_place() {
    let (dir, _) = install(None);
    let home = dir.path().join("home");
    let jlo_home = home.join(".jlo");
    let binary = jlo_home.join("bin").join("jlo-bin");

    let out = run_staged(
        Command::new(&binary)
            .args(["__install", "--publish-self"])
            .env("HOME", &home)
            .env("JLO_HOME", &jlo_home)
            .env("SHELL", "/bin/zsh"),
    );
    assert!(
        out.status.success(),
        "__install --publish-self failed on an in-place binary: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(binary.is_file(), "{binary:?} went missing");
}

/// A completed install leaves nothing transient behind.
#[test]
fn install_sh_leaves_no_staging_directory_behind() {
    let (dir, _) = install(None);
    let bin = dir.path().join("home").join(".jlo").join("bin");

    let leftovers: Vec<_> = std::fs::read_dir(&bin)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with(".jlo-install"))
        .collect();
    assert!(
        leftovers.is_empty(),
        "the installer left staging directories in {bin:?}: {leftovers:?}"
    );
}

/// Runs the real `install.sh` a second time against the same `HOME`, reusing
/// the stubbed `curl` and `tar` the first run was given.
fn reinstall_with_installer(dir: &Path, home: &Path) -> Output {
    let path = format!(
        "{}:{}",
        dir.join("stubbin").display(),
        std::env::var("PATH").unwrap_or_default()
    );
    Command::new("/bin/sh")
        .arg(manifest().join("install.sh"))
        .env("HOME", home)
        .env("PATH", path)
        .env("SHELL", "/bin/zsh")
        .env_remove("JLO_HOME")
        .output()
        .unwrap()
}

/// The binary must not be replaced outside the publication lock.
///
/// `install.sh` used to unpack straight over `$JLO_HOME/bin/jlo-bin` and only
/// then hand over to the install verb, which is where the lock is first taken.
/// The one file every other part of the layout is generated *from* was
/// therefore written by a publisher standing outside the gate: a concurrent
/// `selfupdate` holding the lock could have the executable swapped under it.
///
/// With the lock held by somebody else, a correct installer changes nothing at
/// the destination.
///
/// The holder is a `python3` one-liner rather than `flock(1)`, which is a
/// util-linux tool and absent on macOS - a test that silently skips on the
/// developer's own machine is not a test.
#[test]
fn a_held_lock_leaves_the_installed_binary_untouched() {
    let (dir, _) = install(None);
    let home = dir.path().join("home");
    let jlo_home = home.join(".jlo");
    let binary = jlo_home.join("bin").join("jlo-bin");

    // A sentinel rather than the real binary: anything that reaches the
    // destination while the lock is held overwrites it, and that is the whole
    // assertion.
    let sentinel = b"held by somebody else\n";
    std::fs::write(&binary, sentinel).unwrap();

    let lock = jlo_home.join(".selfupdate.lock");
    let ready = dir.path().join("lock-held");
    let holder = Command::new("python3")
        .arg("-c")
        .arg(
            "import fcntl, pathlib, sys, time\n\
             f = open(sys.argv[1], 'w')\n\
             fcntl.flock(f, fcntl.LOCK_EX)\n\
             pathlib.Path(sys.argv[2]).write_text('held')\n\
             time.sleep(30)\n",
        )
        .arg(&lock)
        .arg(&ready)
        .spawn();
    let Ok(mut holder) = holder else {
        eprintln!(
            "SKIP a_held_lock_leaves_the_installed_binary_untouched: python3 is not installed here."
        );
        return;
    };

    let mut held = false;
    for _ in 0..100 {
        if ready.exists() {
            held = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }

    let out = held.then(|| reinstall_with_installer(dir.path(), &home));
    let _ = holder.kill();
    let _ = holder.wait();
    assert!(held, "the holder never took the lock");

    let out = out.unwrap();
    assert!(
        !out.status.success(),
        "the installer ignored the lock: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    // Compared as a boolean: a mismatch here means the real binary landed,
    // and printing a few megabytes of Mach-O helps nobody.
    assert!(
        std::fs::read(&binary).unwrap() == sentinel,
        "{binary:?} was replaced although another publisher held the lock"
    );

    // The installer `exec`s the staged binary and has no line left to run, so
    // a refused publish can only be cleaned up by the process that was
    // refused. Otherwise every failed install leaves a copy of J'Lo in `bin/`.
    let bin = jlo_home.join("bin");
    let leftovers: Vec<_> = std::fs::read_dir(&bin)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with(".jlo-install"))
        .collect();
    assert!(
        leftovers.is_empty(),
        "the refused install left staging directories in {bin:?}: {leftovers:?}"
    );
}

/// The download origin is injectable, the way `JLO_ADOPTIUM_API_URL` and
/// `JLO_RELEASE_API_URL` are for the binary's two remotes.
///
/// Without it nothing can drive the half of this script that talks to the
/// network against anything but GitHub, so the only part these tests could
/// ever reach is the layout the binary writes afterwards.
#[test]
fn the_download_origin_is_overridable() {
    let dir = tempfile::tempdir().unwrap();
    let tarball = release_tarball(dir.path());
    let stubbin = stub_curl(dir.path(), &tarball, Checksum::Correct);
    stub_gnu_tar(&stubbin);
    let home = dir.path().join("home");
    std::fs::create_dir_all(&home).unwrap();

    let path = format!(
        "{}:{}",
        stubbin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let out = Command::new("/bin/sh")
        .arg(manifest().join("install.sh"))
        .env("HOME", &home)
        .env("PATH", path)
        .env("SHELL", "/bin/zsh")
        .env_remove("JLO_HOME")
        .env("JLO_INSTALL_BASE_URL", "https://example.invalid/jlo")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "install.sh failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // The exact assets, not just the origin: the stub serves the fixture by
    // *output* file name, so an installer that asked for the wrong package -
    // or skipped the checksum entirely - would still produce a working install
    // and satisfy a prefix check.
    let os = if cfg!(target_os = "macos") {
        "macos"
    } else {
        "linux"
    };
    let package = format!("jlo-{os}-{}.tar.gz", uname_m());
    let asked: Vec<String> = std::fs::read_to_string(dir.path().join("curl-urls"))
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect();
    assert_eq!(
        asked,
        vec![
            format!("https://example.invalid/jlo/{package}"),
            format!("https://example.invalid/jlo/{package}.sha256"),
        ],
        "the installer did not fetch exactly the overridden tarball and its checksum"
    );
}

/// `uname -m`, which is what `install.sh` interpolates unfiltered, and which
/// is not Rust's `consts::ARCH`: the same machine is `aarch64` to Rust on both
/// platforms, but `uname` calls it `aarch64` on Linux and `arm64` on macOS.
/// That difference is why the published package names differ, so the test has
/// to ask the same question the installer asks.
fn uname_m() -> String {
    let out = Command::new("uname").arg("-m").output().unwrap();
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

/// The cleanup guard deletes a staging directory, so what counts as one must
/// be established rather than guessed from a name.
///
/// Here the directory is named like a staging directory and holds a binary,
/// but belongs to a different install: the verb was pointed at another
/// `JLO_HOME` entirely. Deleting it would take somebody else's files with it.
#[test]
fn a_staging_name_outside_the_install_directory_is_left_alone() {
    let (dir, _) = install(None);
    let home = dir.path().join("home");
    let jlo_home = home.join(".jlo");

    let impostor = dir.path().join(".jlo-install-backups");
    std::fs::create_dir_all(&impostor).unwrap();
    std::fs::copy(
        jlo_home.join("bin").join("jlo-bin"),
        impostor.join("jlo-bin"),
    )
    .unwrap();
    let bystander = impostor.join("please-keep-me");
    std::fs::write(&bystander, "not jlo's\n").unwrap();

    let elsewhere = dir.path().join("other-home");
    let out = run_staged(
        Command::new(impostor.join("jlo-bin"))
            .args(["__install", "--publish-self"])
            .env("HOME", &home)
            .env("JLO_HOME", &elsewhere)
            .env("SHELL", "/bin/zsh"),
    );
    assert!(
        out.status.success(),
        "the install verb failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    assert!(
        bystander.is_file(),
        "{bystander:?} was deleted: a directory was treated as staging on the \
         strength of its name alone"
    );
}

/// Even a real staging directory is only swept of what was staged in it. A
/// file nobody here put there stops the removal rather than going with it.
#[test]
fn staging_cleanup_stops_at_anything_it_did_not_put_there() {
    let (dir, _) = install(None);
    let home = dir.path().join("home");
    let jlo_home = home.join(".jlo");
    let binary = jlo_home.join("bin").join("jlo-bin");

    let stage = jlo_home.join("bin").join(".jlo-install-test");
    std::fs::create_dir_all(&stage).unwrap();
    std::fs::copy(&binary, stage.join("jlo-bin")).unwrap();
    let bystander = stage.join("unexpected");
    std::fs::write(&bystander, "somebody else's\n").unwrap();

    let out = run_staged(
        Command::new(stage.join("jlo-bin"))
            .args(["__install", "--publish-self"])
            .env("HOME", &home)
            .env("JLO_HOME", &jlo_home)
            .env("SHELL", "/bin/zsh"),
    );
    assert!(
        out.status.success(),
        "the install verb failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    assert!(
        bystander.is_file(),
        "{bystander:?} was swept away with the staging directory"
    );
}
