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

use common::{INTERPRETERS, chmod, hermetic, jlo_bin, shells, skip_missing, squote};
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
/// `SHELL` is pinned to `shell` so the profile the installer names is
/// deterministic - it reads the *login* shell from there, which is the right
/// signal for a file the user will edit, and the wrong one for the dialect
/// dispatch.
fn run_installer(
    jlo_home: Option<&str>,
    checksum: Checksum,
    shell: &str,
) -> (tempfile::TempDir, Output) {
    run_installer_over(&[], jlo_home, checksum, shell)
}

/// [`run_installer`] over a home that already holds `dotfiles`, as a real
/// user's does: which profile file the installer names depends on which ones
/// exist when it runs.
fn run_installer_over(
    dotfiles: &[(&str, &str)],
    jlo_home: Option<&str>,
    checksum: Checksum,
    shell: &str,
) -> (tempfile::TempDir, Output) {
    let dir = tempfile::tempdir().unwrap();
    let tarball = release_tarball(dir.path());
    let stubbin = stub_curl(dir.path(), &tarball, checksum);
    // Every install test runs against it: install.sh must never hand tar a
    // path it could reinterpret, whatever the test is otherwise about.
    stub_gnu_tar(&stubbin);
    let home = dir.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    for (name, body) in dotfiles {
        std::fs::write(home.join(name), body).unwrap();
    }

    let path = format!(
        "{}:{}",
        stubbin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let mut cmd = Command::new("/bin/sh");
    cmd.arg(manifest().join("install.sh"))
        .env("HOME", &home)
        .env("PATH", path)
        .env("SHELL", shell)
        .env_remove("JLO_HOME");
    if let Some(h) = jlo_home {
        cmd.env("JLO_HOME", h.replace("$HOME", &home.display().to_string()));
    }
    let out = cmd.output().unwrap();
    (dir, out)
}

fn install(jlo_home: Option<&str>) -> (tempfile::TempDir, Output) {
    let (dir, out) = run_installer(jlo_home, Checksum::Correct, "/bin/zsh");
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
        "the heredoc does not end with a bare EOF: {block:#?}"
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
    // The PATH hint is a second heredoc when it fires; the activation block
    // itself must still be printed exactly once.
    assert_eq!(
        printed
            .lines()
            .map(visible)
            .filter(|l| l.starts_with("cat >>") && !l.contains(".zshenv"))
            .count(),
        1,
        "expected exactly one activation heredoc:\n{printed}"
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

/// The heredoc as the user would select it: from `cat >>` to the closing `EOF`,
/// with the escape bytes stripped so the lines are the ones that reach the
/// profile.
fn heredoc_block(printed: &str) -> Option<Vec<String>> {
    let lines: Vec<String> = printed.lines().map(visible).collect();
    let start = lines.iter().position(|l| l.starts_with("cat >>"))?;
    let quoted = lines[start].rsplit_once("<<'")?.1;
    let delimiter = quoted.strip_suffix('\'')?.to_string();
    let end = start + 1 + lines[start + 1..].iter().position(|l| *l == delimiter)?;
    Some(lines[start..=end].to_vec())
}

/// The heredoc that puts `~/.local/bin` on PATH, as the user would select it.
/// Told apart from the activation block by its body, not by position.
fn path_block(printed: &str) -> Option<Vec<String>> {
    let lines: Vec<String> = printed.lines().map(visible).collect();
    lines
        .iter()
        .enumerate()
        .filter(|(_, l)| l.starts_with("cat >>"))
        .find_map(|(start, opener)| {
            let delimiter = opener.rsplit_once("<<'")?.1.strip_suffix('\'')?.to_string();
            let end = start + 1 + lines[start + 1..].iter().position(|l| *l == delimiter)?;
            let block = lines[start..=end].to_vec();
            block
                .iter()
                .any(|l| l.contains("$HOME/.local/bin"))
                .then_some(block)
        })
}

/// The PATH hint exists for shells that never read the interactive profile.
/// Its block, run as pasted - twice, as nested shells would - has to put `jlo`
/// on PATH for a clean `zsh -c`, exactly once, and it must *append*: the old
/// hint prepended, so a revert to that shape is plausible and has to fail
/// this test - hence checking that `~/.local/bin` lands last, not merely that
/// it is present.
#[test]
fn the_printed_path_block_reaches_a_non_interactive_zsh() {
    if skip_missing(
        "the_printed_path_block_reaches_a_non_interactive_zsh",
        "zsh",
    ) {
        return;
    }
    let (dir, out) = install(None);
    let home = dir.path().join("home");
    let block = path_block(&printed(&out)).expect("installer printed no PATH heredoc");

    for _ in 0..2 {
        let ran = Command::new("/bin/sh")
            .arg("-c")
            .arg(block.join("\n"))
            .env("HOME", &home)
            .output()
            .unwrap();
        assert!(
            ran.status.success(),
            "the printed PATH block did not run: {}",
            String::from_utf8_lossy(&ran.stderr)
        );
    }

    let probe = Command::new("zsh")
        .args(["-c", r#"command -v jlo; print -r -- "PATH=$PATH""#])
        .env_clear()
        .env("HOME", &home)
        .env("PATH", "/usr/bin:/bin")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&probe.stdout);
    let local_bin = home.join(".local").join("bin");
    assert!(
        stdout
            .lines()
            .any(|l| l == local_bin.join("jlo").display().to_string()),
        "a clean zsh -c does not find jlo: {stdout}"
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

/// Pastes each printed block, in order, as one script - exactly the shape a
/// user copying them one after another produces.
fn paste_blocks(home: &Path, blocks: &[Vec<String>]) {
    let script = blocks
        .iter()
        .map(|block| block.join("\n"))
        .collect::<Vec<_>>()
        .join("\n");
    let ran = Command::new("/bin/sh")
        .arg("-c")
        .arg(script)
        .env("HOME", home)
        .output()
        .unwrap();
    assert!(
        ran.status.success(),
        "pasting the printed blocks failed: {}",
        String::from_utf8_lossy(&ran.stderr)
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
    let out = Command::new("/bin/bash")
        .args([
            "-lc",
            &format!(
                r#"printf 'jlo=[%s]\n' "$(type -P jlo)"; printf 'kind=[%s]\n' "$(type -t jlo)"; printf 'marker=[%s]\n' "${{{marker_var}-}}""#
            ),
        ])
        .env_clear()
        .env("HOME", home)
        .env("PATH", "/usr/bin:/bin")
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

/// The only end-to-end coverage of the *bash* half of the two printed blocks.
///
/// `bash -l` reads only the first existing of `~/.bash_profile`,
/// `~/.bash_login` and `~/.profile`. On macOS, where Terminal.app starts
/// login shells and the activation block therefore targets one of those
/// files, a wrong choice fails silently: a file the login bash never reads,
/// such as `~/.bashrc`, leaves it without the `jlo` function (both cases); a
/// PATH line in `~/.profile` behind a new `~/.bash_profile` is never read
/// (case 1); creating `~/.bash_profile` in front of an existing `~/.profile`
/// switches the user's file off (case 2).
///
/// On Linux the activation block targets `~/.bashrc`, which is no login file,
/// so it can shadow nothing - and a login bash does not read it, so only
/// macOS is expected to have the `jlo` function after the login files ran.
#[test]
fn a_login_bash_finds_jlo_after_pasting_both_printed_blocks() {
    if skip_missing(
        "a_login_bash_finds_jlo_after_pasting_both_printed_blocks",
        "/bin/bash",
    ) {
        return;
    }

    // Case 1: a completely fresh HOME, nothing but what the installer itself
    // is about to write.
    let (dir, out) = run_installer(None, Checksum::Correct, "/bin/bash");
    assert!(out.status.success(), "install.sh failed: {}", printed(&out));
    let home = dir.path().join("home");
    let printed_out = printed(&out);
    let activation = heredoc_block(&printed_out).expect("installer printed no activation heredoc");
    let path_hint = path_block(&printed_out).expect("installer printed no PATH heredoc");
    paste_blocks(&home, &[activation, path_hint]);

    let local_bin_jlo = home.join(".local").join("bin").join("jlo");
    let probe = bash_login_probe(&home, "JLO_TEST_MARKER");
    assert_eq!(
        probe.jlo.as_deref(),
        Some(local_bin_jlo.display().to_string().as_str()),
        "a login bash does not find jlo after pasting both printed blocks: {probe:?}"
    );
    if cfg!(target_os = "macos") {
        assert_eq!(
            probe.kind.as_deref(),
            Some("function"),
            "a login bash did not load J'Lo from the printed activation block: {probe:?}"
        );
    }

    // Case 2: the user's login setup lives in ~/.profile, there before J'Lo
    // is, exporting a marker only they put there.
    let (dir2, out2) = run_installer_over(
        &[(".profile", "export JLO_TEST_MARKER=1\n")],
        None,
        Checksum::Correct,
        "/bin/bash",
    );
    assert!(
        out2.status.success(),
        "install.sh failed: {}",
        printed(&out2)
    );
    let home2 = dir2.path().join("home");
    let printed_out2 = printed(&out2);
    let activation2 =
        heredoc_block(&printed_out2).expect("installer printed no activation heredoc");
    let path_hint2 = path_block(&printed_out2).expect("installer printed no PATH heredoc");
    paste_blocks(&home2, &[activation2, path_hint2]);

    let local_bin_jlo2 = home2.join(".local").join("bin").join("jlo");
    let probe2 = bash_login_probe(&home2, "JLO_TEST_MARKER");
    assert_eq!(
        probe2.jlo.as_deref(),
        Some(local_bin_jlo2.display().to_string().as_str()),
        "a login bash with a pre-existing ~/.profile does not find jlo: {probe2:?}"
    );
    assert_eq!(
        probe2.marker.as_deref(),
        Some("1"),
        "pasting the printed blocks switched off the user's existing ~/.profile: {probe2:?}"
    );
    if cfg!(target_os = "macos") {
        assert_eq!(
            probe2.kind.as_deref(),
            Some("function"),
            "a login bash did not load J'Lo from the printed activation block: {probe2:?}"
        );
    }
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
    assert_no_indented_commands(&printed(&out), 4, "fresh install");

    let coloured = hermetic(home.join(".jlo").join("bin").join("jlo-bin"), &home)
        .arg("__install")
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
    // The short form's one source line, and the PATH block's `cat >>` and
    // `EOF`.
    assert_no_indented_commands(&painted, 3, "coloured re-install");
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
    let (dir, out) = run_installer(None, Checksum::Correct, "/bin/zsh");
    assert!(out.status.success());
    drop(dir);
    assert!(
        printed(&out).contains(">> ~/.zshrc"),
        "installer did not name the zsh profile: {}",
        printed(&out)
    );
}

/// A reinstall cannot know whether the profile sources jlo.sh - a text scan
/// cannot prove a line runs - so it prints the same short form whatever the
/// profile holds: one footnote and the one line, never the first-install block.
#[test]
fn a_reinstall_prints_the_short_form() {
    let (dir, first) = install(None);
    let home = dir.path().join("home");
    assert!(
        printed(&first).contains("To activate"),
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
        assert!(
            !printed.contains("To activate") && !printed.contains(">> ~/.zshrc"),
            "{profile:?}: the upgrade repeated the first-install block: {printed}"
        );
    }
}

/// Runs the install verb again over an existing `$JLO_HOME`, the way a second
/// `install.sh` run would once the tarball is unpacked.
fn reinstall_over(home: &Path) -> Output {
    hermetic(home.join(".jlo").join("bin").join("jlo-bin"), home)
        .arg("__install")
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

    for name in ["jlo.sh", "autoload.sh", "completions.sh"] {
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
    let entry = squote(home.join(".jlo").join("completions.sh"));
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
        "bin/jlo-init.sh",
        "bin/jlo-autoload.sh",
        "bin/jlo-completions.zsh",
        "completions/jlo.bash",
        "completions/_jlo",
        // Kept, not unlinked: a J'Lo 0.5.0 or older publisher still races an
        // unlink against the next `open`.
        ".selfupdate.lock",
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
// The generated entry files under the running shell
// ---------------------------------------------------------------------------

/// Sources `jlo.sh` and then `autoload.sh` under `sh`, after `prologue`, and
/// runs `probe`.
fn source_autoload(sh: &str, home: &Path, prologue: &str, probe: &str) -> Output {
    let entry = squote(home.join(".jlo").join("jlo.sh"));
    let autoload = squote(home.join(".jlo").join("autoload.sh"));
    Command::new(sh)
        .arg("-c")
        .arg(format!("{prologue}\n. {entry}\n. {autoload}\n{probe}\n"))
        .current_dir(home)
        .env("HOME", home)
        .env_remove("JLO_HOME")
        .env_remove("PROMPT_COMMAND")
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
        let out = Command::new(sh)
            .arg("-c")
            .arg(format!(
                "set -eu\n. {}\njlo --version\n",
                squote(home.join(".jlo").join("jlo.sh"))
            ))
            .env("HOME", &home)
            .env_remove("JLO_HOME")
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
    let (dir, out) = run_installer(None, Checksum::Wrong, "/bin/zsh");
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
    let (dir, out) = run_installer(None, Checksum::Missing, "/bin/zsh");
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
    std::fs::remove_file(jlo.join("bin").join("jlo-init.sh")).unwrap();

    // Any command at all, and one that needs no network.
    let out = hermetic(jlo.join("bin").join("jlo-bin"), &home)
        .arg("--version")
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
        jlo.join("bin").join("jlo-init.sh").is_file(),
        "the wrapper was not regenerated"
    );
    assert!(
        std::fs::read_to_string(&receipt)
            .unwrap()
            .contains(&format!("\"version\": \"{}\"", env!("CARGO_PKG_VERSION"))),
        "the receipt still disagrees with the binary"
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
        sources.push_str(&squote(jlo.join(name)));
        sources.push('\n');
    }
    let reload = hermetic(jlo.join("bin").join("jlo-bin"), home)
        .args(["__install", "--reload"])
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
                let out = Command::new(sh)
                    .arg("-e")
                    .arg("-c")
                    .arg(format!(
                        "jlo() {{ :; }}\n. {}\n\
                         echo \"alive markers=[${{_JLO_AUTOLOAD-}}${{_JLO_COMPLETIONS-}}]\"",
                        squote(jlo.join(stub))
                    ))
                    .env("HOME", &home)
                    .env_remove("JLO_HOME")
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
/// 0.2.0 and 0.3.0 are the only tags there are, and both print a profile block
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
        let out = Command::new(sh)
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

/// A newline in `JLO_HOME` would end the pasted heredoc, or a comment in a
/// generated file, early, so it is refused. The installer checks before its
/// first `mkdir`: the binary would refuse too, but only after the installer
/// had created `$JLO_HOME` and unpacked a staged copy into it, which a refusal
/// at that point leaves behind.
#[test]
fn an_installer_refuses_a_jlo_home_with_a_newline() {
    let (dir, out) = run_installer(Some("$HOME/jlo\nhome"), Checksum::Correct, "/bin/zsh");
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
        .env("SHELL", "/bin/zsh")
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
        hermetic(&staged, &home)
            .args(["__install", "--publish-self"])
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
    let version = hermetic(&binary, &home).arg("--version").output().unwrap();
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
        hermetic(&binary, &home)
            .args(["__install", "--publish-self"])
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
        hermetic(impostor.join("jlo-bin"), &home)
            .args(["__install", "--publish-self"])
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
        hermetic(stage.join("jlo-bin"), &home)
            .args(["__install", "--publish-self"])
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
