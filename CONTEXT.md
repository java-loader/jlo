# J'Lo (Java Loader)

A minimalistic CLI tool for downloading and managing JDK installations, wiring them into the shell via `JAVA_HOME` and `PATH`.

## Language

**Adoptium**:
The Eclipse project whose API and Temurin builds are J'Lo's source of JDKs.
_Avoid_: AdoptOpenJDK (the project's former name)

**AdoptiumClient**:
The single point of contact with Adoptium — discovering available releases, fetching JDK metadata, and downloading packages. All Adoptium HTTP interaction goes through it.
_Avoid_: API wrapper, downloader, fetcher

**Active JDK**:
The JDK `JAVA_HOME` points at in the current shell, whatever put it there. Distinct from the *pinned* version, which is what a config file asks for — the two disagreeing is an ordinary state, and the one `jlo current` exists to report.
_Avoid_: current version (ambiguous — it also reads as "newest")

**JdkMetadata**:
Adoptium's description of one concrete JDK build: its semantic version, release name, package name, download link, and checksum.
_Avoid_: release info, asset

**Managed installation**:
A JDK directory that J'Lo installed and owns, marked with a `.jlo-managed` file. Only managed installations are eligible for deletion, by either `prune` or `remove`.
_Avoid_: installed JDK (ambiguous — JDKs installed by other means are not managed)

**Prune**:
Deleting installed JDKs *by rule*: keep the newest minor of every installed major, delete the superseded ones, skip unmanaged installations. The command is `jlo prune`.
_Avoid_: clean (the former name of the command, removed before 1.0 — it suggests regenerable build output rather than deleted JDKs)

**Remove**:
Deleting the installed JDKs a *named target* selects: every build of a major version, or one exact build. The explicit counterpart to prune, and the one that refuses rather than skips — an unmanaged target, or one `JAVA_HOME` points at, deletes nothing at all. The command is `jlo remove`.
_Avoid_: uninstall, delete

**Provenance**:
Where the Java version in play came from: a command-line argument, the nearest `.jlorc`, `~/.jlo/default.jlorc`, or a `JAVA_HOME` set outside J'Lo. Carried alongside the version through resolution rather than recomputed at the point of display, so `jlo current` and `jlo env --verbose` cannot drift apart.
_Avoid_: origin, config source

**Entry file**:
One of the three generated stubs under `$JLO_HOME` that a user sources from their profile — `jlo.sh` (required), `autoload.sh` and `completions.sh` (opt-in). They are the whole install contract: the paths they point at may move, the three lines never do.
_Avoid_: profile snippet (it is no longer pasted code), init script

**Wrapper dialect**:
One shell's copy of the shell integration — `bin/jlo-init.bash`, `bin/jlo-init.zsh`, and the two `jlo-autoload.*` beside them. The sources live in `shell/` and are compiled into the binary with `include_str!`; `jlo.sh` picks one at source time, so no dialect has to parse under another shell.
_Avoid_: shell script (ambiguous — `install.sh` is one too and is still dual-parse)

**Install verb**:
`jlo-bin __install`, the hidden command that writes the whole layout under `$JLO_HOME`. Hidden means intercepted from raw argv before clap parses, like the `sing` easter egg — `hide = true` would still leak it into generated completions and typo suggestions. `install.sh`, `install-local.sh` and (from 0.4.0) `selfupdate` all call it, so there is exactly one generator and it always matches the binary.
_Avoid_: bootstrap (that is `install.sh`'s job, which is now only download-and-verify)

**Install receipt**:
`$JLO_HOME/install-receipt.json` — the installed version, the install method and the target paths. Written last, so it is the commit marker for the whole publication: a receipt that disagrees with the running binary means the files beside it are incomplete, and any invocation regenerates them rather than reporting it.
_Avoid_: manifest, lockfile
