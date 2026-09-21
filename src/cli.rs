//! The command-line contract.
//!
//! Every command, argument and line of help text lives here, so the help
//! output and the dispatch are the same declaration. They used to be two
//! hand-maintained strings, which drifted: the shell wrapper accepted `use` as an
//! alias the binary had never heard of.

use clap::{Parser, Subcommand};

const AFTER_HELP: &str = "\
env, home, exec, update and init take a Java major version:

  jlo env [VERSION]          jlo update [VERSION...]
  jlo home [VERSION]         jlo init [VERSION]
  jlo exec [VERSION] -- <COMMAND> [ARGS]...

When VERSION is omitted, env, home, exec and update resolve it from the
nearest .jlorc at or above the current directory, then ~/.jlo/default.jlorc.
init instead pins the latest release.

Examples:
  jlo env 25                       Use Java 25 in this shell
  jlo init 21                      Pin Java 21 for this project
  jlo env                          Use the pinned version
  jlo current                      Show which JDK is active, and why
  jlo exec 21 -- ./gradlew build   Run a build on Java 21
  jlo update --all                 Bring every installed JDK up to date
  jlo remove 11 17                 Remove every installed Java 11 and 17

Environment:
  JLO_HOME   J'Lo's own directory (default ~/.jlo): the shell scripts,
             the generated completions and default.jlorc. This is NOT
             where JDKs are installed - those go to the IntelliJ IDEA
             directory (~/Library/Java/JavaVirtualMachines on macOS,
             ~/.jdks elsewhere), which is not configurable.

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

/// The completion script for `shell`, as bytes.
///
/// Built here rather than at each call site because there are two: `jlo
/// completions <shell>` writes it to stdout, and the install verb writes it
/// into `$JLO_HOME/completions`. Both must see the same clap tree, or a
/// generated file offers flags the binary beside it no longer has.
pub(crate) fn completion_script(shell: clap_complete::Shell) -> Vec<u8> {
    use clap::CommandFactory;

    let mut command = Cli::command();
    let mut out = Vec::new();
    clap_complete::generate(shell, &mut command, "jlo", &mut out);
    out
}

/// Print the top-level help, exactly as `jlo -h` does.
pub(crate) fn print_help() {
    use clap::CommandFactory;

    // A closed stdout (`jlo | head`) is not an error worth reporting.
    // `print_help` already ends its output with a newline; an extra
    // `println!()` here would double it and desync `jlo` from `jlo --help`.
    let _ = Cli::command().print_help();
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
    // `print_help`/`print_long_help` already end their output with a
    // newline; an extra `println!()` here would double it (see the
    // corresponding comment in `print_help`).
    let result = if long {
        exec.print_long_help()
    } else {
        exec.print_help()
    };
    let _ = result;
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
Pass --offline to use only what is already installed: the exports if
the JDK is there, exit status 1 and no exports if it is not, and no
network access either way. That is how the autoload hook calls it, so
entering a directory never starts a download.

When VERSION is omitted, it resolves from the nearest .jlorc at or
above the current directory, then ~/.jlo/default.jlorc. Only major
versions are accepted: 21, not 21.0.5.

Pass --verbose for one line on stderr naming the JDK that is now
active and where the version came from - including when the shell was
already on it. The autoload hook never passes it, which is why the
report is opt-in: it runs on every new shell and every cd."
    )]
    Env {
        /// Java major version. Default: from .jlorc
        version: Option<String>,

        /// Only look at installed JDKs; never download, never touch the network
        #[arg(long)]
        offline: bool,

        /// Report the active JDK and where the version came from, on stderr
        #[arg(short, long)]
        verbose: bool,
    },

    #[command(
        about = "Print the JAVA_HOME path for a version",
        long_about = "\
Print the JAVA_HOME path for a version

Writes the path and nothing else to stdout, so $(jlo home 21) stays
clean. Unlike jlo env it does not modify the current shell.

The JDK is downloaded from Adoptium on demand if it is not installed.
Pass --offline to answer from what is already installed instead: the
path if it is there, exit status 1 if it is not, and no network access
either way.

When VERSION is omitted, it resolves from the nearest .jlorc at or
above the current directory, then ~/.jlo/default.jlorc. Only major
versions are accepted: 21, not 21.0.5."
    )]
    Home {
        /// Java major version. Default: from .jlorc
        version: Option<String>,

        /// Only look at installed JDKs; never download, never touch the network
        #[arg(long)]
        offline: bool,
    },

    #[command(
        about = "Run a command with a given JDK active",
        long_about = "\
Run a command with a given JDK active

