//! The command-line contract.
//!
//! Every command, argument and line of help text lives here, so the help
//! output and the dispatch are the same declaration and cannot drift.

use crate::ui;
use clap::{Parser, Subcommand, ValueEnum};

/// `jlo -h` and a bare `jlo`: enough to get going.
fn after_help() -> String {
    format!(
        "\
{examples}

VERSION is a Java major (25) or a pre-release stream (28-ea). Left out,
env, home, exec and install take it from .jlorc - '--help' has the full
lookup.",
        examples = examples(EXAMPLES.iter().filter(|e| e.2)),
    )
}

/// `jlo --help`: the whole contract.
fn after_long_help() -> String {
    format!(
        "\
env, home, exec, install, update and init take a Java major version or a
pre-release stream (28-ea):

  jlo env [VERSION]          jlo install [VERSION...]
  jlo home [VERSION]         jlo update [VERSION...]
  jlo exec [VERSION] -- <COMMAND> [ARGS]...
  jlo init <VERSION>

When VERSION is omitted, env, home, exec and install resolve it in four
steps: the nearest .jlorc at or above the current directory, then
~/.jlo/default.jlorc, then the newest JDK already installed, then the latest
release, which is downloaded. --offline stops after the third step rather
than downloading. update instead takes every installed name.

{examples}

{environment}
  JLO_HOME   J'Lo's own directory (default ~/.jlo): the shell scripts,
             the generated completions and default.jlorc. This is NOT
             where JDKs are installed - those go to the IntelliJ IDEA
             directory (~/Library/Java/JavaVirtualMachines on macOS,
             ~/.jdks elsewhere), which is not configurable.",
        examples = examples(EXAMPLES.iter()),
        environment = ui::help_heading("Environment:"),
    )
}

/// `jlo exec`'s usage line, shared by `-h` and the argument error's hint.
pub(crate) const EXEC_USAGE: &str = "jlo exec [VERSION] -- <COMMAND> [ARGS]...";

/// Command, what it does, and whether the short help shows it too.
const EXAMPLES: &[(&str, &str, bool)] = &[
    ("jlo env 25", "Use Java 25 in this shell", true),
    ("jlo init 21", "Pin Java 21 for this project", true),
    ("jlo env", "Use the pinned version", false),
    ("jlo current", "Show which JDK is active, and why", false),
    (
        "jlo exec 21 -- ./gradlew build",
        "Run a build on Java 21",
        true,
    ),
    (
        "jlo install 25",
        "Download Java 25 without switching to it",
        false,
    ),
    (
        "jlo install 28-ea",
        "Download the early-access build of Java 28",
        false,
    ),
    ("jlo update", "Bring every installed JDK up to date", true),
    (
        "jlo remove 11 17",
        "Remove every installed Java 11 and 17",
        false,
    ),
    (
        "jlo remove --superseded",
        "Remove every superseded minor release",
        false,
    ),
];

/// The `Examples:` section. Padded by hand, because `format!` width counts
/// the escape codes around a styled command as characters.
fn examples<'a>(rows: impl Iterator<Item = &'a (&'a str, &'a str, bool)> + Clone) -> String {
    let width = rows.clone().map(|(cmd, ..)| cmd.len()).max().unwrap_or(0);
    let lines = rows.map(|(cmd, what, _)| {
        let pad = " ".repeat(width - cmd.len());
        format!("  {}{pad}   {what}", ui::help_literal(cmd))
    });
    std::iter::once(ui::help_heading("Examples:"))
        .chain(lines)
        .collect::<Vec<_>>()
        .join("\n")
}

#[derive(Debug, Parser)]
#[command(
    name = "jlo",
    bin_name = "jlo",
    version,
    about = concat!(
        "J'Lo ",
        env!("CARGO_PKG_VERSION"),
        " - the Java Loader. Download, manage and switch JDKs."
    ),
    after_help = after_help(),
    after_long_help = after_long_help()
)]
pub(crate) struct Cli {
    // Deliberately optional. `arg_required_else_help` would print the help but
    // exit 2; a bare `jlo` is exploration, not a usage error, so `main` handles
    // the `None` case and exits 0.
    #[command(subcommand)]
    pub(crate) command: Option<Command>,
}

/// The shells `jlo completions` accepts: the two jlo supports, not every one
/// `clap_complete` can target, since 1.0 freezes what is accepted.
#[derive(Debug, Clone, Copy, ValueEnum)]
pub(crate) enum CompletionShell {
    Bash,
    Zsh,
}

