//! Which JDK a command runs against, and where it lives. Answers "which JDK",
//! never "what do I print"; the one side effect is [`java_home`]'s on-demand
//! install of a missing JDK when not `--offline`.

use crate::adoptium::AdoptiumClient;
use crate::conf;
use crate::request::Request;
use crate::store::{self, JdkStore};
use crate::{CommandError, ui};
use anyhow::{Context, anyhow};
use std::path::{Path, PathBuf};

/// The verb an offline miss tells the reader to re-run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Verb {
    Env,
    Home,
}

impl Verb {
    fn name(self) -> &'static str {
        match self {
            Self::Env => "env",
            Self::Home => "home",
        }
    }
}

#[derive(Debug)]
pub(crate) struct Target {
    pub request: Request,
    pub java_home: PathBuf,
}

/// What is active in this shell, and why. Wider than any one caller needs, so
/// machine-readable output can report the same facts under the same names.
#[derive(Debug)]
pub(crate) struct Active {
    /// The directory `$JAVA_HOME` points at.
    pub path: PathBuf,
    /// `None` when the JDK is not one of jlo's.
    pub version: Option<String>,
    pub request: Option<Request>,
    /// `None` when it is one of jlo's but nothing accounts for it: nothing is
    /// pinned, or a different name is ([`Self::pinned_elsewhere`]).
    pub source: Option<conf::Source>,
    /// A config that pins a *different* name than the one active.
    pub pinned_elsewhere: Option<conf::Resolved>,
}

/// The explicit version or the cascade's answer, then its java home: from the
/// store alone when `offline`, installing on demand otherwise.
pub(crate) fn java_home(
    client: &AdoptiumClient,
    store: &JdkStore,
    explicit: Option<String>,
    offline: bool,
    verb: Verb,
) -> Result<Target, CommandError> {
    let request = resolve_java_version_from(explicit, store, client, offline)?;
    let java_home = if offline {
        offline_java_home(store, request, verb)?
    } else {
        resolve_java_home(client, store, request)?
    };
    Ok(Target { request, java_home })
}

fn resolve_java_version_from(
    explicit: Option<String>,
    store: &JdkStore,
    client: &AdoptiumClient,
    offline: bool,
) -> anyhow::Result<Request> {
    match explicit {
        Some(version) => Request::parse(&version),
        None => cascade(
            conf::find()?,
            || newest_installed(store),
            offline,
            || {
                client
                    .latest_major()
                    .context("could not fetch latest JDK version")
            },
        ),
    }
}

/// Stages 1-3 of the cascade, written once: `cascade` goes on to stage 4,
/// `provenance` stops here.
fn on_disk(
    configured: Option<conf::Resolved>,
    newest_installed: impl FnOnce() -> Option<conf::Resolved>,
) -> Option<conf::Resolved> {
    configured.or_else(newest_installed)
}

/// The version cascade, once the explicit argument is out of the way:
///
/// 1. the nearest `.jlorc` at or above the cwd,
/// 2. `$JLO_HOME/default.jlorc`,
/// 3. the newest JDK already installed,
/// 4. the latest release Adoptium offers, downloaded.
///
/// Every input is passed in, so the decisions - including the one that must
/// *not* download - are testable without a store, a network or a temp
/// directory.
///
/// Stage 3 is a closure, called only when stages 1 and 2 answer nothing: an
/// eager one would list the store twice on every autoload `cd`.
fn cascade(
    configured: Option<conf::Resolved>,
    newest_installed: impl FnOnce() -> Option<conf::Resolved>,
    offline: bool,
    latest_release: impl FnOnce() -> anyhow::Result<Request>,
) -> anyhow::Result<Request> {
    if let Some(request) = on_disk(configured, newest_installed).map(|r| r.request) {
        return Ok(request);
    }

    // One stage short of the download: this is why the autoload hook's
    // `jlo env --offline` never starts one.
    if offline {
        return Err(conf::nothing_configured());
    }

    latest_release()
}