The literal -- separates the optional version from the command:

  jlo exec 21 -- ./gradlew build
  jlo exec -- java -version

JAVA_HOME is set and the JDK's bin directory is prepended to PATH for
the child only; the current shell is untouched.",
        // The default rendering (`Usage: jlo exec [ARGS]...`) reads as
        // free-form optional args and hides the mandatory `--`. Match the
        // usage line `cmd_exec`'s own error path prints on a parse
        // failure (main.rs), so `-h` and the error agree.
        override_usage = "jlo exec [VERSION] -- <COMMAND> [ARGS]..."
    )]
    Exec {
        /// [VERSION] -- <COMMAND>...
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },

    // Attribute strings rather than a doc comment, for the reason given at
    // the top of this enum.
    //
    // No --check flag: a CI gate wanting "is this shell on the pinned JDK?"
    // is exactly that, and the name is reserved for it. It is not built,
    // because one asker is not yet a case - until a second turns up, the
    // exit code stays "is there an answer at all".
    #[command(
        about = "Show which JDK is active in this shell, and why",
        long_about = "\
Show which JDK is active in this shell, and why

Starts from the live JAVA_HOME, not from .jlorc. The two can
legitimately disagree - a shell left over from before you entered the
project, say - and saying so is most of what this command is for.

One line on stdout names the active version and where it came from; any
advisory goes to stderr. Exits 1 when there is no answer at all: no
JAVA_HOME, or a JAVA_HOME pointing at a jlo install that has since been
removed.

Never touches the network, so there is no --offline flag to pass, and
takes no version argument - current means the active one. To ask where
some other version lives, see jlo home."
    )]
    Current,

    /// Show available and installed JDKs
    #[command(visible_alias = "ls")]
    List {
        /// List only what is installed; never touch the network
        #[arg(long)]
        offline: bool,
    },

    /// Update installed JDKs to their latest minor release
    ///
    /// With no argument, updates the version from the nearest .jlorc at or
    /// above the current directory, or ~/.jlo/default.jlorc. Pass --all to update every installed major
    /// version, or list major versions explicitly.
    Update {
        /// Major versions to update. Default: from .jlorc
        versions: Vec<String>,

        /// Update every installed major version
        #[arg(short, long, conflicts_with = "versions")]
        all: bool,
    },

    /// Remove superseded minor versions
    ///
    /// Keeps the newest minor release of every installed major version and
    /// deletes the rest. Only JDKs J'Lo installed are touched. To remove a
    /// version outright rather than by rule, see jlo remove.
    Prune,

    // Attribute strings rather than a doc comment, for the reason given at
    // the top of this enum: JAVA_HOME below would have to be backtick-quoted
    // in rustdoc, and clap would then print the backticks.
    #[command(
        about = "Remove installed JDKs",
        long_about = "\
Remove installed JDKs

Each VERSION is either a major version - jlo remove 17 removes every
installed 17.x - or the exact version of one install, e.g. 17.0.11+10.
Name several to remove them in one go:

  jlo remove 11 17

An install J'Lo will not delete is reported and skipped; the others
still go. There are three such cases: nothing installed matches the
version, J'Lo did not install it, or JAVA_HOME points at it. Each is an
error only when it leaves nothing to remove at all.

Use jlo prune to remove superseded minor versions by rule instead."
    )]
    Remove {
        /// Major versions, or exact versions of single installs
        #[arg(required = true)]
        versions: Vec<String>,
    },

    /// Write .jlorc pinning this project's Java version
    ///
    /// With no argument, pins the latest version Adoptium offers. Only major
    /// versions are accepted: 21, not 21.0.5.
    ///
    /// --global writes ~/.jlo/default.jlorc instead: the version jlo env
    /// falls back to when no .jlorc is found.
    Init {
        /// Java major version. Default: latest release
        version: Option<String>,

        /// Write ~/.jlo/default.jlorc instead of ./.jlorc
        #[arg(short, long)]
        global: bool,

        /// Overwrite the config file if it already exists
        #[arg(short, long)]
        force: bool,
    },

    /// Update jlo itself
    ///
    /// Handled by the jlo shell function the installer generates, not by
    /// this binary.
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
    // The `sing` easter egg is deliberately NOT a variant here - see
    // `main`'s comment for why.
}