impl From<CompletionShell> for clap_complete::Shell {
    fn from(shell: CompletionShell) -> Self {
        match shell {
            CompletionShell::Bash => Self::Bash,
            CompletionShell::Zsh => Self::Zsh,
        }
    }
}

/// One builder for `jlo completions` and the install verb, so a generated file
/// never offers flags the binary beside it no longer has.
pub(crate) fn completion_script(shell: clap_complete::Shell) -> Vec<u8> {
    use clap::CommandFactory;

    let mut command = Cli::command();
    let mut out = Vec::new();
    clap_complete::generate(shell, &mut command, "jlo", &mut out);
    out
}

pub(crate) fn print_help() {
    use clap::CommandFactory;

    // A closed stdout is not an error worth reporting. No extra `println!()`:
    // `print_help` already ends in a newline.
    let _ = Cli::command().print_help();
}

/// `exec` is intercepted before clap, so clap never sees its `-h`/`--help`.
/// `long` is `--help`.
pub(crate) fn print_exec_help(long: bool) {
    use clap::CommandFactory;

    let mut cmd = Cli::command();
    // Without it the extracted `exec` does not know its usage line starts
    // with "jlo ".
    cmd.build();
    let exec = cmd
        .find_subcommand_mut("exec")
        .expect("the `exec` subcommand is always registered");
    // As in `print_help`: a closed stdout is fine, no extra newline.
    let _ = if long {
        exec.print_long_help()
    } else {
        exec.print_help()
    };
}

#[derive(Debug, Subcommand)]
pub(crate) enum Command {
    // `about`/`long_about` are attribute strings, not doc comments:
    // `clippy::doc_markdown` wants JAVA_HOME backtick-quoted in rustdoc, and
    // clap renders doc-comment backticks literally. No backticks anywhere
    // below, for the same reason.
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

When VERSION is omitted, it resolves in four steps: the nearest
.jlorc at or above the current directory, then ~/.jlo/default.jlorc,
then the newest JDK already installed, then the latest release, which
is downloaded. The third step does not ask Adoptium whether something
newer exists, so a machine holding only Java 17 resolves to 17.
--offline stops after that step instead of downloading. A major
version (21) or a pre-release stream (28-ea) is accepted, not an
exact build: 21, not 21.0.5.

On success stdout carries the export statements and nothing else.
Anything else jlo has to say - download progress, a warning - goes to
stderr, where the shell will not try to execute it. To see which JDK
is active and where the version came from, run jlo current - it starts
from the live JAVA_HOME, so it can also say when the two disagree."
    )]
    Env {
        /// Java version: a major (21) or a pre-release stream (28-ea). Default: .jlorc, the newest installed JDK, then the latest release
        version: Option<String>,

        /// Only look at installed JDKs; never download, never touch the network
        #[arg(long)]
        offline: bool,
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

When VERSION is omitted, it resolves in four steps: the nearest
.jlorc at or above the current directory, then ~/.jlo/default.jlorc,
then the newest JDK already installed, then the latest release, which
is downloaded. A major version (21) or a pre-release stream (28-ea)
is accepted, not an exact build: 21, not 21.0.5."
    )]
    Home {
        /// Java version: a major (21) or a pre-release stream (28-ea). Default: .jlorc, the newest installed JDK, then the latest release
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
        // free-form optional args and hides the mandatory `--`.
        override_usage = EXEC_USAGE
    )]
    Exec {
        /// [VERSION] -- <COMMAND>...
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },

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

    #[command(
        visible_alias = "ls",
        about = "Show available and installed JDKs",
        long_about = "\
Show available and installed JDKs

One row per name (25, 28-ea). Other builds of a name sit on indented
lines below it, with the exact version to pass to jlo remove and a
status word: superseded, an older build jlo installed; unmanaged, a JDK
jlo did not install - moved into the directory by hand under its version
(21.0.11+9) - which jlo never deletes or updates."
    )]
    List {
        /// List only what is installed; never touch the network
        #[arg(long)]
        offline: bool,
    },

    #[command(
        about = "Install JDKs; without a version, the one jlo env would use",
        long_about = "\
Install JDKs; without a version, the one jlo env would use

Downloads the latest build Adoptium offers of every name given - a
major (21) or a pre-release stream (28-ea), not an exact build: 21, not
21.0.5. A name already on that build or a newer one is reported and
left alone - never moved back; one Adoptium has no build of for this
machine is skipped with a warning.

