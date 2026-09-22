mod adoptium;
mod cli;
mod conf;
mod extract;
mod install;
mod selfupdate;
mod shellenv;
mod store;
mod ui;
mod version;

use crate::adoptium::{AdoptiumClient, JdkMetadata};
use crate::shellenv::{parse_exec_args, restore_leading_separator, shell_quote, update_path};
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

/// Determine the requested major version: the explicit CLI argument if
/// present, otherwise the fallback cascade below.
///
/// Returns where the version came from as well as what it is, because
/// `jlo current` reports it, and re-deriving it there
/// would be a second spelling of the same walk.
fn resolve_java_version_from(
    explicit: Option<String>,
    store: &JdkStore,
    client: &AdoptiumClient,
    offline: bool,
) -> anyhow::Result<conf::Resolved> {
    let resolved = match explicit {
        Some(version) => conf::Resolved {
            version,
            source: conf::Source::Argument,
        },
        None => cascade(conf::find()?, newest_installed(store), offline, || {
            client
                .latest_major()
                .context("could not fetch latest JDK version")
        })?,
    };

    assert_java_version(&resolved.version)?;
    Ok(resolved)
}

/// The version-resolution cascade, once the explicit argument is out of the
/// way. Four stages, in order:
///
/// 1. the nearest `.jlorc` at or above the cwd,
/// 2. `$JLO_HOME/default.jlorc`,
/// 3. the newest JDK already installed,
/// 4. the latest release Adoptium offers, downloaded.
///
/// It lives in `main` rather than in `conf` deliberately. `conf` knows about
/// config files and nothing else - not where JDKs are installed, not how to
/// reach Adoptium - and moving the cascade there would hand it both, so the
/// module that answers "what does this file say" would start answering "what
/// is on this machine" and "what does the network offer" too. Stages 1 and 2
/// stay `conf::find`, unchanged; stages 3 and 4 are added here, where the
/// store and the client already are.
///
/// Every input is passed in rather than read from the filesystem or the
/// process environment - the same reason `conf::find_in` takes its cwd - so
/// the decisions here, including the one that must *not* download, are
/// testable without a store, a network or a temp directory.
fn cascade(
    configured: Option<conf::Resolved>,
    newest_installed: Option<conf::Resolved>,
    offline: bool,
    latest_release: impl FnOnce() -> anyhow::Result<String>,
) -> anyhow::Result<conf::Resolved> {
    if let Some(resolved) = configured.or(newest_installed) {
        return Ok(resolved);
    }

    // `--offline` stops here, one stage short of the download, and that is the
    // whole of why entering a directory never starts one: the autoload hook
    // calls `jlo env --offline`, so the cascade it runs ends at what is
    // already on disk.
    if offline {
        return Err(conf::nothing_configured());
    }

    Ok(conf::Resolved {
        version: latest_release()?,
        source: conf::Source::LatestRelease,
    })
}

/// Stage 3 of the cascade: the newest JDK already on disk, whatever major it
/// is.
///
/// Deliberately no comparison against Adoptium. Asking whether the newest
/// installed JDK is also the newest release would put a network round trip on
/// the hottest path there is - every bare `jlo env` - to answer a question
/// `jlo update` already exists for. So a machine holding only an outdated 17
/// resolves to 17 and downloads nothing; stage 4 is reached only when no JDK
/// is installed at all.
///
/// A store holding nothing but pre-8 JDKs falls through rather than resolving
/// to a version the rest of jlo would then reject.
fn newest_installed(store: &JdkStore) -> Option<conf::Resolved> {
    store
        .newest_major()
        .map(|major| conf::Resolved {
            version: major.to_string(),
            source: conf::Source::NewestInstalled,
        })
        .filter(|resolved| conf::is_valid_version(&resolved.version))
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
    let java_home = store.find_matching(java_version).ok_or_else(|| {
        CommandError::with_hint(
            anyhow!("no installed JDK matches Java {java_version}"),
            format!("Run 'jlo {command} {java_version}' without --offline to install it."),
        )
    })?;

    // `env --offline` is how the autoload hook runs, on every new shell and
    // every `cd`, and ADR-0001 keeps that path silent - a line here would
    // print forever. `home --offline` is a person asking a question and gets
    // the warning. This is the only thing that tells the two apart, which is
    // why `command` is threaded down here at all.
    if command != "env" {
        warn_legacy_layout(store, &java_home);
    }

    Ok(java_home)
}

