mod adoptium;
mod cli;
mod conf;
mod extract;
mod install;
mod resolve;
mod selfupdate;
mod shellenv;
mod store;
mod ui;
mod version;

use crate::adoptium::AdoptiumClient;
use crate::resolve::{
    assert_java_version, offline_java_home, requested_versions, resolve_java_home,
    resolve_java_version_from,
};
use crate::shellenv::{parse_exec_args, restore_leading_separator, shell_quote, update_path};
use crate::store::{JdkStore, RemoveError};
use anyhow::{Context, anyhow};
use clap::Parser;
use std::collections::HashSet;
use std::env;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::exit;

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
        cli::Command::Env { version, offline } => cmd_env(&client, version, offline),
        cli::Command::Home { version, offline } => cmd_home(&client, version, offline),
        cli::Command::Exec { args } => cmd_exec(&client, &args),
        cli::Command::Current => cmd_current(),
        cli::Command::List { offline } => cmd_list(&client, offline),
        cli::Command::Install { versions } => cmd_install(&client, versions),
        cli::Command::Update { versions, all } => cmd_update(&client, versions, all),
        cli::Command::Remove {
            versions,
            superseded,
        } => cmd_remove(&versions, superseded),
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

fn cmd_env(
    client: &AdoptiumClient,
    version: Option<String>,
    offline: bool,
) -> Result<(), CommandError> {
    let store = JdkStore::discover()?;
    let resolved = resolve_java_version_from(version, &store, client, offline)?;
    setup(client, &store, &resolved.version, offline)?;

    // The exports on stdout are the whole effect of this command. If stdout is
    // a terminal nothing captured them, so the exit code says success while
    // nothing happened - the failure shape that sends a CI step, a Makefile
    // recipe or an agent looking for the problem somewhere else entirely.
    if std::io::stdout().is_terminal() {
        ui::hint!("{}", ui::unsourced_env_hint(&resolved.version));
    }

    Ok(())
}

