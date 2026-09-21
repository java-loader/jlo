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
