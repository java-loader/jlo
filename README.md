# J'Lo – the Java Loader

The Java Loader (or J'Lo for short) is a minimalistic tool to download and manage Java installations on your machine.
It is written in Rust, with a main focus on simplicity and ease of use.

J'Lo currently supports Linux (x86_64) and macOS (arm64).

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

JDKs are installed to `~/.jdks/` on Linux and `~/Library/Java/JavaVirtualMachines/` on macOS.
This allows automatic discovery of installed JDKs by IDEs like IntelliJ IDEA.

# J’Lo Command Reference

## Table of Contents

1. [Environment Setup](#environment-setup)
2. [Initialization](#initialization)
3. [Updating Java Versions](#updating-java-versions)
4. [Cleaning Installed Versions](#cleaning-installed-versions)
5. [Managing J’Lo Itself](#managing-jlo-itself)

## Environment Setup

The command `jlo env` configures the current shell session by setting the `JAVA_HOME` and `PATH` environment variables
to point to the desired JDK installation.

**Behavior:**
- If the current directory contains a `.jlorc` file, `jlo env` uses the version it specifies.
- Otherwise, it falls back to `~/.jlo/default.jlorc`.
- If the requested Java version is not installed, it will be downloaded and installed automatically.
- If neither config file exists, `jlo env` uses the latest installed Java; if no JDKs are installed, it installs and uses the latest available release.
- This command affects only the current shell session.

**Usage examples:**
```shell
# set environment based on .jlorc (fallbacks described above)
jlo env

# set environment for Java 25
jlo env 25
````

## Initialization

The command `jlo init` creates a `.jlorc` file in the current directory that pins a specific Java version.
The file is used by `jlo env` to determine which Java version to set up.

Specifying a version is optional; if omitted, the latest available Java version will be used.

This command fails if a `.jlorc` file already exists in the current directory.

**Usage examples:**
```shell
# create .jlorc that pins Java 25
jlo init 25

# create .jlorc that pins the latest available Java version
jlo init
```

**Example `.jlorc` file content:**
```
# Java version configured by J'Lo - https://github.com/java-loader/jlo
25
```

## Updating Java Versions

The command `jlo update` updates installed Java versions to their latest minor releases.

**Behavior:**
- Multiple versions can be specified as arguments; each will be updated to its latest minor release.
  Missing versions will be installed automatically.
- A special argument `all` updates all installed Java versions.
- If no arguments are provided, it updates the Java version specified in the `.jlorc` file in the current directory.

**Usage examples:**
```shell
# update Java version specified in .jlorc
jlo update

# update Java versions 21 and 25
jlo update 21 25

# update all installed Java versions
jlo update all
```

## Cleaning Installed Versions

The command `jlo clean` removes older minor versions of installed Java versions, keeping only the latest minor
release for each major version.

Only installations at `~/.jdks/` (or `~/Library/Java/JavaVirtualMachines/` on macOS) managed by J’Lo are
affected.

## Managing J’Lo Itself

- The command `jlo version` prints the currently installed J’Lo version.
- The command `jlo selfupdate` updates J’Lo itself to the latest version.

# Uninstalling J'Lo

To uninstall J'Lo, simply remove the `~/.jlo/` directory and the lines you added to your shell profile during
installation.

You may also want to remove the `~/.jdks/` directory (or `~/Library/Java/JavaVirtualMachines/` on macOS) if you no
longer need the installed JDKs.