/// Say so when the JDK just resolved is one `/usr/libexec/java_home` cannot
/// see, which is every macOS install made before jlo kept the bundle.
///
/// At the two resolution funnels rather than in each command, so a verb added
/// later cannot forget it. Only ever a warning: the install works, and the fix
/// costs a download, so it is the user's to make.
fn warn_legacy_layout(store: &JdkStore, java_home: &Path) {
    if let Some((version, major)) = store.legacy_layout(java_home) {
        ui::legacy_layout(&version, major);
    }
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
                ui::NO_ACTIVE_JDK_HINT,
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
    if superseded {
        let report = store.prune().context("could not remove superseded JDKs")?;
        ui::prune_report(&report);
    } else {
        let report = store.remove(versions, active_java_home().as_deref())?;
        ui::remove_report(&report);
    }
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
    let versions = requested_versions(versions, "install", &store, client)?;
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
        requested_versions(versions, "update", &store, client)?
    };

    install_each(client, &store, versions_to_install)
}

/// The majors an explicit run should download: the list given, or the version
/// the cascade resolves when the list is empty - the same resolution `env`,
/// `home` and `exec` do, so a bare `jlo install` means the same version they
/// would pick.
///
/// Neither verb takes `--offline`, so the cascade here may reach its last
/// stage: `jlo install` on a machine with no config and no JDK installs the
/// latest release, which is the only thing it could sensibly mean.
///
/// An invalid entry is warned about and skipped, so one typo in a list of four
/// does not cost the other three. A list that leaves nothing valid behind is
/// an error naming `verb`, the command that asked.
fn requested_versions(
    versions: Vec<String>,
    verb: &str,
    store: &JdkStore,
    client: &AdoptiumClient,
) -> Result<HashSet<String>, CommandError> {
    if versions.is_empty() {
        let resolved = resolve_java_version_from(None, store, client, false)?;
        return Ok(HashSet::from([resolved.version]));
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
    // wired into an open shell or an IDE. Point at `jlo remove --superseded`
    // instead of doing it here.
    if let Some(hint) = ui::superseded_hint(installed_any, count_superseded(store)) {
        ui::hint!("{hint}");
    }

    Ok(())
}

/// How many installs `jlo remove --superseded` would remove, or 0 if that
/// cannot be determined. A hint is not worth failing an otherwise successful
/// run, so an unreadable JDK directory just means no hint.
fn count_superseded(store: &JdkStore) -> usize {
    store.superseded_count().unwrap_or(0)
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
        warn_legacy_layout(store, &path);
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

    ui::print_lines(exports);

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

    /// A store rooted at a path that does not exist, i.e. one holding no
    /// JDKs. Enough for the tests that never reach stage 3 of the cascade.
    fn empty_store() -> JdkStore {
        JdkStore::at("/nonexistent/jlo-test-store")
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

    // -- cascade --
    //
    // Every input is passed in, so these run without a store, a network or a
    // temp directory. `refuse_network` is the assertion that matters most in
    // half of them: stage 4 is a several-hundred-megabyte download, and the
    // cases below are exactly the ones in which it must not be reached.

    fn configured(version: &str) -> conf::Resolved {
        conf::Resolved {
            version: version.to_string(),
            source: conf::Source::DefaultConfig(PathBuf::from("/home/u/.jlo/default.jlorc")),
        }
    }

    fn installed(version: &str) -> conf::Resolved {
        conf::Resolved {
            version: version.to_string(),
            source: conf::Source::NewestInstalled,
        }
    }

    /// A stage 4 that fails if it is ever called, so "no network access" is an
    /// assertion rather than a comment.
    fn refuse_network() -> anyhow::Result<String> {
        Err(anyhow!("the network was consulted"))
    }

    /// Stage 2 beats stage 3: `jlo init --global` is how a user asks for a
    /// stable answer on neutral ground, and a JDK installed for some other
    /// project must not quietly override it.
    #[test]
    fn cascade_prefers_a_config_over_the_newest_install() {
        let resolved = cascade(
            Some(configured("21")),
            Some(installed("25")),
            false,
            refuse_network,
        )
        .expect("the default config answers");

        assert_eq!(resolved.version, "21");
        assert!(matches!(resolved.source, conf::Source::DefaultConfig(_)));
    }

    /// Stage 3: no config anywhere, so the newest JDK on disk answers - and
    /// answers without a round trip to Adoptium.
    #[test]
    fn cascade_falls_back_to_the_newest_installed_jdk() {
        let resolved = cascade(None, Some(installed("25")), false, refuse_network)
            .expect("the installed JDK answers");

        assert_eq!(resolved.version, "25");
        assert_eq!(resolved.source, conf::Source::NewestInstalled);
    }

    /// The case the cascade is most easily got wrong in: a machine holding
    /// only an outdated major resolves to *that* major. Stage 3 does not ask
    /// Adoptium whether something newer exists - that is what `jlo update` is
    /// for - so nothing is downloaded here.
    #[test]
    fn cascade_keeps_an_outdated_install_rather_than_downloading_a_newer_major() {
        let resolved = cascade(None, Some(installed("17")), false, refuse_network)
            .expect("the outdated install still answers");

        assert_eq!(resolved.version, "17");
        assert_eq!(resolved.source, conf::Source::NewestInstalled);
    }

    /// Stage 4, reached only when nothing is configured *and* nothing is
    /// installed.
    #[test]
    fn cascade_downloads_the_latest_release_when_nothing_is_installed() {
        let resolved = cascade(None, None, false, || Ok("26".to_string()))
            .expect("the latest release answers");

        assert_eq!(resolved.version, "26");
        assert_eq!(resolved.source, conf::Source::LatestRelease);
    }

    /// `--offline` stops one stage short of the download. This is the whole of
    /// why the autoload hook - which calls `jlo env --offline` on every `cd` -
    /// can never start one.
    #[test]
    fn cascade_refuses_to_download_when_offline() {
        let err =
            cascade(None, None, true, refuse_network).expect_err("offline has nowhere left to go");

        assert!(err.to_string().contains(".jlorc"), "{err}");
        assert!(err.to_string().contains("jlo init"), "{err}");
    }

    /// `--offline` stops *after* stage 3, not before it: an installed JDK is
    /// already on disk, so handing it back costs no network at all.
    #[test]
    fn cascade_still_uses_an_installed_jdk_when_offline() {
        let resolved = cascade(None, Some(installed("21")), true, refuse_network)
            .expect("the installed JDK needs no network");

        assert_eq!(resolved.version, "21");
        assert_eq!(resolved.source, conf::Source::NewestInstalled);
    }

    /// Stage 3 reads a major out of a directory name, so a store holding only
    /// pre-8 JDKs would otherwise resolve to a version every other part of jlo
    /// rejects. It falls through to stage 4 instead.
    #[test]
    fn newest_installed_ignores_a_store_of_pre_8_jdks() {
        let dir = tempdir().expect("a temp directory");
        std::fs::create_dir_all(dir.path().join("7.0.4+101")).expect("the fake JDK directory");

        assert_eq!(newest_installed(&JdkStore::at(dir.path())), None);
    }

    /// Nothing installed is `None`, not a failure - including when the store
    /// directory has never been created.
    #[test]
    fn newest_installed_is_none_for_an_empty_store() {
        assert_eq!(newest_installed(&empty_store()), None);
    }

    // -- requested_versions --

    #[test]
    fn requested_versions_keeps_the_valid_entries_of_a_mixed_list() {
        let requested = requested_versions(
            owned(&["21", "abc", "25"]),
            "install",
            &empty_store(),
            &offline_client(),
        )
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
        let requested = requested_versions(
            owned(&["21", "21"]),
            "install",
            &empty_store(),
            &offline_client(),
        )
        .expect("21 is a valid major");
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
