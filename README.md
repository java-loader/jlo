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
You can safely re-run this command at any time — it is the bootstrap *and* the recovery path. For day-to-day upgrades
prefer [`jlo selfupdate`](#managing-jlo-itself), which checks the version first and reloads the shell you run it in.

Then add one line to your shell profile (`~/.zshrc`, `~/.bashrc`):

```shell
[ -s "$HOME/.jlo/jlo.sh" ] && . "$HOME/.jlo/jlo.sh"
```

Two more are optional — add either, both, or neither:

```shell
[ -s "$HOME/.jlo/autoload.sh" ] && . "$HOME/.jlo/autoload.sh"          # switch JDK on cd
[ -s "$HOME/.jlo/completions.sh" ] && . "$HOME/.jlo/completions.sh"    # tab completion
```

The installer ends by printing these as runnable commands — a single heredoc that appends the block to `~/.zshrc`
in one go, plus a `. ~/.jlo/jlo.sh` that makes J'Lo work in the shell you are already in, with no restart. It never
edits your profile itself; you do. Custom [`JLO_HOME`](#jlo_home) paths are substituted for you. The lines never change: an upgrade
regenerates the files they point at, so you only add them once.

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

JDKs are installed to `~/Library/Java/JavaVirtualMachines/` on macOS and `~/.jdks/` on Linux —
the same locations IntelliJ IDEA uses, so both tools see the same JDKs.
This allows automatic discovery of installed JDKs by IDEs like IntelliJ IDEA.

# Commands

`jlo <verb> --help` is the reference for every flag and every default: it is generated from the same declaration
that dispatches the command, so it cannot drift from what the binary does. `jlo --help`, or a bare `jlo`, lists all
of them; `-h` gives the short form of either.

| Command | What it does |
| --- | --- |
| `jlo env [VERSION]` | Set `JAVA_HOME` and `PATH` in the current shell. `jlo use` is an alias. |
| `jlo home [VERSION]` | Print the `JAVA_HOME` path for a version, and nothing else. |
| `jlo exec [VERSION] -- <CMD>` | Run a command with that JDK active, leaving the current shell alone. |
| `jlo current` | Say which JDK is active in this shell, and why. |
| `jlo list` | Show what Adoptium offers and what is installed. `jlo ls` is an alias. |
| `jlo install [VERSION...]` | Download a major version without changing any shell. |
| `jlo update [VERSION...]` | Bring installed JDKs up to their latest minor release. `--all` for every one. |
| `jlo remove <VERSION...>` | Delete installed JDKs by name, or `--superseded` to delete them by rule. |
| `jlo init [VERSION]` | Write a `.jlorc` pinning this project's version. `--global` for the user-wide default. |
| `jlo selfupdate` | Update J'Lo itself. |
| `jlo completions <SHELL>` | Print a completion script. |

Three things are worth knowing before you read any of that:

- **The version is optional on `env`, `home`, `exec`, `install` and `update`.** Left out, it resolves in four steps:
  the nearest `.jlorc` at or above the current directory, then `~/.jlo/default.jlorc`, then the newest JDK already
  installed, then the latest release, which is downloaded. `jlo env --help` spells out the whole cascade and its one
  sharp edge; `jlo current` tells you which step answered.
- **Only major versions are accepted** wherever a JDK is resolved or downloaded: `21`, not `21.0.5`. The one
  exception is `jlo remove`, which names an install rather than resolving one and so also takes an exact build
  (`jlo remove 21.0.5+11`).
- **`--offline` never touches the network.** On `env`, `home` and `list` it answers from what is already installed.
  `env` and `home` fail if that is nothing, since they have no answer to give; `list` says the store is empty and
  exits 0, an empty list being a perfectly good listing. Downloading a JDK and asking whether one is here are
  otherwise the same command, which is no use in a CI step with a short timeout or a network-isolated sandbox.

## How Downloads Are Verified

Every JDK J'Lo downloads is streamed through SHA-256 as it arrives, and the digest is compared against the checksum
Adoptium publishes for that package. A mismatch, or a package Adoptium lists without a checksum at all, aborts the
install before anything is unpacked. The checksum comes from Adoptium's own API over TLS — not from a mirror and not
from a third-party broker. J'Lo does not verify Adoptium's GPG signature, so an attacker who controls or intercepts
the Adoptium API could serve a substituted binary with a matching checksum; that is outside what J'Lo protects
against.

## Using a JDK J'Lo Did Not Install

J'Lo has no command that takes a path, but it does not need one: it finds every JDK in its install directory, whether
or not it put it there. To make a vendor-supplied JDK, a local OpenJDK build, or an **early-access build** usable by
`jlo env`, `jlo home` and `jlo exec`, move it into that directory under a version-shaped name:

```bash
# macOS
mv /path/to/jdk ~/Library/Java/JavaVirtualMachines/21.0.11+9

# Linux
mv /path/to/jdk ~/.jdks/21.0.11+9
```

The directory name is the whole registration: it has to parse as a semantic version — a leading `v` is tolerated, so
`v21.0.11+9` works too — and its major is what `jlo env 21` and friends then match on. The directory itself must hold the
JDK — the `bin`, `lib` and `release` entries. On macOS it may instead be a **JDK bundle**, holding those under
`Contents/Home`; J'Lo takes either, and moving the bundle rather than its `Contents/Home` is the better of the two,
because that is the shape `/usr/libexec/java_home` can see.

Such a JDK is **unmanaged**: J'Lo wrote no `.jlo-managed` marker beside it, so

- `jlo list --offline` shows it, with `unmanaged` as its status word;
- `jlo remove` will not delete it, under either selector — remove it by hand when you are done with it;
- `jlo update` cannot update it. Adoptium does not list it, so there is no newer minor to resolve. If you also install
  that major from Adoptium, both remain, and `jlo env` picks the highest version of the two.

That is deliberately how J'Lo handles early-access and preview builds: they are yours to drop in and yours to
remove, and `jlo update` will never reach for one on your behalf.

## Managing J’Lo Itself

- `jlo --version` prints the currently installed J’Lo version.
- `jlo selfupdate` updates J’Lo itself to the latest release.

`selfupdate` checks the version first and does nothing when you are already current. When there is something newer it
downloads the release for your platform, verifies it against the SHA256 published beside it, and replaces the binary
by an atomic rename — the old one stays in place until the new one is ready, so an interrupted update never leaves you
without a working `jlo`. Failure is loud and the exit status is real, so scripts can rely on it.

It refuses, rather than guessing, in three cases: a J’Lo installed from a local build (`install-local.sh`), one
installed by a package manager, and one whose install receipt does not describe the binary you are running. Re-running
the installer is the documented recovery path for anything `selfupdate` cannot fix — including a `selfupdate` that is
itself broken:

```shell
/bin/bash -c "$(curl -fsSL https://raw.githubusercontent.com/java-loader/jlo/refs/heads/main/install.sh)"
```

> [!NOTE]
> The published checksum shares an origin with the tarball, so it catches a corrupt or truncated download — not a
> compromised release. Releases also carry GitHub build provenance attestations, which narrow that gap for whoever
> checks them. J’Lo does not verify them itself; you can, with
> `gh attestation verify jlo-<platform>.tar.gz --repo java-loader/jlo`.

### Upgrading

`jlo selfupdate` works across the 0.4.0 boundary — there is no reinstall, and nothing to change in your shell profile.
The lines you pasted never change; an upgrade regenerates the files they point at.

- **Restart your shell once after upgrading to 0.4.0** (or run `. ~/.jlo/jlo.sh`). The 0.3.x `jlo` function is already
  resident in your shell and cannot reload itself. From 0.4.0 on the binary prints the reload line and the wrapper
  evaluates it, so this is the last time it is needed — and only ever in the shell that ran the update. Other open
  shells keep their resident wrapper until they are restarted, the same as with any version manager. The reload
  replaces the `jlo` function; a zsh completion that has already been loaded in that shell stays as it was until the
  next shell.
- **If your profile still contains the old multi-line J’Lo block** — the one every 0.2.x and 0.3.0 installer printed:

  ```shell
  export JLO_HOME="$HOME/.jlo"
  [[ -s "$JLO_HOME/bin/jlo-init.sh" ]] && source "$JLO_HOME/bin/jlo-init.sh"
  [[ -s "$JLO_HOME/bin/jlo-autoload.sh" ]] && source "$JLO_HOME/bin/jlo-autoload.sh"
  ```

  it keeps working. 0.4.0 generates `bin/jlo-init.sh` and `bin/jlo-autoload.sh` as compatibility shims that load the
  same wrapper the single line does, so the upgrade needs nothing from you. The installer says so when it sees the
  block, and prints the lines below.

  Replace the block anyway, at your convenience — **the shims are removed in v1.0.0**:

  ```shell
  [ -s "$HOME/.jlo/jlo.sh" ] && . "$HOME/.jlo/jlo.sh"
  [ -s "$HOME/.jlo/autoload.sh" ] && . "$HOME/.jlo/autoload.sh"       # optional: switch JDK on cd
  [ -s "$HOME/.jlo/completions.sh" ] && . "$HOME/.jlo/completions.sh" # optional: tab completion
  ```

  Keep only the optional lines you actually had. If your block also sources `completions/_jlo` or `completions/jlo.bash`
  directly (0.3.0 printed that), `completions.sh` is what replaces it. These three lines never change again: an upgrade
  regenerates the files they point at.

## Supported Shells

The shell integration is written for **bash and zsh**, and is verified against every version in the table below:

| Shell | Versions verified                                      | Notes                                                       |
|-------|--------------------------------------------------------|-------------------------------------------------------------|
| bash  | 3.2.57, 4.0, 4.1, 4.2, 4.3, 4.4, 5.0, 5.1, 5.2, 5.3    | 3.2.57 is what macOS ships as `/bin/bash`                   |
| zsh   | 5.0.8, 5.3, 5.4.2, 5.8, 5.9                            | macOS's default login shell since Catalina                  |

J'Lo writes one wrapper per dialect — `$JLO_HOME/bin/jlo-init.{bash,zsh}` and `jlo-autoload.{bash,zsh}` — and the
three entry files you source (`jlo.sh`, `autoload.sh`, `completions.sh`) pick the right one at source time. Because
the choice happens there, no wrapper ever has to parse under a shell it was not written for.

The entry files themselves, and `install.sh`, are POSIX `sh` clean: they parse and run under `dash`, where the
dispatch simply finds no dialect and does nothing. All of them are safe to source from a profile that runs under
`set -e` or `set -u`.

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

Fish is not supported: the core `jlo` shell function is bash/zsh syntax and cannot be sourced from fish, so
`jlo env`/`jlo use` don't work there regardless of completions. If you use fish anyway and still want
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
| `$JLO_HOME/install-receipt.json` | what was installed, how, and where — read by `jlo selfupdate` |

The three `*.sh` files at the top are generated by the installer with their paths baked in, and are rewritten on every
install — don't edit them. Your profile sources them; it never sets `JLO_HOME` itself.

If `JLO_HOME` is already set when you run the installer, it installs into that directory, bakes that path into the
generated files, and prints it in the lines it tells you to add.

> [!IMPORTANT]
> `JLO_HOME` is **not** where the JDKs go, and it is **not** `JAVA_HOME`. Downloaded JDKs are installed to
> `~/Library/Java/JavaVirtualMachines/` (macOS) or `~/.jdks/` (Linux) — the locations IntelliJ IDEA uses — and
> that directory is not configurable. Pointing `JLO_HOME` elsewhere moves J'Lo's own config and scripts, not your JDKs.


# CI / scripting / AI agents

In interactive shells, `jlo` is a shell function (defined by the wrapper `jlo.sh` sources) — this is what lets `jlo env` mutate your
current session. Non-interactive shells (CI jobs, `Makefile` recipes, scripts, AI coding agents) don't load that
function, so J'Lo's installer also places a real `jlo` binary on your `PATH` at `~/.local/bin/jlo`. Every subcommand
works there directly except `env`/`use`, which must mutate the current shell. `jlo selfupdate` works there too; it
just cannot reload a shell function that was never loaded.

**`jlo exec` is the answer whenever there is no shell to configure.** It runs one command with the right Java on
`PATH` and changes nothing outside that command; `jlo home` prints the `JAVA_HOME` path when you only need the path.
Both install the JDK on demand if needed. Prefer either over `jlo env` outside an interactive shell, since neither
relies on the shell integration.

J'Lo installs **no shims** — there is no `java` on your `PATH` that J'Lo put there to intercept. Outside a shell
`jlo env` has configured, `java` is whatever it always was, and `jlo exec` is how you pick a different one for the
length of one command. A `java` that silently resolves through a version manager is convenient right up to the
point where something else on the machine needs the one it replaced.

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
