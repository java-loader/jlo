//! The command-line contract.
//!
//! Every command, argument and line of help text lives here, so the help
//! output and the dispatch are the same declaration. They used to be two
//! hand-maintained strings, which drifted: `jlo-init.sh` accepted `use` as an
//! alias the binary had never heard of.

use clap::{Parser, Subcommand};

const AFTER_HELP: &str = "\
When VERSION is omitted it is resolved from ./.jlorc, then ~/.jlo/default.jlorc.

Examples:
  jlo env 25                       Use Java 25 in this shell
  jlo init 21                      Pin Java 21 for this project
  jlo env                          Use the pinned version
  jlo exec 21 -- ./gradlew build   Run a build on Java 21
  jlo update all                   Bring every installed JDK up to date

Docs: https://github.com/java-loader/jlo";

#[derive(Debug, Parser)]
#[command(
    name = "jlo",
    bin_name = "jlo",
    version,
    about = "J'Lo - the Java Loader. Download, manage and switch JDKs.",
    after_help = AFTER_HELP
)]
pub(crate) struct Cli {
    // Deliberately optional. `arg_required_else_help` would print the help but
    // exit 2; a bare `jlo` is exploration, not a usage error, so `main` handles
    // the `None` case and exits 0.
    #[command(subcommand)]
    pub(crate) command: Option<Command>,
}

/// Print the top-level help, exactly as `jlo -h` does.
pub(crate) fn print_help() {
    use clap::CommandFactory;

    // A closed stdout (`jlo | head`) is not an error worth reporting.
    let _ = Cli::command().print_help();
    println!();
}

#[derive(Debug, Subcommand)]
pub(crate) enum Command {
    /// Set `JAVA_HOME` and PATH in the current shell
    ///
    /// Prints `export` statements on stdout; the `jlo` shell function sources
    /// them. Running the binary directly does not change your shell.
    ///
    /// The JDK is downloaded from Adoptium on demand if it is not installed.
    #[command(alias = "use")]
    Env {
        /// Java major version, e.g. 21 or 25
        version: Option<String>,
    },

    /// Print the `JAVA_HOME` path for a version
    ///
    /// Writes the path and nothing else to stdout, so `$(jlo home 21)` stays
    /// clean. Unlike `jlo env` it does not modify the current shell.
    Home {
        /// Java major version, e.g. 21 or 25
        version: Option<String>,
    },

    /// Run a command with a given JDK active
    ///
    /// The literal `--` separates the optional version from the command:
    ///
    ///   jlo exec 21 -- ./gradlew build
    ///   jlo exec -- java -version
    ///
    /// `JAVA_HOME` is set and the JDK's bin directory is prepended to PATH for
    /// the child only; the current shell is untouched.
    Exec {
        /// [VERSION] -- <COMMAND>...
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },

    /// Show available and installed JDKs
    List {
        /// List only what is installed; never touch the network
        #[arg(long)]
        offline: bool,
    },

    /// Update installed JDKs to their latest minor release
    ///
    /// With no argument, updates the version from .jlorc or
    /// ~/.jlo/default.jlorc. Pass `all` to update every installed major
    /// version, or list major versions explicitly.
    Update {
        /// Major versions to update, or the literal `all`
        versions: Vec<String>,
    },

    /// Remove superseded minor versions
    ///
    /// Keeps the newest minor release of every installed major version and
    /// deletes the rest. Only JDKs J'Lo installed are touched.
    Clean,

    /// Write .jlorc pinning this project's Java version
    ///
    /// With no argument, pins the latest version Adoptium offers.
    Init {
        /// Java major version, e.g. 21 or 25
        version: Option<String>,
    },

    /// Write ~/.jlo/default.jlorc
    ///
    /// The version used by `jlo env` when the current directory has no .jlorc.
    Default {
        /// Java major version, e.g. 21 or 25
        version: String,
    },

    /// Update jlo itself
    ///
    /// Handled by the `jlo` shell function from jlo-init.sh, not by this
    /// binary.
    Selfupdate,

    /// Print a shell completion script
    ///
    /// The installer writes these to `$JLO_HOME/completions`. To load one
    /// directly:
    ///
    ///   source <(jlo completions bash)
    Completions {
        /// Shell to generate completions for
        shell: clap_complete::Shell,
    },

    /// Print the version
    Version,

    #[command(hide = true)]
    Sing,
}
