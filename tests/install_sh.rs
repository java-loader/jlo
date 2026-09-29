//! Tests for the layout an install produces, and for what it prints.
//!
//! `install.sh` is a bootstrap now: it downloads one file, verifies it,
//! unpacks it and hands over to the binary, which owns everything under
//! `$JLO_HOME` - the three entry stubs, the two wrapper dialects, the
//! completions and the symlink. These tests run the real
//! `install.sh` against a temporary `HOME` with `curl` stubbed out, then
//! source the generated files from real shells.
//!
//! The Rust compiler never checks the generated shell code, so sourcing it
//! here is the only thing that does.

// Test code: an `unwrap` failure here is a test failure, which is the point.
#![allow(clippy::unwrap_used)]

mod common;

use common::{HERMETIC_PATH, INTERPRETERS, chmod, hermetic, jlo_bin, shells, skip_missing, squote};
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
    std::fs::copy(jlo_bin(), stage.join("jlo-bin")).unwrap();
    let tarball = dir.join("jlo.tar.gz");
    let ok = hermetic("tar", dir)
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
    /// Computed here, in Rust: for a test that takes both hashing tools off
    /// `PATH`, where `Correct` would serve an empty sum and exercise the
    /// malformed-checksum refusal instead.
    Precomputed,
    Wrong,
    /// No `.sha256` beside the tarball: `curl -f` fails.
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
        Checksum::Precomputed => {
            use sha2::Digest as _;
            let sum = hex::encode(sha2::Sha256::digest(std::fs::read(tarball).unwrap()));
            format!("echo '{sum}' > \"$out\"")
        }
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
fn run_installer(jlo_home: Option<&str>, checksum: Checksum) -> (tempfile::TempDir, Output) {
    let dir = tempfile::tempdir().unwrap();
    let tarball = release_tarball(dir.path());
    let stubbin = stub_curl(dir.path(), &tarball, checksum);
    // Every install test runs against it: install.sh must never hand tar a
    // path it could reinterpret, whatever the test is otherwise about.
    stub_gnu_tar(&stubbin);
    let home = dir.path().join("home");
    std::fs::create_dir_all(&home).unwrap();

    let path = format!("{}:{HERMETIC_PATH}", stubbin.display());
    let mut cmd = hermetic("/bin/sh", &home);
    cmd.arg(manifest().join("install.sh")).env("PATH", path);
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
/// channel, and from 0.4.0 `selfupdate` prints a `. jlo.sh` line
/// there for the shell wrapper to eval.
fn printed(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// Sources `script` in a fresh interactive-style shell and runs `body`.
fn source_and_run(sh: &str, home: &Path, script: &Path, body: &str) -> Output {
    hermetic(sh, home)
        .arg("-c")
        .arg(format!(". {}\n{body}", squote(script)))
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
    let (dir, _) = install(Some("$HOME/.jlo"));
    let home = dir.path().join("home");
    let entry = home.join(".jlo").join("jlo.sh");
    let body = std::fs::read_to_string(&entry).unwrap();
    assert!(
        body.contains("export JLO_HOME="),
        "generated jlo.sh has no JLO_HOME export: {body}"
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
            let out = hermetic(sh, dir.path())
                .arg("-n")
                .arg(&script)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{name} does not parse under {sh}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }
}

/// The printed instructions are the whole manual step: three lines the user
/// adds to their profile, and one that loads jlo into the shell they are
/// sitting in. If that shape grows, the regression is user-visible.
#[test]
fn the_printed_snippet_is_three_profile_lines_and_one_source_line() {
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
    assert_eq!(
        profile_lines(&printed).len(),
        3,
        "expected three profile lines:\n{printed}"
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

/// The lines the installer offers for the profile, as the user would copy
/// them: the escape bytes stripped, so they are what reaches the file.
fn profile_lines(printed: &str) -> Vec<String> {
    printed
        .lines()
        .map(visible)
        .filter(|l| l.starts_with("[ -s "))
        .collect()
}

/// The line that puts `~/.local/bin` on PATH, as the user would copy it.
fn path_line(printed: &str) -> Option<String> {
    printed
        .lines()
        .map(visible)
        .find(|l| l.starts_with("case "))
}

/// Appends `lines` to `file` under `home`, the way a user adds them in an
/// editor.
fn add_to(home: &Path, file: &str, lines: &[String]) {
    let path = home.join(file);
    let mut body = std::fs::read_to_string(&path).unwrap_or_default();
    for line in lines {
        body.push_str(line);
        body.push('\n');
    }
    std::fs::write(path, body).unwrap();
}

/// The PATH hint exists for shells that never read the interactive profile.
/// Its line, added to `~/.zshenv` as printed, has to put `jlo` on PATH for a
/// clean `zsh -c` - and for one nested in it, exactly once - and it must
/// *append*: the old hint prepended, so a revert to that shape is plausible
/// and has to fail this test - hence checking that `~/.local/bin` lands last,
/// not merely that it is present.
#[test]
fn the_printed_path_line_reaches_a_non_interactive_zsh() {
    if skip_missing("the_printed_path_line_reaches_a_non_interactive_zsh", "zsh") {
        return;
    }
    let (dir, out) = install(None);
    let home = dir.path().join("home");
    let line = path_line(&printed(&out)).expect("installer printed no PATH line");
    add_to(&home, ".zshenv", &[line]);

    let probe = hermetic("zsh", &home)
        .args(["-c", r#"zsh -c 'command -v jlo; print -r -- "PATH=$PATH"'"#])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&probe.stdout);
    let local_bin = home.join(".local").join("bin");
    assert!(
        stdout
            .lines()
            .any(|l| l == local_bin.join("jlo").display().to_string()),
        "a clean zsh -c does not find jlo: {stdout} {}",
        String::from_utf8_lossy(&probe.stderr)
    );
    let path = stdout
        .lines()
        .find_map(|l| l.strip_prefix("PATH="))
        .expect("probe printed no PATH");
    let entries: Vec<PathBuf> = std::env::split_paths(path).collect();
    assert_eq!(
        entries.iter().filter(|p| **p == local_bin).count(),
        1,
        "~/.local/bin is not on PATH exactly once: {path}"
    );
    // Appended, not prepended: J'Lo only needs `jlo` to be found, so it must
    // not change which of the user's other ~/.local/bin tools wins.
    assert_eq!(
        entries.last(),
        Some(&local_bin),
        "~/.local/bin was not appended to PATH: {path}"
    );
}

/// What a login bash - `bash -lc`, the shape `bash -l` scripts and most
/// terminal emulators start - sees with nothing but `HOME` and a minimal
/// `PATH`: whether `jlo` is on PATH, whether the activation lines ran, and
/// whether `marker_var` survived.
#[derive(Debug)]
struct BashLoginProbe {
    /// Read with `type -P`, not `command -v`: the activation lines define a
    /// `jlo` shell function in this very shell, and `-P` is the one form
    /// that still searches PATH instead of answering with the function.
    jlo: Option<String>,
    /// `type -t jlo`: `function` once the activation lines have run in this
    /// shell, `file` when only PATH has it.
    kind: Option<String>,
    marker: Option<String>,
}

fn bash_login_probe(home: &Path, marker_var: &str) -> BashLoginProbe {
    let out = hermetic("/bin/bash", home)
        .args([
            "-lc",
            &format!(
                r#"printf 'jlo=[%s]\n' "$(type -P jlo)"; printf 'kind=[%s]\n' "$(type -t jlo)"; printf 'marker=[%s]\n' "${{{marker_var}-}}""#
            ),
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let field = |prefix: &str| {
        stdout
            .lines()
            .find_map(|l| {
                l.strip_prefix(prefix)?
                    .strip_suffix(']')
                    .map(str::to_string)
            })
            .filter(|s| !s.is_empty())
    };
    BashLoginProbe {
        jlo: field("jlo=["),
        kind: field("kind=["),
        marker: field("marker=["),
    }
}

/// The only end-to-end coverage of the printed lines under a *login* bash -
/// what macOS terminals start, which never reads `~/.bashrc`. The printed
/// text sends a login bash to the first existing of `~/.bash_profile`,
/// `~/.bash_login` and `~/.profile`; the profile lines and the PATH line have
/// to work from either end of that list, `~/.profile` included, which
/// `sh -l` reads too - and leave the user's own lines in it working.
#[test]
fn a_login_bash_finds_jlo_after_adding_the_printed_lines() {
    if skip_missing(
        "a_login_bash_finds_jlo_after_adding_the_printed_lines",
        "/bin/bash",
    ) {
        return;
    }
    let (dir, out) = install(None);
    let home = dir.path().join("home");
    let printed_out = printed(&out);
    let mut lines = profile_lines(&printed_out);
    lines.push(path_line(&printed_out).expect("installer printed no PATH line"));
    let local_bin_jlo = home.join(".local").join("bin").join("jlo");

    let check = |file: &str, marker: Option<&str>| {
        let probe = bash_login_probe(&home, "JLO_TEST_MARKER");
        assert_eq!(
            probe.jlo.as_deref(),
            Some(local_bin_jlo.display().to_string().as_str()),
            "{file}: a login bash does not find jlo on PATH: {probe:?}"
        );
        assert_eq!(
            probe.kind.as_deref(),
            Some("function"),
            "{file}: a login bash did not load J'Lo from the printed lines: {probe:?}"
        );
        assert_eq!(
            probe.marker.as_deref(),
            marker,
            "{file}: the printed lines broke the user's own: {probe:?}"
        );
    };

    // The user's login setup lives in ~/.profile, exporting a marker only
    // they put there; the printed lines go after it.
    std::fs::write(home.join(".profile"), "export JLO_TEST_MARKER=1\n").unwrap();
    add_to(&home, ".profile", &lines);
    check(".profile", Some("1"));

    // No login file at all: the fallback the printed text names.
    std::fs::remove_file(home.join(".profile")).unwrap();
    add_to(&home, ".bash_profile", &lines);
    check(".bash_profile", None);
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
/// matching exactly the lines it is meant to police.
#[test]
fn no_copyable_line_is_indented() {
    let (dir, out) = install(None);
    let home = dir.path().join("home");
    assert_no_indented_commands(&printed(&out), 5, "fresh install");

    let coloured = hermetic(home.join(".jlo").join("bin").join("jlo-bin"), &home)
        .arg("__install")
        .env("JLO_HOME", home.join(".jlo"))
        .env("CLICOLOR_FORCE", "1")
        .output()
        .unwrap();
    let painted = printed(&coloured);
    assert!(
        painted.contains('\u{1b}'),
        "CLICOLOR_FORCE produced no escapes, so this run proves nothing: {painted}"
    );
    // The short form's one source line, and the PATH line.
    assert_no_indented_commands(&painted, 2, "coloured re-install");
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
        t.starts_with(". \"")
            || t.starts_with(". '")
            || t.starts_with("case ")
            || t.starts_with("export ")
            || t.starts_with("[ -s ")
            || t.starts_with("ln -s ")
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

/// A reinstall cannot know whether the profile sources jlo.sh - a text scan
/// cannot prove a line runs - so it prints the same short form whatever the
/// profile holds: one footnote and the one line, never the first-install block.
#[test]
fn a_reinstall_prints_the_short_form() {
    let (dir, first) = install(None);
    let home = dir.path().join("home");
    assert_eq!(
        profile_lines(&printed(&first)).len(),
        3,
        "the first install withheld the instructions"
    );
    let line = "[ -s \"$HOME/.jlo/jlo.sh\" ] && . \"$HOME/.jlo/jlo.sh\"";

    for profile in [None, Some(format!("{line}\n"))] {
        if let Some(body) = &profile {
            std::fs::write(home.join(".zshrc"), body).unwrap();
        }
        let out = reinstall_over(&home);
        let printed = printed(&out);
        assert!(out.status.success(), "{profile:?}: {printed}");
        assert!(
            printed.contains("installed to ~/.jlo"),
            "{profile:?}: the upgrade said nothing at all: {printed}"
        );
        assert!(
            printed.contains("Already in your profile? Nothing to do. Otherwise add:"),
            "{profile:?}: no footnote: {printed}"
        );
        let sources: Vec<&str> = printed
            .lines()
            .filter(|l| l.starts_with("[ -s ") || l.starts_with(". "))
            .collect();
        assert_eq!(sources, [line], "{profile:?}: {printed}");
    }
}

/// Runs the install verb again over an existing `$JLO_HOME`, the way a second
/// `install.sh` run would once the tarball is unpacked.
fn reinstall_over(home: &Path) -> Output {
    hermetic(home.join(".jlo").join("bin").join("jlo-bin"), home)
        .arg("__install")
        .env("JLO_HOME", home.join(".jlo"))
        .output()
        .unwrap()
}

/// Paths are pasted into the generated files as shell literals, so a character
/// that ends a quoted string early turns every one of them into a syntax error,
/// which the user discovers only when their profile breaks. An apostrophe is the
/// one that actually closes the quote; `$` and a backtick must survive as data
/// rather than being expanded when the file is sourced. The printed profile
/// lines carry the same path, and are checked the same way.
#[test]
fn a_jlo_home_with_shell_metacharacters_still_generates_valid_files() {
    let (dir, out) = install(Some("$HOME/o'brien $x `id` a\\nb"));
    let home = dir.path().join("home");
    let custom = home.join("o'brien $x `id` a\\nb");

    for name in ["jlo.sh", "autoload.sh", "completions.sh"] {
        let script = custom.join(name);
        assert!(script.is_file(), "install.sh did not generate {script:?}");
        for sh in shells(
            "a_jlo_home_with_shell_metacharacters_still_generates_valid_files",
            INTERPRETERS.iter().chain(["/bin/sh"].iter()),
        ) {
            let parsed = hermetic(sh, &home).arg("-n").arg(&script).output().unwrap();
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

    // What the installer prints is what the *user* puts in their profile:
    // added as printed, it has to load the wrapper under both shells.
    let lines = profile_lines(&printed(&out));
    assert_eq!(lines.len(), 3, "expected three profile lines: {lines:#?}");
    for (i, sh) in shells(
        "a_jlo_home_with_shell_metacharacters_still_generates_valid_files",
        INTERPRETERS,
    )
    .enumerate()
    {
        let profile = format!(".profile{i}");
        add_to(&home, &profile, &lines);
        let ran = hermetic(sh, &home)
            .arg("-c")
            .arg(format!(". {}\ntype jlo", squote(home.join(&profile))))
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&ran.stdout);
        assert!(
            stdout.contains("function"),
            "{sh}: the printed profile lines do not load the wrapper: {stdout:?} \
             lines={lines:?} stderr={:?}",
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
    let stubbin = stub_curl(dir.path(), &tarball, Checksum::Correct);
    let home = dir.path().join("home");
    std::fs::create_dir_all(&home).unwrap();

    // A JLO_HOME whose own directory is read-only: bin/ and the binary land
    // there first, then the entry file cannot be created next to them.
    let jlo_home = home.join("ro-jlo");
    std::fs::create_dir_all(jlo_home.join("bin")).unwrap();

    let path = format!("{}:{HERMETIC_PATH}", stubbin.display());
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

    let out = hermetic("/bin/sh", &home)
        .arg(&script)
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
}

// ---------------------------------------------------------------------------
// Completions: zsh autoloads, bash cannot
// ---------------------------------------------------------------------------

/// Sources the generated `completions.sh` in a zsh that has run `pre` first,
/// then reports what the shell ended up with: which function completes `jlo`,
/// and whether that function has been read yet. `-f` on purpose - the
/// developer's own dotfiles must not decide whether this passes.
fn zsh_completion_state(home: &Path, pre: &str) -> Output {
    let entry = squote(home.join(".jlo").join("completions.sh"));
    let script = format!(
        r#"{pre}
. {entry}
print -r -- "comps=[${{_comps[jlo]-}}]"
print -r -- "whence=[$(whence -v _jlo 2>&1)]"
"#
    );
    hermetic("zsh", home)
        .args(["-f", "-c", &script])
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

        let out = hermetic("/bin/sh", &home)
            .arg(&script)
            .env("PATH", format!("{}:{HERMETIC_PATH}", stubbin.display()))
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
        "bin/jlo-bin",
        "bin/jlo-init.sh",
        "bin/jlo-autoload.sh",
        "bin/jlo-completions.zsh",
        "completions/jlo.bash",
        "completions/_jlo",
    ] {
        assert!(jlo.join(rel).is_file(), "install did not write {rel}");
    }
    // The path every released profile block sources, so it must be the hook
    // itself rather than anything that merely points at one.
    let body = std::fs::read_to_string(jlo.join("bin/jlo-autoload.sh")).unwrap();
    assert!(
        body.contains("jlo_after_cd()"),
        "bin/jlo-autoload.sh does not define the cd hook: {body}"
    );
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

    let out = hermetic("/bin/sh", &home)
        .arg(manifest().join("install.sh"))
        .env("PATH", format!("{}:{HERMETIC_PATH}", stubbin.display()))
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
// The generated entry files under the running shell
// ---------------------------------------------------------------------------

/// Sources `jlo.sh` and then `autoload.sh` under `sh`, after `prologue`, and
/// runs `probe`.
fn source_autoload(sh: &str, home: &Path, prologue: &str, probe: &str) -> Output {
    let entry = squote(home.join(".jlo").join("jlo.sh"));
    let autoload = squote(home.join(".jlo").join("autoload.sh"));
    hermetic(sh, home)
        .arg("-c")
        .arg(format!("{prologue}\n. {entry}\n. {autoload}\n{probe}\n"))
        .current_dir(home)
        .output()
        .unwrap()
}

/// `jlo-autoload.sh` picks its hook mechanism at run time, and the builtin
/// test is the whole point: `ZSH_VERSION` is not exported *by default*, but
/// nothing stops a user from exporting it, and a bash child then inherits it.
/// Trusting the variable alone would send bash down the zsh branch, into an
/// `add-zsh-hook` it does not have, and leave the cd hook unregistered.
#[test]
fn the_autoload_hook_picks_the_running_shell_and_resists_a_spoof() {
    let (dir, _) = install(None);
    let home = dir.path().join("home");

    let bash = "/bin/bash";
    if !skip_missing("the_autoload_hook_picks_the_running_shell", bash) {
        let out = source_autoload(
            bash,
            &home,
            "export ZSH_VERSION=5.9",
            "echo \"pc=[${PROMPT_COMMAND-}]\"",
        );
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stdout.contains("jlo_after_cd"),
            "{bash} with an exported ZSH_VERSION registered no hook: {stdout:?} \
             stderr={stderr:?}"
        );
        assert!(
            stderr.is_empty(),
            "{bash} with an exported ZSH_VERSION complained: {stderr:?}"
        );
    }

    let zsh = "zsh";
    if !skip_missing("the_autoload_hook_picks_the_running_shell", zsh) {
        let out = source_autoload(
            zsh,
            &home,
            "export BASH_VERSION=5.2",
            "echo \"hooks=[${chpwd_functions[*]-}]\"",
        );
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            stdout.contains("jlo_after_cd"),
            "{zsh} with an exported BASH_VERSION registered no chpwd hook: {stdout:?} \
             stderr={:?}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

/// The wrapper is POSIX plus `local`, so a plain `sh` that sources `jlo.sh`
/// gets a working `jlo` too - and a profile under `set -eu` survives it.
#[test]
fn jlo_sh_defines_a_working_jlo_under_sh() {
    let (dir, _) = install(None);
    let home = dir.path().join("home");
    for sh in shells(
        "jlo_sh_defines_a_working_jlo_under_sh",
        ["/bin/sh", "/bin/dash"],
    ) {
        let out = hermetic(sh, &home)
            .arg("-c")
            .arg(format!(
                "set -eu\n. {}\njlo --version\n",
                squote(home.join(".jlo").join("jlo.sh"))
            ))
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{sh}: jlo.sh gave no working jlo: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(
            String::from_utf8_lossy(&out.stdout).trim(),
            format!("jlo {}", env!("CARGO_PKG_VERSION")),
            "{sh}: jlo --version did not reach the binary"
        );
    }
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

/// What a refused reinstall must leave: the binary that was there (same
/// inode), the layout that was there, and no staging directory. `bin/` itself
/// may have been created.
fn assert_existing_install_untouched(home: &Path, binary_ino: u64, jlo_sh: &str) {
    let jlo = home.join(".jlo");
    assert_eq!(
        std::fs::metadata(jlo.join("bin").join("jlo-bin"))
            .unwrap()
            .ino(),
        binary_ino,
        "the refused install replaced the binary"
    );
    assert_eq!(
        std::fs::read_to_string(jlo.join("jlo.sh")).unwrap(),
        jlo_sh,
        "the refused install rewrote the layout"
    );
    let leftovers: Vec<_> = std::fs::read_dir(jlo.join("bin"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with(".jlo-install"))
        .collect();
    assert!(leftovers.is_empty(), "staging left behind: {leftovers:?}");
}

/// `install.sh` is the only verification a new J'Lo gets - `jlo selfupdate`
/// runs it too - and every release it can fetch publishes a checksum. A
/// missing one is a broken release, and refused like a mismatch.
#[test]
fn a_missing_checksum_refuses_and_keeps_the_existing_install() {
    let (dir, _) = install(None);
    let home = dir.path().join("home");
    let jlo = home.join(".jlo");
    let binary_ino = std::fs::metadata(jlo.join("bin").join("jlo-bin"))
        .unwrap()
        .ino();
    let jlo_sh = std::fs::read_to_string(jlo.join("jlo.sh")).unwrap();

    // Same stub directory as the first run, now serving no checksum.
    stub_curl(
        dir.path(),
        &dir.path().join("jlo.tar.gz"),
        Checksum::Missing,
    );
    let out = reinstall_with_installer(dir.path(), &home);

    assert!(
        !out.status.success(),
        "installed without a checksum: {}",
        printed(&out)
    );
    assert!(
        printed(&out).contains("No published checksum"),
        "the installer did not say why it stopped: {}",
        printed(&out)
    );
    assert_existing_install_untouched(&home, binary_ino, &jlo_sh);
}

/// Neither `shasum` nor `sha256sum`: nothing can check the download, so
/// nothing is installed.
#[test]
fn a_missing_hashing_tool_refuses_and_keeps_the_existing_install() {
    let (dir, _) = install(None);
    let home = dir.path().join("home");
    let jlo = home.join(".jlo");
    let binary_ino = std::fs::metadata(jlo.join("bin").join("jlo-bin"))
        .unwrap()
        .ino();
    let jlo_sh = std::fs::read_to_string(jlo.join("jlo.sh")).unwrap();

    // Every tool install.sh and the stubs need, and neither hashing tool.
    let tools = dir.path().join("tools");
    std::fs::create_dir_all(&tools).unwrap();
    for tool in [
        "sh", "uname", "tr", "mkdir", "rm", "head", "cut", "cp", "cat", "gzip",
    ] {
        let found = ["/usr/bin", "/bin"]
            .iter()
            .map(|d| Path::new(d).join(tool))
            .find(|p| p.exists())
            .unwrap_or_else(|| panic!("{tool} not found in /usr/bin or /bin"));
        std::os::unix::fs::symlink(found, tools.join(tool)).unwrap();
    }
    stub_curl(
        dir.path(),
        &dir.path().join("jlo.tar.gz"),
        Checksum::Precomputed,
    );
    // `stub_gnu_tar` already sits in stubbin and calls /usr/bin/tar by path.
    let out = hermetic("/bin/sh", &home)
        .arg(manifest().join("install.sh"))
        .env(
            "PATH",
            format!(
                "{}:{}",
                dir.path().join("stubbin").display(),
                tools.display()
            ),
        )
        .output()
        .unwrap();

    assert!(
        !out.status.success(),
        "installed unverified: {}",
        printed(&out)
    );
    assert!(
        printed(&out).contains("Neither shasum nor sha256sum"),
        "the installer did not say why it stopped: {}",
        printed(&out)
    );
    assert_existing_install_untouched(&home, binary_ino, &jlo_sh);
}

/// An optional stub that cannot be written is a warning, not a failure: a
/// convenience lost is not a broken install. The user gets the warning, by
/// name, and the repair.
#[test]
fn an_optional_stub_that_cannot_be_written_warns_and_names_the_repair() {
    let (dir, _) = install(None);
    let home = dir.path().join("home");
    let jlo = home.join(".jlo");
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
}

/// A checksum file that is the right length but not hex is a file we could not
/// read, not a digest. It must not reach the "no verification tool here"
/// branch, which exists for a missing `shasum` and would let it through.
#[test]
fn a_malformed_checksum_file_aborts_the_install() {
    // Two shapes, and the second is the one the obvious validation misses: 64
    // characters counting the newline, and hex either side of it, so a
    // length-plus-alphabet check that never looks at line structure waves it
    // through - and it would install against a digest nobody published.
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
    let out = hermetic("/bin/sh", &home)
        .arg(manifest().join("install.sh"))
        .env("PATH", format!("{}:{HERMETIC_PATH}", stubbin.display()))
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

// ---------------------------------------------------------------------------
// Stubs
// ---------------------------------------------------------------------------

/// A stub whose target is gone, or fails to load, returns 0 without setting
/// its marker: the install that failed to write the target already reported
/// it, and a login profile under `set -e` must survive sourcing it.
#[test]
fn stubs_are_inert_under_set_e_when_the_target_is_missing_or_fails() {
    let (dir, _) = install(None);
    let home = dir.path().join("home");
    let jlo = home.join(".jlo");
    let mut failures = Vec::new();

    for (case, contents) in [("missing", None), ("failing", Some("return 7\n"))] {
        for target in [
            "bin/jlo-init.sh",
            "bin/jlo-autoload.sh",
            "bin/jlo-completions.zsh",
            "completions/jlo.bash",
            "completions/_jlo",
        ] {
            let target = jlo.join(target);
            match contents {
                None => std::fs::remove_file(target).unwrap(),
                Some(contents) => std::fs::write(target, contents).unwrap(),
            }
        }

        for sh in shells(
            "stubs_are_inert_under_set_e_when_the_target_is_missing_or_fails",
            INTERPRETERS,
        ) {
            for stub in ["jlo.sh", "autoload.sh", "completions.sh"] {
                // A stand-in `jlo`: without it autoload.sh is inert before it
                // ever looks for its target.
                let out = hermetic(sh, &home)
                    .arg("-e")
                    .arg("-c")
                    .arg(format!(
                        "jlo() {{ :; }}\n. {}\n\
                         echo \"alive markers=[${{_JLO_AUTOLOAD-}}${{_JLO_COMPLETIONS-}}]\"",
                        squote(jlo.join(stub))
                    ))
                    .output()
                    .unwrap();
                let stdout = String::from_utf8_lossy(&out.stdout);
                if !(out.status.success() && stdout.contains("alive markers=[]")) {
                    failures.push(format!(
                        "{sh}: {stub} with a {case} target: {stdout:?} stderr={:?}",
                        String::from_utf8_lossy(&out.stderr)
                    ));
                }
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

// ---------------------------------------------------------------------------
// The 0.2.0/0.3.0 profile block
// ---------------------------------------------------------------------------

/// The migration path from every version that was ever released.
///
/// 0.2.0 and 0.3.0 both print a profile block
/// that sources `bin/jlo-init.sh` and `bin/jlo-autoload.sh` directly - the
/// generated entry files landed after 0.3.0 was tagged, so no released
/// installer knows `jlo.sh` exists. Leaving those two paths alone left every
/// existing user loading the *old* wrapper permanently, `curl | bash`
/// selfupdate and all, with nothing looking broken.
///
/// Both paths are now the real files - the wrapper and the cd hook - so the old
/// block loads exactly what the new one does. The foreign contents below stand
/// in for the 0.3.0 originals: the test is that none of it survives, in both
/// shells.
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
        // The old block's own three lines, verbatim.
        let out = hermetic(sh, &home)
            .arg("-c")
            .arg(
                "export JLO_HOME=\"$HOME/.jlo\"\n\
                 [[ -s \"$JLO_HOME/bin/jlo-init.sh\" ]] && source \"$JLO_HOME/bin/jlo-init.sh\"\n\
                 [[ -s \"$JLO_HOME/bin/jlo-autoload.sh\" ]] && source \"$JLO_HOME/bin/jlo-autoload.sh\"\n\
                 typeset -f jlo\n\
                 typeset -f jlo_after_cd\n\
                 echo \"marker=[${_JLO_AUTOLOAD-}]\"\n\
                 jlo --version",
            )
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
        // The hook sets its own marker, so a later 'jlo selfupdate'
        // re-sources for this shell exactly what it had.
        assert!(
            stdout.contains("marker=[1]"),
            "{sh}: the old autoload path left no marker for the reload line: {stdout:?}"
        );
        assert!(
            stdout.contains(&format!("jlo {}", env!("CARGO_PKG_VERSION"))),
            "{sh}: the loaded wrapper did not reach the binary: {stdout:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// What 0.4.0 and 0.5.0 left behind
// ---------------------------------------------------------------------------

/// The dialect files 0.4.0 and 0.5.0 wrote, by their path under `$JLO_HOME`.
const RETIRED_DIALECTS: [&str; 4] = [
    "bin/jlo-init.bash",
    "bin/jlo-init.zsh",
    "bin/jlo-autoload.bash",
    "bin/jlo-autoload.zsh",
];

/// Lays down what 0.5.0 left under `jlo`: its receipt, its dialect files and
/// the empty lock file.
fn leave_0_5_0_files(jlo: &Path) {
    std::fs::write(
        jlo.join("install-receipt.json"),
        format!(
            "{{\n  \"version\": \"0.5.0\",\n  \"method\": \"installer\",\n  \
             \"jlo_home\": \"{home}\",\n  \"binary\": \"{home}/bin/jlo-bin\",\n  \
             \"symlink\": \"/nowhere/.local/bin/jlo\"\n}}\n",
            home = jlo.display()
        ),
    )
    .unwrap();
    for name in RETIRED_DIALECTS {
        std::fs::write(
            jlo.join(name),
            "# Generated by J'Lo - do not edit. Rewritten on every install and update.\n\
             #\n# a dialect\n",
        )
        .unwrap();
    }
    std::fs::write(jlo.join(".selfupdate.lock"), "").unwrap();
}

/// What 0.5.0 wrote and nothing reads any more goes on reinstall. The empty
/// lock file proves nothing about who made it, so it stays.
#[test]
fn a_reinstall_removes_what_earlier_releases_left_behind() {
    let (dir, _) = install(None);
    let home = dir.path().join("home");
    let jlo = home.join(".jlo");
    leave_0_5_0_files(&jlo);

    let out = reinstall_over(&home);
    assert!(out.status.success(), "{}", printed(&out));
    for name in RETIRED_DIALECTS.iter().chain(&["install-receipt.json"]) {
        assert!(!jlo.join(name).exists(), "{name} survived the reinstall");
    }
    assert!(jlo.join(".selfupdate.lock").is_file());
}

/// `$JLO_HOME` can be a directory the user already kept things in, so a file
/// with a retired name but not J'Lo's contents is theirs: kept, and not
/// mentioned.
#[test]
fn a_foreign_file_with_a_retired_name_survives() {
    let (dir, _) = install(None);
    let home = dir.path().join("home");
    let jlo = home.join(".jlo");
    let names: Vec<&str> = RETIRED_DIALECTS
        .iter()
        .copied()
        .chain(["install-receipt.json"])
        .collect();
    for name in &names {
        std::fs::write(jlo.join(name), "{\"mine\": true}\n").unwrap();
    }

    let out = reinstall_over(&home);
    assert!(out.status.success(), "{}", printed(&out));
    for name in &names {
        assert!(jlo.join(name).is_file(), "{name} was not J'Lo's to remove");
    }
    assert!(
        !printed(&out).contains("no longer uses"),
        "{}",
        printed(&out)
    );
}

/// An `autoload.sh` this run could not replace is still 0.5.0's, and sources
/// the dialect files - so they stay until it is replaced. The receipt is read
/// by no stub, so it goes regardless.
#[test]
fn an_old_stub_that_was_not_replaced_keeps_its_targets() {
    let (dir, _) = install(None);
    let home = dir.path().join("home");
    let jlo = home.join(".jlo");
    leave_0_5_0_files(&jlo);
    std::fs::remove_file(jlo.join("autoload.sh")).unwrap();
    std::fs::create_dir(jlo.join("autoload.sh")).unwrap();

    let out = reinstall_over(&home);
    assert!(out.status.success(), "{}", printed(&out));
    assert!(printed(&out).contains("autoload.sh"), "{}", printed(&out));
    for name in RETIRED_DIALECTS {
        assert!(
            jlo.join(name).is_file(),
            "{name} went while a stub needs it"
        );
    }
    assert!(!jlo.join("install-receipt.json").exists());
}

/// A newline in `JLO_HOME` would split a printed profile line, or end a
/// comment in a generated file early, so it is refused. The installer checks before its
/// first `mkdir`: the binary would refuse too, but only after the installer
/// had created `$JLO_HOME` and unpacked a staged copy into it, which a refusal
/// at that point leaves behind.
#[test]
fn an_installer_refuses_a_jlo_home_with_a_newline() {
    let (dir, out) = run_installer(Some("$HOME/jlo\nhome"), Checksum::Correct);
    let home = dir.path().join("home");
    let jlo_home = home.join("jlo\nhome");
    assert_eq!(out.status.code(), Some(1), "{}", printed(&out));
    assert!(
        printed(&out).contains("control characters"),
        "{}",
        printed(&out)
    );
    assert!(
        !jlo_home.exists(),
        "the refused install created {jlo_home:?}"
    );
    // Nothing at all under `$HOME`: no staging directory, wherever it landed.
    let leftovers: Vec<_> = std::fs::read_dir(&home)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert!(
        leftovers.is_empty(),
        "the refused install left {leftovers:?} in {home:?}"
    );

    // The verb run directly refuses the same value on its own account.
    let direct = hermetic(jlo_bin(), &home)
        .arg("__install")
        .env("JLO_HOME", &jlo_home)
        .output()
        .unwrap();
    assert_eq!(direct.status.code(), Some(1), "{}", printed(&direct));
    assert!(
        printed(&direct).contains("JLO_HOME must not contain control characters"),
        "{}",
        printed(&direct)
    );
    assert!(!jlo_home.exists(), "the refused verb created {jlo_home:?}");

    // Non-ASCII is not a control character, and a shell under the C locale
    // must not mistake it for one: `curl | sh` often runs with no locale set.
    let dir = tempfile::tempdir().unwrap();
    let tarball = release_tarball(dir.path());
    let stubbin = stub_curl(dir.path(), &tarball, Checksum::Correct);
    stub_gnu_tar(&stubbin);
    let home = dir.path().join("home");
    let jlo_home = home.join("J\u{f6}rg").join(".jlo");
    std::fs::create_dir_all(&home).unwrap();
    let path = format!("{}:{HERMETIC_PATH}", stubbin.display());
    let out = hermetic("/bin/sh", &home)
        .arg(manifest().join("install.sh"))
        .env("PATH", path)
        .env("LC_ALL", "C")
        .env("JLO_HOME", &jlo_home)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", printed(&out));
    assert!(
        jlo_home.join("jlo.sh").is_file(),
        "the installer did not write jlo.sh under {jlo_home:?}"
    );
}

// ---------------------------------------------------------------------------
// Staged publication
// ---------------------------------------------------------------------------

/// The installer must not write `bin/jlo-bin` itself.
///
/// It unpacks into a staging directory beside the destination and hands the
/// staged binary to the install verb, which renames it into place. On Linux
/// overwriting a running executable is `ETXTBSY`.
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
        hermetic(&staged, &home)
            .args(["__install", "--publish-self"])
            .env("JLO_HOME", &jlo_home),
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
    let version = hermetic(&binary, &home).arg("--version").output().unwrap();
    assert!(
        version.status.success(),
        "{binary:?} is not runnable after the publish: {}",
        String::from_utf8_lossy(&version.stderr)
    );
}

/// `install-local.sh` and a re-run both run the binary that is *already*
/// published. Renaming it onto itself would be a no-op at best, so the verb
/// has to recognise that case rather than trip over it.
#[test]
fn publish_self_is_a_no_op_when_the_binary_is_already_in_place() {
    let (dir, _) = install(None);
    let home = dir.path().join("home");
    let jlo_home = home.join(".jlo");
    let binary = jlo_home.join("bin").join("jlo-bin");

    let out = run_staged(
        hermetic(&binary, &home)
            .args(["__install", "--publish-self"])
            .env("JLO_HOME", &jlo_home),
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
    let path = format!("{}:{HERMETIC_PATH}", dir.join("stubbin").display());
    hermetic("/bin/sh", home)
        .arg(manifest().join("install.sh"))
        .env("PATH", path)
        .output()
        .unwrap()
}

/// A refused publish is cleaned up by the process that was refused: the
/// installer `exec`d it and has no line left to run. The trigger is a rejected
/// option, which is read *after* the staging guard is armed - otherwise every
/// such refusal leaves a copy of J'Lo in `bin/`.
#[test]
fn a_rejected_option_leaves_the_binary_and_no_staging() {
    let (dir, _) = install(None);
    let home = dir.path().join("home");
    let jlo_home = home.join(".jlo");
    let binary = jlo_home.join("bin").join("jlo-bin");
    let sentinel = b"the installed binary\n";
    std::fs::write(&binary, sentinel).unwrap();

    let stage = jlo_home.join("bin").join(".jlo-install-test");
    std::fs::create_dir_all(&stage).unwrap();
    std::fs::copy(jlo_bin(), stage.join("jlo-bin")).unwrap();

    let out = run_staged(
        hermetic(stage.join("jlo-bin"), &home)
            .args(["__install", "--publish-self", "--no-such-option"])
            .env("JLO_HOME", &jlo_home),
    );
    assert_eq!(out.status.code(), Some(1), "{}", printed(&out));
    assert!(
        printed(&out).contains("unknown option"),
        "{}",
        printed(&out)
    );
    assert!(
        std::fs::read(&binary).unwrap() == sentinel,
        "{binary:?} was replaced by a refused publish"
    );
    assert!(!stage.exists(), "the refused publish left {stage:?} behind");
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

    let path = format!("{}:{HERMETIC_PATH}", stubbin.display());
    let out = hermetic("/bin/sh", &home)
        .arg(manifest().join("install.sh"))
        .env("PATH", path)
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
    let home = tempfile::tempdir().unwrap();
    let out = hermetic("uname", home.path()).arg("-m").output().unwrap();
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
        hermetic(impostor.join("jlo-bin"), &home)
            .args(["__install", "--publish-self"])
            .env("JLO_HOME", &elsewhere),
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
        hermetic(stage.join("jlo-bin"), &home)
            .args(["__install", "--publish-self"])
            .env("JLO_HOME", &jlo_home),
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

/// An installer killed after unpacking leaves `.jlo-install-<pid>`, and every
/// run has a new pid, so only an age-based sweep in the one publisher ever
/// removes it. A fresh one may belong to an install running right now.
#[test]
fn the_install_verb_sweeps_abandoned_staging_only() {
    let (dir, _) = install(None);
    let home = dir.path().join("home");
    let bin = home.join(".jlo").join("bin");

    let abandoned = bin.join(".jlo-install-4242");
    std::fs::create_dir_all(&abandoned).unwrap();
    std::fs::write(abandoned.join("jlo-bin"), "").unwrap();
    let two_hours_ago = std::time::SystemTime::now() - std::time::Duration::from_hours(2);
    std::fs::File::open(&abandoned)
        .unwrap()
        .set_modified(two_hours_ago)
        .unwrap();
    let fresh = bin.join(".jlo-install-in-progress");
    std::fs::create_dir_all(&fresh).unwrap();

    let out = reinstall_over(&home);
    assert!(out.status.success(), "{}", printed(&out));
    assert!(
        !abandoned.exists(),
        "an abandoned staging directory survived"
    );
    assert!(fresh.exists(), "a fresh staging directory was swept");
}

/// An installer suspended for over an hour between unpacking and its `exec`
/// (a laptop asleep, a `SIGSTOP`) hands over from a staging directory the
/// sweep counts as abandoned. Sweeping it would delete the running binary
/// before it is published.
#[test]
fn the_sweep_spares_the_staging_directory_the_running_binary_came_from() {
    let (dir, _) = install(None);
    let home = dir.path().join("home");
    let jlo_home = home.join(".jlo");
    let bin = jlo_home.join("bin");
    let binary = bin.join("jlo-bin");
    let two_hours_ago = std::time::SystemTime::now() - std::time::Duration::from_hours(2);

    let stage = bin.join(".jlo-install-test");
    std::fs::create_dir_all(&stage).unwrap();
    let staged = stage.join("jlo-bin");
    std::fs::copy(&binary, &staged).unwrap();
    let staged_inode = std::fs::metadata(&staged).unwrap().ino();
    std::fs::File::open(&stage)
        .unwrap()
        .set_modified(two_hours_ago)
        .unwrap();

    let abandoned = bin.join(".jlo-install-4242");
    std::fs::create_dir_all(&abandoned).unwrap();
    std::fs::File::open(&abandoned)
        .unwrap()
        .set_modified(two_hours_ago)
        .unwrap();

    let out = run_staged(
        hermetic(&staged, &home)
            .args(["__install", "--publish-self"])
            .env("JLO_HOME", &jlo_home),
    );
    assert!(
        out.status.success(),
        "__install --publish-self failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        std::fs::metadata(&binary).unwrap().ino(),
        staged_inode,
        "{binary:?} is not the file that was staged"
    );
    assert!(
        !abandoned.exists(),
        "an abandoned staging directory survived"
    );
    assert!(
        !stage.exists(),
        "the staging directory the binary came from was not cleared"
    );
}

/// The installer's channel of the same rule: `jlo use 21` from a terminal
/// without the function fails, naming the portable `$HOME/.jlo` line the
/// installer itself prints. A binary no install wrote a `jlo.sh` for gets the
/// refusal without a line.
#[test]
fn env_on_a_terminal_without_the_function_names_the_profile_line() {
    let home = tempfile::tempdir().unwrap();
    let home = home.path();
    let bin = home.join(".jlo/bin");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::copy(jlo_bin(), bin.join("jlo-bin")).unwrap();
    let out = hermetic(bin.join("jlo-bin"), home)
        .arg("__install")
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");

    let out = common::on_a_terminal(home, &bin.join("jlo-bin"), &["use", "21"]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(1), "{text}");
    assert!(
        text.lines()
            .any(|l| l.contains(r#"[ -s "$HOME/.jlo/jlo.sh" ] && . "$HOME/.jlo/jlo.sh""#)),
        "the profile line is missing: {text}"
    );

    // A custom JLO_HOME the shell does not export: found from the binary.
    let custom = home.join("tools/jlo");
    std::fs::create_dir_all(custom.join("bin")).unwrap();
    std::fs::copy(jlo_bin(), custom.join("bin/jlo-bin")).unwrap();
    let out = hermetic(custom.join("bin/jlo-bin"), home)
        .arg("__install")
        .env("JLO_HOME", &custom)
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let out = common::on_a_terminal(home, &custom.join("bin/jlo-bin"), &["use", "21"]);
    let text = String::from_utf8_lossy(&out.stdout);
    let jlo_sh = custom.canonicalize().unwrap().join("jlo.sh");
    assert!(
        text.contains(&format!("[ -s '{0}' ] && . '{0}'", jlo_sh.display())),
        "the custom home's line is missing: {text}"
    );

    let out = common::on_a_terminal(home, &jlo_bin(), &["use", "21"]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(1), "{text}");
    assert!(!text.contains("jlo.sh"), "a build names a line: {text}");
}
