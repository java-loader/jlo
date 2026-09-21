mod adoptium;
mod cli;
mod conf;
mod extract;
mod install;
mod selfupdate;
mod store;
mod ui;
mod version;

use crate::adoptium::{AdoptiumClient, JdkMetadata};
use crate::store::{JdkStore, RemoveError};
use crate::ui::InstallUi;
use anyhow::{Context, anyhow};
use clap::Parser;
use std::collections::HashSet;
use std::env;
use std::fs::File;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::exit;
use tempfile::tempdir;

/// Name of J'Lo's state directory under `$HOME`, used when `JLO_HOME` is unset.
/// Must stay in sync with `install.sh`.
pub(crate) const JLO_HOME_DIR_NAME: &str = ".jlo";

/// A failed command: the error to report, plus the advice line that belongs
/// *under* it, if any.
///
/// Reporting an error is `main`'s job alone, so a command that wants to add a
/// hint has to hand it over rather than print it. Carrying it keeps the two
/// lines in the order the user has always seen - the error first, the dimmed
/// advice second - which printing at the failure site would invert.
#[derive(Debug)]
pub(crate) struct CommandError {
    error: anyhow::Error,
    hint: Option<String>,
}

impl CommandError {
    pub(crate) fn with_hint(error: anyhow::Error, hint: impl Into<String>) -> Self {
        Self {
            error,
            hint: Some(hint.into()),
        }
    }
}

impl From<anyhow::Error> for CommandError {
    fn from(error: anyhow::Error) -> Self {
        Self { error, hint: None }
    }
}

/// `JdkStore::remove` refuses with a typed error that already knows which
/// advice line belongs under it, so the conversion is the whole of
/// `cmd_remove`'s error handling - no matching on message text.
impl From<RemoveError> for CommandError {
    fn from(error: RemoveError) -> Self {
        let hint = error.hint();
        // Only the store variant carries a context chain worth preserving;
        // the refusals are a single sentence this type formats itself.
        let error = match error {
            RemoveError::Store(e) => e,
            refusal => anyhow!("{refusal}"),
        };
        match hint {
            Some(hint) => Self::with_hint(error, hint),
            None => error.into(),
        }
    }
}

/// The one place a command failure turns into a message and a non-zero exit
/// status. Every `cmd_*` below hands its error back rather than ending the
/// process; the only other `exit` calls left are the `exec` path, which cannot
/// return because it has replaced the process image, and `ui`'s `print_lines`,
/// which fails on the very stream it is writing the output to.
fn main() {
    if let Err(e) = run() {
        ui::error!("{:#}", e.error);
        if let Some(hint) = e.hint {
            ui::hint!("{hint}");
        }
        exit(1);
    }
}

fn run() -> Result<(), CommandError> {
    // The easter egg is deliberately not a clap subcommand: `hide = true`
    // only suppresses it from `--help`. `clap_complete` still emits hidden
    // subcommands into generated completion scripts, and clap's "did you
    // mean" suggestion engine still offers it for typos (e.g. `jlo sng`).
    // Intercepting the raw token before `Cli::parse()` keeps it out of
    // help, completions, and typo suggestions in one move.
    let argv: Vec<String> = env::args().skip(1).collect();
    if argv.first().map(String::as_str) == Some("sing") {
        eprintln!("There are no Easter Eggs in this program. Trust me. 💃");
        return Ok(());
    }

    // The install verb is intercepted here for the same reason, and the reason
    // is sharper still: it writes the shell layout, so it must not show up in
    // the completions it generates. `install.sh`, `install-local.sh` and
    // `selfupdate` are its only callers.
    if argv.first().map(String::as_str) == Some(install::VERB) {
        return Ok(install::cmd_install(&argv[1..])?);
    }

    // A receipt that disagrees with this binary is the known-incomplete state:
    // the binary landed, the generated files did not. Any invocation clears it
    // rather than reporting it, which is why there is no `--repair` verb.
    install::self_heal();

    let cli = cli::Cli::parse();

    let Some(command) = cli.command else {
        cli::print_help();
        return Ok(());
    };

    let api_url =
        env::var("JLO_ADOPTIUM_API_URL").unwrap_or_else(|_| adoptium::ADOPTIUM_API_URL.to_string());
    let client = AdoptiumClient::new(api_url);

    match command {
        cli::Command::Env {
            version,
            offline,
            verbose,
        } => cmd_env(&client, version, offline, verbose),
        cli::Command::Home { version, offline } => cmd_home(&client, version, offline),
        cli::Command::Exec { args } => cmd_exec(&client, &args),
        cli::Command::Current => cmd_current(),
        cli::Command::List { offline } => cmd_list(&client, offline),
        cli::Command::Install { versions } => cmd_install(&client, versions),
        cli::Command::Update { versions, all } => cmd_update(&client, versions, all),
        cli::Command::Prune => cmd_prune(),
        cli::Command::Remove { versions } => cmd_remove(&versions),
        cli::Command::Init {
            version,
            global,
            force,
        } => cmd_init(&client, version, global, force),
        cli::Command::Selfupdate => selfupdate::cmd_selfupdate(),
        cli::Command::Completions { shell } => {
            cmd_completions(shell);
            Ok(())
        }
    }
}

