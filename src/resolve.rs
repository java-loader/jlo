//! What version of Java a command should run against, and where that JDK
//! lives.
//!
//! The cascade is jlo's one rule, and four commands ask it the same question,
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
use std::fmt;
use std::path::{Path, PathBuf};

/// The command asking, where the answer differs by who asked: the verb an
/// offline miss tells the reader to re-run, and whether the legacy-layout
/// warning may be printed at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Verb {
    Env,
    Home,
    Exec,
}

impl fmt::Display for Verb {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Env => "env",
            Self::Home => "home",
            Self::Exec => "exec",
        })
    }
}

/// A resolved JDK: the version name the cascade settled on, and where that
/// JDK lives.
#[derive(Debug)]
pub(crate) struct Target {
    pub request: Request,
    pub java_home: PathBuf,
}

/// What is active in this shell, and why.
///
/// Built by [`provenance`] and handed to `ui::provenance_line` formatted-but-
/// undecided: `ui` never consults the store, the config or the environment -
/// it turns this into a line. The shape is deliberately wider than any one
/// caller needs, so the machine-readable output still to come reports the
/// same four facts under the same names rather than inventing a second
/// schema.
#[derive(Debug)]
pub(crate) struct Active {
    /// The directory `$JAVA_HOME` points at.
    pub path: PathBuf,
    /// The install's version, e.g. `25.0.4+101`. `None` when the JDK is not
    /// one of jlo's, which is the one case that reports a path instead.
    pub version: Option<String>,
    /// The version name of `version` - its major and its stream, so a GA
    /// build and a pre-release of one major are told apart here too.
    pub request: Option<Request>,
    /// Where the active JDK came from. `None` when it is one of jlo's but
    /// nothing accounts for it - either nothing is pinned, or what is pinned
    /// is a different name, which `pinned_elsewhere` distinguishes.
    pub source: Option<conf::Source>,
    /// A config that pins a *different* name than the one active. Set only
    /// when the two disagree; that disagreement is the whole reason this
    /// command answers "and why" rather than just "what".
    pub pinned_elsewhere: Option<conf::Resolved>,
}

/// The JDK `verb` runs against: the explicit version or the cascade's answer,
/// then its java home - from the store alone when `offline`, installing on
/// demand otherwise.
pub(crate) fn java_home(
    client: &AdoptiumClient,
    store: &JdkStore,
    explicit: Option<String>,
    offline: bool,
    verb: Verb,
) -> Result<Target, CommandError> {
    let request = resolve_java_version_from(explicit, store, client, offline)?.request;
    let java_home = if offline {
        offline_java_home(store, request, verb)?
    } else {
        resolve_java_home(client, store, request)?
    };
    Ok(Target { request, java_home })
}

/// Determine the requested version name: the explicit CLI argument if
/// present, otherwise the fallback cascade below.
///
/// Returns where the version came from as well as what it is: `cascade`
/// produces a `Resolved`, and its own tests assert on the stage that
/// answered.
fn resolve_java_version_from(
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

/// Stages 1-3 of the cascade: what this machine answers without the network.
///
/// Named on its own so the stage order is written once: `cascade` goes on to
/// stage 4 from here, and `provenance` stops here, the way `jlo current`
/// must.
fn on_disk(
    configured: Option<conf::Resolved>,
    newest_installed: Option<conf::Resolved>,
) -> Option<conf::Resolved> {
    configured.or(newest_installed)
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
    latest_release: impl FnOnce() -> anyhow::Result<Request>,
) -> anyhow::Result<conf::Resolved> {
    if let Some(resolved) = on_disk(configured, newest_installed) {
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
        request: latest_release()?,
        source: conf::Source::LatestRelease,
    })
}