fn cmd_home(
    client: &AdoptiumClient,
    version: Option<String>,
    offline: bool,
) -> Result<(), CommandError> {
    let store = JdkStore::discover()?;
    let java_version = resolve_java_version_from(version, &store, client, offline)?.version;
    let java_home = if offline {
        offline_java_home(&store, &java_version, "home")?
    } else {
        resolve_java_home(client, &store, &java_version)?
    };
    // The bare path on stdout, for `$(jlo home 21)`. Lossy would hand the
    // caller a path that does not exist; see `path_str`.
    println!("{}", path_str(&java_home)?);
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

/// Resolve the JDK (installing on demand) and replace the current process with
/// the command. On non-Unix targets `exec` is unsupported, so bail out *before*
/// downloading anything.
#[cfg(unix)]
fn run_exec(client: &AdoptiumClient, version: Option<String>, command: &[String]) -> ! {
    // This function never returns, so it reports its own failures rather than
    // handing them back to `main`.
    // No --offline flag on `exec`: the command's whole job is to run
    // something on that JDK, so declining to fetch it would only move the
    // failure. The cascade may therefore reach its last stage here.
    let java_home = JdkStore::discover()
        .and_then(|store| {
            let resolved = resolve_java_version_from(version, &store, client, false)?;
            resolve_java_home(client, &store, &resolved.version)
        })
        .unwrap_or_else(|e| {
            ui::error!("{e:#}");
            exit(1);
        });

    shellenv::exec_command(&java_home, command);
}

// A real `execvp` is Unix-only. A native Windows build would replace this with a
// spawn-and-wait fallback that propagates the child's exit code.
#[cfg(not(unix))]
fn run_exec(_client: &AdoptiumClient, _version: Option<String>, _command: &[String]) -> ! {
    ui::error!("'jlo exec' is not supported on this platform");
    exit(1);
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
            ui::NO_ACTIVE_JDK_HINT,
        ));
    };

    let store = JdkStore::discover()?;

    // Asked before the listing, because the listing cannot answer it. A
    // `$JAVA_HOME` inside the store that is simply *gone* is what `jlo
    // remove` on the live JDK leaves behind, and reporting that as a JDK set
    // outside jlo would be wrong - the install was ours.
    //
    // Existence is the whole of the test, and it has to be asked of
    // `$JAVA_HOME` itself rather than inferred from the listing, for two
    // reasons that pull in opposite directions. A directory the listing
    // cannot name may be perfectly present: a vendor-named entry
    // (`temurin-21.0.1`), which is what the IDE's own downloads land as,
    // shares the store by design and is deliberately unlistable - `jlo list
    // --offline` calls it foreign, and this has to agree. And a version the
    // listing *can* name may be gone: on macOS `$JAVA_HOME` is the bundle's
    // `Contents/Home`, and `owns` matches that spelling without asking the
    // filesystem anything - deliberately, since it is also the guard that
    // refuses to delete the live JDK and must not be switchable off by a
    // directory that cannot be stat'd.
    if is_inside(store.base(), &java_home) && !java_home.exists() {
        return Err(CommandError::with_hint(
            anyhow!(
                "$JAVA_HOME points at a jlo install that is no longer there ({}).",
                java_home.display()
            ),
            ui::NO_ACTIVE_JDK_HINT,
        ));
    }

    let installed = store.list().context("could not list installed JDKs")?;

    let Some(version) = store.active_version(&installed, Some(&java_home)) else {
        // A JDK jlo does not manage. No config is consulted: whatever is
        // pinned, jlo is not what put this here, and the path says that
        // completely.
        ui::print_lines([ui::provenance_line(&ui::Active {
            path: java_home,
            version: None,
            major: None,
            source: Some(conf::Source::Foreign),
            pinned_elsewhere: None,
        })]);
        return Ok(());
    };

    // Whether the active JDK is the *exact* install stage 3 of the cascade
    // would pick, not merely one of its major. `list` yields newest first, so
    // that is its head. The distinction matters because the cascade resolves
    // a major and `jlo env` then takes the newest build of it: a shell on
    // 21.0.5 with 21.0.6 sitting beside it agrees on the major but is not
    // what a bare `jlo env` would hand back, so calling it "the newest
    // installed JDK" would claim more than is true. The version floor is
    // checked here for the same reason it is checked in `newest_installed`: a
    // store of nothing but pre-8 JDKs is one the cascade walks straight past.
    let is_newest_install = installed.first().is_some_and(|newest| {
        newest.version == version && conf::is_valid_version(&newest.major.to_string())
    });

    let mut active = ui::Active {
        major: installed
            .iter()
            .find(|jdk| jdk.version == version)
            .map(|jdk| jdk.major),
        path: java_home,
        version: Some(version),
        source: None,
        pinned_elsewhere: None,
    };

    // The same cascade `jlo env` resolves through, stopped after stage 3:
    // this command never touches the network, so "download the latest
    // release" is not an answer it can give - and it would be a strange one
    // anyway, since something is demonstrably active already.
    //
    // A config that fails to load is still a failure: it is a file the user
    // wrote and meant, and answering around it would hide the mistake.
    if let Some(configured) = conf::find()? {
        if configured.version.parse::<i64>().ok() == active.major {
            active.source = Some(configured.source);
        } else {
            active.pinned_elsewhere = Some(configured);
        }
    } else if is_newest_install {
        // Stage 3. No mismatch counterpart: nobody asked for the newest
        // installed JDK, so a shell that is on something else is not wrong
        // about anything and gets no warning - it reads as "nothing pinned".
        active.source = Some(conf::Source::NewestInstalled);
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

/// Delete installed JDKs, selected either by name or by the superseded rule.
///
/// One verb, two selectors: clap guarantees exactly one of them arrives, so
/// the split here is the whole difference between them. They keep separate
/// reports because they answer differently for an install jlo did not make -
/// the rule skips it, a name refuses.
fn cmd_remove(versions: &[String], superseded: bool) -> Result<(), CommandError> {
    let store = JdkStore::discover()?;
    let active = active_java_home();

    // Both selectors report what they did and then fail on the same
    // condition. A deletion that could not be made is the one outcome that
    // must not exit 0: the report has already named each failure, but a
    // script chaining `jlo remove ... && ...` reads the status, not the
    // lines, and would go on believing the store had been reduced.
    let failures = if superseded {
        let report = store
            .prune(active.as_deref())
            .context("could not remove superseded JDKs")?;
        ui::prune_report(&report);
        report.failures.len()
    } else {
        let report = store.remove(versions, active.as_deref())?;
        ui::remove_report(&report);
        report.failures.len()
    };

    if failures > 0 {
        return Err(anyhow!(
            "{failures} JDK{} could not be removed",
            if failures == 1 { "" } else { "s" }
        )
        .into());
    }
    Ok(())
}

/// A path as a `&str`, or an error naming it.
///
/// Every path jlo hands out - on stdout, into an `export`, or to `execvp` -
/// goes through here rather than through `to_string_lossy`, which silently
/// replaces undecodable bytes and so answers with a path that does not exist.
/// There is no useful thing jlo can do with a JDK it cannot name.
fn path_str(path: &Path) -> anyhow::Result<&str> {
    path.to_str()
        .with_context(|| format!("path is not valid UTF-8: {}", path.display()))
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
    let versions = requested_versions(versions, "install", &store, client)?;
    Ok(store::install_each(client, &store, versions)?)
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
        requested_versions(versions, "update", &store, client)?
    };

    Ok(store::install_each(client, &store, versions_to_install)?)
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
    store: &JdkStore,
    java_version: &str,
    offline: bool,
) -> Result<(), CommandError> {
    let java_home = if offline {
        offline_java_home(store, java_version, "env")?
    } else {
        resolve_java_home(client, store, java_version)?
    };

    // Collected rather than printed as they are decided: both lines are one
    // environment, and `print_lines` is also the only writer here that treats
    // a closed pipe as an ending rather than panicking - `jlo env | head` is
    // an ordinary thing to type.
    let mut exports = Vec::new();

    let current_java_home = env::var("JAVA_HOME").unwrap_or_default();
    // `to_string_lossy` is the wrong shape on this path for the same reason
    // `unwrap_or_default` was wrong for `PATH`: it substitutes U+FFFD for
    // bytes it cannot decode and hands back a path that does not exist, and
    // the caller then exports it as `JAVA_HOME`. An undecodable install
    // directory is unusable, so say so rather than exporting a near miss.
    let java_home_str = path_str(&java_home)?;
    if current_java_home != java_home_str {
        exports.push(format!("export JAVA_HOME={}", shell_quote(java_home_str)));
    }

    let java_bin = java_home.join("bin");
    let java_bin_path = path_str(&java_bin)?.to_string();
    let current_path = shellenv::current_path()?;
    if let Some(updated_path) = update_path(&java_bin_path, &current_path, store.base())? {
        exports.push(format!("export PATH={}", shell_quote(&updated_path)));
    }

    ui::print_lines(exports);

    Ok(())
}

