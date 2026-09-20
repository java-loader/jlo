# J'Lo – the Java Loader

The Java Loader (or J'Lo for short) is a minimalistic tool to download and manage Java installations on your machine.
It is written in Rust, with a main focus on simplicity and ease of use.

J'Lo currently supports Linux (x86_64, aarch64) and macOS (arm64).

At the moment, only the [Eclipse Temurin](https://adoptium.net/de/temurin/releases) distribution is available.
Java versions are supported starting from Java 8, with all newer versions working automatically.

## Installing J'Lo

To install J'Lo on Unix-like systems (Linux, macOS, WSL, etc.):

```shell
/bin/bash -c "$(curl -fsSL https://raw.githubusercontent.com/java-loader/jlo/refs/heads/main/install.sh)"
```

This will download the latest J'Lo binary and install it to `~/.jlo/`.
You can safely re-run this command to update J'Lo to the latest version, which is basically the same as running
`jlo selfupdate`.

> [!IMPORTANT]
> Watch the output closely, as you will have to add some lines to your shell profile to make `jlo` available in your
> terminal.

## Quick Start

Setup environment for Java 25 (installing it first, if necessary):

```shell
# Update JAVA_HOME and PATH for Java 25 in the current shell session.
jlo env 25

# Optionally, verify that the correct Java version is being used:
echo $JAVA_HOME
which java
```

Alternatively:

```shell
cd /path/to/your/project

# One-time setup: create a .jlorc file that pins Java 25 for this project.
jlo init 25

# Setup JAVA_HOME and PATH for the Java version specified in the .jlorc file.
jlo env
```

> [!TIP]
> If you enabled the J'Lo autoload feature during installation, J'Lo will automatically set up the Java environment
> whenever you `cd` into a directory that contains a `.jlorc` file.

JDKs are installed to `~/Library/Java/JavaVirtualMachines/` on macOS and `~/.jdks/` on Linux and Windows —
the same locations IntelliJ IDEA uses, so both tools see the same JDKs.
This allows automatic discovery of installed JDKs by IDEs like IntelliJ IDEA.

# J’Lo Command Reference

## Table of Contents

1. [Environment Setup](#environment-setup)
2. [Resolving JAVA_HOME](#resolving-java_home)
3. [Executing a Command](#executing-a-command)
4. [Initialization](#initialization)
5. [Updating Java Versions](#updating-java-versions)
6. [Cleaning Installed Versions](#cleaning-installed-versions)
7. [Managing J’Lo Itself](#managing-jlo-itself)
8. [Getting Help](#getting-help)
9. [Shell Completions](#shell-completions)

## Environment Setup

The command `jlo env` configures the current shell session by setting the `JAVA_HOME` and `PATH` environment variables
to point to the desired JDK installation.

`jlo use` is an alias for `jlo env`.

**Behavior:**
- If the current directory contains a `.jlorc` file, `jlo env` uses the version it specifies.
- Otherwise, it falls back to `~/.jlo/default.jlorc`.
- If the requested Java version is not installed, it will be downloaded and installed automatically.
- This command affects only the current shell session.
- VERSION is a major version only, e.g. `25`, not `25.0.5`. This applies everywhere a command takes a VERSION
  argument.

**Usage examples:**
```shell
# set environment based on .jlorc (fallbacks described above)
jlo env

# set environment for Java 25
jlo env 25
````

## Resolving JAVA_HOME

The command `jlo home` prints the `JAVA_HOME` path for the requested Java version to standard output — and nothing else.
Unlike `jlo env`, it does not modify the current shell; it just tells you where a JDK lives. This makes it suitable for
scripts, Makefiles, CI pipelines, and other non-interactive contexts (see
[CI / scripting / AI agents](#ci--scripting--ai-agents)).

**Behavior:**
- Version resolution is identical to `jlo env`: an explicit argument wins, otherwise the current directory's `.jlorc`,
  otherwise `~/.jlo/default.jlorc`.
- If the requested Java version is not installed, it will be downloaded and installed automatically.
- Only the resolved path is written to standard output; all diagnostics (download progress, etc.) go to standard error,
  so `$(jlo home …)` stays clean.

It is the J'Lo equivalent of macOS's `/usr/libexec/java_home -v <version>`.

**Usage examples:**
```shell
# print JAVA_HOME for the version from .jlorc / default.jlorc
jlo home

# print JAVA_HOME for Java 25
jlo home 25

# capture it into an environment variable
export JAVA_HOME="$(jlo home 25)"
```

## Executing a Command

The command `jlo exec [version] -- <command> [args...]` runs a command with `JAVA_HOME` set and the JDK's `bin`
directory prepended to `PATH`, without changing the current shell. This is the most convenient way to run a build or
tool against a specific Java version from CI, scripts, or an AI agent (see
[CI / scripting / AI agents](#ci--scripting--ai-agents)). It is the J'Lo equivalent of `mise exec` / `asdf exec`.

**Behavior:**
- The literal `--` separates the optional version from the command. Version resolution is identical to `jlo env`:
  an explicit version wins, otherwise `.jlorc`, otherwise `~/.jlo/default.jlorc`.
- If the requested Java version is not installed, it will be downloaded and installed automatically.
- On Unix the command replaces the J'Lo process (`execvp`), so its exit code and signals propagate transparently.

**Usage examples:**
```shell
# run a Gradle build with Java 21
jlo exec 21 -- ./gradlew build

# use the version from .jlorc / default.jlorc (no version before --)
jlo exec -- java -version
```

## Initialization

The command `jlo init` creates a `.jlorc` file in the current directory that pins a specific Java version.
The file is used by `jlo env` to determine which Java version to set up.

Specifying a version is optional; if omitted, the latest available Java version will be used.

With `--global`, the file is written to `~/.jlo/default.jlorc` instead — the default Java version used by
`jlo env` when no `.jlorc` file is found in the current directory.

This command fails if the config file already exists; pass `--force` to overwrite it.

**Usage examples:**
```shell
# create .jlorc that pins Java 25
jlo init 25

# create .jlorc that pins the latest available Java version
jlo init

# set the user-wide default version
jlo init --global 25

# change a version that is already pinned
jlo init --force 21
```

**Example `.jlorc` file content:**
```
# Java version configured by J'Lo - https://github.com/java-loader/jlo
25
```

## Updating Java Versions

The command `jlo update` updates installed Java versions to their latest minor releases.

The quickest way to deal with everything `jlo list` flags as `outdated` is:

```shell
jlo update --all
```

That updates every installed major version in one go — no need to name them individually.

**Behavior:**
- The `--all` flag updates all installed Java versions; it cannot be combined with explicit versions.
- Multiple versions can be specified as arguments; each will be updated to its latest minor release.
  Missing versions will be installed automatically.
- If no arguments are provided, it updates the Java version specified in the `.jlorc` file in the current directory,
  falling back to `~/.jlo/default.jlorc` if none is found.
- The superseded minor release stays on disk — an open shell or IDE may still point at it. When an update leaves one
  behind, `jlo update` ends with a reminder to run [`jlo clean`](#cleaning-installed-versions).

**Usage examples:**
```shell
# update all installed Java versions
jlo update --all

# update Java version specified in .jlorc or ~/.jlo/default.jlorc
jlo update

# update Java versions 21 and 25
jlo update 21 25
```

## Listing Versions

The command `jlo list` shows every JDK Adoptium offers for this OS and architecture, newest first, with the
latest build of each major version — annotated with what you already have installed.

```bash
jlo list
```

```
26  26.0.2+101              installed
25  25.0.4+101.0.LTS   LTS  installed
24  24.0.2+12
21  21.0.12+101.0.LTS  LTS  outdated (21.0.11+10.0.LTS)
17  17.0.20+101        LTS  installed
11  11.0.32+101        LTS
```

Every line starts with the **major version** — that is the number `jlo update`, `jlo exec` and `.jlorc` expect, so
you can read a row and use it directly:

```bash
jlo update 21
```

When any row is marked `outdated`, `jlo list` prints a reminder:

```
TIP: Use `jlo update --all` to update all outdated JDKs.
```

The tip goes to standard error, so it never ends up in a pipe alongside the listing.

Major versions Adoptium has no build for on this platform are omitted, so everything listed is installable.

Add `--offline` to skip the network and list only what is installed locally — including every minor version you
have, not just the newest per major. Installations J'Lo did not create are marked `(unmanaged)`; `jlo clean`
leaves those alone.

```bash
jlo list --offline
```

Colours switch off automatically when the output is not a terminal, so `jlo list | grep LTS` and friends work as
expected.

## Cleaning Installed Versions

The command `jlo clean` removes older minor versions of installed Java versions, keeping only the latest minor
release for each major version.

J'Lo only removes installations at `~/.jdks/` (or `~/Library/Java/JavaVirtualMachines/` on macOS) that were installed
by J'Lo itself.

## Managing J’Lo Itself

- `jlo --version` prints the currently installed J’Lo version.
- The command `jlo selfupdate` updates J’Lo itself to the latest version.

## Getting Help

Every command documents itself:

```shell
# overview of all commands
jlo --help

# details for one command, including its arguments
jlo env --help
jlo exec --help
```

Running `jlo` with no arguments prints the same overview as `jlo --help`. `-h` prints a shorter summary of the same
help; `--help` prints the long form (with more explanation on subcommands).

`jlo --version` (or `jlo -V`) prints the installed version.

## Shell Completions

The installer generates completion scripts for **bash and zsh** and writes them to `$JLO_HOME/completions`, then
prints the lines to add to your shell profile:

```shell
# Shell completions:
if [ -n "$BASH_VERSION" ]; then
  [[ -s "$JLO_HOME/completions/jlo.bash" ]] && source "$JLO_HOME/completions/jlo.bash"
fi
if [ -n "$ZSH_VERSION" ]; then
  (( $+functions[compdef] )) || { autoload -Uz compinit && compinit -i; }
  [[ -s "$JLO_HOME/completions/_jlo" ]] && source "$JLO_HOME/completions/_jlo"
fi
```

Fish is not supported: the core `jlo` shell function (`jlo-init.sh`) is bash/zsh syntax and cannot be sourced from
fish, so `jlo env`/`jlo use` don't work there regardless of completions. If you use fish anyway and still want
completions for the subset of J'Lo that works as a plain binary, you can generate a script yourself:

```shell
jlo completions fish > ~/.config/fish/completions/jlo.fish
```

`jlo completions <shell>` supports `bash`, `elvish`, `fish`, `powershell` and `zsh`, and can also be used to load
completions without a file:

```shell
source <(jlo completions bash)
```

# CI / scripting / AI agents

In interactive shells, `jlo` is a shell function (defined by `jlo-init.sh`) — this is what lets `jlo env` mutate your
current session. Non-interactive shells (CI jobs, `Makefile` recipes, scripts, AI coding agents) don't load that
function, so J'Lo's installer also places a real `jlo` binary on your `PATH` at `~/.local/bin/jlo`. Every subcommand
works there directly except `env`/`use` (which must mutate the current shell) and `selfupdate` (which is handled by the
shell function) — those need the interactive shell integration.

For non-interactive use, prefer `jlo exec` and `jlo home` over `jlo env`, since they don't rely on shell integration.
`jlo exec` runs a command with the right Java on `PATH`; `jlo home` just prints the `JAVA_HOME` path. Both install the
JDK on demand if needed.

```shell
# Run a build against a specific Java version without any shell integration:
jlo exec 21 -- ./gradlew build

# Or, when you only need the path (e.g. to export it for several later commands):
export JAVA_HOME="$(jlo home 21)"
```

```makefile
# Makefile
build:
	jlo exec 21 -- ./gradlew build
```

> **Note:** `~/.local/bin` is on `PATH` by default on most Linux setups but not on macOS. If `jlo` isn't found in a
> non-interactive shell, add `export PATH="$HOME/.local/bin:$PATH"` to your profile (the installer prints this hint when
> needed).

# Uninstalling J'Lo

To uninstall J'Lo, remove the `~/.jlo/` directory, the `~/.local/bin/jlo` symlink, and the lines you added to your
shell profile during installation.

You may also want to remove the `~/.jdks/` directory (or `~/Library/Java/JavaVirtualMachines/` on macOS) if you no
longer need the installed JDKs.
