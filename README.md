# J'Lo – the Java Loader

On your machine you rarely need Java 25.0.1+8. You need Java 25 — the newest build.

J'Lo pins major versions and keeps them current: a project's `.jlorc` says `25`, `jlo env` installs the newest
[Eclipse Temurin](https://adoptium.net/temurin/releases) 25, and `jlo update` replaces it once a newer build ships.
One build per version, no shims, and the JDKs land where IntelliJ IDEA and Gradle toolchains already look.

Linux (x86_64, aarch64) and macOS (arm64), bash and zsh.

## Install

```shell
/bin/bash -c "$(curl -fsSL https://github.com/java-loader/jlo/releases/latest/download/install.sh)"
```

Or, on macOS, `brew install java-loader/tap/jlo` — pick one, not both. Either way you get a line to add to your shell
profile; J'Lo never edits it itself.

## Use a JDK

```shell
jlo env 25        # Java 25 in this shell, downloaded if needed
jlo env 28-ea     # or an early-access build
jlo current       # which JDK is active, and why
```

## Pin a Project

```shell
cd my-project
jlo init 21       # writes .jlorc
jlo env           # uses it
```

Commit `.jlorc`, and everyone on the project gets Java 21. With the optional autoload line the installer offers,
`cd` into the project switches for you.

## Stay Current

```shell
jlo list          # what is installed, and what has an update
jlo update        # newest build of every installed version; the old one goes
jlo selfupdate    # J'Lo itself (brew upgrade jlo with Homebrew)
```

## Scripts and AI Agents

`jlo env` changes your interactive shell; a script or an AI agent has none to change. Use these instead:

```shell
jlo exec 21 -- ./gradlew build          # one command on Java 21
export JAVA_HOME="$(jlo home 21)"       # just the path
jlo list --json                         # machine-readable, see JSON.md
```

Everything else is in `jlo --help` and `jlo <command> --help`.

## When J'Lo Is the Wrong Tool

- You need a reproducible, exactly pinned JDK — in CI or on a server.
- You need more than a JDK — Maven, Gradle, Node, Python.
- You need a distribution other than Eclipse Temurin.
- You're on Windows.
- You use a shell other than bash or zsh.

## Uninstall

Remove `~/.jlo/`, the `~/.local/bin/jlo` symlink and the lines you added to your shell profile — or, with Homebrew,
`brew uninstall jlo`, the profile lines and `~/.jlo/`. The JDKs stay in `~/.jdks/` or
`~/Library/Java/JavaVirtualMachines/`, in directories named `jlo-temurin-<version>`; delete them by hand if you
no longer need them.
