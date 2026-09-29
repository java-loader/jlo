// `exec` and the installers' executable bits are Unix; a build that compiled
// elsewhere would only fail at run time.
#[cfg(not(unix))]
compile_error!("jlo supports Linux and macOS only");

mod adoptium;
mod cli;
mod conf;
mod extract;
mod install;
mod request;
mod resolve;
mod selfupdate;
mod shellenv;
mod store;
mod ui;
mod version;

use crate::adoptium::AdoptiumClient;
use crate::request::Request;
use crate::resolve::{Verb, requested_versions};
use crate::shellenv::parse_exec_args;
use crate::store::{JdkStore, RemoveError};
use anyhow::{Context, anyhow};
use clap::Parser;
use std::env;
use std::io::IsTerminal;
use std::path::PathBuf;
use std::process::exit;

/// Used when `JLO_HOME` is unset. Must stay in sync with `install.sh`.
pub(crate) const JLO_HOME_DIR_NAME: &str = ".jlo";

/// A failed command: the error, plus the advice line that belongs *under* it.
/// Carried to `main` because printing the hint at the failure site would put
/// it above the error.
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

impl From<RemoveError> for CommandError {
    fn from(error: RemoveError) -> Self {
        let hint = ui::remove_refusal_hint(&error);
        // Only the store variant carries a context chain worth preserving.
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

/// The one place a command failure becomes a message and exit 1. The other
/// exits: `shellenv::exec_command`, which cannot return, and `ui::print_lines`,
/// which fails on the stream it writes to.
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
    // Exactly one prefix is stripped: a second one is left for clap to reject.
    let mut argv: Vec<String> = env::args().collect();
    let wrapped = argv.get(1).is_some_and(|arg| arg == shellenv::WRAPPED);
    if wrapped {
        argv.remove(1);
    }
    let first = argv.get(1).map(String::as_str);

    // Not a clap subcommand: `hide = true` would still leak it into generated
    // completions and "did you mean" suggestions.
    if first == Some("sing") {
        eprintln!("There are no Easter Eggs in this program. Trust me. 💃");
        return Ok(());
    }

    // Likewise: it must not show up in the completions it writes.
    if first == Some(install::VERB) {
        return install::cmd_install(&argv[2..]);
    }

    let api_url =
        env::var("JLO_ADOPTIUM_API_URL").unwrap_or_else(|_| adoptium::ADOPTIUM_API_URL.to_string());
    let client = AdoptiumClient::new(api_url);

    // clap's `trailing_var_arg` eats a `--` right after `exec`, the separator
    // `exec` requires.
    if first == Some("exec") {
        return cmd_exec(&client, &argv[2..]);
    }

    // `parse_from`, not `parse`: the latter would reread the prefix from the
    // process's own argv.
    let cli = cli::Cli::parse_from(argv);

    let Some(command) = cli.command else {
        cli::print_help();
        return Ok(());
    };

    match command {
        cli::Command::Env { version, offline } => cmd_env(&client, version, offline, wrapped),
        cli::Command::Home { version, offline } => cmd_home(&client, version, offline),
        // Kept in `cli` only so help and the completion scripts list it.
        cli::Command::Exec { .. } => unreachable!("exec is intercepted from raw argv before clap"),
        cli::Command::Current => cmd_current(),
        cli::Command::List { offline } => cmd_list(&client, offline),
        cli::Command::Install { versions } => cmd_install(&client, versions, wrapped),
        cli::Command::Update { versions } => cmd_update(&client, versions, wrapped),
        cli::Command::Remove {
            versions,
            superseded,
        } => cmd_remove(&versions, superseded),
        cli::Command::Init {
            version,
            global,
            force,
        } => cmd_init(&version, global, force),
        cli::Command::Selfupdate => selfupdate::cmd_selfupdate(wrapped),
        cli::Command::Completions { shell } => {
            cmd_completions(shell);
            Ok(())
        }
    }
}

fn cmd_completions(shell: cli::CompletionShell) {
    use std::io::Write as _;

    // A closed stdout (`| head`) is not an error worth reporting.
    let _ = std::io::stdout().write_all(&cli::completion_script(shell.into()));
}

/// No status line on stderr for a changed environment: the autoload hook
/// calls this on every new shell and every `cd`.
///
/// `offline` is the whole of the "a `cd` must not start a download" rule,
/// decided in Rust rather than in the shell hook.
///
/// Resolution fails before anything reaches stdout: a partial export would be
/// worse than none.
fn cmd_env(
    client: &AdoptiumClient,
    version: Option<String>,
    offline: bool,
    wrapped: bool,
) -> Result<(), CommandError> {
    let store = JdkStore::discover()?;
    let target = resolve::java_home(client, &store, version, offline, Verb::Env)?;
    let exports = shellenv::export_lines(
        &target.java_home,
        active_java_home().as_deref(),
        &shellenv::current_path()?,
        store.base(),
    )?;
    shellenv::emit(&exports, wrapped)?;

    // On a terminal nothing captured the exports: exit 0 with nothing changed
    // would send a CI step or an agent looking elsewhere for the problem.
    if std::io::stdout().is_terminal() {
        ui::hint!("{}", ui::unsourced_env_hint(&target.request.to_string()));
    }

    Ok(())
}

fn cmd_home(
    client: &AdoptiumClient,
    version: Option<String>,
    offline: bool,
) -> Result<(), CommandError> {
    let store = JdkStore::discover()?;
    let java_home = resolve::java_home(client, &store, version, offline, Verb::Home)?.java_home;
    // Lossy would hand `$(jlo home 21)` a path that does not exist.
    println!("{}", shellenv::path_str(&java_home)?);
    Ok(())
}

/// Diverges on success: `shellenv::exec_command` replaces the process image.
fn cmd_exec(client: &AdoptiumClient, args: &[String]) -> Result<(), CommandError> {
    // Help only before the `--`: anything after it belongs to the child.
    let separator = args.iter().position(|a| a == "--").unwrap_or(args.len());
    // `--help` wins over `-h` wherever each appears: `true` sorts above `false`.
    let help = args[..separator]
        .iter()
        .filter_map(|a| match a.as_str() {
            "--help" => Some(true),
            "-h" => Some(false),
            _ => None,
        })
        .max();
    if let Some(long) = help {
        cli::print_exec_help(long);
        return Ok(());
    }

    let (version, command) = parse_exec_args(args)
        .map_err(|e| CommandError::with_hint(e, format!("Usage: {}", cli::EXEC_USAGE)))?;

    // Never offline: declining to fetch the JDK would only move the failure.
    let store = JdkStore::discover()?;
    // `verb` only matters offline.
    let target = resolve::java_home(client, &store, version, false, Verb::Home)?;
    shellenv::exec_command(&target.java_home, &command)
}

fn cmd_list(client: &AdoptiumClient, offline: bool) -> Result<(), CommandError> {
    let store = JdkStore::discover()?;
    let installed = store.list().context("could not list installed JDKs")?;

    let active = store.active_version(&installed, active_java_home().as_deref());
    let groups = store::group_by_name(&installed);
    let names = store
        .foreign()
        .context("could not list the other directories in the JDK store")?;
    let foreign = ui::Foreign {
        names: &names,
        base: store.base(),
    };

    if offline {
        ui::offline_list(&groups, active.as_deref(), &foreign);
    } else {
        let available = client.available_jdks().map_err(|e| {
            CommandError::with_hint(
                e,
                "Use 'jlo list --offline' to list the JDKs already installed.",
            )
        })?;
        ui::remote_list(&available, &groups, active.as_deref(), &foreign);
    }

    Ok(())
}

/// Starts from the live `$JAVA_HOME`, not `.jlorc`: the two can disagree, and
/// saying so is most of what this command is for.
///
/// Exit 1 exactly when stdout is empty, so `VER=$(jlo current)` never hands
/// back an empty string over a success code.
fn cmd_current() -> Result<(), CommandError> {
    let Some(java_home) = active_java_home() else {
        return Err(CommandError::with_hint(
            anyhow!("No JDK is active."),
            ui::NO_ACTIVE_JDK_HINT,
        ));
    };

    let store = JdkStore::discover()?;
    let active = resolve::provenance(&store, java_home, conf::find)?;
    ui::print_lines([ui::provenance_line(&active)]);

    // After the answer and with exit 0: "what is active" has an answer, and a
    // stale shell in a pinned project is when it is most wanted.
    if let Some(pinned) = &active.pinned_elsewhere {
        ui::pin_mismatch(pinned);
    }

    Ok(())
}

fn cmd_remove(versions: &[String], superseded: bool) -> Result<(), CommandError> {
    let store = JdkStore::discover()?;
    let active = active_java_home();

    // A failed deletion must not exit 0: `jlo remove ... && ...` reads the
    // status, not the report.
    let report = if superseded {
        store
            .prune(active.as_deref())
            .context("could not remove superseded JDKs")?
    } else {
        store.remove(versions, active.as_deref())?
    };
    ui::remove_report(&report);

    removal_failed(report.failures.len(), "JDK")
}

fn removal_failed(failures: usize, noun: &str) -> Result<(), CommandError> {
    if failures == 0 {
        return Ok(());
    }
    Err(anyhow!(
        "{failures} {noun}{} could not be removed",
        ui::plural(failures)
    )
    .into())
}

/// Read here rather than in `JdkStore`, so the store's guards are testable
/// without mutating the process environment.
fn active_java_home() -> Option<PathBuf> {
    env::var_os("JAVA_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn cmd_init(version: &str, global: bool, force: bool) -> Result<(), CommandError> {
    // Parsed rather than checked, so the file holds the name jlo would print.
    let request = Request::parse(version)?;

    conf::init(request, global, force).map_err(|e| {
        let already_exists = e.is::<conf::AlreadyExists>();
        let e = e.context("could not create config file");
        if already_exists {
            CommandError::with_hint(e, "Re-run with --force to overwrite it.")
        } else {
            e.into()
        }
    })
}

/// The names given, or the one the cascade resolves.
fn cmd_install(
    client: &AdoptiumClient,
    versions: Vec<String>,
    wrapped: bool,
) -> Result<(), CommandError> {
    let store = JdkStore::discover()?;
    let requests = requested_versions(versions, "install", &store, client)?;
    install_names(client, &store, requests, wrapped)
}

/// Asks about a newer J'Lo after the update whatever its outcome - a newer
/// J'Lo may be the fix - and after the payload is written, so it stays the
/// update's own.
fn cmd_update(
    client: &AdoptiumClient,
    versions: Vec<String>,
    wrapped: bool,
) -> Result<(), CommandError> {
    let result = update_jdks(client, versions, wrapped);
    selfupdate::announce_newer_release();
    result
}

/// The names given, or every installed name, pre-release streams included.
fn update_jdks(
    client: &AdoptiumClient,
    versions: Vec<String>,
    wrapped: bool,
) -> Result<(), CommandError> {
    let store = JdkStore::discover()?;
    let requests = if versions.is_empty() {
        let installed = store
            .installed_requests()
            .context("could not determine installed JDK versions")?;
        if installed.is_empty() {
            return Err(anyhow!("no installed JDKs to update").into());
        }
        installed
    } else {
        requested_versions(versions, "update", &store, client)?
    };
    install_names(client, &store, requests, wrapped)
}

/// The build `$JAVA_HOME` points at goes too only when `wrapped`: only then is
/// stdout known to be evaluated. The payload is written and flushed before
/// anything is deleted, so the wrapper still moves the shell even if the
/// binary fails afterwards.
fn install_names(
    client: &AdoptiumClient,
    store: &JdkStore,
    requests: Vec<Request>,
    wrapped: bool,
) -> Result<(), CommandError> {
    let active = active_java_home();
    let run = store::install_each(
        client,
        store,
        requests,
        active.as_deref(),
        wrapped.then(shellenv::Payload::stdout),
    );
    ui::update_report(&run);

    if let Some(e) = run.error {
        return Err(e);
    }
    removal_failed(run.failure_count(), "superseded JDK")
}

/// The fallback must match what `install.sh` exports: scripts calling
/// `jlo-bin` directly have no `JLO_HOME` and must resolve the same files.
pub(crate) fn jlo_home_dir() -> anyhow::Result<PathBuf> {
    // Empty is unset: taken literally it roots the whole layout at `/`.
    let path = env::var_os("JLO_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| env::home_dir().map(|home| home.join(JLO_HOME_DIR_NAME)))
        .context("could not determine home directory.")?;

    // The layout is full of paths that outlive this process (the symlink
    // target, the stubs), and a relative one names a different directory
    // from everywhere they are used.
    if !path.is_absolute() {
        return Err(anyhow!(
            "JLO_HOME must be an absolute path, but is '{}'",
            path.display()
        ));
    }

    // The stubs spell this path with `display()`, which substitutes U+FFFD:
    // the files would name a directory that does not exist.
    if path.to_str().is_none() {
        return Err(anyhow!(
            "JLO_HOME is not valid UTF-8, so jlo cannot write it into the shell code it generates: '{}'",
            path.display()
        ));
    }

    // A newline would split a copied profile line or end a generated comment
    // early, and no quoting survives that.
    if path.to_string_lossy().chars().any(char::is_control) {
        return Err(anyhow!(
            "JLO_HOME must not contain control characters (a newline, say): '{}'",
            path.display().to_string().escape_debug()
        ));
    }

    Ok(path)
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
            "unsupported version 'nope': expected a major version (8, 11, 21) or a pre-release stream ('28-ea')"
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
            "unsupported version 'nope': expected a major version (8, 11, 21) or a pre-release stream ('28-ea')"
        );
        assert!(err.hint.is_none(), "{:?}", err.hint);
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

    #[test]
    #[serial_test::serial]
    fn jlo_home_dir_uses_env_var() {
        let dir = tempdir().unwrap();
        let result = with_jlo_home(dir.path().as_os_str(), jlo_home_dir).unwrap();
        assert_eq!(result, dir.path());
    }

    /// The values that cannot be a home, refused here rather than at
    /// each of the places that write the layout out. `install` spells this
    /// path into the generated stubs and into the `~/.local/bin/jlo` symlink
    /// target, so a value that survives to there produces an install that is
    /// wrong in a different way for each of them.
    #[test]
    #[serial_test::serial]
    fn jlo_home_dir_refuses_a_value_it_could_not_write_down() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        // Empty: a shell spells "unset" this way, and taking it literally
        // roots the whole layout at `/`.
        let empty = with_jlo_home(&OsString::new(), jlo_home_dir);
        assert_eq!(
            empty.expect("empty falls back to $HOME/.jlo"),
            env::home_dir().unwrap().join(".jlo")
        );

        // Relative: a different directory from every working directory.
        let relative = with_jlo_home(&OsString::from("jlo-home"), jlo_home_dir)
            .expect_err("a relative home is refused");
        assert!(relative.to_string().contains("absolute"), "{relative}");

        // Undecodable: written into the stubs as U+FFFD, naming a directory
        // that does not exist.
        let undecodable = OsString::from_vec(vec![b'/', b't', b'm', b'p', b'/', 0xff]);
        let err = with_jlo_home(&undecodable, jlo_home_dir).expect_err("refused");
        assert!(err.to_string().contains("not valid UTF-8"), "{err}");

        // A newline: splits a copied profile line, or ends a generated comment early.
        let newline = with_jlo_home(&OsString::from("/tmp/jlo\nx"), jlo_home_dir)
            .expect_err("a newline is refused");
        assert!(
            newline.to_string().contains("control characters"),
            "{newline}"
        );
    }

    /// Run `f` with `JLO_HOME` set to `value`, restoring the variable after.
    fn with_jlo_home<T>(value: &std::ffi::OsStr, f: impl FnOnce() -> T) -> T {
        let previous = env::var_os("JLO_HOME");
        unsafe {
            env::set_var("JLO_HOME", value);
        }
        let out = f();
        unsafe {
            match previous {
                Some(previous) => env::set_var("JLO_HOME", previous),
                None => env::remove_var("JLO_HOME"),
            }
        }
        out
    }
}
