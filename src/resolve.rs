//! What version of Java a command should run against, and where that JDK
//! lives.
//!
//! The cascade is jlo's one rule, and five commands ask it the same question,
//! so it gets a name here rather than a copy in each of them. Everything in
//! this module answers "which JDK", never "what do I print" or "how do I
//! install it" - those are `ui` and `store`.

use crate::adoptium::AdoptiumClient;
use crate::conf;
use crate::request::Request;
use crate::store::{self, JdkStore};
use crate::{CommandError, ui};
use anyhow::{Context, anyhow};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// Determine the requested major version: the explicit CLI argument if
/// present, otherwise the fallback cascade below.
///
/// Returns where the version came from as well as what it is, because
/// `jlo current` reports it, and re-deriving it there
/// would be a second spelling of the same walk.
pub(crate) fn resolve_java_version_from(
    explicit: Option<String>,
    store: &JdkStore,
    client: &AdoptiumClient,
    offline: bool,
) -> anyhow::Result<conf::Resolved> {
    match explicit {
        Some(version) => Ok(conf::Resolved {
            request: Request::parse(&version)?,
            source: conf::Source::Argument,
        }),
        None => cascade(conf::find()?, newest_installed(store), offline, || {
            client
                .latest_major()
                .context("could not fetch latest JDK version")
        }),
    }
}

/// The version-resolution cascade, once the explicit argument is out of the
/// way. Four stages, in order:
///
/// 1. the nearest `.jlorc` at or above the cwd,
/// 2. `$JLO_HOME/default.jlorc`,
/// 3. the newest JDK already installed,
/// 4. the latest release Adoptium offers, downloaded.
///
/// It lives here rather than in `conf` deliberately. `conf` knows about
/// config files and nothing else - not where JDKs are installed, not how to
/// reach Adoptium - and moving the cascade there would hand it both, so the
/// module that answers "what does this file say" would start answering "what
/// is on this machine" and "what does the network offer" too. Stages 1 and 2
/// stay `conf::find`, unchanged; stages 3 and 4 are added here, where the
/// store and the client already are.
///
/// Every input is passed in rather than read from the filesystem or the
/// process environment - the same reason `conf::find_in` takes its cwd - so
/// the decisions here, including the one that must *not* download, are
/// testable without a store, a network or a temp directory.
fn cascade(
    configured: Option<conf::Resolved>,
    newest_installed: Option<conf::Resolved>,
    offline: bool,
    latest_release: impl FnOnce() -> anyhow::Result<String>,
) -> anyhow::Result<conf::Resolved> {
    if let Some(resolved) = configured.or(newest_installed) {
        return Ok(resolved);
    }

    // `--offline` stops here, one stage short of the download, and that is the
    // whole of why entering a directory never starts one: the autoload hook
    // calls `jlo env --offline`, so the cascade it runs ends at what is
    // already on disk.
    if offline {
        return Err(conf::nothing_configured());
    }

    Ok(conf::Resolved {
        // Adoptium's `available_releases` holds majors that have shipped, so
        // this is a GA name by construction; parsing it rather than assuming
        // so keeps the one grammar in one place.
        request: Request::parse(&latest_release()?)?,
        source: conf::Source::LatestRelease,
    })
}

/// Stage 3 of the cascade: the newest JDK already on disk, whatever major it
/// is.
///
/// Deliberately no comparison against Adoptium. Asking whether the newest
/// installed JDK is also the newest release would put a network round trip on
/// the hottest path there is - every bare `jlo env` - to answer a question
/// `jlo update` already exists for. So a machine holding only an outdated 17
/// resolves to 17 and downloads nothing; stage 4 is reached only when no JDK
/// is installed at all.
///
/// A store holding nothing but pre-8 JDKs falls through rather than resolving
/// to a version the rest of jlo would then reject.
fn newest_installed(store: &JdkStore) -> Option<conf::Resolved> {
    store
        .newest_major()
        .and_then(|major| Request::parse(&major.to_string()).ok())
        .map(|request| conf::Resolved {
            request,
            source: conf::Source::NewestInstalled,
        })
}