/// Stage 3. Deliberately no comparison against Adoptium: that would put a
/// network round trip on every bare `jlo env` to answer what `jlo update` is
/// for. A machine holding only an outdated 17 resolves to 17.
fn newest_installed(store: &JdkStore) -> Option<conf::Resolved> {
    store.newest_ga_request().map(|request| conf::Resolved {
        request,
        source: conf::Source::NewestInstalled,
    })
}

/// `--offline`: from the store alone, failing fast rather than starting a
/// several-hundred-megabyte download.
fn offline_java_home(
    store: &JdkStore,
    request: Request,
    verb: Verb,
) -> Result<PathBuf, CommandError> {
    let java_home = store.find_matching(request).ok_or_else(|| {
        CommandError::with_hint(
            anyhow!("no installed JDK matches Java {request}"),
            format!(
                "Run 'jlo {} {request}' without --offline to install it.",
                verb.name()
            ),
        )
    })?;

    Ok(java_home)
}

/// The list given, or the version the cascade resolves when it is empty - so
/// a bare `jlo install` means what `env` would pick.
///
/// An invalid entry is warned about and skipped, so one typo in a list of four
/// does not cost the other three.
pub(crate) fn requested_versions(
    versions: Vec<String>,
    verb: &str,
    store: &JdkStore,
    client: &AdoptiumClient,
) -> Result<Vec<Request>, CommandError> {
    if versions.is_empty() {
        let request = resolve_java_version_from(None, store, client, false)?;
        return Ok(vec![request]);
    }

    let mut requested = Vec::new();
    for v in versions {
        match Request::parse(&v) {
            Ok(request) => requested.push(request),
            // The grammar's own wording, which names what is accepted.
            Err(e) => ui::warning!("skipping {e:#}"),
        }
    }

    if requested.is_empty() {
        return Err(anyhow!("no valid Java versions provided to {verb}").into());
    }

    Ok(requested)
}

/// Installs the JDK on demand if it is not already present.
fn resolve_java_home(
    client: &AdoptiumClient,
    store: &JdkStore,
    request: Request,
) -> anyhow::Result<PathBuf> {
    if let Some(path) = store.find_matching(request) {
        Ok(path)
    } else {
        let metadata = client
            .fetch_metadata(request)?
            .ok_or_else(|| anyhow!("{}", ui::not_offered(&[request])))?;
        store::install_jdk(client, store, &metadata)
    }
}

/// `$JAVA_HOME` read against the store and, for a JDK the store recognizes,
/// against the cascade stopped after stage 3. Never touches the network.
///
/// `configured` is stages 1 and 2, a closure so a foreign or vanished
/// `$JAVA_HOME` is answered without reading config: a broken `.jlorc` cannot
/// fail it.
pub(crate) fn provenance(
    store: &JdkStore,
    java_home: PathBuf,
    configured: impl FnOnce() -> anyhow::Result<Option<conf::Resolved>>,
) -> Result<Active, CommandError> {
    // A `$JAVA_HOME` inside the store that is *gone* was ours, not foreign.
    // Asked of `$JAVA_HOME` itself, not inferred from the listing: an IDE's
    // `temurin-21.0.1` is present but unlistable (foreign), and a listed
    // bundle's `Contents/Home` may be gone, since `owns` never asks the
    // filesystem.
    if is_inside(store.base(), &java_home) && !java_home.exists() {
        return Err(CommandError::with_hint(
            anyhow!(
                "$JAVA_HOME points at a jlo install that is no longer there ({}).",
                java_home.display()
            ),
            ui::NO_ACTIVE_JDK_HINT,
        ));
    }

    let installed = store.list().context("could not list installed JDKs")?;

    let Some(version) = store.active_version(&installed, Some(&java_home)) else {
        // Not jlo's: no config is consulted, whatever is pinned.
        return Ok(Active {
            path: java_home,
            version: None,
            request: None,
            source: Some(conf::Source::Foreign),
            pinned_elsewhere: None,
        });
    };

    // Stage 3 is credited only to the *exact* build a bare `jlo env` would
    // hand back: a shell on 21.0.5 beside 21.0.6 agrees on the name but is
    // not it. Asked of `newest_ga` rather than re-derived, so its GA-only and
    // floor rules apply here too.
    let newest = store::newest_ga(&installed);
    let stage_3 = newest.map(|jdk| conf::Resolved {
        request: jdk.request,
        source: conf::Source::NewestInstalled,
    });

    let mut active = Active {
        request: installed
            .iter()
            .find(|jdk| jdk.version == version)
            .map(|jdk| jdk.request),
        path: java_home,
        version: Some(version),
        source: None,
        pinned_elsewhere: None,
    };

    // A config that fails to load is still a failure: answering around a
    // file the user wrote would hide the mistake.
    match on_disk(configured()?, || stage_3) {
        // No mismatch warning: nobody asked for the newest installed JDK.
        // Not collapsible into the guard: a name match on a build that is not
        // the exact newest one must fall through to nothing, not to the next
        // arm's by-name comparison, which would credit it anyway.
        Some(answer) if answer.source == conf::Source::NewestInstalled => {
            active.source = newest
                .is_some_and(|jdk| active.version.as_ref() == Some(&jdk.version))
                .then_some(answer.source);
        }
        // The whole name, so a `.jlorc` pinning `28-ea` is a mismatch on a
        // shell holding the GA build of 28.
        Some(answer) if Some(answer.request) == active.request => {
            active.source = Some(answer.source);
        }
        Some(answer) => active.pinned_elsewhere = Some(answer),
        None => {}
    }

    Ok(active)
}

