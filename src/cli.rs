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

/// Print `jlo exec`'s own help, exactly as `jlo exec -h`/`jlo exec --help`
/// would.
///
/// clap intercepts `-h`/`--help` for us only until a value has bound to
/// `exec`'s `args` positional; `jlo exec 21 --help` arrives past that point,
/// so `cmd_exec` calls this directly instead. `long` selects between the
/// short (`-h`) and long (`--help`) renderings, matching clap's own
/// convention.
pub(crate) fn print_exec_help(long: bool) {
    use clap::CommandFactory;

    let mut cmd = Cli::command();
    // Propagates `bin_name` ("jlo" -> "jlo exec") and other derived state
    // down to subcommands; without it the extracted `exec` Command doesn't
    // know its own usage line starts with "jlo ".
    cmd.build();
    let exec = cmd
        .find_subcommand_mut("exec")
        .expect("the `exec` subcommand is always registered");
    // A closed stdout (`jlo exec 21 --help | head`) is not an error worth
    // reporting.
    let result = if long {
        exec.print_long_help()
    } else {
        exec.print_help()
    };
    let _ = result;
    println!();
}

#[derive(Debug, Subcommand)]
pub(crate) enum Command {
    // `about`/`long_about` are given explicitly here rather than as a doc
    // comment: `clippy::doc_markdown` (part of `clippy::pedantic`, which is
    // on for this crate) requires bare identifiers like JAVA_HOME to be
    // backtick-quoted in rustdoc, but clap renders doc-comment backticks
    // *literally* in terminal help - there is no markdown stripping. Plain
    // attribute strings are not rustdoc, so the lint doesn't apply to them,
    // and the help text can say JAVA_HOME without stray backticks leaking
    // into what the user sees. (No backticks appear anywhere below, in any
    // command's about/long_about or doc comment, for the same reason.)
    #[command(
        visible_alias = "use",
        about = "Set JAVA_HOME and PATH in the current shell",
        long_about = "\
Set JAVA_HOME and PATH in the current shell

Prints export statements on stdout; the jlo shell function sources
them. Running the binary directly does not change your shell.

The JDK is downloaded from Adoptium on demand if it is not installed.

When VERSION is omitted, it resolves from ./.jlorc, then
~/.jlo/default.jlorc. Only major versions are accepted: 21, not
21.0.5."
    )]
    Env {
        /// Java major version. Default: from .jlorc
        version: Option<String>,
    },

    #[command(
        about = "Print the JAVA_HOME path for a version",
        long_about = "\
Print the JAVA_HOME path for a version

Writes the path and nothing else to stdout, so $(jlo home 21) stays
clean. Unlike jlo env it does not modify the current shell.

When VERSION is omitted, it resolves from ./.jlorc, then
~/.jlo/default.jlorc. Only major versions are accepted: 21, not
21.0.5."
    )]
    Home {
        /// Java major version. Default: from .jlorc
        version: Option<String>,
    },

    #[command(
        about = "Run a command with a given JDK active",
        long_about = "\
Run a command with a given JDK active

The literal -- separates the optional version from the command:

  jlo exec 21 -- ./gradlew build
  jlo exec -- java -version

JAVA_HOME is set and the JDK's bin directory is prepended to PATH for
the child only; the current shell is untouched."
    )]
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
    /// ~/.jlo/default.jlorc. Pass 'all' to update every installed major
    /// version, or list major versions explicitly.
    Update {
        /// Major versions to update, or 'all'. Default: from .jlorc
        versions: Vec<String>,
    },

    /// Remove superseded minor versions
    ///
    /// Keeps the newest minor release of every installed major version and
    /// deletes the rest. Only JDKs J'Lo installed are touched.
    Clean,

    /// Write .jlorc pinning this project's Java version
    ///
    /// With no argument, pins the latest version Adoptium offers. Only major
    /// versions are accepted: 21, not 21.0.5.
    Init {
        /// Java major version. Default: latest release
        version: Option<String>,
    },

    /// Write ~/.jlo/default.jlorc
    ///
    /// The version used by jlo env when the current directory has no
    /// .jlorc. Only major versions are accepted: 21, not 21.0.5.
    Default {
        /// Java major version, e.g. 21
        version: String,
    },

    /// Update jlo itself
    ///
    /// Handled by the jlo shell function from jlo-init.sh, not by this
    /// binary.
    Selfupdate,

    #[command(
        about = "Print a shell completion script",
        long_about = "\
Print a shell completion script

The installer writes these to $JLO_HOME/completions. To load one
directly:

  source <(jlo completions bash)"
    )]
    Completions {
        /// Shell to generate completions for
        shell: clap_complete::Shell,
    },

    /// Print the version
    Version,
    // The `sing` easter egg is deliberately NOT a variant here - see
    // `main`'s comment for why.
}
