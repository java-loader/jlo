# J'Lo – the Java Loader

On your machine you rarely need Java 25.0.1+8. You need Java 25 — the newest build.

J'Lo pins major versions and keeps them current: a project's `.jlorc` holds `25`, `jlo env` installs the newest
[Eclipse Temurin](https://adoptium.net/temurin/releases) 25, and once a newer build ships, `jlo update` replaces it.
Nothing in the repo goes stale; the next developer to join gets a working setup.

- **Pin the major, not the build** — `.jlorc` says `25`, not a build that goes stale
- **One build per major** — updates replace, they don't pile up
- **Found by your tools** — JDKs land where IntelliJ IDEA and Gradle toolchains look, `JAVA_HOME` covers the rest,
  and J'Lo never deletes or changes a JDK it didn't install
- **No shims** — `jlo exec` runs one command with a JDK, for scripts and AI agents
- **Single binary; JDKs checked against Adoptium's SHA-256**

Linux (x86_64, aarch64) and macOS (arm64), bash ≥ 3.2 and zsh ≥ 5.0.8. Any Java version Temurin builds for your
platform — from 8 on Linux, from 11 on macOS.

## Installing

```shell
/bin/bash -c "$(curl -fsSL https://raw.githubusercontent.com/java-loader/jlo/refs/heads/main/install.sh)"
```

The installer ends by printing what to do next: one line to add to your shell profile (plus two optional ones —
switching JDK on `cd`, and tab completion) and the command that loads J'Lo into the shell you are in. It never edits
your profile itself.

For upgrades, run `jlo selfupdate`. Re-running the installer is always safe, and it is the way back should
`selfupdate` ever fail.

## Quick Start

```shell
jlo env 25        # use Java 25 in this shell, downloading its newest build if needed
```

For a project:

```shell
cd /path/to/your/project
jlo init 25       # write a .jlorc pinning Java 25
jlo env           # use whatever .jlorc says
```

With the optional autoload line in your profile, `cd` into the project does the `jlo env` for you. Autoload never
downloads: a pinned JDK that is not installed yet gets a notice, and `jlo env` fetches it. Outside a project,
`jlo init --global 25` sets your default — with autoload, every new shell starts on it.

JDKs are installed to `~/Library/Java/JavaVirtualMachines/` on macOS and `~/.jdks/` on Linux.

A version is a major (`25`) or a pre-release stream (`28-ea`). `jlo --help` lists every command.

## Scripts and AI Agents

Your profile makes `jlo` a shell function, which is what lets `jlo env` change your current shell. A script, a
`Makefile` or an AI coding agent usually doesn't load your profile, or runs each command in a fresh shell — either
way, `jlo env` has no shell to change. Use `exec` or `home`; they work with the plain binary the installer links at
`~/.local/bin/jlo`:

```shell
jlo exec 25 -- ./gradlew build          # one command on Java 25
export JAVA_HOME="$(jlo home 25)"        # just the path
```

The plain binary is only found where `~/.local/bin` is on `PATH` without your interactive profile. For zsh, that is
`~/.zshenv`:

```shell
cat >> ~/.zshenv <<'EOF'

case ":${PATH-}:" in *":$HOME/.local/bin:"*) ;; *) export PATH="${PATH:+$PATH:}$HOME/.local/bin" ;; esac
EOF
```

The installer prints this, with the right file for your shell, whenever `~/.local/bin` is missing from your `PATH`. A
bare `bash -c` from something that never ran a login shell sees only the `PATH` it inherited; call `~/.local/bin/jlo`
by its full path there.

J'Lo puts no `java` of its own on your `PATH`. Outside a shell where `jlo env` ran — by hand or through autoload —
`java` is whatever it was before.

## When J'Lo Is the Wrong Tool

- You need a reproducible, exactly pinned JDK — in CI or on a server.
- You need more than a JDK — Maven, Gradle, Node, Python.
- You need a distribution other than Eclipse Temurin.
- You're on Windows, or on a shell other than bash or zsh.

## Uninstalling

Remove `~/.jlo/`, the `~/.local/bin/jlo` symlink and the lines you added to your shell profile. The JDKs stay in
`~/.jdks/` or `~/Library/Java/JavaVirtualMachines/` — delete them too if you no longer need them.
