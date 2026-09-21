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

Then add one line to your shell profile (`~/.zshrc`, `~/.bashrc`):

```shell
[ -s "$HOME/.jlo/jlo.sh" ] && . "$HOME/.jlo/jlo.sh"
```

Two more are optional — add either, both, or neither:

```shell
[ -s "$HOME/.jlo/autoload.sh" ] && . "$HOME/.jlo/autoload.sh"          # switch JDK on cd
[ -s "$HOME/.jlo/completions.sh" ] && . "$HOME/.jlo/completions.sh"    # tab completion
```

The installer prints these same lines at the end, with the right paths if you use a custom
[`JLO_HOME`](#jlo_home). They never change: an upgrade regenerates the files they point at, so you only paste once.

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
> whenever you `cd` into a project with a `.jlorc` file — including its subdirectories. It never downloads anything:
> entering a project that pins a JDK you do not have prints one line telling you so, and leaves the environment alone.
> Run `jlo env` yourself to install it.

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
6. [Listing Versions](#listing-versions)
7. [Removing Versions](#removing-versions)
8. [Pruning Superseded Versions](#pruning-superseded-versions)
9. [Using a JDK J'Lo Did Not Install](#using-a-jdk-jlo-did-not-install)
10. [Managing J’Lo Itself](#managing-jlo-itself)
11. [Getting Help](#getting-help)
12. [Supported Shells](#supported-shells)
13. [Shell Completions](#shell-completions)
14. [Environment Variables](#environment-variables)

## Environment Setup

The command `jlo env` configures the current shell session by setting the `JAVA_HOME` and `PATH` environment variables
to point to the desired JDK installation.

`jlo use` is an alias for `jlo env`.

**Behavior:**
- `jlo env` uses the nearest `.jlorc` file, searching the current directory and then its parents. The search stops
  at your home directory or at a repository root (a directory containing `.git`), so a `.jlorc` outside the project
  is never picked up.
- If no `.jlorc` is found, it falls back to `~/.jlo/default.jlorc`.
- If the requested Java version is not installed, it will be downloaded and installed automatically.
- `--offline` uses only what is already installed: it sets the environment if the version is there, and otherwise
  prints one line to standard error and exits with status 1, leaving the environment untouched. This is how the
  autoload hook calls it, so entering a project never starts a download — see the note below.
- This command affects only the current shell session.
- VERSION is a major version only, e.g. `25`, not `25.0.5`. This applies everywhere a command takes a VERSION
  argument.

**Usage examples:**
```shell
# set environment based on .jlorc (fallbacks described above)
jlo env

# set environment for Java 25
jlo env 25

# set it only if the JDK is already here; never download
jlo env --offline
````

## Resolving JAVA_HOME

The command `jlo home` prints the `JAVA_HOME` path for the requested Java version to standard output — and nothing else.
Unlike `jlo env`, it does not modify the current shell; it just tells you where a JDK lives. This makes it suitable for
scripts, Makefiles, CI pipelines, and other non-interactive contexts (see
[CI / scripting / AI agents](#ci--scripting--ai-agents)).

**Behavior:**
- Version resolution is identical to `jlo env`: an explicit argument wins, otherwise the nearest `.jlorc` at or above
  the current directory, otherwise `~/.jlo/default.jlorc`.
- If the requested Java version is not installed, it will be downloaded and installed automatically.
- Only the resolved path is written to standard output; all diagnostics (download progress, etc.) go to standard error,
  so `$(jlo home …)` stays clean.
- `--offline` answers from what is already installed and never touches the network: it prints the path if the version
  is there, and exits with status 1 if it is not. Use it to *ask* whether a JDK is available without risking a
  several-hundred-megabyte download — in a CI step with a short timeout, or in a network-isolated sandbox.

It is the J'Lo equivalent of macOS's `/usr/libexec/java_home -v <version>`.

**Usage examples:**
```shell
# print JAVA_HOME for the version from .jlorc / default.jlorc
jlo home

# print JAVA_HOME for Java 25
jlo home 25

# capture it into an environment variable
export JAVA_HOME="$(jlo home 25)"

# is Java 21 available here? no download, no network, exit code is the answer
jlo home --offline 21
```

## Executing a Command

The command `jlo exec [version] -- <command> [args...]` runs a command with `JAVA_HOME` set and the JDK's `bin`
directory prepended to `PATH`, without changing the current shell. This is the most convenient way to run a build or
tool against a specific Java version from CI, scripts, or an AI agent (see
[CI / scripting / AI agents](#ci--scripting--ai-agents)). It is the J'Lo equivalent of `mise exec` / `asdf exec`.

**Behavior:**
- The literal `--` separates the optional version from the command. Version resolution is identical to `jlo env`:
  an explicit version wins, otherwise the nearest `.jlorc` at or above the current directory, otherwise
  `~/.jlo/default.jlorc`.
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
`jlo env` when no `.jlorc` file is found.

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
- If no arguments are provided, it updates the Java version specified in the nearest `.jlorc` file at or above the
  current directory, falling back to `~/.jlo/default.jlorc` if none is found.
- The superseded minor release stays on disk — an open shell or IDE may still point at it. When an update leaves one
  behind, `jlo update` ends with a reminder to run [`jlo prune`](#pruning-superseded-versions).

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

The command `jlo list` shows one row per JDK version, newest first: what Adoptium offers for this OS and
architecture, merged with everything installed locally.

```bash
jlo list
```

`jlo ls` is an alias for `jlo list`.

```
    26  26.0.2+101              installed
    25  25.0.4+101.0.LTS   LTS  update
    25  25.0.1+9.0.LTS     LTS  installed
    24  24.0.2+12
 →  21  21.0.12+101.0.LTS  LTS  installed
    21  21.0.9+10.0.LTS    LTS  superseded
    17  17.0.20+101        LTS  installed
     8  8.0.412+8               unmanaged
```

The arrow in the left-hand gutter marks the install your `$JAVA_HOME` currently points at. The gutter is always
there, so the columns sit in the same place whether or not anything is active. If `$JAVA_HOME` points somewhere
J'Lo did not install, no row is marked and `jlo list` says so on standard error.

After the gutter, every row starts with the **major version** — that is the number `jlo update`, `jlo exec` and
`.jlorc` expect, so you can read a row and use it directly:

```bash
jlo update 21
```

Each row ends in at most one status word, and each one names exactly one command:

| Status | Meaning | What acts on it |
| --- | --- | --- |
| *(blank)* | Adoptium offers it; you do not have it | `jlo env <major>` |
| `update` | Adoptium offers it, and it is newer than every build of that major you have | `jlo update <major>` |
| `installed` | Installed, and the newest build of its major that is | — |
| `superseded` | Installed, but a newer build of the same major is installed too | `jlo prune` |
| `unmanaged` | Installed without J'Lo's marker, so J'Lo will not delete it | remove it by hand |

They are single words on purpose: `jlo list | grep superseded` is a usable way to ask which installs `jlo prune`
would take.

`update` and `superseded` are deliberately different rows. Being behind Adoptium is a fact about a **major**, and
it lands on the row for the build you do not have yet. Being superseded is a fact about one **build** sitting next
to a newer sibling of its own major, and it lands on that build's own row — which is also where you read the exact
version `jlo remove` takes.

When either applies, `jlo list` ends on a single line of advice:

```
TIP: `jlo update --all` (2 outdated) · `jlo prune` (2 superseded)
```

One line, whatever applies — this prints on every `jlo list`, and a stack of suggestions under every listing reads
as nagging rather than as help. It goes to standard error, so it never ends up in a pipe alongside the rows.

Every row is either installable or installed: a major Adoptium has no build for on this platform is left out of the
catalogue, but a JDK you have installed always gets a row, so there is nowhere for a removable install to hide.

Add `--offline` to skip the network and list only what is installed. The rows and the status words are the same,
minus the catalogue — so nothing is ever marked `update`, and the LTS column is dropped, there being no catalogue
to read it from.

```bash
jlo list --offline
```

Colours switch off automatically when the output is not a terminal, so `jlo list | grep LTS` and friends work as
expected.

## Removing Versions

The command `jlo remove` deletes the installed JDKs you name. Its counterpart,
[`jlo prune`](#pruning-superseded-versions), deletes by rule instead — `remove` is the versions you chose, `prune` is
whatever "keep the newest minor of each major" leaves over.

```bash
jlo remove 17
```

Several can go at once, and a version may be a whole major or one exact build:

```bash
jlo remove 11 17.0.11+10
```

**Behavior:**
- Each VERSION is either a major version (removing every installed build of it) or the exact version of one install,
  as shown by `jlo list`. It is not a version range: `17.0` matches nothing.
- **An installation J'Lo will not delete is reported and skipped; the rest still go.** There are three such cases,
  and each is an error only when it leaves nothing to remove at all:
  - nothing installed matches the version — the JDK is already absent, which is what you asked for, so this is a
    note rather than a failure. `jlo remove 3 4 5 17` removes Java 17 and reports that nothing matched `3`, `4` or `5`.
  - J'Lo did not install it — no `.jlo-managed` marker. Remove it by hand when you are done with it.
  - your `JAVA_HOME` points at it. Deleting that one would leave your shell pointing at a path that no longer exists,
    so J'Lo leaves it and says so. Switch away (`jlo env 21`) and run the command again to get it. There is no flag to
    override this.
- None of the three can delete anything, so none of them stops the versions that can: `jlo remove 21 24 25 26` while
  your shell is on 26 removes 21, 24 and 25, and warns that 26 was left behind.
- Exit status is 1 when nothing was removed, 0 otherwise.

**Usage examples:**
```shell
# remove every installed Java 17
jlo remove 17

# remove Java 11 and Java 17; a version that is not installed is just reported
jlo remove 11 17

# remove one exact build, leaving its siblings in the same major alone
jlo remove 17.0.11+10
```

## Pruning Superseded Versions

The command `jlo prune` removes older minor versions of installed Java versions, keeping only the latest minor
release for each major version.

```bash
jlo prune
```

J'Lo only removes installations at `~/.jdks/` (or `~/Library/Java/JavaVirtualMachines/` on macOS) that were installed
by J'Lo itself.

> This command was called `jlo clean` before 1.0. The old name was removed rather than kept as an alias: `clean`
> suggested build output (`cargo clean`, `gradle clean`) — cheap, regenerable, safe — while this command deletes real
> JDKs, and leaving both names alive would have kept that reading available.

## Using a JDK J'Lo Did Not Install

J'Lo has no command that takes a path, but it does not need one: it finds every JDK in its install directory, whether
or not it put it there. To make a vendor-supplied JDK, a local OpenJDK build, or an early-access build usable by
`jlo env`, `jlo home` and `jlo exec`, move it into that directory under a version-shaped name:

```bash
# macOS
mv /path/to/jdk ~/Library/Java/JavaVirtualMachines/21.0.11+9

# Linux
mv /path/to/jdk ~/.jdks/21.0.11+9
```

The directory name is the whole registration: it has to parse as a semantic version with a major above zero, and that
major is what `jlo env 21` and friends then match on. The directory itself must be a JDK root — the one holding `bin`,
`lib` and `release`. On macOS that is the `Contents/Home` directory inside a `.jdk` bundle, not the bundle.

Such a JDK is **unmanaged**: it carries no `.jlo-managed` marker, so

- `jlo list --offline` shows it, annotated `(unmanaged)`;
- `jlo prune` and `jlo remove` will not delete it — remove it by hand when you are done with it;
- `jlo update` cannot update it. Adoptium does not list it, so there is no newer minor to resolve. If you also install
  that major from Adoptium, both remain, and `jlo env` picks the highest version of the two.

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

## Supported Shells

The shell integration (`jlo-init.sh`, `jlo-autoload.sh`) is written for **bash and zsh**, and is verified against
every version in the table below:

| Shell | Versions verified                                      | Notes                                                       |
|-------|--------------------------------------------------------|-------------------------------------------------------------|
| bash  | 3.2.57, 4.0, 4.1, 4.2, 4.3, 4.4, 5.0, 5.1, 5.2, 5.3    | 3.2.57 is what macOS ships as `/bin/bash`                   |
| zsh   | 5.0.8, 5.3, 5.4.2, 5.8, 5.9                            | macOS's default login shell since Catalina                  |

The installer (`install.sh`) and the autoload hook (`jlo-autoload.sh`) are POSIX `sh` clean and also parse and run
under `dash`. `jlo-init.sh` uses `local`, so it needs bash, zsh or another shell that has it — but it parses
everywhere, so a profile that sources it unconditionally will not fail with a syntax error.

All three files are safe to source from a profile that runs under `set -e` or `set -u`.

Fish is not supported — see the note under [Shell Completions](#shell-completions).

> **If you are on macOS's `/bin/bash`, update J'Lo.** Earlier releases sourced the binary's output from a process
> substitution, which bash 3.2 cannot read: `jlo env` set nothing and still exited 0, with no error. zsh and
> bash 4.0+ were never affected. Run `jlo selfupdate` to pick up the fix.

## Shell Completions

The installer generates completion scripts for **bash and zsh**, writes them to `$JLO_HOME/completions/`, and wraps
them in `$JLO_HOME/completions.sh`, which picks the right one for whichever shell sources it. Opt in with:

```shell
[ -s "$HOME/.jlo/completions.sh" ] && . "$HOME/.jlo/completions.sh"
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
eval "$(jlo completions bash)"
```

> Use `eval "$(...)"`, not `source <(...)`: bash 3.2 — the `/bin/bash` macOS ships — cannot `source` a process
> substitution. It reads nothing, sets up no completions, and still exits 0.

## Environment Variables

### `JLO_HOME`

Where J'Lo keeps **its own files** — it defaults to `~/.jlo` and holds:

| Path                        | Contents                                              |
|-----------------------------|-------------------------------------------------------|
| `$JLO_HOME/jlo.sh`          | profile entry point: exports `JLO_HOME`, defines the `jlo` function |
| `$JLO_HOME/autoload.sh`     | profile entry point, optional: switch JDK on `cd`      |
| `$JLO_HOME/completions.sh`  | profile entry point, optional: tab completion         |
| `$JLO_HOME/bin/`            | the `jlo-bin` binary and the `jlo-*.sh` shell scripts |
| `$JLO_HOME/completions/`    | the generated bash and zsh completion scripts         |
| `$JLO_HOME/default.jlorc`   | the user-wide default Java version (`jlo init --global`) |

The three `*.sh` files at the top are generated by the installer with their paths baked in, and are rewritten on every
install — don't edit them. Your profile sources them; it never sets `JLO_HOME` itself.

If `JLO_HOME` is already set when you run the installer, it installs into that directory, bakes that path into the
generated files, and prints it in the lines it tells you to add.

> [!IMPORTANT]
> `JLO_HOME` is **not** where the JDKs go, and it is **not** `JAVA_HOME`. Downloaded JDKs are installed to
> `~/Library/Java/JavaVirtualMachines/` (macOS) or `~/.jdks/` (Linux, Windows) — the locations IntelliJ IDEA uses — and
> that directory is not configurable. Pointing `JLO_HOME` elsewhere moves J'Lo's own config and scripts, not your JDKs.


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

# Probe first, when a download would be unwelcome: --offline never touches the
# network and exits non-zero if the JDK is not already installed.
if jlo home --offline 21 >/dev/null; then echo "Java 21 is here"; fi
```

Inside a step with a short timeout, or in a network-isolated sandbox, use `jlo home --offline` to ask whether a JDK is
available: without it, the question and the several-hundred-megabyte answer are the same command.

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