install and update are one operation. They differ only when no version
is given: install resolves one in four steps - the nearest .jlorc at or
above the current directory, then ~/.jlo/default.jlorc, then the newest
JDK already installed, then the latest release - while update takes
every installed name.

The new build replaces the old one: J'Lo keeps one build per name, so
each download deletes the builds it supersedes - including the one
JAVA_HOME points at, in which case this shell moves to the new build.
Other shells still on it need 'jlo env' again. Otherwise no shell is
touched.

The usual route is jlo env, which switches the current shell and
downloads the JDK on demand if it is missing, so an explicit install
is for the cases that come before that - warming a CI cache, preparing
for offline work, or seeding a machine without switching it."
    )]
    Install {
        /// Java version: a major (21) or a pre-release stream (28-ea). Default: .jlorc, the newest installed JDK, then the latest release
        versions: Vec<String>,
    },

    #[command(
        about = "Update JDKs; without a version, every installed one",
        long_about = "\
Update JDKs; without a version, every installed one

Downloads the latest build Adoptium offers of every name given - a
major (21) or a pre-release stream (28-ea), not an exact build: 21, not
21.0.5. A name already on that build or a newer one is reported and
left alone - never moved back; one Adoptium has no build of for this
machine is skipped with a warning.

install and update are one operation. They differ only when no version
is given: update takes every installed name, pre-release streams
included, while install resolves one version the way jlo env does.

The new build replaces the old one: J'Lo keeps one build per name, so
each download deletes the builds it supersedes - including the one
JAVA_HOME points at, in which case this shell moves to the new build.
Other shells still on it need 'jlo env' again."
    )]
    Update {
        /// Java version: a major (21) or a pre-release stream (28-ea). Default: every installed name
        versions: Vec<String>,
    },

    #[command(
        about = "Remove installed JDKs",
        long_about = "\
Remove installed JDKs

Name what goes, either way round: by version, or by rule.

Each VERSION is either a name - a major (17) or a pre-release stream
(28-ea) - or the exact version of one install, e.g. 17.0.11+10. Naming
one leaves the rest alone: jlo remove 17 removes every installed
17.x, but leaves 17-ea alone. Name several to remove them in one go:

  jlo remove 11 17

An install J'Lo will not delete is reported and skipped; the others
still go. There are three such cases: nothing installed matches the
version, J'Lo did not install it, or JAVA_HOME points at it. Each is an
error only when it leaves nothing to remove at all.

--superseded names them by rule instead: keep the newest build of
every installed name, delete the rest. It takes no VERSION - the
rule is the selector - and, being nobody's explicit request, it leaves
an install J'Lo did not make alone without calling it an error."
    )]
    Remove {
        /// Java version: a major (21) or a pre-release stream (28-ea), or the exact version of a single install
        versions: Vec<String>,

        /// Remove every superseded minor release instead of a named version
        #[arg(
            long,
            conflicts_with = "versions",
            required_unless_present = "versions"
        )]
        superseded: bool,
    },

    /// Write .jlorc pinning this project's Java version
    ///
    /// A major version (21) or a pre-release stream (28-ea), not an exact
    /// build: 21, not 21.0.5.
    ///
    /// --global writes ~/.jlo/default.jlorc instead: the version jlo env
    /// falls back to when no .jlorc is found, ahead of the newest JDK
    /// already installed.
    Init {
        // No default: a committed .jlorc pins a choice, not whatever was
        // newest. A default can be added after 1.0; taking one back cannot.
        /// Java version: a major (21) or a pre-release stream (28-ea)
        version: String,

        /// Write ~/.jlo/default.jlorc instead of ./.jlorc
        #[arg(short, long)]
        global: bool,

        /// Overwrite the config file if it already exists
        #[arg(short, long)]
        force: bool,
    },

    /// Update jlo itself
    ///
    /// Runs the installer of the latest release, which verifies its published
    /// SHA256 and replaces this binary. When jlo is already current, rewrites
    /// its shell files instead. Either way the calling shell is reloaded.
    Selfupdate,

    #[command(
        about = "Print a shell completion script",
        long_about = "\
Print a shell completion script

The installer writes these to $JLO_HOME/completions. To load one
directly:

  eval \"$(jlo completions bash)\"

Not 'source <(...)': process substitution silently sources nothing
under the bash 3.2 macOS ships, and still exits 0."
    )]
    Completions {
        /// Shell to generate completions for
        shell: CompletionShell,
    },
}
