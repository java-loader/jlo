//! The shell side: the `PATH` algebra behind `jlo env`, the quoting that makes
//! an `export` line safe to source, the wrapper's payload, and `jlo exec`.

use crate::ui;
use anyhow::{Context, anyhow, bail};
use std::env;
use std::path::{Path, PathBuf};
use std::process::exit;

/// Quote a value so the shell assigns it rather than interpreting it.
///
/// stdout is evaluated, so a `$`, backtick, backslash or double quote would
/// be expanded - or executed - instead of stored. `PATH` is the sharp case: it
/// is echoed back from the caller's environment, so a `$(...)` in it would run
/// in the user's shell. Anything else printed to stdout must come through here
/// too.
///
/// A single quote is spliced in as `'\''`: close, escaped quote, reopen.
pub(crate) fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

/// The argv prefix by which the `jlo` shell function says it will evaluate
/// stdout. An older binary rejects it as an unknown subcommand before doing
/// any work, so a newer wrapper over it evaluates nothing. Its meaning is a
/// cross-version contract: a new payload format needs a new prefix.
pub(crate) const WRAPPED: &str = "__wrapped";

/// The last line of every payload; the wrapper evaluates only output ending
/// in it, which help text, an older binary's output and a cut-short payload
/// lack. Data cannot forge it: [`shell_quote`] spells every `'` as `'\''`.
/// Never changed: an older wrapper would print, not evaluate, a payload whose
/// binary had already deleted the live JDK.
const END: &str = "# jlo'end";

/// As a [`Payload`] for the wrapper, one statement per line for anyone else.
pub(crate) fn emit(statements: &[String], wrapped: bool) -> anyhow::Result<()> {
    if !wrapped {
        ui::print_lines(statements.iter().cloned());
        return Ok(());
    }
    Payload::stdout().write(statements)
}

/// The stdout of a *wrapped call*. Owning one is knowing the calling shell
/// follows what is written here, which is what lets `install` and `update`
/// delete the build it is on.
///
/// Written once and whole: the statements joined with `&&`, so the first that
/// fails fails the `eval`; then [`END`], also when there are none, so "nothing
/// to do" is told apart from "cut short"; then flushed. Any write or flush
/// failure is an error: the shell did not apply it.
pub(crate) struct Payload<W: std::io::Write> {
    out: W,
}

impl Payload<std::io::Stdout> {
    pub(crate) fn stdout() -> Self {
        Self {
            out: std::io::stdout(),
        }
    }
}

impl<W: std::io::Write> Payload<W> {
    #[cfg(test)]
    pub(crate) fn new(out: W) -> Self {
        Self { out }
    }

    pub(crate) fn write(mut self, statements: &[String]) -> anyhow::Result<()> {
        write_payload(&mut self.out, statements).context("could not write to stdout")
    }

    /// The [`export_lines`] `env` would print, or with `None` nothing to do.
    pub(crate) fn follow(
        self,
        java_home: Option<&Path>,
        active: Option<&Path>,
        jdk_base: &Path,
    ) -> anyhow::Result<()> {
        let exports = match java_home {
            Some(java_home) => export_lines(java_home, active, &current_path()?, jdk_base)?,
            None => Vec::new(),
        };
        self.write(&exports)
    }
}

fn write_payload(out: &mut impl std::io::Write, statements: &[String]) -> std::io::Result<()> {
    for (i, statement) in statements.iter().enumerate() {
        let joiner = if i + 1 < statements.len() { " &&" } else { "" };
        writeln!(out, "{statement}{joiner}")?;
    }
    writeln!(out, "{END}")?;
    out.flush()
}

/// `JAVA_HOME` when it differs from `active`, `PATH` when the JDK's `bin` is
/// not already where it belongs. Current values passed in, so the decision is
/// testable without mutating the process environment.
pub(crate) fn export_lines(
    java_home: &Path,
    active: Option<&Path>,
    current_path: &str,
    jdk_base: &Path,
) -> anyhow::Result<Vec<String>> {
    let mut exports = Vec::new();

    let java_home_str = path_str(java_home)?;
    // Compared as strings, not as `Path`s: `Path` equality ignores a trailing
    // slash, and a `JAVA_HOME` spelled differently is re-exported.
    if active.is_none_or(|current| current.as_os_str() != java_home_str) {
        exports.push(format!("export JAVA_HOME={}", shell_quote(java_home_str)));
    }

    let java_bin = java_home.join("bin");
    if let Some(updated_path) = update_path(path_str(&java_bin)?, current_path, jdk_base)? {
        exports.push(format!("export PATH={}", shell_quote(&updated_path)));
    }

    Ok(exports)
}

