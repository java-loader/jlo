mod adoptium;
mod cli;
mod conf;
mod extract;
mod ui;

use crate::adoptium::{
    AdoptiumClient, JdkMetadata, RemoteJdk, clean_jdks, find_installed_jdk, find_installed_jdks,
    find_installed_major_versions, find_suitable_jdk,
};
use crate::ui::InstallUi;
use anyhow::Context;
use clap::Parser;
use console::style;
use std::collections::HashSet;
use std::env;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::exit;
use tempfile::tempdir;

fn main() {
    // The easter egg is deliberately not a clap subcommand: `hide = true`
    // only suppresses it from `--help`. `clap_complete` still emits hidden
    // subcommands into generated completion scripts, and clap's "did you
    // mean" suggestion engine still offers it for typos (e.g. `jlo sng`).
    // Intercepting the raw token before `Cli::parse()` keeps it out of
    // help, completions, and typo suggestions in one move.
    if env::args().nth(1).as_deref() == Some("sing") {
        eprintln!("There are no Easter Eggs in this program. Trust me. 💃");
        return;
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
        return;
    }

    let cli = cli::Cli::parse();

    let Some(command) = cli.command else {
        cli::print_help();
        return;
    };

    let api_url =
        env::var("JLO_ADOPTIUM_API_URL").unwrap_or_else(|_| adoptium::ADOPTIUM_API_URL.to_string());
    let client = AdoptiumClient::new(api_url);

    match command {
        cli::Command::Env { version } => cmd_env(&client, version),
        cli::Command::Home { version } => cmd_home(&client, version),
        cli::Command::Exec { args } => cmd_exec(&client, &args),
        cli::Command::List { offline } => cmd_list(&client, offline),
        cli::Command::Update { versions } => cmd_update(&client, versions),
        cli::Command::Clean => cmd_clean(),
        cli::Command::Init { version } => cmd_init(&client, version),
        cli::Command::Default { version } => cmd_default(&version),
        cli::Command::Selfupdate => {
            ui::error!(
                "Self-update is handled by the jlo shell function. Source jlo-init.sh from your shell profile, or re-run the installer."
            );
            exit(1);
        }
        cli::Command::Completions { shell } => cmd_completions(shell),
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
fn resolve_java_version_from(explicit: Option<String>) -> String {
    let java_version = explicit.unwrap_or_else(|| {
        conf::load_config_java_version().unwrap_or_else(|e| {
            ui::error!("{e:#}");
            exit(1);
        })
    });

    assert_java_version(&java_version);
    java_version
}

fn cmd_env(client: &AdoptiumClient, version: Option<String>) {
    let java_version = resolve_java_version_from(version);
    if let Err(e) = setup(client, &java_version) {
        ui::error!("{e:#}");
        exit(1);
    }
}

fn cmd_home(client: &AdoptiumClient, version: Option<String>) {
    let java_version = resolve_java_version_from(version);
    let java_home = resolve_java_home(client, &java_version).unwrap_or_else(|e| {
        ui::error!("{e:#}");
        exit(1);
    });
    println!("{}", java_home.to_string_lossy());
}

fn cmd_exec(client: &AdoptiumClient, args: &[String]) {
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
        return;
    }
    if before_separator.iter().any(|a| a == "-h") {
        cli::print_exec_help(false);
        return;
    }

    let (version, command) = parse_exec_args(&args).unwrap_or_else(|e| {
        ui::error!("{e}");
        ui::hint!("Usage: jlo exec [VERSION] -- <COMMAND> [ARGS]...");
        exit(1);
    });

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
    let java_version = resolve_java_version_from(version);
    let java_home = resolve_java_home(client, &java_version).unwrap_or_else(|e| {
        ui::error!("{e:#}");
        exit(1);
    });

    exec_command(&java_home, command);
}

// A real `execvp` is Unix-only. A native Windows build would replace this with a
// spawn-and-wait fallback that propagates the child's exit code.
#[cfg(not(unix))]
fn run_exec(_client: &AdoptiumClient, _version: Option<String>, _command: &[String]) -> ! {
    ui::error!("'jlo exec' is not supported on this platform.");
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
        .context("Could not join PATH components")?
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
/// what is already installed.
///
/// Every line starts with the major version, because that - not the full build
/// version - is what `jlo update`, `jlo exec` and `.jlorc` take. Colours switch
/// themselves off when stdout is not a terminal, so a pipe sees plain text.
fn cmd_list(client: &AdoptiumClient, offline: bool) {
    let jdk_base = jdk_base_dir().unwrap_or_else(|e| {
        ui::error!("{e:#}");
        exit(1);
    });
    let installed = find_installed_jdks(&jdk_base).unwrap_or_else(|e| {
        ui::error!("Could not list installed JDKs: {e:#}");
        exit(1);
    });

    if offline {
        print_offline_list(&installed, &jdk_base);
    } else {
        let available = client.available_jdks().unwrap_or_else(|e| {
            ui::error!("Could not fetch available JDKs: {e:#}");
            ui::hint!("Use 'jlo list --offline' to list the JDKs already installed.");
            exit(1);
        });
        print_remote_list(&available, &installed);
    }
}

fn print_offline_list(installed: &[adoptium::InstalledJdk], jdk_base: &Path) {
    if installed.is_empty() {
        eprintln!("No JDKs installed in {}.", jdk_base.display());
        return;
    }

    let major_width = major_column_width(installed.iter().map(|jdk| jdk.major));

    print_lines(installed.iter().map(|jdk| {
        let row = format!(
            "{:<major_width$}  {}",
            style(jdk.major).dim(),
            jdk.version,
            major_width = major_width
        );
        if jdk.managed {
            row
        } else {
            // `jlo clean` leaves these alone; say so rather than let the user
            // wonder why a version never goes away.
            format!("{} {}", row, style("(unmanaged)").dim())
        }
    }));
}

/// Width of the leading major-version column. Styling adds invisible escape
/// bytes, so the width has to come from the plain numbers.
fn major_column_width(majors: impl IntoIterator<Item = i64>) -> usize {
    majors
        .into_iter()
        .map(|major| major.to_string().len())
        .max()
        .unwrap_or(0)
}

fn print_remote_list(available: &[RemoteJdk], installed: &[adoptium::InstalledJdk]) {
    if available.is_empty() {
        eprintln!("Adoptium offers no JDKs for this OS and architecture.");
        return;
    }

    let width = available
        .iter()
        .map(|jdk| jdk.version.len())
        .max()
        .unwrap_or(0);
    let major_width = major_column_width(available.iter().map(|jdk| jdk.major));

    print_lines(available.iter().map(|jdk| {
        // The LTS tag is padded to its *visible* width - the styled string
        // carries escape bytes that must not count towards the column.
        let lts = if jdk.lts {
            style("LTS").cyan().to_string()
        } else {
            "   ".to_string()
        };

        let status = match installed_status(jdk, installed) {
            InstalledStatus::Latest => style("installed").green().to_string(),
            InstalledStatus::Older(version) => {
                style(format!("outdated ({version})")).yellow().to_string()
            }
            InstalledStatus::None => String::new(),
        };

        // Trailing whitespace is ugly in a terminal, so build the row and trim
        // it rather than padding fields that may be empty.
        format!(
            "{:<major_width$}  {:<width$}  {}  {}",
            style(jdk.major).dim(),
            jdk.version,
            lts,
            status,
            major_width = major_width,
            width = width
        )
        .trim_end()
        .to_string()
    }));

    if has_outdated(available, installed) {
        // stderr, so the tip never lands in a pipe alongside the listing.
        eprintln!(
            "\n{} Use `{}` to update all outdated JDKs.",
            style("TIP:").cyan().bold().for_stderr(),
            style("jlo update all").bold().for_stderr()
        );
    }
}

/// Whether any major version has an older build installed than Adoptium offers.
fn has_outdated(available: &[RemoteJdk], installed: &[adoptium::InstalledJdk]) -> bool {
    available
        .iter()
        .any(|jdk| matches!(installed_status(jdk, installed), InstalledStatus::Older(_)))
}

/// `println!` panics when the reader goes away, and this output is meant to be
/// piped (`jlo list | head`), so treat a closed pipe as a normal end of output.
fn print_lines(lines: impl IntoIterator<Item = String>) {
    use std::io::Write;

    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    for line in lines {
        match writeln!(out, "{line}") {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => return,
            Err(e) => {
                ui::error!("could not write to stdout: {e}");
                exit(1);
            }
        }
    }
}

enum InstalledStatus {
    /// The newest build Adoptium offers for this major version is installed.
    Latest,
    /// Some build of this major version is installed, but an older one.
    Older(String),
    None,
}

fn installed_status(jdk: &RemoteJdk, installed: &[adoptium::InstalledJdk]) -> InstalledStatus {
    if installed.iter().any(|i| i.version == jdk.version) {
        return InstalledStatus::Latest;
    }

    match installed
        .iter()
        .find(|i| i.major == jdk.major)
        .map(|i| i.version.clone())
    {
        Some(version) => InstalledStatus::Older(version),
        None => InstalledStatus::None,
    }
}

fn cmd_clean() {
    let jdk_base = jdk_base_dir().unwrap_or_else(|e| {
        ui::error!("{e:#}");
        exit(1);
    });
    let report = clean_jdks(&jdk_base).unwrap_or_else(|e| {
        ui::error!("Could not clean JDKs: {e:#}");
        exit(1);
    });
    ui::clean_report(&report);
}

fn cmd_default(java_version: &str) {
    assert_java_version(java_version);
    conf::init_default_config(java_version).unwrap_or_else(|e| {
        ui::error!("Could not create default config file: {e:#}");
        exit(1);
    });
}

fn cmd_init(client: &AdoptiumClient, version: Option<String>) {
    let java_version = version.unwrap_or_else(|| {
        client.latest_major().unwrap_or_else(|e| {
            ui::error!("Could not fetch latest JDK version: {e:#}");
            exit(1);
        })
    });

    assert_java_version(&java_version);

    conf::init_project_config(&java_version).unwrap_or_else(|e| {
        ui::error!("Could not create config file: {e:#}");
        exit(1);
    });
}

fn cmd_update(client: &AdoptiumClient, versions: Vec<String>) {
    let mut versions_to_install: HashSet<String> = HashSet::new();

    if versions.is_empty() {
        let java_version = conf::load_config_java_version().unwrap_or_else(|e| {
            ui::error!("Could not load configuration: {e:#}");
            exit(1);
        });
        versions_to_install.insert(java_version);
    } else {
        if versions.iter().any(|arg| arg == "all") {
            find_installed_major_versions(&jdk_base_dir().unwrap_or_else(|e| {
                ui::error!("{e:#}");
                exit(1);
            }))
            .unwrap_or_else(|e| {
                ui::error!("Could not determine installed JDK versions: {e:#}");
                exit(1);
            })
            .into_iter()
            .for_each(|v| {
                versions_to_install.insert(v.to_string());
            });
        }

        versions
            .into_iter()
            .filter(|arg| arg != "all")
            .for_each(|v| {
                if conf::is_valid_version(&v) {
                    versions_to_install.insert(v);
                } else {
                    ui::warning!("Skipping invalid version: '{v}'.");
                }
            });

        if versions_to_install.is_empty() {
            ui::error!("No valid Java versions provided to update.");
            exit(1);
        }
    }

    // Sort versions_to_install alphabetically for consistent processing order
    let mut versions_to_install: Vec<_> = versions_to_install.into_iter().collect();
    versions_to_install.sort();

    for java_version in versions_to_install {
        update(client, &java_version);
    }
}

fn update(client: &AdoptiumClient, java_version: &str) {
    let jdk_metadata = client.fetch_metadata(java_version).unwrap_or_else(|e| {
        ui::error!("Could not fetch JDK metadata: {e:#}");
        exit(1);
    });

    let jdk_base = jdk_base_dir().unwrap_or_else(|e| {
        ui::error!("{e:#}");
        exit(1);
    });

    if find_installed_jdk(&jdk_metadata, &jdk_base).is_some() {
        ui::up_to_date(java_version, &jdk_metadata.semver);
    } else {
        install_jdk(client, &jdk_base, &jdk_metadata).unwrap_or_else(|e| {
            ui::error!("Could not install JDK: {e:#}");
            exit(1);
        });
    }
}

/// Resolve the `JAVA_HOME` for the requested major version, installing the JDK on
/// demand if it is not already present. Diagnostics go to stderr; this returns
/// the path so callers decide what (if anything) to print to stdout.
fn resolve_java_home(client: &AdoptiumClient, java_version: &str) -> anyhow::Result<PathBuf> {
    let jdk_base = jdk_base_dir()?;

    if let Some(path) = find_suitable_jdk(&jdk_base, java_version) {
        Ok(path)
    } else {
        let metadata = client.fetch_metadata(java_version)?;
        install_jdk(client, &jdk_base, &metadata)
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
    let java_home = resolve_java_home(client, java_version)?;
    let jdk_base = jdk_base_dir()?;

    let current_java_home = env::var("JAVA_HOME").unwrap_or_default();
    if current_java_home != java_home.to_string_lossy() {
        println!("export JAVA_HOME=\"{}\"", java_home.to_string_lossy());
    }

    let java_bin_path = java_home.join("bin").to_string_lossy().into_owned();
    let current_path = env::var("PATH").unwrap_or_default();
    if let Some(updated_path) = update_path(&java_bin_path, &current_path, &jdk_base)? {
        println!("export PATH=\"{updated_path}\"");
    }

    Ok(())
}

fn install_jdk(
    client: &AdoptiumClient,
    jdk_base: &Path,
    jdk_metadata: &JdkMetadata,
) -> anyhow::Result<PathBuf> {
    // One progress region spans all three phases, so the terminal shows a
    // single line that changes rather than three bars stacking up.
    let ui = InstallUi::new(&jdk_metadata.semver);
    let dest_dir = jdk_base.join(&jdk_metadata.semver);

    match install_jdk_inner(client, jdk_metadata, &dest_dir, &ui) {
        Ok(()) => {
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
    jdk_metadata: &JdkMetadata,
    dest_dir: &Path,
    ui: &InstallUi,
) -> anyhow::Result<()> {
    // Download JDK
    let temp_dir = tempdir().context("could not create temporary directory")?;
    let temp_file = temp_dir.path().join(&jdk_metadata.package_name);
    let file = &mut File::create(&temp_file).context("could not create temporary file")?;
    client.download(jdk_metadata, file, ui)?;

    // Extract JDK to temp dir
    extract::extract(&temp_file, temp_dir.path(), ui)?;

    adoptium::install_jdk(jdk_metadata, temp_dir.path(), dest_dir, ui)?;

    temp_dir.close().unwrap_or_else(|err| {
        ui::warning!("Could not delete temporary directory: {err}");
    });

    Ok(())
}

fn jlo_home_dir() -> anyhow::Result<PathBuf> {
    let path = env::var_os("JLO_HOME")
        .map(PathBuf::from)
        .or_else(env::home_dir)
        .context("Could not determine home directory.")?;
    Ok(path)
}

fn jdk_base_dir() -> anyhow::Result<PathBuf> {
    let home = env::home_dir().context("Could not determine home directory")?;
    Ok(jdk_base_dir_for(env::consts::OS, &home))
}

/// JDK install location, matching `IntelliJ` IDEA's layout so both tools see the
/// same JDKs. Split out from [`jdk_base_dir`] so every platform is testable from
/// any host.
fn jdk_base_dir_for(os: &str, home: &Path) -> PathBuf {
    match os {
        "macos" => home.join("Library/Java/JavaVirtualMachines"),
        _ => home.join(".jdks"),
    }
}

/// Prepend `java_path` to `current_path`, dropping any entry already under
/// `jdk_base`. `jdk_base` must be the JDK install directory ([`jdk_base_dir`]) —
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
        .context("Could not join PATH components")?
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

fn assert_java_version(java_version: &str) {
    if !conf::is_valid_version(java_version) {
        ui::error!(
            "Unsupported version: '{java_version}'. Only major versions 8, 11, ... are supported."
        );
        exit(1);
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
    fn jdk_base_dir_matches_intellij_layout_on_macos() {
        let home = Path::new("/Users/u");
        assert_eq!(
            jdk_base_dir_for("macos", home),
            home.join("Library/Java/JavaVirtualMachines")
        );
    }

    #[test]
    fn jdk_base_dir_matches_intellij_layout_on_linux() {
        let home = Path::new("/home/u");
        assert_eq!(jdk_base_dir_for("linux", home), home.join(".jdks"));
    }

    #[test]
    fn jdk_base_dir_matches_intellij_layout_on_windows() {
        let home = Path::new("/Users/u");
        assert_eq!(jdk_base_dir_for("windows", home), home.join(".jdks"));
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

    fn remote(version: &str, major: i64) -> RemoteJdk {
        RemoteJdk {
            version: version.to_string(),
            major,
            lts: false,
        }
    }

    fn local(version: &str, major: i64) -> adoptium::InstalledJdk {
        adoptium::InstalledJdk {
            version: version.to_string(),
            major,
            managed: true,
        }
    }

    #[test]
    fn installed_status_exact_match_is_latest() {
        let installed = vec![local("21.0.12+101.0.LTS", 21)];
        assert!(matches!(
            installed_status(&remote("21.0.12+101.0.LTS", 21), &installed),
            InstalledStatus::Latest
        ));
    }

    #[test]
    fn installed_status_older_build_of_same_major_is_outdated() {
        let installed = vec![local("21.0.11+10.0.LTS", 21)];
        match installed_status(&remote("21.0.12+101.0.LTS", 21), &installed) {
            InstalledStatus::Older(v) => assert_eq!(v, "21.0.11+10.0.LTS"),
            _ => panic!("expected Older"),
        }
    }

    #[test]
    fn installed_status_other_majors_do_not_count() {
        let installed = vec![local("17.0.20+101", 17)];
        assert!(matches!(
            installed_status(&remote("21.0.12+101.0.LTS", 21), &installed),
            InstalledStatus::None
        ));
    }

    #[test]
    fn installed_status_reports_newest_local_build_of_the_major() {
        // find_installed_jdks yields newest first, so the first match for a
        // major is the newest build the user has.
        let installed = vec![local("21.0.11+10.0.LTS", 21), local("21.0.9+10.0.LTS", 21)];
        match installed_status(&remote("21.0.12+101.0.LTS", 21), &installed) {
            InstalledStatus::Older(v) => assert_eq!(v, "21.0.11+10.0.LTS"),
            _ => panic!("expected Older"),
        }
    }

    #[test]
    fn has_outdated_is_true_when_a_major_has_an_older_build() {
        let available = vec![remote("21.0.12+101.0.LTS", 21)];
        let installed = vec![local("21.0.11+10.0.LTS", 21)];
        assert!(has_outdated(&available, &installed));
    }

    #[test]
    fn has_outdated_is_false_when_everything_is_current() {
        let available = vec![remote("21.0.12+101.0.LTS", 21), remote("17.0.20+101", 17)];
        let installed = vec![local("21.0.12+101.0.LTS", 21)];
        assert!(!has_outdated(&available, &installed));
    }

    #[test]
    fn has_outdated_is_false_with_nothing_installed() {
        let available = vec![remote("21.0.12+101.0.LTS", 21)];
        assert!(!has_outdated(&available, &[]));
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