/// `--offline`: answer from the store alone.
///
/// The point of the flag is that asking the question cannot trigger the
/// several-hundred-megabyte answer - a CI step with a short timeout, a
/// network-isolated sandbox, or the autoload hook on a `cd`, needs a probe
/// that fails fast rather than one that hangs on a connection attempt. The
/// exit status is the answer, so there is no distinct code for "not
/// installed": 1, like every other failure here.
///
/// `command` is the subcommand to name in the advice line, so `env` does not
/// send the reader to `home` (and vice versa).
pub(crate) fn offline_java_home(
    store: &JdkStore,
    request: Request,
    command: &str,
) -> Result<PathBuf, CommandError> {
    let java_home = store.find_matching(request).ok_or_else(|| {
        CommandError::with_hint(
            anyhow!("no installed JDK matches Java {request}"),
            format!("Run 'jlo {command} {request}' without --offline to install it."),
        )
    })?;

    // `env --offline` is how the autoload hook runs, on every new shell and
    // every `cd`, and ADR-0001 keeps that path silent - a line here would
    // print forever. `home --offline` is a person asking a question and gets
    // the warning. This is the only thing that tells the two apart, which is
    // why `command` is threaded down here at all.
    if command != "env" {
        warn_legacy_layout(store, &java_home);
    }

    Ok(java_home)
}

/// Say so when the JDK just resolved is one `/usr/libexec/java_home` cannot
/// see, which is every macOS install made before jlo kept the bundle.
///
/// At the two resolution funnels rather than in each command, so a verb added
/// later cannot forget it. Only ever a warning: the install works, and the fix
/// costs a download, so it is the user's to make.
fn warn_legacy_layout(store: &JdkStore, java_home: &Path) {
    if let Some((version, request)) = store.legacy_layout(java_home) {
        ui::legacy_layout(&version, request);
    }
}

/// The version names an explicit run should download: the list given, or the version
/// the cascade resolves when the list is empty - the same resolution `env`,
/// `home` and `exec` do, so a bare `jlo install` means the same version they
/// would pick.
///
/// Neither verb takes `--offline`, so the cascade here may reach its last
/// stage: `jlo install` on a machine with no config and no JDK installs the
/// latest release, which is the only thing it could sensibly mean.
///
/// An invalid entry is warned about and skipped, so one typo in a list of four
/// does not cost the other three. A list that leaves nothing valid behind is
/// an error naming `verb`, the command that asked.
pub(crate) fn requested_versions(
    versions: Vec<String>,
    verb: &str,
    store: &JdkStore,
    client: &AdoptiumClient,
) -> Result<HashSet<Request>, CommandError> {
    if versions.is_empty() {
        let resolved = resolve_java_version_from(None, store, client, false)?;
        return Ok(HashSet::from([resolved.request]));
    }

    let mut requested = HashSet::new();
    for v in versions {
        match Request::parse(&v) {
            Ok(request) => {
                requested.insert(request);
            }
            // The grammar's own wording, not a second one: this is the only
            // path on which a typo in a version list is reported, and "what
            // is accepted?" is the only question it raises.
            Err(e) => ui::warning!("skipping {e:#}"),
        }
    }

    if requested.is_empty() {
        return Err(anyhow!("no valid Java versions provided to {verb}").into());
    }

    Ok(requested)
}

/// Resolve the `JAVA_HOME` for the requested version name, installing the JDK on
/// demand if it is not already present. Diagnostics go to stderr; this returns
/// the path so callers decide what (if anything) to print to stdout.
pub(crate) fn resolve_java_home(
    client: &AdoptiumClient,
    store: &JdkStore,
    request: Request,
) -> anyhow::Result<PathBuf> {
    if let Some(path) = store.find_matching(request) {
        warn_legacy_layout(store, &path);
        Ok(path)
    } else {
        let metadata = client.fetch_metadata(request)?;
        store::install_jdk(client, store, &metadata)
    }
}

