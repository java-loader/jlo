//! The shell side of what jlo does: the `PATH` algebra behind `jlo env`, the
//! quoting that makes an `export` line safe to source, and the argument
//! plumbing and `execvp` behind `jlo exec`.
//!
//! Split out of dispatch because none of it is dispatch: every function here
//! answers a question about a string or a process, and none of them knows
//! which subcommand asked.

use crate::ui;
use anyhow::{Context, anyhow, bail};
use std::env;
use std::path::{Path, PathBuf};
use std::process::exit;

/// Quote a value so the shell assigns it rather than interpreting it.
///
/// stdout is the environment channel: the `jlo` shell function evaluates what
/// arrives there, so a value carrying `$`, a backtick, a backslash or a double
/// quote would be expanded - or executed - instead of stored. `PATH` is the
/// sharp case: it is echoed back from the caller's own environment, so a
/// `$(...)` anywhere in it would run in the user's shell.
///
/// The hazard belongs to the channel, not to these two variables: anything
/// else this function's callers ever print must go through here too.
///
/// Single quotes suppress every expansion. The one character they cannot hold
/// is a single quote, which is spliced in as `'\''`: close, backslash-escaped
/// quote, reopen.
pub(crate) fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

/// The argv prefix by which the `jlo` shell function says it will evaluate
/// stdout. Intercepted before clap, so it never reaches help, completions or
/// typo suggestions - and an older binary rejects it as an unknown subcommand
/// before doing any work, so a newer wrapper over it evaluates nothing.
pub(crate) const WRAPPED: &str = "__wrapped";

/// The last line of every payload written for the wrapper, which evaluates
/// only output ending in it: help text, an older binary's output and a
/// payload cut short all lack it. Data cannot forge it, because
/// [`shell_quote`] spells every `'` in a value as `'\''` and everything
/// outside quotes is code jlo writes.
const END: &str = "# jlo'end";

/// Write shell statements to stdout.
///
/// For the wrapper they are joined with `&&`, so the first one that fails
/// stops the rest and fails the `eval`, and end with [`END`] - also when there
/// are none, so "nothing to do" is told apart from "cut short". Any write
/// failure is an error: a payload that did not arrive whole is one the shell
/// did not apply. Anyone else gets one statement per line, as always.
pub(crate) fn emit(statements: &[String], wrapped: bool) -> anyhow::Result<()> {
    if !wrapped {
        ui::print_lines(statements.iter().cloned());
        return Ok(());
    }
    write_payload(&mut std::io::stdout().lock(), statements).context("could not write to stdout")
}

fn write_payload(out: &mut impl std::io::Write, statements: &[String]) -> std::io::Result<()> {
    for (i, statement) in statements.iter().enumerate() {
        let joiner = if i + 1 < statements.len() { " &&" } else { "" };
        writeln!(out, "{statement}{joiner}")?;
    }
    writeln!(out, "{END}")?;
    out.flush()
}

/// Prepend `java_path` to `current_path`, dropping any entry already under
/// `jdk_base`, or `None` when that changes nothing. `jdk_base` must be the JDK
/// install directory ([`JdkStore::base`]) — the only tree whose PATH entries
/// J'Lo owns. Passing a broader directory (the home directory, say) would strip
/// unrelated user entries.
pub(crate) fn update_path(
    java_path: &str,
    current_path: &str,
    jdk_base: &Path,
) -> anyhow::Result<Option<String>> {
    let new_path = prepend(java_path, current_path, |p| !p.starts_with(jdk_base))?;
    Ok((new_path != current_path).then_some(new_path))
}

