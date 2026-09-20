mod adoptium;
mod cli;
mod conf;
mod extract;
mod store;
mod ui;

use crate::adoptium::{AdoptiumClient, JdkMetadata};
use crate::store::JdkStore;
use crate::ui::InstallUi;
use anyhow::{Context, anyhow};
use clap::Parser;
use std::collections::HashSet;
use std::env;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::exit;
use tempfile::tempdir;

/// Name of J'Lo's state directory under `$HOME`, used when `JLO_HOME` is unset.
/// Must stay in sync with `install.sh`.
const JLO_HOME_DIR_NAME: &str = ".jlo";

/// A failed command: the error to report, plus the advice line that belongs
/// *under* it, if any.
///
/// Reporting an error is `main`'s job alone, so a command that wants to add a
/// hint has to hand it over rather than print it. Carrying it keeps the two
/// lines in the order the user has always seen - the error first, the dimmed
/// advice second - which printing at the failure site would invert.
#[derive(Debug)]
struct CommandError {
    error: anyhow::Error,
    hint: Option<String>,
}

impl CommandError {
    fn with_hint(error: anyhow::Error, hint: impl Into<String>) -> Self {
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
    if env::args().nth(1).as_deref() == Some("sing") {
        eprintln!("There are no Easter Eggs in this program. Trust me. 💃");
        return Ok(());
    }

    // TRANSITION SHIM - delete after the next release.
    //
    // The `version` subcommand was removed in favour of `-V`/`--version`.
    // But `jlo selfupdate` swaps the binary out from under an *already
    // resident* shell function: the old `jlo-init.sh` body (sourced before
    // the update ran) calls `"$J" version` itself, right after invoking
    // this same new binary to install itself. Without this shim, someone's
    // very first selfupdate onto this release ends with the new binary
    // rejecting the old wrapper's `version` call:
    //
    //   Version before update: 0.2.0
    //   ...installer output...
    //   Version after update: error: unrecognized subcommand 'version'
    //
    // The update itself succeeded, but it looks broken and the wrapper
    // returns a nonzero exit code. Print the bare crate version - not
    // clap's `jlo 0.2.0` form - so the old wrapper's
    // `echo -n "..."; "$J" --version` output still reads as a clean
    // version string.
    //
    // This intercepts the raw token before `Cli::parse()`, exactly like
    // the `sing` easter egg above and for the same reason: putting
    // `version` back in the `Command` enum would reintroduce it into
    // `--help`, generated completions, and clap's typo-suggestion engine,
    // which is precisely the drift this CLI rewrite exists to eliminate.
    if env::args().nth(1).as_deref() == Some("version") {
        println!(env!("CARGO_PKG_VERSION"));
        return Ok(());
    }

    let cli = cli::Cli::parse();

    let Some(command) = cli.command else {
        cli::print_help();
        return Ok(());
    };

    let api_url =
        env::var("JLO_ADOPTIUM_API_URL").unwrap_or_else(|_| adoptium::ADOPTIUM_API_URL.to_string());
    let client = AdoptiumClient::new(api_url);

    match command {
        cli::Command::Env { version } => cmd_env(&client, version),
        cli::Command::Home { version } => cmd_home(&client, version),
        cli::Command::Exec { args } => cmd_exec(&client, &args),
        cli::Command::List { offline } => cmd_list(&client, offline),
        cli::Command::Update { versions, all } => cmd_update(&client, versions, all),
        cli::Command::Clean => cmd_clean(),
        cli::Command::Init {
            version,
            global,
            force,
        } => cmd_init(&client, version, global, force),
        cli::Command::Selfupdate => Err(anyhow!(
            "self-update is handled by the jlo shell function. Source jlo-init.sh from your shell profile, or re-run the installer."
        )
        .into()),
        cli::Command::Completions { shell } => {
            cmd_completions(shell);
            Ok(())
        }
    }
}

/// Write a shell completion script to stdout.
///
/// The completion function registers against the command word `jlo`, which
/// resolves to the shell function from jlo-init.sh, so the wrapper is
/// transparent to completion.
fn cmd_completions(shell: clap_complete::Shell) {
    use clap::CommandFactory;

    let mut command = cli::Cli::command();
    clap_complete::generate(shell, &mut command, "jlo", &mut std::io::stdout());
}

/// Determine the requested major version: the explicit CLI argument if present,
/// otherwise the project `.jlorc` / user default config.
fn resolve_java_version_from(explicit: Option<String>) -> anyhow::Result<String> {
    let java_version = match explicit {
        Some(version) => version,
        None => conf::load_config_java_version()?,
    };

    assert_java_version(&java_version)?;
    Ok(java_version)
}

fn cmd_env(client: &AdoptiumClient, version: Option<String>) -> Result<(), CommandError> {
    let java_version = resolve_java_version_from(version)?;
    setup(client, &java_version)?;
    Ok(())
}

fn cmd_home(client: &AdoptiumClient, version: Option<String>) -> Result<(), CommandError> {
    let java_version = resolve_java_version_from(version)?;
    let store = JdkStore::discover()?;
    let java_home = resolve_java_home(client, &store, &java_version)?;
    println!("{}", java_home.to_string_lossy());
    Ok(())
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
        .and_then(|java_version| {
            let store = JdkStore::discover()?;
            resolve_java_home(client, &store, &java_version)
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

    if offline {
        ui::offline_list(&installed, &store);
    } else {
        let available = client.available_jdks().map_err(|e| {
            CommandError::with_hint(
                e,
                "Use 'jlo list --offline' to list the JDKs already installed.",
            )
        })?;
        ui::remote_list(&available, &installed);
    }

    Ok(())
}

fn cmd_clean() -> Result<(), CommandError> {
    let store = JdkStore::discover()?;
    let report = store.clean().context("could not clean JDKs")?;
    ui::clean_report(&report);
    Ok(())
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

fn cmd_update(
    client: &AdoptiumClient,
    versions: Vec<String>,
    all: bool,
) -> Result<(), CommandError> {
    let store = JdkStore::discover()?;
    let mut versions_to_install: HashSet<String> = HashSet::new();

    // `--all` and explicit versions are mutually exclusive (clap enforces it),
    // so these three arms are the whole input space.
    if all {
        store
            .installed_majors()
            .context("could not determine installed JDK versions")?
            .into_iter()
            .for_each(|v| {
                versions_to_install.insert(v.to_string());
            });

        if versions_to_install.is_empty() {
            return Err(anyhow!("no installed JDKs to update").into());
        }
    } else if versions.is_empty() {
        versions_to_install.insert(conf::load_config_java_version()?);
    } else {
        for v in versions {
            if conf::is_valid_version(&v) {
                versions_to_install.insert(v);
            } else {
                ui::warning!("skipping invalid version '{v}'");
            }
        }

        if versions_to_install.is_empty() {
            return Err(anyhow!("no valid Java versions provided to update").into());
        }
    }

    // Sort versions_to_install alphabetically for consistent processing order
    let mut versions_to_install: Vec<_> = versions_to_install.into_iter().collect();
    versions_to_install.sort();

    let mut installed_any = false;
    for java_version in versions_to_install {
        installed_any |= update(client, &store, &java_version)?;
    }

    // An update leaves the superseded minor on disk on purpose - a command
    // that downloads should not also delete, and the old JDK may still be
    // wired into an open shell or an IDE. Point at `jlo clean` instead of
    // doing it here.
    if let Some(hint) = superseded_hint(installed_any, count_superseded(&store)) {
        ui::hint!("{hint}");
    }

    Ok(())
}

/// How many installs `jlo clean` would remove, or 0 if that cannot be
/// determined. A hint is not worth failing an otherwise successful update, so
/// an unreadable JDK directory just means no hint.
fn count_superseded(store: &JdkStore) -> usize {
    store.superseded_count().unwrap_or(0)
}

/// The line `jlo update` ends on when this run left an older minor behind.
///
/// `None` when there is nothing to say: no install happened (the leftovers
/// predate this run, and nagging on every no-op update trains the user to
/// ignore the line), or nothing is superseded.
fn superseded_hint(installed_any: bool, superseded: usize) -> Option<String> {
    if !installed_any || superseded == 0 {
        return None;
    }

    let plural = if superseded == 1 { "" } else { "s" };
    Some(format!(
        "{superseded} superseded JDK{plural} still installed - run 'jlo clean' to remove {}.",
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

/// Emit the `export` lines for the requested version.
///
/// Nothing is written to stderr on this path, even when the environment does
/// change: the autoload hook calls it from `PROMPT_COMMAND`/`chpwd`, so any
/// status line here would print on every new shell and every `cd`. Exporting a
/// variable lasts only as long as the shell and is implied by the command the
/// user ran - it is the install (a JDK on disk) that earns a line, not this.
fn setup(client: &AdoptiumClient, java_version: &str) -> anyhow::Result<()> {
    let store = JdkStore::discover()?;
    let java_home = resolve_java_home(client, &store, java_version)?;

    let current_java_home = env::var("JAVA_HOME").unwrap_or_default();
    if current_java_home != java_home.to_string_lossy() {
        println!("export JAVA_HOME=\"{}\"", java_home.to_string_lossy());
    }

    let java_bin_path = java_home.join("bin").to_string_lossy().into_owned();
    let current_path = env::var("PATH").unwrap_or_default();
    if let Some(updated_path) = update_path(&java_bin_path, &current_path, store.base())? {
        println!("export PATH=\"{updated_path}\"");
    }

    Ok(())
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
/// `$HOME`: interactive shells get `JLO_HOME` from `jlo-init.sh`, but scripts
/// and CI invoking `jlo-bin` directly do not, and those two must resolve the
/// same file.
fn jlo_home_dir() -> anyhow::Result<PathBuf> {
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

    // -- cmd_* error paths --
    //
    // These used to be reachable only by spawning the binary, because each one
    // ended in `exit(1)`.

    #[test]
    fn cmd_env_rejects_an_unsupported_version() {
        let err = cmd_env(&offline_client(), Some("nope".to_string()))
            .expect_err("'nope' is not a major version");
        assert_eq!(
            format!("{:#}", err.error),
            "unsupported version 'nope': only major versions 8, 11, ... are supported"
        );
        assert!(err.hint.is_none(), "{:?}", err.hint);
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

    // -- superseded_hint --

    #[test]
    fn superseded_hint_names_the_command_and_the_count() {
        let hint = superseded_hint(true, 2).expect("an install plus leftovers earns a hint");
        assert!(hint.contains('2'), "hint should say how many: {hint}");
        assert!(
            hint.contains("jlo clean"),
            "hint should name the command: {hint}"
        );
    }

    #[test]
    fn superseded_hint_singular_for_one() {
        let hint = superseded_hint(true, 1).unwrap();
        assert!(hint.contains("1 superseded JDK "), "{hint}");
    }

    /// Nothing was superseded, so pointing at `jlo clean` would send the user
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