/// Write a shell completion script to stdout.
///
/// The completion function registers against the command word `jlo`, which
/// resolves to the shell function the installer generates, so the wrapper is
/// transparent to completion.
fn cmd_completions(shell: clap_complete::Shell) {
    use std::io::Write as _;

    // The script is built once and written once, because `install.rs` needs
    // the same bytes to write into `$JLO_HOME/completions`. A closed stdout
    // (`jlo completions bash | head`) is not an error worth reporting - the
    // rule `print_lines` already follows.
    let _ = std::io::stdout().write_all(&cli::completion_script(shell));
}

/// Determine the requested major version: the explicit CLI argument if present,
/// otherwise the project `.jlorc` / user default config.
///
/// Returns where the version came from as well as what it is, because
/// `jlo env --verbose` reports it and re-deriving it there would be a second
/// spelling of the same walk.
fn resolve_java_version_from(explicit: Option<String>) -> anyhow::Result<conf::Resolved> {
    let resolved = match explicit {
        Some(version) => conf::Resolved {
            version,
            source: conf::Source::Argument,
        },
        None => conf::resolve()?,
    };

    assert_java_version(&resolved.version)?;
    Ok(resolved)
}

fn cmd_env(
    client: &AdoptiumClient,
    version: Option<String>,
    offline: bool,
    verbose: bool,
) -> Result<(), CommandError> {
    let resolved = resolve_java_version_from(version)?;
    let change = setup(client, &resolved.version, offline)?;

    // Opt-in, for the reason `setup` prints nothing at all: the autoload hook
    // calls it from PROMPT_COMMAND/chpwd, and the hook never passes --verbose.
    // Reported here rather than inside `setup` so the hook path keeps exactly
    // one writer, and through the same formatter `jlo current` uses so the two
    // answers cannot drift.
    if verbose {
        ui::env_report(&ui::Active {
            version: change
                .java_home
                .file_name()
                .map(|name| name.to_string_lossy().into_owned()),
            major: resolved.version.parse().ok(),
            path: change.java_home,
            source: Some(resolved.source),
            // `env` asked for this version, so nothing can be pinned elsewhere
            // - the config either is the source or was overridden by hand.
            pinned_elsewhere: None,
            unchanged: change.unchanged,
        });
    }

    // The exports on stdout are the whole effect of this command. If stdout is
    // a terminal nothing captured them, so the exit code says success while
    // nothing happened - the failure shape that sends a CI step, a Makefile
    // recipe or an agent looking for the problem somewhere else entirely.
    if std::io::stdout().is_terminal() {
        ui::hint!("{}", unsourced_env_hint(&resolved.version));
    }

    Ok(())
}

/// The line `jlo env` ends on when its exports went nowhere.
///
/// Keyed on stdout being a terminal, which is a reliable enough negative: the
/// `jlo` shell function sources the exports out of a process substitution
/// (`. <(jlo-bin env ...)`), and the autoload hook calls that same function, so
/// on the sourced path stdout is a pipe and this never fires - not even on the
/// `cd` hook that runs on every directory change.
/// `jlo-bin env 21 > file` stays silent too - an accepted gap, since the case
/// that actually misleads is the interactive/agent one.
fn unsourced_env_hint(java_version: &str) -> String {
    format!(
        "jlo env prints exports for a shell to source; it did not change anything. \
         Use 'jlo exec {java_version} -- <command>' or \
         'export JAVA_HOME=\"$(jlo home {java_version})\"'."
    )
}

fn cmd_home(
    client: &AdoptiumClient,
    version: Option<String>,
    offline: bool,
) -> Result<(), CommandError> {
    let java_version = resolve_java_version_from(version)?.version;
    let store = JdkStore::discover()?;
    let java_home = if offline {
        offline_java_home(&store, &java_version, "home")?
    } else {
        resolve_java_home(client, &store, &java_version)?
    };
    println!("{}", java_home.to_string_lossy());
    Ok(())
}

/// `--offline`: answer from the store alone.
///
/// The point of the flag is that asking the question cannot trigger the
/// several-hundred-megabyte answer - a CI step with a short timeout, a
/// network-isolated sandbox, or the autoload hook on a `cd`, needs a probe
/// that fails fast rather than one that hangs on a connection attempt. The
/// exit status is the answer, so there is no distinct code for "not
/// installed": 1, like every other failure here.
///
/// `command` is the subcommand to name in the advice line, so `env` does not
/// send the reader to `home` (and vice versa).
fn offline_java_home(
    store: &JdkStore,
    java_version: &str,
    command: &str,
) -> Result<PathBuf, CommandError> {
    store.find_matching(java_version).ok_or_else(|| {
        CommandError::with_hint(
            anyhow!("no installed JDK matches Java {java_version}"),
            format!("Run 'jlo {command} {java_version}' without --offline to install it."),
        )
    })
}