/// `java_path` followed by the entries of `current_path` that `keep` accepts.
fn prepend(
    java_path: &str,
    current_path: &str,
    keep: impl Fn(&Path) -> bool,
) -> anyhow::Result<String> {
    // An empty `PATH` is *no* entries, not one empty entry. `split_paths("")`
    // yields the latter, which would leave `<jdk>/bin:` - and an empty `PATH`
    // component means the working directory, so the shell would search
    // whatever the user happened to have cd'd into. `env -i` in a CI step is
    // the ordinary way to arrive here.
    //
    // Skipping the split rather than the join: `join_paths` is also what
    // refuses a `java_path` containing a `:`, which would otherwise be handed
    // to the shell as two entries. That check has to apply whether or not the
    // caller had a `PATH`.
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

/// The caller's `PATH`, or an error when it is not valid UTF-8.
///
/// `env::var(..).unwrap_or_default()` is the wrong shape here, and quietly so:
/// it maps a non-UTF-8 `PATH` to the empty string, and the empty string is a
/// legitimate value meaning "nothing on PATH". The JDK's `bin` would then be
/// the only entry `jlo env` emits - the user's whole `PATH` replaced, with a
/// trailing empty component that makes the shell search the working
/// directory. Unset really does mean empty; undecodable does not, and is the
/// one case that has to stop before anything reaches stdout.
///
/// This is also what makes the `PATH contains non-UTF-8 characters` context
/// below reachable in principle; by the time a `&str` has been taken, the
/// question has already been answered.
pub(crate) fn current_path() -> anyhow::Result<String> {
    classify_path(env::var("PATH"))
}

/// The decision behind [`current_path`], taking the lookup's result rather
/// than making it, so the three arms are testable without mutating the
/// process environment.
fn classify_path(looked_up: Result<String, env::VarError>) -> anyhow::Result<String> {
    match looked_up {
        Ok(path) => Ok(path),
        Err(env::VarError::NotPresent) => Ok(String::new()),
        Err(env::VarError::NotUnicode(_)) => Err(anyhow!(
            "PATH is not valid UTF-8, so jlo cannot rewrite it without losing entries"
        )),
    }
}

/// The child's `PATH`: the caller's with the JDK's `bin` directory prepended.
fn child_path(java_home: &Path) -> anyhow::Result<String> {
    let java_bin = java_home.join("bin");
    let bin = java_bin
        .to_str()
        .with_context(|| format!("path is not valid UTF-8: {}", java_bin.display()))?;
    prepend(bin, &current_path()?, |_| true)
}

/// Split the arguments following `exec` into an optional version and the command
/// to run. The literal `--` separates them; everything before it is the version
/// (zero or one token), everything after is the command.
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

/// Whether the real, unparsed command line has a literal `--` as the token
/// immediately following `exec`, i.e. no version was given before it.
fn separator_immediately_follows_exec(mut raw_args: impl Iterator<Item = String>) -> bool {
    raw_args
        .find(|a| a == "exec")
        .and_then(|_| raw_args.next())
        .is_some_and(|a| a == "--")
}

/// clap's `trailing_var_arg` treats a literal `--` as the options/positional
/// boundary rather than a value whenever it is the very first token handed to
/// the subcommand - which is exactly `jlo exec -- <command>` (version
/// omitted). It gets consumed before reaching us, so `args` arrives here
/// without the separator `parse_exec_args` requires.
///
/// This is unambiguous precisely because it only ever happens to the
/// *first* token: once any value (a version, or the reinstated `--` itself)
/// has bound to the positional, every later token - including a second,
/// user-typed `--` that is genuinely part of the command - survives
/// untouched. So `args` here never already contains the eaten separator;
/// any `--` already present in it is a distinct, later token that must be
/// left exactly where it is, not mistaken for "already restored".
pub(crate) fn restore_leading_separator(args: &[String]) -> Vec<String> {
    if !separator_immediately_follows_exec(env::args()) {
        return args.to_vec();
    }

    let mut restored = Vec::with_capacity(args.len() + 1);
    restored.push("--".to_string());
    restored.extend_from_slice(args);
    restored
}

/// Replace the current process with `command`, having set `JAVA_HOME` and
/// prepended the JDK's `bin` to `PATH`. On Unix this is a real `execvp`, so the
/// child's exit code and signals propagate transparently.
#[cfg(unix)]
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

/// Map a launch failure to a shell-conventional exit code: 126 for a command
/// that exists but can't be run (e.g. not executable), 127 otherwise.
fn exec_failure_code(kind: std::io::ErrorKind) -> i32 {
    match kind {
        std::io::ErrorKind::PermissionDenied => 126,
        _ => 127,
    }
}

#[cfg(test)]
mod exec_arg_recovery_tests {
    use super::separator_immediately_follows_exec;

    fn raw(tokens: &[&str]) -> impl Iterator<Item = String> {
        tokens
            .iter()
            .map(|s| (*s).to_string())
            .collect::<Vec<_>>()
            .into_iter()
    }

    #[test]
    fn detects_separator_right_after_exec() {
        // `jlo exec -- java -version`: clap eats this `--` before `cmd_exec`
        // ever sees it.
        assert!(separator_immediately_follows_exec(raw(&[
            "jlo-bin", "exec", "--", "java", "-version"
        ])));
    }

    #[test]
    fn does_not_trigger_when_a_version_precedes_it() {
        // `jlo exec 21 -- java -version`: the version binds first, so clap
        // never touches this `--`.
        assert!(!separator_immediately_follows_exec(raw(&[
            "jlo-bin", "exec", "21", "--", "java", "-version"
        ])));
    }

    #[test]
    fn still_detects_it_when_the_command_has_its_own_dash_dash() {
        // `jlo exec -- -- echo hi`: the first `--` is still the one clap
        // eats, even though a second, user-typed `--` (part of the command)
        // immediately follows it. (Regression for the bug where
        // `args.first() == "--"` was used as a stand-in for "already
        // restored": that second `--` would land at `args[0]` after clap's
        // parse and get mistaken for the already-restored separator.)
        assert!(separator_immediately_follows_exec(raw(&[
            "jlo-bin", "exec", "--", "--", "echo", "hi"
        ])));
    }

    #[test]
    fn does_not_trigger_without_any_separator() {
        assert!(!separator_immediately_follows_exec(raw(&[
            "jlo-bin", "exec", "java", "-version"
        ])));
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

    #[test]
    fn parse_exec_args_version_and_command() {
        let (version, command) =
            parse_exec_args(&owned(&["21", "--", "java", "-version"])).unwrap();
        assert_eq!(version, Some("21".to_string()));
        assert_eq!(command, owned(&["java", "-version"]));
    }

    #[test]
    fn parse_exec_args_no_version_uses_none() {
        let (version, command) = parse_exec_args(&owned(&["--", "java", "-version"])).unwrap();
        assert_eq!(version, None);
        assert_eq!(command, owned(&["java", "-version"]));
    }

    #[test]
    fn parse_exec_args_missing_separator_errors() {
        assert!(parse_exec_args(&owned(&["21", "java", "-version"])).is_err());
    }

    #[test]
    fn parse_exec_args_empty_command_errors() {
        assert!(parse_exec_args(&owned(&["21", "--"])).is_err());
    }

    #[test]
    fn parse_exec_args_multiple_versions_error() {
        assert!(parse_exec_args(&owned(&["21", "25", "--", "java"])).is_err());
    }

    #[test]
    fn exec_failure_code_distinguishes_not_found_and_not_executable() {
        use std::io::ErrorKind;
        assert_eq!(exec_failure_code(ErrorKind::NotFound), 127);
        assert_eq!(exec_failure_code(ErrorKind::PermissionDenied), 126);
        assert_eq!(exec_failure_code(ErrorKind::Other), 127);
    }

    #[test]
    fn parse_exec_args_no_args_errors() {
        assert!(parse_exec_args(&owned(&[])).is_err());
    }

    #[test]
    fn parse_exec_args_only_separator_errors() {
        // "--" alone: no version, no command
        assert!(parse_exec_args(&owned(&["--"])).is_err());
    }

    #[test]
    fn parse_exec_args_double_dash_in_command_is_preserved() {
        // only the first "--" separates; later ones belong to the command
        let (version, command) =
            parse_exec_args(&owned(&["21", "--", "sh", "-c", "--", "x"])).unwrap();
        assert_eq!(version, Some("21".to_string()));
        assert_eq!(command, owned(&["sh", "-c", "--", "x"]));
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