/// The canonicalized retry covers a `$HOME` reached through a symlink. `path`
/// is deliberately not canonicalized: the case this decides is the one where
/// it no longer exists.
fn is_inside(base: &Path, path: &Path) -> bool {
    path.starts_with(base)
        || base
            .canonicalize()
            .is_ok_and(|canonical| path.starts_with(canonical))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::request::Stream;
    use crate::request::request;
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

    #[test]
    fn java_home_offline_answers_from_the_store() {
        let dir = tempdir().unwrap();
        let store = store_with(dir.path(), "21.0.3+9");

        let target = java_home(
            &offline_client(),
            &store,
            Some("21".into()),
            true,
            Verb::Home,
        )
        .expect("21 is installed");
        assert_eq!(target.java_home, dir.path().join("21.0.3+9"));
        assert_eq!(target.request, request("21"));
    }

    /// Online, an installed JDK is answered before the client is used: the
    /// client here points at a port nothing listens on.
    #[test]
    fn java_home_online_answers_an_installed_jdk_without_the_network() {
        let dir = tempdir().unwrap();
        let store = store_with(dir.path(), "21.0.3+9");

        let target = java_home(
            &offline_client(),
            &store,
            Some("21".into()),
            false,
            Verb::Home,
        )
        .expect("21 is installed");
        assert_eq!(target.java_home, dir.path().join("21.0.3+9"));
    }

    // -- cascade --
    //
    // Every input is passed in, so these run without a store, a network or a
    // temp directory. `refuse_network` is the assertion that matters most in
    // half of them: stage 4 is a several-hundred-megabyte download, and the
    // cases below are exactly the ones in which it must not be reached.

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
    fn refuse_network() -> anyhow::Result<Request> {
        Err(anyhow!("the network was consulted"))
    }

    /// A stage 3 that fails if it is ever called: listing the store is what it
    /// costs, and a pin makes that listing wasted work.
    fn refuse_store_listing() -> Option<conf::Resolved> {
        panic!("the store was listed for stage 3")
    }

    /// A pin answers before stage 3 is even asked. Offline, because that is
    /// the autoload hook's case: `jlo env --offline` on every `cd` into a
    /// pinned directory, which then lists the store once to find the JDK.
    #[test]
    fn cascade_does_not_list_the_store_when_a_config_answers() {
        let resolved = cascade(
            Some(configured("21")),
            refuse_store_listing,
            true,
            refuse_network,
        )
        .expect("the config answers");

        assert_eq!(resolved, request("21"));
    }

    /// Stage 2 beats stage 3: `jlo init --global` is how a user asks for a
    /// stable answer on neutral ground, and a JDK installed for some other
    /// project must not quietly override it.
    #[test]
    fn cascade_prefers_a_config_over_the_newest_install() {
        let resolved = cascade(
            Some(configured("21")),
            || Some(installed("25")),
            false,
            refuse_network,
        )
        .expect("the default config answers");

        assert_eq!(resolved, request("21"));
    }

    /// Stage 3: no config anywhere, so the newest JDK on disk answers - and
    /// answers without a round trip to Adoptium, however outdated it is.
    /// Whether something newer exists is `jlo update`'s question, not this
    /// one's.
    #[test]
    fn cascade_falls_back_to_the_newest_installed_jdk() {
        let resolved = cascade(None, || Some(installed("25")), false, refuse_network)
            .expect("the installed JDK answers");

        assert_eq!(resolved, request("25"));
    }

    /// Stage 4, reached only when nothing is configured *and* nothing is
    /// installed.
    #[test]
    fn cascade_downloads_the_latest_release_when_nothing_is_installed() {
        let resolved = cascade(None, || None, false, || Ok(request("26")))
            .expect("the latest release answers");

        assert_eq!(resolved, request("26"));
    }

    /// `--offline` stops one stage short of the download. This is the whole of
    /// why the autoload hook - which calls `jlo env --offline` on every `cd` -
    /// can never start one.
    #[test]
    fn cascade_refuses_to_download_when_offline() {
        let err = cascade(None, || None, true, refuse_network)
            .expect_err("offline has nowhere left to go");

        assert!(err.to_string().contains(".jlorc"), "{err}");
        assert!(err.to_string().contains("jlo install"), "{err}");
    }

    /// `--offline` stops *after* stage 3, not before it: an installed JDK is
    /// already on disk, so handing it back costs no network at all.
    #[test]
    fn cascade_still_uses_an_installed_jdk_when_offline() {
        let resolved = cascade(None, || Some(installed("21")), true, refuse_network)
            .expect("the installed JDK needs no network");

        assert_eq!(resolved, request("21"));
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
        assert_eq!(requested, vec![request("21"), request("25")]);
    }

    // -- java_home --
    //
    // Against an injected `JdkStore`. The `--offline` refusal, hint included,
    // is asserted per verb end to end in `tests/test.rs`
    // (`offline_fails_without_touching_the_network`).

    /// A fake store holding one JDK directory, marked managed the way an
    /// install leaves it.
    fn store_with(base: &Path, version: &str) -> JdkStore {
        let dir = base.join(version);
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        std::fs::write(dir.join("bin").join("java"), "").unwrap();
        std::fs::File::create(dir.join(".jlo-managed")).unwrap();
        JdkStore::at(base)
    }

    // -- provenance --

    fn store_holding(base: &Path, versions: &[&str]) -> JdkStore {
        for version in versions {
            store_with(base, version);
        }
        JdkStore::at(base)
    }

    fn project_pins(version: &str) -> impl FnOnce() -> anyhow::Result<Option<conf::Resolved>> {
        let resolved = conf::Resolved {
            request: request(version),
            source: conf::Source::ProjectConfig(PathBuf::from("./.jlorc")),
        };
        move || Ok(Some(resolved))
    }

    /// Config is read only for a JDK the store recognizes; these branches
    /// must answer without it, so a broken `.jlorc` cannot fail them.
    fn config_must_not_be_read() -> anyhow::Result<Option<conf::Resolved>> {
        panic!("config was consulted")
    }

    #[test]
    fn provenance_names_the_config_when_it_agrees() {
        let dir = tempdir().unwrap();
        let store = store_holding(dir.path(), &["25.0.4+101"]);
        let active = provenance(&store, dir.path().join("25.0.4+101"), project_pins("25")).unwrap();
        assert_eq!(active.version.as_deref(), Some("25.0.4+101"));
        assert!(matches!(
            active.source,
            Some(conf::Source::ProjectConfig(_))
        ));
        assert!(active.pinned_elsewhere.is_none());
    }

    #[test]
    fn provenance_reports_a_config_that_pins_another_name() {
        let dir = tempdir().unwrap();
        let store = store_holding(dir.path(), &["25.0.4+101"]);
        let active = provenance(&store, dir.path().join("25.0.4+101"), project_pins("21")).unwrap();
        assert_eq!(active.source, None);
        assert_eq!(
            active.pinned_elsewhere.map(|p| p.request),
            Some(request("21"))
        );
    }

    #[test]
    fn provenance_credits_stage_3_only_for_the_exact_newest_build() {
        let dir = tempdir().unwrap();
        let store = store_holding(dir.path(), &["21.0.5+11", "21.0.6+7"]);

        let newest = provenance(&store, dir.path().join("21.0.6+7"), || Ok(None)).unwrap();
        assert_eq!(newest.source, Some(conf::Source::NewestInstalled));

        // Same name, older build: a bare `jlo env` would hand back 21.0.6+7.
        let older = provenance(&store, dir.path().join("21.0.5+11"), || Ok(None)).unwrap();
        assert_eq!(older.source, None);
        assert!(older.pinned_elsewhere.is_none());
    }

    #[test]
    fn provenance_says_nothing_pinned_when_stage_3_picks_another_major() {
        let dir = tempdir().unwrap();
        let store = store_holding(dir.path(), &["17.0.11+9", "25.0.4+101"]);
        let active = provenance(&store, dir.path().join("17.0.11+9"), || Ok(None)).unwrap();
        assert_eq!(active.source, None);
        assert!(active.pinned_elsewhere.is_none());
    }

    #[test]
    fn provenance_never_calls_a_pre_release_the_newest_install() {
        let dir = tempdir().unwrap();
        let store = store_holding(dir.path(), &["21.0.5+11", "28.0.0-beta+16.0.ea"]);
        let active =
            provenance(&store, dir.path().join("28.0.0-beta+16.0.ea"), || Ok(None)).unwrap();
        assert_eq!(active.source, None);
        assert!(active.pinned_elsewhere.is_none());
        assert_eq!(active.request.map(|r| r.stream), Some(Stream::Ea));
    }

    #[test]
    fn provenance_reports_a_foreign_java_home_by_path_without_reading_config() {
        let dir = tempdir().unwrap();
        let store = store_holding(dir.path(), &["25.0.4+101"]);
        let active = provenance(
            &store,
            PathBuf::from("/opt/jdk-21"),
            config_must_not_be_read,
        )
        .unwrap();
        assert_eq!(active.version, None);
        assert_eq!(active.source, Some(conf::Source::Foreign));
    }

    /// A vendor-named directory is inside the store and present, but
    /// unlistable: foreign, not gone.
    #[test]
    fn provenance_calls_a_vendor_named_jdk_in_the_store_foreign() {
        let dir = tempdir().unwrap();
        let store = store_holding(dir.path(), &["25.0.4+101"]);
        let vendor = dir.path().join("temurin-17.0.9");
        std::fs::create_dir_all(vendor.join("bin")).unwrap();
        let active = provenance(&store, vendor, config_must_not_be_read).unwrap();
        assert_eq!(active.source, Some(conf::Source::Foreign));
    }

    #[test]
    fn provenance_refuses_a_removed_install_without_reading_config() {
        let dir = tempdir().unwrap();
        let store = store_holding(dir.path(), &["25.0.4+101"]);
        let gone = dir.path().join("25.0.4+101");
        std::fs::remove_dir_all(&gone).unwrap();
        let err =
            provenance(&store, gone, config_must_not_be_read).expect_err("the install is gone");
        assert!(format!("{:#}", err.error).contains("no longer there"));
        assert_eq!(err.hint.as_deref(), Some(ui::NO_ACTIVE_JDK_HINT));
    }

    /// A config the user wrote and meant fails the command rather than being
    /// answered around.
    #[test]
    fn provenance_fails_when_the_config_cannot_be_read() {
        let dir = tempdir().unwrap();
        let store = store_holding(dir.path(), &["25.0.4+101"]);
        let err = provenance(&store, dir.path().join("25.0.4+101"), || {
            Err(anyhow!("bad .jlorc"))
        })
        .expect_err("config failure propagates");
        assert!(format!("{:#}", err.error).contains("bad .jlorc"));
    }
}