/// Stage 3 of the cascade: the newest released JDK already on disk, whatever
/// major it is.
///
/// Deliberately no comparison against Adoptium. Asking whether the newest
/// installed JDK is also the newest release would put a network round trip on
/// the hottest path there is - every bare `jlo env` - to answer a question
/// `jlo update` already exists for. So a machine holding only an outdated 17
/// resolves to 17 and downloads nothing; stage 4 is reached only when no JDK
/// is installed at all.
///
/// Which build counts - released only, at or above the version floor - is
/// [`store::newest_ga`]'s rule, so `jlo current` and the cascade cannot
/// disagree about it.
fn newest_installed(store: &JdkStore) -> Option<conf::Resolved> {
    store.newest_ga_request().map(|request| conf::Resolved {
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
/// `verb` is the subcommand to name in the advice line, so `env` does not
/// send the reader to `home` (and vice versa).
fn offline_java_home(
    store: &JdkStore,
    request: Request,
    verb: Verb,
) -> Result<PathBuf, CommandError> {
    let java_home = store.find_matching(request).ok_or_else(|| {
        CommandError::with_hint(
            anyhow!("no installed JDK matches Java {request}"),
            format!("Run 'jlo {verb} {request}' without --offline to install it."),
        )
    })?;

    // `env --offline` is how the autoload hook runs, on every new shell and
    // every `cd`, so it stays silent - a line here would print forever.
    // `home --offline` is a person asking a question and gets the warning.
    // This is the only place that distinction exists.
    if verb != Verb::Env {
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
    if let Some(jdk) = store.legacy_layout(java_home) {
        ui::legacy_layout(&jdk.version, jdk.request);
    }
}

/// The version names an explicit run should download: the list given, or the version
/// the cascade resolves when the list is empty - the same resolution `env`,
/// `home` and `exec` do, so a bare `jlo install` means the same version they
/// would pick.
///
/// `install` takes no `--offline`, so the cascade here may reach its last
/// stage: `jlo install` on a machine with no config and no JDK installs the
/// latest release, which is the only thing it could sensibly mean. `update`
/// comes here only with a list: without one it means every installed name,
/// which is not a resolution at all.
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
fn resolve_java_home(
    client: &AdoptiumClient,
    store: &JdkStore,
    request: Request,
) -> anyhow::Result<PathBuf> {
    if let Some(path) = store.find_matching(request) {
        warn_legacy_layout(store, &path);
        Ok(path)
    } else {
        let metadata = client
            .fetch_metadata(request)?
            .ok_or_else(|| anyhow!("{}", ui::not_offered(&[request])))?;
        store::install_jdk(client, store, &metadata)
    }
}

/// What is active in this shell, and why: `$JAVA_HOME` read against the
/// store and, for a JDK the store recognizes, against the cascade stopped
/// after stage 3 - this never touches the network.
///
/// `configured` is stages 1 and 2 (`conf::find`), taken as a closure because
/// it is consulted only on the branch that needs it: a foreign or vanished
/// `$JAVA_HOME` is answered without reading any config, so a broken `.jlorc`
/// cannot fail it.
pub(crate) fn provenance(
    store: &JdkStore,
    java_home: PathBuf,
    configured: impl FnOnce() -> anyhow::Result<Option<conf::Resolved>>,
) -> Result<Active, CommandError> {
    // Asked before the listing, because the listing cannot answer it. A
    // `$JAVA_HOME` inside the store that is simply *gone* is what `jlo
    // remove` on the live JDK leaves behind, and reporting that as a JDK set
    // outside jlo would be wrong - the install was ours.
    //
    // Existence is the whole of the test, and it has to be asked of
    // `$JAVA_HOME` itself rather than inferred from the listing, for two
    // reasons that pull in opposite directions. A directory the listing
    // cannot name may be perfectly present: a vendor-named entry
    // (`temurin-21.0.1`), which is what the IDE's own downloads land as,
    // shares the store by design and is deliberately unlistable - `jlo list
    // --offline` calls it foreign, and this has to agree. And a version the
    // listing *can* name may be gone: on macOS `$JAVA_HOME` is the bundle's
    // `Contents/Home`, and `owns` matches that spelling without asking the
    // filesystem anything - deliberately, since it is also the guard that
    // refuses to delete the live JDK and must not be switchable off by a
    // directory that cannot be stat'd.
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
        // A JDK jlo does not manage. No config is consulted: whatever is
        // pinned, jlo is not what put this here, and the path says that
        // completely.
        return Ok(Active {
            path: java_home,
            version: None,
            request: None,
            source: Some(conf::Source::Foreign),
            pinned_elsewhere: None,
        });
    };

    // Whether the active JDK is the *exact* install stage 3 of the cascade
    // would pick, not merely one of its name. The distinction matters because
    // the cascade resolves a name and `jlo env` then takes the newest build of
    // it: a shell on 21.0.5 with 21.0.6 sitting beside it agrees on the name
    // but is not what a bare `jlo env` would hand back, so calling it "the
    // newest installed JDK" would claim more than is true. Asked of the
    // selector rather than re-derived, so this says "from the newest installed
    // JDK" exactly when a bare `jlo env` would hand back this build - which
    // puts both of the selector's rules here too: a shell on a pre-release
    // with a released build installed is not the cascade's answer, and a shell
    // below the version floor never was.
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

    // The same cascade `jlo env` resolves through, stopped after stage 3:
    // this command never touches the network, so "download the latest
    // release" is not an answer it can give - and it would be a strange one
    // anyway, since something is demonstrably active already.
    //
    // A config that fails to load is still a failure: it is a file the user
    // wrote and meant, and answering around it would hide the mistake.
    match on_disk(configured()?, stage_3) {
        // Stage 3, credited only to the exact build a bare `jlo env` would
        // hand back - not merely to the name, which `newest` alone would give
        // it. No mismatch counterpart: nobody asked for the newest installed
        // JDK, so a shell that is on something else is not wrong about
        // anything and gets no warning - it reads as "nothing pinned".
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

/// Whether `path` lies under `base`.
///
/// `$JAVA_HOME` is normally spelled exactly as the store spelled it, because
/// `jlo env` is what set it; the canonicalized retry covers a `$HOME` that
/// reaches the store through a symlink. `path` itself is deliberately not
/// canonicalized - the case this decides is the one where it no longer exists.
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

    /// The exit status is the answer a script wants, and the hint has to name
    /// the command that would actually install it - the whole point of the
    /// flag is that this one did not. `env` must not send the reader to
    /// `home`: the advice line names the command they actually ran.
    #[test]
    fn java_home_offline_fails_without_installing_anything() {
        for (verb, command) in [(Verb::Env, "env"), (Verb::Home, "home")] {
            let dir = tempdir().unwrap();
            let store = store_with(dir.path(), "21.0.3+9");

            let err = java_home(&offline_client(), &store, Some("17".into()), true, verb)
                .expect_err("17 is not installed");
            assert_eq!(
                format!("{:#}", err.error),
                "no installed JDK matches Java 17"
            );
            assert_eq!(
                err.hint.as_deref(),
                Some(format!("Run 'jlo {command} 17' without --offline to install it.").as_str())
            );
            assert!(
                !dir.path().join("17").exists(),
                "--offline must not create anything"
            );
        }
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
            Verb::Exec,
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
    /// answers without a round trip to Adoptium, however outdated it is.
    /// Whether something newer exists is `jlo update`'s question, not this
    /// one's.
    #[test]
    fn cascade_falls_back_to_the_newest_installed_jdk() {
        let resolved = cascade(None, Some(installed("25")), false, refuse_network)
            .expect("the installed JDK answers");

        assert_eq!(resolved.request, request("25"));
        assert_eq!(resolved.source, conf::Source::NewestInstalled);
    }

    /// Stage 4, reached only when nothing is configured *and* nothing is
    /// installed.
    #[test]
    fn cascade_downloads_the_latest_release_when_nothing_is_installed() {
        let resolved =
            cascade(None, None, false, || Ok(request("26"))).expect("the latest release answers");

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

    /// Stage 3 of the cascade answers from the store, and a pre-release is
    /// never the answer: `jlo env` with nothing configured must not start
    /// handing out betas because someone once tried one.
    #[test]
    fn the_cascade_never_falls_back_to_a_pre_release() {
        let dir = tempdir().unwrap();
        let store = store_with(dir.path(), "28.0.0-beta+16.0.ea");

        let resolved = cascade(None, newest_installed(&store), false, || Ok(request("27")))
            .expect("stage 4 answers when stage 3 declines");

        assert_eq!(resolved.request, request("27"));
        assert_eq!(resolved.source, conf::Source::LatestRelease);
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

    // -- java_home --
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

    /// Below the version floor stage 3 has no answer at all, so nothing is
    /// credited and nothing is a pin.
    #[test]
    fn provenance_credits_nothing_for_a_store_below_the_version_floor() {
        let dir = tempdir().unwrap();
        let store = store_holding(dir.path(), &["7.0.4+101"]);
        let active = provenance(&store, dir.path().join("7.0.4+101"), || Ok(None)).unwrap();
        assert_eq!(active.version.as_deref(), Some("7.0.4+101"));
        assert_eq!(active.source, None);
        assert!(active.pinned_elsewhere.is_none());
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