/// J'Lo's own state directory — where `default.jlorc` lives.
///
/// The fallback must match what `install.sh` exports (`$HOME/.jlo`), not bare
/// `$HOME`: interactive shells get `JLO_HOME` from the generated `jlo.sh`, but scripts
/// and CI invoking `jlo-bin` directly do not, and those two must resolve the
/// same file.
pub(crate) fn jlo_home_dir() -> anyhow::Result<PathBuf> {
    // An empty `JLO_HOME` is treated as unset, as `$JAVA_HOME` is: an
    // exported-but-empty variable is how a shell spells "I did not set this",
    // and taking it literally roots the whole layout at `/`.
    let path = env::var_os("JLO_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| env::home_dir().map(|home| home.join(JLO_HOME_DIR_NAME)))
        .context("could not determine home directory.")?;

    // A relative `JLO_HOME` does not name one directory, it names a different
    // one from every working directory - and the installed layout is full of
    // paths that outlive the process that wrote them: the `~/.local/bin/jlo`
    // symlink target, which a relative path resolves against the *link's*
    // directory, and the generated stubs, which the user sources from
    // wherever they happen to be. Refuse it rather than write an install that
    // works only from the directory it was made in.
    if !path.is_absolute() {
        return Err(anyhow!(
            "JLO_HOME must be an absolute path, but is '{}'",
            path.display()
        ));
    }

    // Checked once, here, rather than at each of the places that write it
    // out. `install` spells this path into the generated stubs and into
    // `install-receipt.json`, and it spells it with `display()`, which
    // substitutes U+FFFD for bytes it cannot decode: the files would land at
    // the real path while naming a different one, leaving a wrapper pointing
    // at a directory that does not exist and a receipt that fails every
    // later ownership check. There is nothing jlo can do with a home it
    // cannot write down.
    if path.to_str().is_none() {
        return Err(anyhow!(
            "JLO_HOME is not valid UTF-8, so jlo cannot write it into the shell code it generates: '{}'",
            path.display()
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
        let err = cmd_env(&offline_client(), Some("nope".to_string()), false)
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
        let err = cmd_env(&offline_client(), Some("nope".to_string()), true)
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

    /// The three values that cannot be a home, refused here rather than at
    /// each of the places that write the layout out. `install` spells this
    /// path into the generated stubs, into the `~/.local/bin/jlo` symlink
    /// target and into `install-receipt.json`, so a value that survives to
    /// there produces an install that is wrong in a different way for each
    /// of them.
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
