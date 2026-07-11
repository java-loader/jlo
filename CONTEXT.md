# J'Lo (Java Loader)

A minimalistic CLI tool for downloading and managing JDK installations, wiring them into the shell via `JAVA_HOME` and `PATH`.

## Language

**Adoptium**:
The Eclipse project whose API and Temurin builds are J'Lo's source of JDKs.
_Avoid_: AdoptOpenJDK (the project's former name)

**AdoptiumClient**:
The single point of contact with Adoptium — discovering available releases, fetching JDK metadata, and downloading packages. All Adoptium HTTP interaction goes through it.
_Avoid_: API wrapper, downloader, fetcher

**JdkMetadata**:
Adoptium's description of one concrete JDK build: its semantic version, release name, package name, download link, and checksum.
_Avoid_: release info, asset

**Managed installation**:
A JDK directory that J'Lo installed and owns, marked with a `.jlo-managed` file. Only managed installations are eligible for cleanup.
_Avoid_: installed JDK (ambiguous — JDKs installed by other means are not managed)