pub(crate) fn assert_java_version(java_version: &str) -> anyhow::Result<()> {
    if conf::is_valid_version(java_version) {
        Ok(())
    } else {
        Err(anyhow!(crate::request::Request::rejection(java_version)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn owned(items: &[&str]) -> Vec<String> {
        items.iter().map(std::string::ToString::to_string).collect()
    }

    /// A client pointed at an address nothing listens on. Every call below must
    /// fail before it would reach the network, so a connection error here would
    /// be the test itself reporting a regression.
    fn offline_client() -> AdoptiumClient {
        AdoptiumClient::new("http://127.0.0.1:1")
    }

    /// A store rooted at a path that does not exist, i.e. one holding no
    /// JDKs. Enough for the tests that never reach stage 3 of the cascade.
    fn empty_store() -> JdkStore {
        JdkStore::at("/nonexistent/jlo-test-store")
    }

    /// `env` must not send the reader to `home`: the advice line names the
    /// command they actually ran.
    #[test]
    fn offline_java_home_names_the_calling_command_in_its_hint() {
        let store = JdkStore::at(tempdir().unwrap().path());
        let err = offline_java_home(&store, request("99"), "env").expect_err("the store is empty");
        assert_eq!(
            format!("{:#}", err.error),
            "no installed JDK matches Java 99"
        );
        assert_eq!(
            err.hint.as_deref(),
            Some("Run 'jlo env 99' without --offline to install it.")
        );
    }

    // -- cascade --
    //
    // Every input is passed in, so these run without a store, a network or a
    // temp directory. `refuse_network` is the assertion that matters most in
    // half of them: stage 4 is a several-hundred-megabyte download, and the
    // cases below are exactly the ones in which it must not be reached.

    fn request(version: &str) -> Request {
        Request::parse(version).expect("the fixture names a valid version")
    }

    fn configured(version: &str) -> conf::Resolved {
        conf::Resolved {
            request: request(version),
            source: conf::Source::DefaultConfig(PathBuf::from("/home/u/.jlo/default.jlorc")),
        }
    }

    fn installed(version: &str) -> conf::Resolved {
        conf::Resolved {
            request: request(version),
            source: conf::Source::NewestInstalled,
        }
    }

    /// A stage 4 that fails if it is ever called, so "no network access" is an
    /// assertion rather than a comment.
    fn refuse_network() -> anyhow::Result<String> {
        Err(anyhow!("the network was consulted"))
    }

    /// Stage 2 beats stage 3: `jlo init --global` is how a user asks for a
    /// stable answer on neutral ground, and a JDK installed for some other
    /// project must not quietly override it.
    #[test]
    fn cascade_prefers_a_config_over_the_newest_install() {
        let resolved = cascade(
            Some(configured("21")),
            Some(installed("25")),
            false,
            refuse_network,
        )
        .expect("the default config answers");

        assert_eq!(resolved.request, request("21"));
        assert!(matches!(resolved.source, conf::Source::DefaultConfig(_)));
    }

    /// Stage 3: no config anywhere, so the newest JDK on disk answers - and
    /// answers without a round trip to Adoptium.
    #[test]
    fn cascade_falls_back_to_the_newest_installed_jdk() {
        let resolved = cascade(None, Some(installed("25")), false, refuse_network)
            .expect("the installed JDK answers");

        assert_eq!(resolved.request, request("25"));
        assert_eq!(resolved.source, conf::Source::NewestInstalled);
    }

    /// The case the cascade is most easily got wrong in: a machine holding
    /// only an outdated major resolves to *that* major. Stage 3 does not ask
    /// Adoptium whether something newer exists - that is what `jlo update` is
    /// for - so nothing is downloaded here.
    #[test]
    fn cascade_keeps_an_outdated_install_rather_than_downloading_a_newer_major() {
        let resolved = cascade(None, Some(installed("17")), false, refuse_network)
            .expect("the outdated install still answers");

        assert_eq!(resolved.request, request("17"));
        assert_eq!(resolved.source, conf::Source::NewestInstalled);
    }

    /// Stage 4, reached only when nothing is configured *and* nothing is
    /// installed.
    #[test]
    fn cascade_downloads_the_latest_release_when_nothing_is_installed() {
        let resolved = cascade(None, None, false, || Ok("26".to_string()))
            .expect("the latest release answers");

        assert_eq!(resolved.request, request("26"));
        assert_eq!(resolved.source, conf::Source::LatestRelease);
    }

    /// `--offline` stops one stage short of the download. This is the whole of
    /// why the autoload hook - which calls `jlo env --offline` on every `cd` -
    /// can never start one.
    #[test]
    fn cascade_refuses_to_download_when_offline() {
        let err =
            cascade(None, None, true, refuse_network).expect_err("offline has nowhere left to go");

        assert!(err.to_string().contains(".jlorc"), "{err}");
        assert!(err.to_string().contains("jlo init"), "{err}");
    }

    /// `--offline` stops *after* stage 3, not before it: an installed JDK is
    /// already on disk, so handing it back costs no network at all.
    #[test]
    fn cascade_still_uses_an_installed_jdk_when_offline() {
        let resolved = cascade(None, Some(installed("21")), true, refuse_network)
            .expect("the installed JDK needs no network");

        assert_eq!(resolved.request, request("21"));
        assert_eq!(resolved.source, conf::Source::NewestInstalled);
    }

    /// Stage 3 reads a major out of a directory name, so a store holding only
    /// pre-8 JDKs would otherwise resolve to a version every other part of jlo
    /// rejects. It falls through to stage 4 instead.
    #[test]
    fn newest_installed_ignores_a_store_of_pre_8_jdks() {
        let dir = tempdir().expect("a temp directory");
        std::fs::create_dir_all(dir.path().join("7.0.4+101")).expect("the fake JDK directory");

        assert_eq!(newest_installed(&JdkStore::at(dir.path())), None);
    }

    /// Nothing installed is `None`, not a failure - including when the store
    /// directory has never been created.
    #[test]
    fn newest_installed_is_none_for_an_empty_store() {
        assert_eq!(newest_installed(&empty_store()), None);
    }

    // -- requested_versions --

    #[test]
    fn requested_versions_keeps_the_valid_entries_of_a_mixed_list() {
        let requested = requested_versions(
            owned(&["21", "abc", "25"]),
            "install",
            &empty_store(),
            &offline_client(),
        )
        .expect("two of the three are valid");
        assert_eq!(requested, HashSet::from([request("21"), request("25")]));
    }

    /// A major named twice is one download, not two: the set is what reaches
    /// `install_each`.
    #[test]
    fn requested_versions_deduplicates() {
        let requested = requested_versions(
            owned(&["21", "21"]),
            "install",
            &empty_store(),
            &offline_client(),
        )
        .expect("21 is a valid major");
        assert_eq!(requested, HashSet::from([request("21")]));
    }

    // -- offline_java_home --
    //
    // The install directory is not configurable, so `jlo home
    // --offline` is covered here against an injected `JdkStore` rather than
    // by spawning the binary; the integration suite asserts only the exit
    // status and that no network call happens.

    /// A fake store holding one JDK directory, marked managed the way an
    /// install leaves it.
    fn store_with(base: &Path, version: &str) -> JdkStore {
        let dir = base.join(version);
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        std::fs::write(dir.join("bin").join("java"), "").unwrap();
        std::fs::File::create(dir.join(".jlo-managed")).unwrap();
        JdkStore::at(base)
    }

    #[test]
    fn offline_java_home_answers_from_the_store() {
        let dir = tempdir().unwrap();
        let store = store_with(dir.path(), "21.0.3+9");

        let path = offline_java_home(&store, request("21"), "home").expect("21 is installed");
        assert_eq!(path, dir.path().join("21.0.3+9"));
    }

    /// The exit status is the answer a script wants, and the hint has to name
    /// the command that would actually install it - the whole point of the
    /// flag is that this one did not.
    #[test]
    fn offline_java_home_fails_without_installing_anything() {
        let dir = tempdir().unwrap();
        let store = store_with(dir.path(), "21.0.3+9");

        let err =
            offline_java_home(&store, request("17"), "home").expect_err("17 is not installed");
        assert_eq!(
            format!("{:#}", err.error),
            "no installed JDK matches Java 17"
        );
        assert_eq!(
            err.hint.as_deref(),
            Some("Run 'jlo home 17' without --offline to install it.")
        );
        assert!(
            !dir.path().join("17").exists(),
            "--offline must not create anything"
        );
    }
}