/// Diverges on success: `run_exec` replaces the process image. The `Result` is
/// for the argument errors that can still be reported the ordinary way.
fn cmd_exec(client: &AdoptiumClient, args: &[String]) -> Result<(), CommandError> {
    let args = restore_leading_separator(args);

    // clap's own `-h`/`--help` interception only fires before any value has
    // bound to the `args` positional; once a version is present (`jlo exec
    // 21 --help`) it no longer triggers, because by then the parser is in
    // `trailing_var_arg` value-collection mode. Handle it ourselves, but
    // only for tokens before the `--`: anything after it belongs to the
    // child command and must be passed through untouched (see
    // `exec_passes_hyphen_args_through_to_the_child`).
    let separator = args.iter().position(|a| a == "--").unwrap_or(args.len());
    let before_separator = &args[..separator];
    if before_separator.iter().any(|a| a == "--help") {
        cli::print_exec_help(true);
        return Ok(());
    }
    if before_separator.iter().any(|a| a == "-h") {
        cli::print_exec_help(false);
        return Ok(());
    }

    let (version, command) = parse_exec_args(&args).map_err(|e| {
        CommandError::with_hint(
            anyhow!("{e}"),
            "Usage: jlo exec [VERSION] -- <COMMAND> [ARGS]...",
        )
    })?;

    // `-> !`, so this tail expression never produces the `Ok(())` its type
    // says it does.
    run_exec(client, version, &command);
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
fn restore_leading_separator(args: &[String]) -> Vec<String> {
    if !separator_immediately_follows_exec(env::args()) {
        return args.to_vec();
    }

    let mut restored = Vec::with_capacity(args.len() + 1);
    restored.push("--".to_string());
    restored.extend_from_slice(args);
    restored
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

/// Resolve the JDK (installing on demand) and replace the current process with
/// the command. On non-Unix targets `exec` is unsupported, so bail out *before*
/// downloading anything.
#[cfg(unix)]
fn run_exec(client: &AdoptiumClient, version: Option<String>, command: &[String]) -> ! {
    // This function never returns, so it reports its own failures rather than
    // handing them back to `main`.
    let java_home = resolve_java_version_from(version)
        .and_then(|resolved| {
            let store = JdkStore::discover()?;
            resolve_java_home(client, &store, &resolved.version)
        })
        .unwrap_or_else(|e| {
            ui::error!("{e:#}");
            exit(1);
        });

    exec_command(&java_home, command);
}

// A real `execvp` is Unix-only. A native Windows build would replace this with a
// spawn-and-wait fallback that propagates the child's exit code.
#[cfg(not(unix))]
fn run_exec(_client: &AdoptiumClient, _version: Option<String>, _command: &[String]) -> ! {
    ui::error!("'jlo exec' is not supported on this platform");
    exit(1);
}

/// Split the arguments following `exec` into an optional version and the command
/// to run. The literal `--` separates them; everything before it is the version
/// (zero or one token), everything after is the command.
fn parse_exec_args(args: &[String]) -> Result<(Option<String>, Vec<String>), String> {
    let sep = args
        .iter()
        .position(|a| a == "--")
        .ok_or("expected '--' before the command, e.g. jlo exec 21 -- java -version")?;

    let version = match &args[..sep] {
        [] => None,
        [v] => Some(v.clone()),
        _ => return Err("only one version may be given before '--'".to_string()),
    };

    let command = args[sep + 1..].to_vec();
    if command.is_empty() {
        return Err("no command given after '--'".to_string());
    }

    Ok((version, command))
}

/// Build the child `PATH` with the JDK's `bin` directory prepended.
fn child_path(java_bin: &str, current_path: &str) -> anyhow::Result<String> {
    if current_path.is_empty() {
        return Ok(java_bin.to_string());
    }

    let mut paths = vec![PathBuf::from(java_bin)];
    paths.extend(env::split_paths(current_path));

    Ok(env::join_paths(paths)
        .context("could not join PATH components")?
        .to_str()
        .context("PATH contains non-UTF-8 characters")?
        .to_string())
}

/// Replace the current process with `command`, having set `JAVA_HOME` and
/// prepended the JDK's `bin` to `PATH`. On Unix this is a real `execvp`, so the
/// child's exit code and signals propagate transparently.
#[cfg(unix)]
fn exec_command(java_home: &Path, command: &[String]) -> ! {
    use std::os::unix::process::CommandExt;
    use std::process::Command;

    let (program, args) = command
        .split_first()
        .expect("command is non-empty (checked in parse_exec_args)");

    let java_bin = java_home.join("bin");
    let new_path = child_path(
        &java_bin.to_string_lossy(),
        &env::var("PATH").unwrap_or_default(),
    )
    .unwrap_or_else(|e| {
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

/// Print the JDKs Adoptium offers for this machine, newest first, annotated
/// with what is installed locally. `--offline` skips the network and lists only
/// what is already installed. The tables themselves are `ui`'s.
fn cmd_list(client: &AdoptiumClient, offline: bool) -> Result<(), CommandError> {
    let store = JdkStore::discover()?;
    let installed = store.list().context("could not list installed JDKs")?;

    // Resolved once, before either listing: the gutter marks a row by version
    // name, and the path-to-version step is the store's job, not the UI's.
    let java_home = active_java_home();
    let active = store.active_version(&installed, java_home.as_deref());

    if offline {
        ui::offline_list(&installed, active.as_deref(), &store);
    } else {
        let available = client.available_jdks().map_err(|e| {
            CommandError::with_hint(
                e,
                "Use 'jlo list --offline' to list the JDKs already installed.",
            )
        })?;
        ui::remote_list(&available, &installed, active.as_deref());
    }

    // After the listing, so it reads as a footnote to the missing gutter mark
    // rather than as a warning about the command.
    if let (Some(path), None) = (&java_home, &active) {
        ui::foreign_java_home(path);
    }

    Ok(())
}

/// The advice line under both of the states in which `jlo current` has no
/// answer to print. Naming `jlo env` is the whole of it: that is the command
/// that puts a JDK back in this shell.
const NO_ACTIVE_JDK_HINT: &str = "Run 'jlo env' to activate a JDK in this shell.";

/// `jlo current`: what is active in this shell, and why.
///
/// Starts from the live `$JAVA_HOME` rather than from `.jlorc`, because the
/// two can legitimately disagree and saying so is most of what this command is
/// for. Never touches the network - every fact it reports is on disk or in the
/// environment - so there is no `--offline` flag to pass.
///
/// stdout carries the one answer line, stderr any advisory: the same split
/// `jlo list` makes, and safe here because the `jlo` shell function sources
/// stdout for `env`/`use` alone.
///
/// The exit status is 1 exactly when stdout is empty, so
/// `VER=$(jlo current)` never hands back an empty string over a success code.
fn cmd_current() -> Result<(), CommandError> {
    let Some(java_home) = active_java_home() else {
        return Err(CommandError::with_hint(
            anyhow!("No JDK is active."),
            NO_ACTIVE_JDK_HINT,
        ));
    };

    let store = JdkStore::discover()?;
    let installed = store.list().context("could not list installed JDKs")?;

    let Some(version) = store.active_version(&installed, Some(&java_home)) else {
        // Inside the store but not among the installs it can list: the
        // directory went away under a shell that is still pointing at it,
        // which is what 'jlo remove' on the live JDK leaves behind. Reporting
        // that as a JDK set outside jlo would be wrong - the install was ours.
        if is_inside(store.base(), &java_home) {
            return Err(CommandError::with_hint(
                anyhow!(
                    "$JAVA_HOME points at a jlo install that is no longer there ({}).",
                    java_home.display()
                ),
                NO_ACTIVE_JDK_HINT,
            ));
        }

        // A JDK jlo does not manage. No config is consulted: whatever is
        // pinned, jlo is not what put this here, and the path says that
        // completely.
        ui::print_lines([ui::provenance_line(&ui::Active {
            path: java_home,
            version: None,
            major: None,
            source: Some(conf::Source::Foreign),
            pinned_elsewhere: None,
            unchanged: false,
        })]);
        return Ok(());
    };

    let mut active = ui::Active {
        major: installed
            .iter()
            .find(|jdk| jdk.version == version)
            .map(|jdk| jdk.major),
        path: java_home,
        version: Some(version),
        source: None,
        pinned_elsewhere: None,
        unchanged: false,
    };

    // A config that fails to load is still a failure: it is a file the user
    // wrote and meant, and answering around it would hide the mistake.
    if let Some(pinned) = conf::find()? {
        if pinned.version.parse::<i64>().ok() == active.major {
            active.source = Some(pinned.source);
        } else {
            active.pinned_elsewhere = Some(pinned);
        }
    }

    ui::print_lines([ui::provenance_line(&active)]);

    // After the answer, so it reads as a footnote to it rather than in place
    // of it. The question asked was "what is active", and it has an answer -
    // hence exit 0, which is what keeps this command usable in exactly the
    // situation you most want to read the version: a stale shell inside a
    // pinned project.
    if let Some(pinned) = &active.pinned_elsewhere {
        ui::pin_mismatch(pinned);
    }

    Ok(())
}

/// Whether `path` lies under `base`.
///
/// `$JAVA_HOME` is normally spelled exactly as the store spelled it, because
/// `jlo env` is what set it; the canonicalized retry covers a `$HOME` that
/// reaches the store through a symlink. `path` itself is deliberately not
/// canonicalized - the case this decides is the one where it no longer exists.
fn is_inside(base: &Path, path: &Path) -> bool {
    path.starts_with(base)
        || base
            .canonicalize()
            .is_ok_and(|canonical| path.starts_with(canonical))
}

fn cmd_prune() -> Result<(), CommandError> {
    let store = JdkStore::discover()?;
    let report = store.prune().context("could not prune JDKs")?;
    ui::prune_report(&report);
    Ok(())
}

fn cmd_remove(versions: &[String]) -> Result<(), CommandError> {
    let store = JdkStore::discover()?;
    let report = store.remove(versions, active_java_home().as_deref())?;
    ui::remove_report(&report);
    Ok(())
}

/// The directory `$JAVA_HOME` currently points at, if the variable is set to
/// anything.
///
/// Read here rather than in `JdkStore` so the store stays a filesystem
/// module: `remove` takes the live JDK as an argument, which is also what
/// makes its refusal testable without mutating the process environment.
fn active_java_home() -> Option<PathBuf> {
    env::var_os("JAVA_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn cmd_init(
    client: &AdoptiumClient,
    version: Option<String>,
    global: bool,
    force: bool,
) -> Result<(), CommandError> {
    let java_version = match version {
        Some(version) => version,
        None => client
            .latest_major()
            .context("could not fetch latest JDK version")?,
    };

    assert_java_version(&java_version)?;

    let result = if global {
        conf::init_default_config(&java_version, force)
    } else {
        conf::init_project_config(&java_version, force)
    };

    result.map_err(|e| {
        // `--force` answers exactly one of the failures below, so the hint is
        // keyed off the message `conf` produced. Read it before the context is
        // attached: `to_string` renders only the outermost message.
        let already_exists = e.to_string().contains("already exists");
        let e = e.context("could not create config file");
        if already_exists {
            CommandError::with_hint(e, "Re-run with --force to overwrite it.")
        } else {
            e.into()
        }
    })
}

/// Download the latest build of each major named, without touching the
/// current shell.
///
/// Deliberately a second verb rather than an alias for `update`: "make sure
/// this major is here" and "bring what is here up to date" are different
/// questions, and they coincide only because jlo keeps exactly one build per
/// major. Hence no `--all` here - there is no such thing as installing every
/// major - while `update` keeps its own meaning and wording.
fn cmd_install(client: &AdoptiumClient, versions: Vec<String>) -> Result<(), CommandError> {
    let store = JdkStore::discover()?;
    let versions = requested_versions(versions, "install")?;
    install_each(client, &store, versions)
}

fn cmd_update(
    client: &AdoptiumClient,
    versions: Vec<String>,
    all: bool,
) -> Result<(), CommandError> {
    let store = JdkStore::discover()?;

    // `--all` and explicit versions are mutually exclusive (clap enforces it),
    // so these two arms are the whole input space.
    let versions_to_install = if all {
        let installed: HashSet<String> = store
            .installed_majors()
            .context("could not determine installed JDK versions")?
            .into_iter()
            .map(|v| v.to_string())
            .collect();

        if installed.is_empty() {
            return Err(anyhow!("no installed JDKs to update").into());
        }
        installed
    } else {
        requested_versions(versions, "update")?
    };

    install_each(client, &store, versions_to_install)
}

/// The majors an explicit run should download: the list given, or the version
/// resolved from config when the list is empty - the same resolution `env`,
/// `home` and `exec` do, so a bare `jlo install` means the pinned version like
/// everywhere else.
///
/// An invalid entry is warned about and skipped, so one typo in a list of four
/// does not cost the other three. A list that leaves nothing valid behind is
/// an error naming `verb`, the command that asked.
fn requested_versions(versions: Vec<String>, verb: &str) -> Result<HashSet<String>, CommandError> {
    if versions.is_empty() {
        return Ok(HashSet::from([conf::resolve()?.version]));
    }

    let mut requested = HashSet::new();
    for v in versions {
        if conf::is_valid_version(&v) {
            requested.insert(v);
        } else {
            ui::warning!("skipping invalid version '{v}'");
        }
    }

    if requested.is_empty() {
        return Err(anyhow!("no valid Java versions provided to {verb}").into());
    }

    Ok(requested)
}

/// The one download site behind both `install` and `update`: the two verbs
/// differ only in how they arrive at this set of majors.
fn install_each(
    client: &AdoptiumClient,
    store: &JdkStore,
    versions: HashSet<String>,
) -> Result<(), CommandError> {
    // Sorted for a stable processing order, rather than whatever order the
    // hash set happens to iterate in.
    let mut versions: Vec<_> = versions.into_iter().collect();
    versions.sort();

    let mut installed_any = false;
    for java_version in versions {
        installed_any |= update(client, store, &java_version)?;
    }

    // A download leaves the superseded minor on disk on purpose - a command
    // that downloads should not also delete, and the old JDK may still be
    // wired into an open shell or an IDE. Point at `jlo prune` instead of
    // doing it here.
    if let Some(hint) = superseded_hint(installed_any, count_superseded(store)) {
        ui::hint!("{hint}");
    }

    Ok(())
}

/// How many installs `jlo prune` would remove, or 0 if that cannot be
/// determined. A hint is not worth failing an otherwise successful run, so an
/// unreadable JDK directory just means no hint.
fn count_superseded(store: &JdkStore) -> usize {
    store.superseded_count().unwrap_or(0)
}

/// The line `jlo install` and `jlo update` end on when this run left an older
/// minor behind.
///
/// `None` when there is nothing to say: no install happened (the leftovers
/// predate this run, and nagging on every no-op run trains the user to ignore
/// the line), or nothing is superseded.
fn superseded_hint(installed_any: bool, superseded: usize) -> Option<String> {
    if !installed_any || superseded == 0 {
        return None;
    }

    let plural = if superseded == 1 { "" } else { "s" };
    Some(format!(
        "{superseded} superseded JDK{plural} still installed - run 'jlo prune' to remove {}.",
        if superseded == 1 { "it" } else { "them" }
    ))
}

/// Returns whether a JDK was installed, so the caller can tell a real update
/// from an already-current one.
fn update(client: &AdoptiumClient, store: &JdkStore, java_version: &str) -> anyhow::Result<bool> {
    let jdk_metadata = client.fetch_metadata(java_version)?;

    if store.find_exact(&jdk_metadata).is_some() {
        ui::up_to_date(java_version, &jdk_metadata.semver);
        Ok(false)
    } else {
        install_jdk(client, store, &jdk_metadata).context("could not install JDK")?;
        Ok(true)
    }
}

/// Resolve the `JAVA_HOME` for the requested major version, installing the JDK on
/// demand if it is not already present. Diagnostics go to stderr; this returns
/// the path so callers decide what (if anything) to print to stdout.
fn resolve_java_home(
    client: &AdoptiumClient,
    store: &JdkStore,
    java_version: &str,
) -> anyhow::Result<PathBuf> {
    if let Some(path) = store.find_matching(java_version) {
        Ok(path)
    } else {
        let metadata = client.fetch_metadata(java_version)?;
        install_jdk(client, store, &metadata)
    }
}

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
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

/// Emit the `export` lines for the requested version.
///
/// Nothing is written to stderr on this path, even when the environment does
/// change: the autoload hook calls it from `PROMPT_COMMAND`/`chpwd`, so any
/// status line here would print on every new shell and every `cd`. Exporting a
/// variable lasts only as long as the shell and is implied by the command the
/// user ran - it is the install (a JDK on disk) that earns a line, not this.
///
/// `offline` is the whole of the "a `cd` must not start a download" rule, and
/// it lives here rather than in `jlo-autoload.sh` so it is decided once, in
/// Rust, instead of once per shell dialect. When it declines, it declines
/// before anything reaches stdout: the hook sources that stream, so a partial
/// export would be worse than no export at all.
fn setup(
    client: &AdoptiumClient,
    java_version: &str,
    offline: bool,
) -> Result<EnvChange, CommandError> {
    let store = JdkStore::discover()?;
    let java_home = if offline {
        offline_java_home(&store, java_version, "env")?
    } else {
        resolve_java_home(client, &store, java_version)?
    };

    // Collected rather than printed as they are decided: both lines are one
    // environment, and `print_lines` is also the only writer here that treats
    // a closed pipe as an ending rather than panicking - `jlo env | head` is
    // an ordinary thing to type.
    let mut exports = Vec::new();

    let current_java_home = env::var("JAVA_HOME").unwrap_or_default();
    if current_java_home != java_home.to_string_lossy() {
        exports.push(format!(
            "export JAVA_HOME={}",
            shell_quote(&java_home.to_string_lossy())
        ));
    }

    let java_bin_path = java_home.join("bin").to_string_lossy().into_owned();
    let current_path = env::var("PATH").unwrap_or_default();
    if let Some(updated_path) = update_path(&java_bin_path, &current_path, store.base())? {
        exports.push(format!("export PATH={}", shell_quote(&updated_path)));
    }

    // Read before the vector is consumed: "nothing to export" is the whole of
    // what --verbose needs to distinguish "already correct" from "did
    // nothing", which were indistinguishable while this path was silent.
    let unchanged = exports.is_empty();
    ui::print_lines(exports);

    Ok(EnvChange {
        java_home,
        unchanged,
    })
}

/// What a `setup` call did, for the caller that may have been asked to report
/// it.
#[derive(Debug)]
struct EnvChange {
    java_home: PathBuf,
    /// No exports were needed: `$JAVA_HOME` and `PATH` were already right.
    unchanged: bool,
}

fn install_jdk(
    client: &AdoptiumClient,
    store: &JdkStore,
    jdk_metadata: &JdkMetadata,
) -> anyhow::Result<PathBuf> {
    // One progress region spans all three phases, so the terminal shows a
    // single line that changes rather than three bars stacking up.
    let ui = InstallUi::new(&jdk_metadata.semver);

    match install_jdk_inner(client, store, jdk_metadata, &ui) {
        Ok(dest_dir) => {
            ui.finish(&dest_dir);
            Ok(dest_dir)
        }
        Err(e) => {
            // Clear the live region first: a half-drawn bar above the error
            // only gets in the way of reading it.
            ui.abandon();
            Err(e)
        }
    }
}

fn install_jdk_inner(
    client: &AdoptiumClient,
    store: &JdkStore,
    jdk_metadata: &JdkMetadata,
    ui: &InstallUi,
) -> anyhow::Result<PathBuf> {
    // Download JDK
    let temp_dir = tempdir().context("could not create temporary directory")?;
    let temp_file = temp_dir.path().join(&jdk_metadata.package_name);
    let file = &mut File::create(&temp_file).context("could not create temporary file")?;
    client.download(jdk_metadata, file, ui)?;

    // Extract JDK to temp dir
    extract::extract(&temp_file, temp_dir.path(), ui)?;

    let dest_dir = store.install(jdk_metadata, temp_dir.path(), ui)?;

    temp_dir.close().unwrap_or_else(|err| {
        ui::warning!("could not delete temporary directory: {err}");
    });

    Ok(dest_dir)
}

/// J'Lo's own state directory — where `default.jlorc` lives.
///
/// The fallback must match what `install.sh` exports (`$HOME/.jlo`), not bare
/// `$HOME`: interactive shells get `JLO_HOME` from the generated `jlo.sh`, but scripts
/// and CI invoking `jlo-bin` directly do not, and those two must resolve the
/// same file.
pub(crate) fn jlo_home_dir() -> anyhow::Result<PathBuf> {
    let path = env::var_os("JLO_HOME")
        .map(PathBuf::from)
        .or_else(|| env::home_dir().map(|home| home.join(JLO_HOME_DIR_NAME)))
        .context("could not determine home directory.")?;
    Ok(path)
}

/// Prepend `java_path` to `current_path`, dropping any entry already under
/// `jdk_base`. `jdk_base` must be the JDK install directory ([`JdkStore::base`]) —
/// the only tree whose PATH entries J'Lo owns. Passing a broader directory (the
/// home directory, say) would strip unrelated user entries.
fn update_path(
    java_path: &str,
    current_path: &str,
    jdk_base: &Path,
) -> anyhow::Result<Option<String>> {
    // Remove JDK bin entries from earlier runs to avoid duplicates
    let mut path_vector: Vec<_> = env::split_paths(current_path)
        .filter(|p| !p.starts_with(jdk_base))
        .collect();

    // Insert the new path at the beginning
    path_vector.insert(0, java_path.into());

    // Join paths back into a single string
    let new_path = env::join_paths(path_vector)
        .context("could not join PATH components")?
        .to_str()
        .context("PATH contains non-UTF-8 characters")?
        .to_string();

    // Only return if the path has changed
    if new_path == current_path {
        Ok(None)
    } else {
        Ok(Some(new_path))
    }
}

fn assert_java_version(java_version: &str) -> anyhow::Result<()> {
    if conf::is_valid_version(java_version) {
        Ok(())
    } else {
        Err(anyhow!(
            "unsupported version '{java_version}': only major versions 8, 11, ... are supported"
        ))
    }
}

#[cfg(test)]
// `env::set_var` requires `unsafe` under edition 2024; the mutations here are
// guarded by `serial_test`.
#[allow(unsafe_code)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn owned(items: &[&str]) -> Vec<String> {
        items.iter().map(std::string::ToString::to_string).collect()
    }

    /// A client pointed at an address nothing listens on. Every command below
    /// must fail on its arguments before it would reach the network, so a
    /// connection error here would be the test itself reporting a regression.
    fn offline_client() -> AdoptiumClient {
        AdoptiumClient::new("http://127.0.0.1:1")
    }

    // -- shell_quote --

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

    /// Undo `shell_quote` the way a shell would, so the tests above assert a
    /// real round trip rather than a hand-copied expected string.
    fn strip_single_quotes(quoted: &str) -> String {
        let body = quoted
            .strip_prefix('\'')
            .and_then(|q| q.strip_suffix('\''))
            .expect("shell_quote must wrap its output in single quotes");
        body.replace(r"'\''", "'")
    }

    // -- cmd_* error paths --
    //
    // These used to be reachable only by spawning the binary, because each one
    // ended in `exit(1)`.

    #[test]
    fn cmd_env_rejects_an_unsupported_version() {
        let err = cmd_env(&offline_client(), Some("nope".to_string()), false, false)
            .expect_err("'nope' is not a major version");
        assert_eq!(
            format!("{:#}", err.error),
            "unsupported version 'nope': only major versions 8, 11, ... are supported"
        );
        assert!(err.hint.is_none(), "{:?}", err.hint);
    }

    /// The argument check has to come before the store lookup, so `--offline`
    /// reports the same thing the online form does rather than "no installed
    /// JDK matches Java nope".
    #[test]
    fn cmd_env_offline_rejects_an_unsupported_version() {
        let err = cmd_env(&offline_client(), Some("nope".to_string()), true, false)
            .expect_err("'nope' is not a major version");
        assert_eq!(
            format!("{:#}", err.error),
            "unsupported version 'nope': only major versions 8, 11, ... are supported"
        );
        assert!(err.hint.is_none(), "{:?}", err.hint);
    }

    /// `env` must not send the reader to `home`: the advice line names the
    /// command they actually ran.
    #[test]
    fn offline_java_home_names_the_calling_command_in_its_hint() {
        let store = JdkStore::at(tempdir().unwrap().path());
        let err = offline_java_home(&store, "99", "env").expect_err("the store is empty");
        assert_eq!(
            format!("{:#}", err.error),
            "no installed JDK matches Java 99"
        );
        assert_eq!(
            err.hint.as_deref(),
            Some("Run 'jlo env 99' without --offline to install it.")
        );
    }

    #[test]
    fn cmd_update_rejects_a_list_of_only_invalid_versions() {
        let err = cmd_update(&offline_client(), owned(&["abc"]), false)
            .expect_err("nothing was left to update");
        assert_eq!(
            format!("{:#}", err.error),
            "no valid Java versions provided to update"
        );
    }

    /// Same rejection as `update`, but the message names the verb the user
    /// actually typed - the two commands share the check, not the wording.
    #[test]
    fn cmd_install_rejects_a_list_of_only_invalid_versions() {
        let err = cmd_install(&offline_client(), owned(&["abc"]))
            .expect_err("nothing was left to install");
        assert_eq!(
            format!("{:#}", err.error),
            "no valid Java versions provided to install"
        );
    }

    // -- requested_versions --

    #[test]
    fn requested_versions_keeps_the_valid_entries_of_a_mixed_list() {
        let requested = requested_versions(owned(&["21", "abc", "25"]), "install")
            .expect("two of the three are valid");
        assert_eq!(
            requested,
            HashSet::from(["21".to_string(), "25".to_string()])
        );
    }

    /// A major named twice is one download, not two: the set is what reaches
    /// `install_each`.
    #[test]
    fn requested_versions_deduplicates() {
        let requested =
            requested_versions(owned(&["21", "21"]), "install").expect("21 is a valid major");
        assert_eq!(requested, HashSet::from(["21".to_string()]));
    }

    /// The usage line is advice printed *under* the error, so it travels with
    /// it rather than being printed where the failure happens.
    #[test]
    fn cmd_exec_carries_the_usage_hint_when_the_separator_is_missing() {
        let err = cmd_exec(&offline_client(), &owned(&["java", "-version"]))
            .expect_err("no '--' before the command");
        assert_eq!(
            format!("{:#}", err.error),
            "expected '--' before the command, e.g. jlo exec 21 -- java -version"
        );
        assert_eq!(
            err.hint.as_deref(),
            Some("Usage: jlo exec [VERSION] -- <COMMAND> [ARGS]...")
        );
    }

    // -- offline_java_home --
    //
    // The install directory is not configurable, so `jlo home
    // --offline` is covered here against an injected `JdkStore` rather than
    // by spawning the binary; the integration suite asserts only the exit
    // status and that no network call happens.

    /// A fake store holding one JDK directory, marked managed the way an
    /// install leaves it.
    fn store_with(base: &Path, version: &str) -> JdkStore {
        let dir = base.join(version);
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        std::fs::write(dir.join("bin").join("java"), "").unwrap();
        std::fs::File::create(dir.join(".jlo-managed")).unwrap();
        JdkStore::at(base)
    }

    #[test]
    fn offline_java_home_answers_from_the_store() {
        let dir = tempdir().unwrap();
        let store = store_with(dir.path(), "21.0.3+9");

        let path = offline_java_home(&store, "21", "home").expect("21 is installed");
        assert_eq!(path, dir.path().join("21.0.3+9"));
    }

    /// The exit status is the answer a script wants, and the hint has to name
    /// the command that would actually install it - the whole point of the
    /// flag is that this one did not.
    #[test]
    fn offline_java_home_fails_without_installing_anything() {
        let dir = tempdir().unwrap();
        let store = store_with(dir.path(), "21.0.3+9");

        let err = offline_java_home(&store, "17", "home").expect_err("17 is not installed");
        assert_eq!(
            format!("{:#}", err.error),
            "no installed JDK matches Java 17"
        );
        assert_eq!(
            err.hint.as_deref(),
            Some("Run 'jlo home 17' without --offline to install it.")
        );
        assert!(
            !dir.path().join("17").exists(),
            "--offline must not create anything"
        );
    }

    // -- unsourced_env_hint --

    /// The hint exists to hand the caller a command that does work without a
    /// sourcing shell, so it has to name both alternatives and carry the
    /// version the user actually asked for.
    #[test]
    fn unsourced_env_hint_names_both_alternatives() {
        let hint = unsourced_env_hint("21");
        assert!(hint.contains("did not change anything"), "{hint}");
        assert!(hint.contains("jlo exec 21 -- <command>"), "{hint}");
        assert!(
            hint.contains("export JAVA_HOME=\"$(jlo home 21)\""),
            "{hint}"
        );
    }

    // -- superseded_hint --

    #[test]
    fn superseded_hint_names_the_command_and_the_count() {
        let hint = superseded_hint(true, 2).expect("an install plus leftovers earns a hint");
        assert!(hint.contains('2'), "hint should say how many: {hint}");
        assert!(
            hint.contains("jlo prune"),
            "hint should name the command: {hint}"
        );
    }

    #[test]
    fn superseded_hint_singular_for_one() {
        let hint = superseded_hint(true, 1).unwrap();
        assert!(hint.contains("1 superseded JDK "), "{hint}");
    }

    /// Nothing was superseded, so pointing at `jlo prune` would send the user
    /// to a command that removes nothing.
    #[test]
    fn superseded_hint_silent_when_nothing_is_superseded() {
        assert!(superseded_hint(true, 0).is_none());
    }

    /// Every JDK was already current: the leftovers are pre-existing clutter,
    /// not something this run caused, and `jlo update` would nag on every run.
    #[test]
    fn superseded_hint_silent_when_nothing_was_installed() {
        assert!(superseded_hint(false, 3).is_none());
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

    #[test]
    fn child_path_prepends_java_bin() {
        assert_eq!(
            child_path("/jdk/21/bin", "/usr/bin:/bin").unwrap(),
            "/jdk/21/bin:/usr/bin:/bin"
        );
    }

    #[test]
    fn child_path_handles_empty_path() {
        assert_eq!(child_path("/jdk/21/bin", "").unwrap(), "/jdk/21/bin");
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

    #[test]
    fn update_path_handles_empty_path() {
        let jdk_base = Path::new("/home/u/.jdks");
        let result = update_path("/home/u/.jdks/21.0.12/bin", "", jdk_base).unwrap();
        assert_eq!(result.unwrap(), "/home/u/.jdks/21.0.12/bin:");
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

    #[test]
    #[serial_test::serial]
    fn jlo_home_dir_falls_back_to_dot_jlo_under_home() {
        unsafe {
            env::remove_var("JLO_HOME");
        }
        let expected = env::home_dir().unwrap().join(".jlo");
        assert_eq!(jlo_home_dir().unwrap(), expected);
    }

    #[test]
    #[serial_test::serial]
    fn jlo_home_dir_uses_env_var() {
        let dir = tempdir().unwrap();
        unsafe {
            env::set_var("JLO_HOME", dir.path());
        }
        let result = jlo_home_dir().unwrap();
        assert_eq!(result, dir.path());
        unsafe {
            env::remove_var("JLO_HOME");
        }
    }
}