/// Every path jlo hands out goes through here, not `to_string_lossy`, which
/// silently answers with a path that does not exist.
pub(crate) fn path_str(path: &Path) -> anyhow::Result<&str> {
    path.to_str()
        .with_context(|| format!("path is not valid UTF-8: {}", path.display()))
}

/// Prepend `java_path`, dropping entries under `jdk_base`; `None` when that
/// changes nothing. `jdk_base` must be the JDK store, the only tree whose PATH
/// entries jlo owns: a broader one would strip the user's own entries.
fn update_path(
    java_path: &str,
    current_path: &str,
    jdk_base: &Path,
) -> anyhow::Result<Option<String>> {
    let new_path = prepend(java_path, current_path, |p| !p.starts_with(jdk_base))?;
    Ok((new_path != current_path).then_some(new_path))
}

fn prepend(
    java_path: &str,
    current_path: &str,
    keep: impl Fn(&Path) -> bool,
) -> anyhow::Result<String> {
    // An empty `PATH` is *no* entries: `split_paths("")` yields one empty
    // entry, and `<jdk>/bin:` would search the working directory.
    //
    // Skipping the split, not the join: `join_paths` also refuses a
    // `java_path` containing a `:`, which must apply either way.
    let inherited: Vec<PathBuf> = if current_path.is_empty() {
        Vec::new()
    } else {
        env::split_paths(current_path).collect()
    };

    let entries =
        std::iter::once(PathBuf::from(java_path)).chain(inherited.into_iter().filter(|p| keep(p)));

    Ok(env::join_paths(entries)
        .context("could not join PATH components")?
        .to_str()
        .context("PATH contains non-UTF-8 characters")?
        .to_string())
}

/// The caller's `PATH`, or an error when it is not valid UTF-8. Not
/// `unwrap_or_default()`: a non-UTF-8 `PATH` read as empty would have
/// `jlo env` replace the user's whole `PATH`. Unset means empty; undecodable
/// does not.
pub(crate) fn current_path() -> anyhow::Result<String> {
    classify_path(env::var("PATH"))
}

/// Takes the lookup's result, so it is testable without mutating the process
/// environment.
fn classify_path(looked_up: Result<String, env::VarError>) -> anyhow::Result<String> {
    match looked_up {
        Ok(path) => Ok(path),
        Err(env::VarError::NotPresent) => Ok(String::new()),
        Err(env::VarError::NotUnicode(_)) => Err(anyhow!(
            "PATH is not valid UTF-8, so jlo cannot rewrite it without losing entries"
        )),
    }
}

fn child_path(java_home: &Path) -> anyhow::Result<String> {
    let java_bin = java_home.join("bin");
    prepend(path_str(&java_bin)?, &current_path()?, |_| true)
}

/// `[<version>] -- <command>...`
pub(crate) fn parse_exec_args(args: &[String]) -> anyhow::Result<(Option<String>, Vec<String>)> {
    let sep = args
        .iter()
        .position(|a| a == "--")
        .context("expected '--' before the command, e.g. jlo exec 21 -- java -version")?;

    let version = match &args[..sep] {
        [] => None,
        [v] => Some(v.clone()),
        _ => bail!("only one version may be given before '--'"),
    };

    let command = args[sep + 1..].to_vec();
    if command.is_empty() {
        bail!("no command given after '--'");
    }

    Ok((version, command))
}

/// A real `execvp`, so the child's exit code and signals propagate
/// transparently.
pub(crate) fn exec_command(java_home: &Path, command: &[String]) -> ! {
    use std::os::unix::process::CommandExt;
    use std::process::Command;

    let (program, args) = command
        .split_first()
        .expect("command is non-empty (checked in parse_exec_args)");

    let new_path = child_path(java_home).unwrap_or_else(|e| {
        ui::error!("{e:#}");
        exit(1);
    });

    // `exec` only returns if it failed to launch the program.
    let err = Command::new(program)
        .args(args)
        .env("JAVA_HOME", java_home)
        .env("PATH", new_path)
        .exec();

    ui::error!("could not execute '{program}': {err}");
    exit(exec_failure_code(err.kind()));
}

