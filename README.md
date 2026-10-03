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

Install J'Lo with the install script on Linux and macOS, or with Homebrew on macOS — pick one, not both.

### Install Script

```shell
/bin/bash -c "$(curl -fsSL https://github.com/java-loader/jlo/releases/latest/download/install.sh)"
```

The installer ends by printing what to do next: one line to add to your shell profile (plus two optional ones —
switching JDK on `cd`, and tab completion) and the command that loads J'Lo into the shell you are in. It never edits
your profile itself.

For upgrades, run `jlo selfupdate`. Re-running the installer is always safe, and it is the way back should
`selfupdate` ever fail.

### Homebrew

On macOS (arm64), J'Lo is also in a Homebrew tap:

```shell
brew install java-loader/tap/jlo
```

Brew prints the profile lines to add; tab completion comes from Homebrew's own setup. Upgrade with `brew upgrade jlo`
— `jlo selfupdate` is for installer installs only.

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

JDKs are installed to `~/Library/Java/JavaVirtualMachines/` on macOS and `~/.jdks/` on Linux, each in a directory
named `jlo-temurin-<version>`. J'Lo never touches anything else in there; `jlo list` shows it so you can delete it by
hand.

A version is a major (`25`) or a pre-release stream (`28-ea`). `jlo --help` lists every command.

## Scripts and AI Agents

Your profile makes `jlo` a shell function, which is what lets `jlo env` change your current shell. A script, a
`Makefile` or an AI coding agent usually doesn't load your profile, or runs each command in a fresh shell — either
way, `jlo env` has no shell to change. Use `exec` or `home`; they work with the plain binary the installer links at
`~/.local/bin/jlo`:

```shell
jlo exec 25 -- ./gradlew build          # one command on Java 25
export JAVA_HOME="$(jlo home 25)"       # just the path
```

Outside your interactive profile the plain binary still needs `~/.local/bin` on `PATH`; when it is missing, the
installer prints the line that adds it. With Homebrew, the binary is on brew's `PATH` already.

J'Lo puts no `java` of its own on your `PATH`. Outside a shell where `jlo env` ran — by hand or through autoload —
`java` is whatever it was before.

### Machine-readable output

`jlo list --json` and `jlo current --json` print one JSON document on one line. Keys are only ever added — never
removed, renamed or retyped — so ignore keys you do not know, and expect new values in `status` and `source.kind`.
What J'Lo does not know is `null` rather than a missing key; the one key that comes and goes is `source.path`,
present only for the two config kinds. On failure the exit status is 1 and stdout is empty — except when writing
stdout itself fails, so check the exit status.

```shell
jlo list --json | jq -r '.names[] | select(.update) | .name'   # names with an update
jlo current --json | jq -r .version
```

`jlo list --json` (`--offline` composes):

```json
{"names":[{"name":"25","major":25,"distribution":"temurin",
  "installed":[{"version":"25.0.1+8","path":"…/Contents/Home","active":true,"status":"installed"},
               {"version":"25.0.0+36","path":"…/Contents/Home","active":false,"status":"superseded"}],
  "offered":"25.0.2+10","update":true}]}
```

- `names` — every name installed or offered, newest major first.
- `installed` — newest build first; the first is the one the name stands for. `path` is its `JAVA_HOME`.
  `active`: your `JAVA_HOME` points into it. `status`: `installed`, or `superseded` by a newer build of the name.
  `active` and `status` are independent: a shell still on an old build is both.
- `offered` / `update` — Adoptium's newest build of the name, and whether it is newer than every installed build
  (`false` for a name that is only offered, not installed).
  Both `null` when J'Lo did not ask: with `--offline`, or for a pre-release stream whose version has been released.
  `offered: null` with `update: false`: asked, and Adoptium has no build for your machine. If any lookup fails,
  the command fails rather than print a partial list.

`jlo current --json`:

```json
{"name":"25","major":25,"distribution":"temurin","version":"25.0.1+8","path":"…/Contents/Home",
 "source":{"kind":"project_config","path":"/home/you/project/.jlorc"},"pinned_elsewhere":null}
```

- `path` — `JAVA_HOME` exactly as set, so compare `version`, not paths, against `jlo list`.
- `name`, `major`, `distribution`, `version` — `null` when `JAVA_HOME` is not a JDK J'Lo installed.
- `source` — why this JDK: `kind` is `project_config` or `default_config` (each with the file's absolute `path`),
  `newest_installed`, or `foreign` (`JAVA_HOME` set outside J'Lo); `null` when nothing accounts for it.
- `pinned_elsewhere` — `null`, or `{name, major, distribution, source}` of a config that asks for a different JDK.

## When J'Lo Is the Wrong Tool

- You need a reproducible, exactly pinned JDK — in CI or on a server.
- You need more than a JDK — Maven, Gradle, Node, Python.
- You need a distribution other than Eclipse Temurin.
- You're on Windows, or on a shell other than bash or zsh.

## Uninstalling

Remove `~/.jlo/`, the `~/.local/bin/jlo` symlink and the lines you added to your shell profile. With Homebrew,
`brew uninstall jlo`, then remove the profile lines and `~/.jlo/` (it holds only your `jlo init --global` default). The JDKs stay in
`~/.jdks/` or `~/Library/Java/JavaVirtualMachines/` — delete them too if you no longer need them.