/// Shell convention: 126 for a command that exists but cannot run, else 127.
fn exec_failure_code(kind: std::io::ErrorKind) -> i32 {
    match kind {
        std::io::ErrorKind::PermissionDenied => 126,
        _ => 127,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn owned(items: &[&str]) -> Vec<String> {
        items.iter().map(std::string::ToString::to_string).collect()
    }

    /// A plain path needs no escaping, but is still quoted: an unquoted value
    /// would word-split on a space.
    #[test]
    fn shell_quote_wraps_a_plain_value() {
        assert_eq!(shell_quote("/opt/jdk-21"), "'/opt/jdk-21'");
        assert_eq!(shell_quote("/opt/My JDK"), "'/opt/My JDK'");
    }

    /// The characters that stay live inside double quotes - which is what this
    /// function replaced - must all come back out verbatim.
    #[test]
    fn shell_quote_neutralises_expansion_characters() {
        for raw in [
            "/opt/$(touch pwned)",
            "/opt/`touch pwned`",
            "/opt/${HOME}",
            "/opt/a\\b",
            "/opt/a\"b",
        ] {
            let quoted = shell_quote(raw);
            assert_eq!(
                strip_single_quotes(&quoted),
                raw,
                "round trip failed for {raw:?} (quoted as {quoted:?})"
            );
        }
    }

    /// A single quote cannot appear inside single quotes, so it is spliced in
    /// as `'\''`. This is the case a naive implementation gets wrong, and
    /// getting it wrong is an injection, not a cosmetic bug.
    #[test]
    fn shell_quote_splices_embedded_single_quotes() {
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
        assert_eq!(
            strip_single_quotes(&shell_quote("/opt/'; touch pwned; '")),
            "/opt/'; touch pwned; '"
        );
    }

    #[test]
    fn shell_quote_handles_an_empty_value() {
        assert_eq!(shell_quote(""), "''");
    }

    /// The wrapper evaluates output ending in the marker, so a payload cut
    /// short inside a value must not end in it - which holds only while no
    /// quoted value can contain it.
    #[test]
    fn a_quoted_value_cannot_contain_the_end_marker() {
        for raw in [END, "/opt/# jlo'end", "# jlo'end/bin"] {
            assert!(!shell_quote(raw).contains(END), "{raw}");
        }
    }

    fn payload(statements: &[&str]) -> String {
        let mut out = Vec::new();
        write_payload(&mut out, &owned(statements)).unwrap();
        String::from_utf8(out).unwrap()
    }

    /// `&&`, so an early statement that fails stops the rest and fails the
    /// wrapper's `eval`; the marker even with nothing to say, so "nothing to
    /// do" is not mistaken for "cut short".
    #[test]
    fn a_payload_is_and_joined_and_always_ends_with_the_marker() {
        assert_eq!(payload(&["a", "b", "c"]), "a &&\nb &&\nc\n# jlo'end\n");
        assert_eq!(payload(&["a"]), "a\n# jlo'end\n");
        assert_eq!(payload(&[]), "# jlo'end\n");
    }

    /// Fails the `fail_at`-th call to `write` (counting from 0) and, when
    /// `flush_fails`, the flush; every other call succeeds. A fault that does
    /// not repeat, so a write whose error is dropped is not rescued by the
    /// next one failing too.
    struct Faulty {
        calls: usize,
        fail_at: Option<usize>,
        flush_fails: bool,
    }

    impl std::io::Write for Faulty {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            let call = self.calls;
            self.calls += 1;
            if self.fail_at == Some(call) {
                return Err(std::io::ErrorKind::BrokenPipe.into());
            }
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            if self.flush_fails {
                return Err(std::io::ErrorKind::BrokenPipe.into());
            }
            Ok(())
        }
    }

    /// Unlike `print_lines`, which treats a closed pipe as an ending, a
    /// payload that did not arrive whole is a failure - wherever the write
    /// fails, the marker included, and when only the flush does.
    #[test]
    fn a_payload_that_does_not_arrive_whole_fails() {
        let statements = owned(&["a", "b"]);
        let faulty = |fail_at, flush_fails| Faulty {
            calls: 0,
            fail_at,
            flush_fails,
        };

        let mut clean = faulty(None, false);
        write_payload(&mut clean, &statements).unwrap();
        assert!(clean.calls >= 3, "one write per line at least");

        for fail_at in 0..clean.calls {
            let err = write_payload(&mut faulty(Some(fail_at), false), &statements)
                .expect_err("a write failed");
            assert_eq!(
                err.kind(),
                std::io::ErrorKind::BrokenPipe,
                "write {fail_at}"
            );
        }
        let err = write_payload(&mut faulty(None, true), &statements).expect_err("flush failed");
        assert_eq!(err.kind(), std::io::ErrorKind::BrokenPipe);
    }

    /// Undo `shell_quote` the way a shell would, so the tests above assert a
    /// real round trip rather than a hand-copied expected string.
    fn strip_single_quotes(quoted: &str) -> String {
        let body = quoted
            .strip_prefix('\'')
            .and_then(|q| q.strip_suffix('\''))
            .expect("shell_quote must wrap its output in single quotes");
        body.replace(r"'\''", "'")
    }

    /// At most one version before the first `--`, a command after it, and
    /// every later `--` belongs to the command.
    #[test]
    fn parse_exec_args_splits_at_the_first_separator() {
        type Parsed<'a> = Option<(Option<&'a str>, &'a [&'a str])>;
        let cases: [(&[&str], Parsed); 8] = [
            (
                &["21", "--", "java", "-version"],
                Some((Some("21"), &["java", "-version"])),
            ),
            (
                &["--", "java", "-version"],
                Some((None, &["java", "-version"])),
            ),
            (
                &["21", "--", "sh", "-c", "--", "x"],
                Some((Some("21"), &["sh", "-c", "--", "x"])),
            ),
            (&["21", "java", "-version"], None),
            (&["21", "--"], None),
            (&["21", "25", "--", "java"], None),
            (&[], None),
            (&["--"], None),
        ];
        for (args, expected) in cases {
            let parsed = parse_exec_args(&owned(args)).ok();
            let expected =
                expected.map(|(version, command)| (version.map(String::from), owned(command)));
            assert_eq!(parsed, expected, "{args:?}");
        }
    }

    #[test]
    fn exec_failure_code_distinguishes_not_found_and_not_executable() {
        use std::io::ErrorKind;
        assert_eq!(exec_failure_code(ErrorKind::NotFound), 127);
        assert_eq!(exec_failure_code(ErrorKind::PermissionDenied), 126);
        assert_eq!(exec_failure_code(ErrorKind::Other), 127);
    }

    /// `env::var(..).unwrap_or_default()` used to sit where `current_path`
    /// does, and it maps an undecodable `PATH` to the empty string - which
    /// `update_path` reads as "nothing on PATH". `jlo env` then emitted
    /// `export PATH='<jdk>/bin:'`: the user's whole PATH gone, and a trailing
    /// empty component that makes the shell search the working directory.
    /// Neither the compiler nor clippy sees the difference between the two
    /// `VarError` arms, which is the whole reason this is pinned.
    #[test]
    fn an_empty_path_is_not_the_same_as_an_undecodable_one() {
        // Unset is genuinely empty, and the JDK's bin is the whole answer.
        assert_eq!(prepend("/jdk/21/bin", "", |_| true).unwrap(), "/jdk/21/bin");

        // Undecodable has to stop instead, because there is no correct
        // rewrite of a PATH jlo cannot read.
        let err = classify_path(Err(env::VarError::NotUnicode(std::ffi::OsString::new())))
            .expect_err("an undecodable PATH has no safe rewrite");
        assert!(err.to_string().contains("not valid UTF-8"), "{err}");

        assert_eq!(
            classify_path(Err(env::VarError::NotPresent)).unwrap(),
            String::new()
        );
    }

    #[test]
    fn export_lines_skips_a_java_home_that_is_already_set() {
        let jdk_base = Path::new("/home/u/.jdks");
        let java_home = Path::new("/home/u/.jdks/21.0.12");
        let lines = export_lines(java_home, Some(java_home), "/usr/bin", jdk_base).unwrap();
        assert_eq!(
            lines,
            owned(&["export PATH='/home/u/.jdks/21.0.12/bin:/usr/bin'"])
        );
    }

    /// `Path` equality would call these equal; the shell holds the other
    /// spelling, so it is re-exported. `PATH` already leads with the JDK's
    /// `bin`, so it is not.
    #[test]
    fn export_lines_reexports_a_java_home_spelled_with_a_trailing_slash() {
        let jdk_base = Path::new("/home/u/.jdks");
        let lines = export_lines(
            Path::new("/home/u/.jdks/21.0.12"),
            Some(Path::new("/home/u/.jdks/21.0.12/")),
            "/home/u/.jdks/21.0.12/bin:/usr/bin",
            jdk_base,
        )
        .unwrap();
        assert_eq!(lines, owned(&["export JAVA_HOME='/home/u/.jdks/21.0.12'"]));
    }

    /// `JAVA_HOME` first, then `PATH`, each quoted.
    #[test]
    fn export_lines_exports_both_when_both_differ() {
        let jdk_base = Path::new("/home/u/.jdks");
        let lines = export_lines(
            Path::new("/home/u/.jdks/21.0.12"),
            Some(Path::new("/home/u/.jdks/17.0.13")),
            "/home/u/.jdks/17.0.13/bin:/usr/bin",
            jdk_base,
        )
        .unwrap();
        assert_eq!(
            lines,
            owned(&[
                "export JAVA_HOME='/home/u/.jdks/21.0.12'",
                "export PATH='/home/u/.jdks/21.0.12/bin:/usr/bin'",
            ])
        );
    }

    /// A lossy rendering would export a path that does not exist.
    #[test]
    fn export_lines_refuses_an_undecodable_java_home() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        let java_home = Path::new(OsStr::from_bytes(b"/home/u/.jdks/21\xff"));
        let err = export_lines(java_home, None, "/usr/bin", Path::new("/home/u/.jdks"))
            .expect_err("an undecodable JAVA_HOME has no export");
        let message = err.to_string();
        assert!(message.contains("not valid UTF-8"), "{message}");
        assert!(message.contains("/home/u/.jdks/21"), "{message}");
    }

    #[test]
    fn update_path_inserts_at_front() {
        let jdk_base = Path::new("/home/u/.jdks");
        let result = update_path(
            "/home/u/.jdks/21.0.12/bin",
            "/usr/bin:/usr/local/bin",
            jdk_base,
        )
        .unwrap();
        assert_eq!(
            result.unwrap(),
            "/home/u/.jdks/21.0.12/bin:/usr/bin:/usr/local/bin"
        );
    }

    /// An empty `PATH` must not become `<jdk>/bin:`. The trailing separator
    /// leaves an empty entry, and an empty `PATH` entry is the working
    /// directory - so the shell would search whatever directory the user is
    /// standing in, ahead of nothing at all. The assertion here used to spell
    /// out the wrong answer.
    #[test]
    fn update_path_handles_empty_path() {
        let jdk_base = Path::new("/home/u/.jdks");
        let result = update_path("/home/u/.jdks/21.0.12/bin", "", jdk_base).unwrap();
        assert_eq!(result.unwrap(), "/home/u/.jdks/21.0.12/bin");
    }

    /// A `java_path` carrying a `:` is two `PATH` entries once the shell reads
    /// it back, and the second of them is relative. `join_paths` refuses it -
    /// but only if it is reached, and an empty `PATH` used to return before
    /// the join, so the check applied to some callers and not others.
    #[test]
    fn a_colon_in_the_jdk_path_is_refused_whether_or_not_path_is_set() {
        let jdk_base = Path::new("/home/u/.jdks");
        let hostile = "/home/u/.jdks/21:evil/bin";

        assert!(update_path(hostile, "", jdk_base).is_err(), "empty PATH");
        assert!(
            update_path(hostile, "/usr/bin", jdk_base).is_err(),
            "populated PATH"
        );
    }

    #[test]
    fn update_path_removes_stale_jdk_entries() {
        let jdk_base = Path::new("/home/u/.jdks");
        let result = update_path(
            "/home/u/.jdks/17.0.13/bin",
            "/home/u/.jdks/21.0.12/bin:/usr/bin",
            jdk_base,
        )
        .unwrap();
        assert_eq!(
            result.as_deref(),
            Some("/home/u/.jdks/17.0.13/bin:/usr/bin")
        );
    }

    #[test]
    fn update_path_keeps_unrelated_home_entries() {
        let jdk_base = Path::new("/home/u/.jdks");
        let result = update_path(
            "/home/u/.jdks/17.0.13/bin",
            "/home/u/.cargo/bin:/home/u/bin:/usr/bin",
            jdk_base,
        )
        .unwrap();
        assert_eq!(
            result.as_deref(),
            Some("/home/u/.jdks/17.0.13/bin:/home/u/.cargo/bin:/home/u/bin:/usr/bin")
        );
    }

    #[test]
    fn update_path_is_idempotent_for_the_same_jdk() {
        let jdk_base = Path::new("/home/u/.jdks");
        let result = update_path(
            "/home/u/.jdks/17.0.13/bin",
            "/home/u/.jdks/17.0.13/bin:/usr/bin",
            jdk_base,
        )
        .unwrap();
        assert_eq!(result, None);
    }

    #[test]
    fn update_path_does_not_match_sibling_directories_by_prefix() {
        let jdk_base = Path::new("/home/u/.jdks");
        let result = update_path(
            "/home/u/.jdks/17.0.13/bin",
            "/home/u/.jdks-backup/bin:/usr/bin",
            jdk_base,
        )
        .unwrap();
        assert_eq!(
            result.as_deref(),
            Some("/home/u/.jdks/17.0.13/bin:/home/u/.jdks-backup/bin:/usr/bin")
        );
    }
}
