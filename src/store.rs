use crate::CommandError;
use crate::adoptium::{AdoptiumClient, JdkMetadata};
use crate::extract;
use crate::request::{OLDEST_MAJOR, Request, Stream};
use crate::shellenv::Payload;
use crate::ui::{self, InstallUi};
use crate::version::{cmp_desc, compare};
use anyhow::{Context, anyhow, bail};
use std::cmp::Ordering;
use std::env;
use std::fs::File;
use std::path::{Path, PathBuf};

/// What makes a store entry jlo's: `jlo-temurin-<semver>`. The name alone
/// decides, whoever put the directory there. No other tool writes a `jlo-`
/// name, and the vendor leaves room for a second one; a bare semver cannot
/// say whose build it is, and `temurin-<version>` is how `IntelliJ` names its
/// own downloads in the same directory.
const ENTRY_PREFIX: &str = "jlo-temurin-";

/// The store entry holding `version`.
pub(crate) fn entry_name(version: &str) -> String {
    format!("{ENTRY_PREFIX}{version}")
}

/// The version an entry name holds, verbatim, and the name it answers to;
/// `None` for anything not named `jlo-temurin-<semver>`.
fn parse_entry(name: &str) -> Option<(String, Request)> {
    let version = name.strip_prefix(ENTRY_PREFIX)?;
    let semver = crate::version::parse(version).ok()?;
    Some((version.to_string(), Request::of_build(&semver)?))
}

/// A macOS bundle's java home, relative to the bundle. One constant because
/// [`java_home_in`] hands this path out and [`owns`] refuses to delete it, and
/// the two must not drift.
const BUNDLE_HOME: [&str; 2] = ["Contents", "Home"];

/// What a `jlo remove` run did, by name or by the superseded rule. The caller
/// owns the presentation.
#[derive(Debug, Default)]
pub(crate) struct RemoveReport {
    /// Newest first for `remove`; `jlo list`'s order for `prune`.
    pub(crate) removed: Vec<String>,
    pub(crate) failures: Vec<String>,
    /// Targets that matched nothing. Always empty for `prune`.
    pub(crate) not_installed: Vec<String>,
    /// Left alone because `$JAVA_HOME` points at it.
    pub(crate) skipped_in_use: Option<String>,
}

/// Why [`JdkStore::remove`] deleted nothing: every variant means the store is
/// unchanged. Typed so each gets its own hint without matching on message text.
#[derive(Debug)]
pub(crate) enum RemoveError {
    /// No target matched an install. Every such target is named.
    NotInstalled(Vec<String>),
    /// Only the JDK `$JAVA_HOME` points at was left; deleting it would leave
    /// the calling shell on a path that no longer exists.
    InUse(String),
    /// The install directory itself could not be read.
    Store(anyhow::Error),
}

impl std::fmt::Display for RemoveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotInstalled(versions) => {
                write!(f, "no installed JDK matches {}", quoted_list(versions))
            }
            Self::InUse(version) => {
                write!(f, "refusing to remove {version}: JAVA_HOME points at it")
            }
            // `{:#}`: the whole `anyhow` chain, as `main` prints every error.
            Self::Store(e) => write!(f, "{e:#}"),
        }
    }
}

/// A JDK in the install directory: an entry named [`entry_name`]`(version)`.
pub(crate) struct InstalledJdk {
    /// The entry name without its prefix, verbatim.
    pub version: String,
    /// The name this install answers to; its stream is read off the version.
    pub request: Request,
}

/// Whether `offered` is newer than every one of `builds` - one name's
/// installs. The one rule behind `jlo list`'s `update` and `install`/`update`
/// downloading, so the two cannot disagree.
///
/// The catalogue can sit *behind* the store (a rolled-back release, an install
/// placed by hand): following it would be a downgrade.
///
/// True for empty `builds`; the listing checks for that itself.
pub(crate) fn supersedes_every_install(offered: &str, builds: &[&InstalledJdk]) -> bool {
    builds
        .iter()
        .all(|jdk| is_older_than(&jdk.version, offered))
}

/// What a build is beside its name's head.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Status {
    /// As new as the head: two spellings of one version (`v21.0.11+9` beside
    /// `21.0.11+9`), so not superseded.
    Installed,
    /// Older than the head.
    Superseded,
}

pub(crate) struct NameGroup<'a> {
    pub request: Request,
    /// The newest build: the one the name reports and
    /// `jlo remove --superseded` keeps.
    pub head: &'a InstalledJdk,
    /// Every other build, newest first.
    pub others: Vec<(&'a InstalledJdk, Status)>,
}

impl NameGroup<'_> {
    /// Every build - what an offer must supersede.
    pub(crate) fn builds(&self) -> Vec<&InstalledJdk> {
        std::iter::once(self.head)
            .chain(self.others.iter().map(|(jdk, _)| *jdk))
            .collect()
    }
}

/// The installs grouped by name, in [`Request::listing_order`]. The one answer
/// `jlo list` renders and `jlo remove --superseded` deletes by.
///
/// By name, not major: a pre-release of a later patch sorts above the current
/// release, so a major-keyed group would make the released build superseded
/// by a beta.
///
/// `installed` may come in any order; the sort is stable, so two spellings of
/// one version keep the order given.
pub(crate) fn group_by_name(installed: &[InstalledJdk]) -> Vec<NameGroup<'_>> {
    let mut names: Vec<Request> = installed.iter().map(|jdk| jdk.request).collect();
    names.sort_unstable_by_key(|name| name.listing_order());
    names.dedup();

    names
        .into_iter()
        .filter_map(|request| {
            let mut builds = installed
                .iter()
                .filter(|jdk| jdk.request == request)
                .collect::<Vec<_>>();
            builds.sort_by(|a, b| cmp_desc(&a.version, &b.version));
            let mut builds = builds.into_iter();
            let head = builds.next()?;
            let others = builds
                .map(|jdk| {
                    let status = if is_older_than(&jdk.version, &head.version) {
                        Status::Superseded
                    } else {
                        Status::Installed
                    };
                    (jdk, status)
                })
                .collect();
            Some(NameGroup {
                request,
                head,
                others,
            })
        })
        .collect()
}

/// The newest *released* build at or above the version floor: cascade stage 3.
///
/// GA only: a machine that once tried `28-ea` must not answer a bare `jlo env`
/// with a beta. Below the floor is a version that cannot be asked for.
///
/// Takes the list so `jlo current` cannot answer differently from the cascade.
/// `installed` is newest first, as [`JdkStore::list`] leaves it.
pub(crate) fn newest_ga(installed: &[InstalledJdk]) -> Option<&InstalledJdk> {
    installed
        .iter()
        .find(|jdk| jdk.request.stream == Stream::Ga && jdk.request.major >= OLDEST_MAJOR)
}

/// One directory in the store, unfiltered: each caller decides what counts as
/// a JDK.
struct Candidate {
    path: PathBuf,
    /// `None` when not valid UTF-8.
    name: Option<String>,
    /// The name without [`ENTRY_PREFIX`] and the name it answers to; `None`
    /// for anything not named `jlo-temurin-<semver>`, which keeps an earlier
    /// jlo's bare `21.0.5+11` and an IDE's `temurin-21.0.5` out of the
    /// listing and out of reach of every deletion.
    jdk: Option<(String, Request)>,
}

/// The directory jlo installs JDKs into.
pub(crate) struct JdkStore {
    base: PathBuf,
}

impl JdkStore {
    /// The real store for this machine. The location is not configurable.
    pub(crate) fn discover() -> anyhow::Result<Self> {
        let home = env::home_dir().context("could not determine home directory")?;
        Ok(Self::at(base_dir_for(env::consts::OS, &home)))
    }

    /// A store rooted at an arbitrary directory, for tests.
    pub(crate) fn at(base: impl Into<PathBuf>) -> Self {
        Self { base: base.into() }
    }

    pub(crate) fn base(&self) -> &Path {
        &self.base
    }

    /// Every JDK entry, newest first. A missing base directory is an empty
    /// store.
    pub(crate) fn list(&self) -> anyhow::Result<Vec<InstalledJdk>> {
        let candidates = self.scan_existing()?;

        // Filtered before sorting: a name that does not parse compares equal
        // to everything, which is no total order to sort by.
        let mut installed: Vec<InstalledJdk> = candidates
            .into_iter()
            .filter_map(|candidate| {
                let (version, request) = candidate.jdk?;
                Some(InstalledJdk { version, request })
            })
            .collect();
        installed.sort_by(|a, b| cmp_desc(&a.version, &b.version));
        Ok(installed)
    }

    /// Every other directory in the store, by name, sorted: what the user may
    /// want to delete by hand, and jlo never will. jlo's own staging
    /// directories are left out. A missing base directory has none.
    pub(crate) fn foreign(&self) -> anyhow::Result<Vec<String>> {
        let mut names: Vec<String> = self
            .scan_existing()?
            .into_iter()
            .filter(|candidate| candidate.jdk.is_none())
            .filter_map(|candidate| {
                let name = candidate.path.file_name()?.to_string_lossy().into_owned();
                (!name.starts_with(STAGING_PREFIX)).then_some(name)
            })
            .collect();
        names.sort_unstable();
        Ok(names)
    }

    /// Whether `path` is a JDK entry or lies inside one, present or not: a
    /// `$JAVA_HOME` that has vanished from under a name jlo owns was jlo's.
    ///
    /// The canonicalized retry covers a `$HOME` reached through a symlink.
    /// `path` is deliberately not canonicalized: the case this decides is the
    /// one where it no longer exists.
    pub(crate) fn names_an_entry(&self, path: &Path) -> bool {
        let canonical = self.base.canonicalize().ok();
        path.strip_prefix(&self.base)
            .ok()
            .or_else(|| path.strip_prefix(canonical.as_deref()?).ok())
            .and_then(|rest| rest.components().next())
            .and_then(|first| first.as_os_str().to_str())
            .is_some_and(|name| parse_entry(name).is_some())
    }

    /// The directory holding `jdk`.
    fn entry(&self, jdk: &InstalledJdk) -> PathBuf {
        self.base.join(entry_name(&jdk.version))
    }

    /// The installed version `$JAVA_HOME` points at; `None` when it is unset
    /// or points outside the store.
    pub(crate) fn active_version(
        &self,
        installed: &[InstalledJdk],
        active_java_home: Option<&Path>,
    ) -> Option<String> {
        installed
            .iter()
            .find(|jdk| self.is_live(jdk, active_java_home))
            .map(|jdk| jdk.version.clone())
    }

    /// Whether a live `$JAVA_HOME` points into `jdk`: the guard every deletion
    /// applies.
    fn is_live(&self, jdk: &InstalledJdk, active: Option<&Path>) -> bool {
        active.is_some_and(|active| owns(&self.entry(jdk), active))
    }

    /// The newest installed JDK answering to `request`.
    ///
    /// Matched on the parsed name, not a prefix or the major: a prefix makes
    /// `1` select `17`, and a major-only match would hand `jlo env 26` a
    /// pre-release of a later patch, which sorts above the current GA build.
    pub(crate) fn find_matching(&self, request: Request) -> Option<PathBuf> {
        self.list()
            .ok()?
            .into_iter()
            .find(|jdk| jdk.request == request)
            .map(|jdk| java_home_in(&self.entry(&jdk)))
    }

    /// [`newest_ga`] against the store. An unreadable store reads as nothing
    /// installed, as in [`Self::find_matching`]: neither is the place to fail
    /// over a directory that cannot be read.
    pub(crate) fn newest_ga_request(&self) -> Option<Request> {
        newest_ga(&self.list().ok()?).map(|jdk| jdk.request)
    }

    /// The installed names, in `jlo list`'s order.
    pub(crate) fn installed_requests(&self) -> anyhow::Result<Vec<Request>> {
        Ok(group_by_name(&self.list()?)
            .into_iter()
            .map(|group| group.request)
            .collect())
    }

    /// Per name, the builds strictly older than the one this run
    /// installed. `installed` is each name with its new build's version and
    /// java home.
    ///
    /// Bounded by that build, not by the name's current newest: a concurrent
    /// run may have installed a newer one meanwhile, and measuring against it
    /// would delete the very build this run exports as `JAVA_HOME`.
    ///
    /// A store that cannot be listed plans no deletion, and says so per name.
    fn plan_replacement<'a>(
        &'a self,
        installed: Vec<(Request, String, PathBuf)>,
        active: Option<&'a Path>,
    ) -> Replacement<'a> {
        let listing = self.list();
        let names = installed
            .into_iter()
            .map(|(request, version, java_home)| {
                let mut plan = NamePlan {
                    request,
                    java_home,
                    builds: Vec::new(),
                    live: Vec::new(),
                    failures: Vec::new(),
                };
                match &listing {
                    Err(e) => plan.failures.push(format!("{e:#}")),
                    Ok(listing) => {
                        let superseded = listing.iter().filter(|jdk| {
                            jdk.request == request && is_older_than(&jdk.version, &version)
                        });
                        for jdk in superseded {
                            if self.is_live(jdk, active) {
                                plan.live.push(jdk.version.clone());
                            }
                            plan.builds.push(jdk.version.clone());
                        }
                    }
                }
                plan
            })
            .collect();
        Replacement {
            store: self,
            active,
            names,
        }
    }

    /// Remove every JDK that is not the newest of its name.
    ///
    /// The build `$JAVA_HOME` points at is skipped, as [`Self::remove`] skips
    /// it: this verb is not evaluated by the wrapper, so it cannot move the
    /// shell off it first, and the shell would be left on a deleted path. The
    /// other superseded builds still go. Passed in so the guard is testable
    /// without mutating the process environment.
    pub(crate) fn prune(&self, active_java_home: Option<&Path>) -> anyhow::Result<RemoveReport> {
        let mut report = RemoveReport::default();

        // A pass of its own for what `list` never sees: directories jlo cannot
        // name, and a missing base directory, which `list` reads as empty.
        for candidate in self.scan_required()? {
            if candidate.name.is_none() {
                crate::ui::warning!("ignoring directory with invalid name {:?}", candidate.path);
            }
        }

        let installed = self.list()?;
        let superseded = group_by_name(&installed)
            .into_iter()
            .flat_map(|group| group.others)
            .filter(|(_, status)| *status == Status::Superseded);

        for (jdk, _) in superseded {
            if self.is_live(jdk, active_java_home) {
                report.skipped_in_use = Some(jdk.version.clone());
                continue;
            }
            remove_recorded(
                &self.base,
                jdk.version.clone(),
                &mut report.removed,
                &mut report.failures,
            );
        }

        Ok(report)
    }

    /// Delete what `targets` select: every build of a name (`17`, `28-ea`), or
    /// one exact build (`17.0.11+10`).
    ///
    /// A target not installed or live (`$JAVA_HOME` points at it) is set aside
    /// and the rest still go: aborting on it would protect nothing. Each is an
    /// error only when nothing is left to remove, because a command told
    /// exactly what to delete must not report success having deleted nothing. `active_java_home` is passed in so the guard is
    /// testable without mutating the process environment.
    pub(crate) fn remove(
        &self,
        targets: &[String],
        active_java_home: Option<&Path>,
    ) -> Result<RemoveReport, RemoveError> {
        let installed = self.list().map_err(RemoveError::Store)?;
        let selectors: Vec<(&String, Selector)> = targets
            .iter()
            .map(|target| (target, Selector::parse(target)))
            .collect();

        let mut missing: Vec<String> = Vec::new();
        for (target, selector) in &selectors {
            if !installed.iter().any(|jdk| selector.matches(jdk)) && !missing.contains(target) {
                missing.push((*target).clone());
            }
        }

        // Filtering `installed` rather than collecting per target: overlapping
        // targets (`17 17.0.2+8`) select a directory once, where deleting it
        // twice would add a spurious "could not remove" line. It also keeps
        // the newest-first order, whatever order the targets came in.
        let matching: Vec<&InstalledJdk> = installed
            .iter()
            .filter(|jdk| selectors.iter().any(|(_, selector)| selector.matches(jdk)))
            .collect();

        let (in_use, removable): (Vec<_>, Vec<_>) = matching
            .into_iter()
            .partition(|jdk| self.is_live(jdk, active_java_home));
        let in_use = in_use.first().map(|jdk| jdk.version.clone());

        // Nothing left to delete: name the reason, most actionable first (the
        // live JDK can be had by switching shells).
        if removable.is_empty() {
            return Err(match in_use {
                Some(version) => RemoveError::InUse(version),
                None => RemoveError::NotInstalled(missing),
            });
        }

        let mut report = RemoveReport {
            not_installed: missing,
            skipped_in_use: in_use,
            ..RemoveReport::default()
        };

        for jdk in removable {
            remove_recorded(
                &self.base,
                jdk.version.clone(),
                &mut report.removed,
                &mut report.failures,
            );
        }

        Ok(report)
    }

    /// Move an extracted JDK from `source_dir` into the store. Returns its
    /// java home.
    pub(crate) fn install(
        &self,
        metadata: &JdkMetadata,
        source_dir: &Path,
        ui: &InstallUi,
    ) -> anyhow::Result<PathBuf> {
        let dest_dir = self.base.join(entry_name(&metadata.semver));

        let extracted_jdk_path =
            find_jdk_path(source_dir).context("could not find the extracted JDK directory")?;

        ui.start_install();

        // No directory to create first: `source_dir` is staged inside the
        // store, and the entry name is a single path component. One rename,
        // so the entry appears whole or not at all.
        std::fs::rename(&extracted_jdk_path, &dest_dir)
            .context("could not move JDK to destination")?;

        Ok(java_home_in(&dest_dir))
    }

    /// Every directory in the store. The raw `io::Error` survives so each
    /// caller decides what a missing base directory means.
    fn scan(&self) -> std::io::Result<Vec<Candidate>> {
        Ok(std::fs::read_dir(&self.base)?
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.is_dir())
            .map(|path| {
                let name = path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .map(ToString::to_string);
                let jdk = name.as_deref().and_then(parse_entry);
                Candidate { path, name, jdk }
            })
            .collect())
    }

    /// An absent base directory is an empty store; an unreadable one fails.
    fn scan_existing(&self) -> anyhow::Result<Vec<Candidate>> {
        match self.scan() {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            result => result.with_context(|| self.read_failure()),
        }
    }

    /// An absent base directory fails too.
    fn scan_required(&self) -> anyhow::Result<Vec<Candidate>> {
        self.scan().with_context(|| self.read_failure())
    }

    fn read_failure(&self) -> String {
        let base = &self.base;
        format!("could not read JDK base directory {base:?}")
    }
}

/// What became of one name in an install run.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum NameResult {
    /// Adoptium offers no build of the name for this machine; skipped.
    NotOffered,
    /// Nothing on offer supersedes every install of the name. Carries the
    /// newest build installed.
    UpToDate(String),
    /// A new build landed. `replaced` are the builds of the name it
    /// superseded and deleted, `failures` one message per build that could
    /// not be deleted.
    Installed {
        replaced: Vec<String>,
        failures: Vec<String>,
    },
}

/// What [`install_each`] did, handed back whole rather than as a `Result`: a
/// failure on the third name must not hide that the first one deleted the
/// build the shell was on.
#[derive(Debug, Default)]
pub(crate) struct InstallRun {
    /// Not offered, then already current, then newly installed - each kind in
    /// processing order. A name the run stopped before, or a new build whose
    /// payload could not be written, has no entry.
    pub(crate) names: Vec<(Request, NameResult)>,
    /// The new build's java home, when the live build was deleted and the
    /// shell told to follow.
    pub(crate) repointed: Option<PathBuf>,
    /// The superseded build kept because `$JAVA_HOME` points at it and the
    /// shell could not be told to follow.
    pub(crate) kept_active: Option<String>,
    pub(crate) error: Option<CommandError>,
    /// The majors Adoptium has released, asked only when a pre-release name is
    /// in play and the run succeeded: a `28-ea` pin whose major is out is worth
    /// a note.
    pub(crate) released: Vec<i64>,
}

impl InstallRun {
    fn replacements(&self) -> impl Iterator<Item = (&[String], &[String])> {
        self.names.iter().filter_map(|(_, result)| match result {
            NameResult::Installed { replaced, failures } => Some((&replaced[..], &failures[..])),
            _ => None,
        })
    }

    pub(crate) fn removed_count(&self) -> usize {
        self.replacements()
            .map(|(replaced, _)| replaced.len())
            .sum()
    }

    pub(crate) fn failure_count(&self) -> usize {
        self.replacements()
            .map(|(_, failures)| failures.len())
            .sum()
    }
}

/// `install` and `update`: per name, download the latest build if it
/// supersedes every install of the name, then delete the builds it supersedes.
///
/// `payload` is the calling shell's stdout when it evaluates it, and only then
/// may the build `active` (`$JAVA_HOME`) points at be deleted; without one it
/// is kept. The payload is written once, after every download and before any
/// deletion: a run killed before it has deleted nothing, one killed after has
/// already told the shell where to go. When it cannot be written, nothing is
/// deleted.
pub(crate) fn install_each<W: std::io::Write>(
    client: &AdoptiumClient,
    store: &JdkStore,
    mut requests: Vec<Request>,
    active: Option<&Path>,
    payload: Option<Payload<W>>,
) -> InstallRun {
    requests.sort_unstable_by_key(|request| request.listing_order());
    requests.dedup();

    let mut run = InstallRun::default();
    let offered = match resolve_offered(client, &requests, &mut run) {
        Ok(offered) => offered,
        Err(e) => {
            run.error = Some(e);
            return run;
        }
    };

    let mut installed = Vec::new();
    for (request, metadata) in offered {
        match install_latest(client, store, request, &metadata) {
            Ok(Latest::Installed(java_home)) => {
                installed.push((request, metadata.semver, java_home));
            }
            Ok(Latest::Current(newest)) => {
                run.names.push((request, NameResult::UpToDate(newest)));
            }
            Err(e) => {
                // Stop, but the names before it still get replaced.
                run.error = Some(e.into());
                break;
            }
        }
    }

    if let Err(e) = store
        .plan_replacement(installed, active)
        .apply(payload, &mut run)
    {
        run.error.get_or_insert(e.into());
        return run;
    }

    // A failed lookup is swallowed: a note is not worth failing a successful
    // command over.
    if run.error.is_none() && requests.iter().any(|request| request.is_ea()) {
        run.released = client.released_majors().unwrap_or_default();
    }

    run
}

/// What one name's new build supersedes.
struct NamePlan {
    request: Request,
    /// The java home of the build this run installed.
    java_home: PathBuf,
    /// The builds to delete, newest first.
    builds: Vec<String>,
    /// The builds among `builds` that `$JAVA_HOME` points into. More than one
    /// when an entry is a symlink to another: both are the shell's JDK.
    live: Vec<String>,
    /// The listing that could not be read, in place of any builds.
    failures: Vec<String>,
}

/// What an install run's new builds supersede, planned from one listing once
/// every download is done. [`Self::apply`] is the only way to act on it and
/// writes the payload before it deletes anything, so the order that keeps the
/// shell off a deleted build is the interface's, not the caller's.
struct Replacement<'a> {
    store: &'a JdkStore,
    active: Option<&'a Path>,
    names: Vec<NamePlan>,
}

impl Replacement<'_> {
    /// Write the payload, if any, then delete what was planned. Without a
    /// payload nothing moves the shell, so the live build is kept; a payload
    /// that cannot be written deletes nothing.
    ///
    /// The plan's listing may be stale at deletion time: [`remove_install`]
    /// fails on a build another run already took.
    fn apply<W: std::io::Write>(
        self,
        payload: Option<Payload<W>>,
        run: &mut InstallRun,
    ) -> anyhow::Result<()> {
        let Self {
            store,
            active,
            mut names,
        } = self;
        match payload {
            Some(payload) => {
                let repoint = names
                    .iter()
                    .rev()
                    .find(|name| !name.live.is_empty())
                    .map(|name| name.java_home.clone());
                payload.follow(repoint.as_deref(), active, store.base())?;
                run.repointed = repoint;
            }
            None => {
                for name in &mut names {
                    let live = &name.live;
                    name.builds.retain(|version| !live.contains(version));
                    if let Some(version) = name.live.first() {
                        run.kept_active = Some(version.clone());
                    }
                }
            }
        }

        for NamePlan {
            request,
            builds,
            mut failures,
            ..
        } in names
        {
            let mut replaced = Vec::new();
            for version in builds {
                remove_recorded(&store.base, version, &mut replaced, &mut failures);
            }
            run.names
                .push((request, NameResult::Installed { replaced, failures }));
        }
        Ok(())
    }
}

/// What Adoptium offers for every name, asked before anything is downloaded,
/// so a failed lookup stops the run with nothing changed.
///
/// A name not offered for this platform is skipped - `jlo install 8 21` on
/// Apple silicon installs 21 - and is an error only when no name is left.
fn resolve_offered(
    client: &AdoptiumClient,
    requests: &[Request],
    run: &mut InstallRun,
) -> Result<Vec<(Request, JdkMetadata)>, CommandError> {
    let mut offered = Vec::new();
    let mut not_offered = Vec::new();
    for &request in requests {
        match client.fetch_metadata(request)? {
            Some(metadata) => offered.push((request, metadata)),
            None => not_offered.push(request),
        }
    }

    if offered.is_empty() {
        return Err(CommandError::with_hint(
            anyhow!("{}", ui::not_offered(&not_offered)),
            ui::NOT_OFFERED_HINT,
        ));
    }
    run.names.extend(
        not_offered
            .into_iter()
            .map(|request| (request, NameResult::NotOffered)),
    );
    Ok(offered)
}

enum Latest {
    /// With the newest build installed.
    Current(String),
    /// With the new build's java home.
    Installed(PathBuf),
}

/// Downloads only an offer that supersedes every install of the name. Asking
/// "is this exact build on disk?" instead would follow a catalogue behind the
/// store, and the name would end up holding two builds.
fn install_latest(
    client: &AdoptiumClient,
    store: &JdkStore,
    request: Request,
    jdk_metadata: &JdkMetadata,
) -> anyhow::Result<Latest> {
    let installed = store.list()?;
    // Only this name: an installed EA of a later patch would otherwise keep
    // a GA offer from counting as newer.
    let builds: Vec<&InstalledJdk> = installed
        .iter()
        .filter(|jdk| jdk.request == request)
        .collect();

    match builds.first() {
        // `list` is newest first.
        Some(newest) if !supersedes_every_install(&jdk_metadata.semver, &builds) => {
            Ok(Latest::Current(newest.version.clone()))
        }
        _ => {
            let java_home =
                install_jdk(client, store, jdk_metadata).context("could not install JDK")?;
            Ok(Latest::Installed(java_home))
        }
    }
}

pub(crate) fn install_jdk(
    client: &AdoptiumClient,
    store: &JdkStore,
    jdk_metadata: &JdkMetadata,
) -> anyhow::Result<PathBuf> {
    // One progress region spans all three phases, so they share one line.
    let ui = InstallUi::new(&jdk_metadata.semver);

    match install_jdk_inner(client, store, jdk_metadata, &ui) {
        Ok(dest_dir) => {
            ui.finish(&dest_dir);
            Ok(dest_dir)
        }
        Err(e) => {
            ui.abandon();
            Err(e)
        }
    }
}

fn install_jdk_inner(
    client: &AdoptiumClient,
    store: &JdkStore,
    jdk_metadata: &JdkMetadata,
    ui: &InstallUi,
) -> anyhow::Result<PathBuf> {
    let temp_dir = staging_dir(store)?;
    let temp_file = temp_dir.path().join(&jdk_metadata.package_name);
    let file = &mut File::create(&temp_file).context("could not create temporary file")?;
    client.download(jdk_metadata, file, ui)?;

    extract::extract(&temp_file, temp_dir.path(), ui)?;

    let dest_dir = store.install(jdk_metadata, temp_dir.path(), ui)?;

    temp_dir.close().unwrap_or_else(|err| {
        ui::warning!("could not delete temporary directory: {err}");
    });

    Ok(dest_dir)
}

/// Where an install is downloaded and unpacked: inside the store, not in
/// `$TMPDIR`. The last step is a `rename` into the store, which only works
/// within one filesystem, and a tmpfs `/tmp` (Fedora, Arch, Debian 13) would
/// fail it with `EXDEV` after the whole download. Its name is no entry name,
/// so `list` passes over it.
fn staging_dir(store: &JdkStore) -> anyhow::Result<tempfile::TempDir> {
    std::fs::create_dir_all(store.base())
        .with_context(|| format!("could not create {}", store.base().display()))?;

    // An install killed with Ctrl-C runs no destructor, and nothing else
    // would ever clear its half a gigabyte from the store.
    sweep_stale_staging(store.base(), STAGING_PREFIX, None);

    tempfile::tempdir_in(store.base())
        .context("could not create a staging directory in the JDK install directory")
}

/// The prefix `tempfile` gives staging directories, for downloads and
/// deletions alike; [`JdkStore::foreign`] leaves them out.
const STAGING_PREFIX: &str = ".tmp";

/// Delete the directories under `base` starting with `prefix` that an
/// interrupted install left behind. Silent on failure: this is housekeeping.
///
/// No stager takes a lock, so one running *right now* is recognised by age:
/// only an entry *known* to be over an hour old goes. One whose age cannot be
/// read, or lies in the future (clock skew, NFS), is kept: a leftover costs
/// disk, a wrong deletion costs a working install.
///
/// `keep` is spared whatever its age: the directory the running binary was
/// staged in, for an installer suspended for over an hour before its `exec`.
pub(crate) fn sweep_stale_staging(base: &Path, prefix: &str, keep: Option<&Path>) {
    let Ok(entries) = std::fs::read_dir(base) else {
        return;
    };
    for entry in entries.flatten() {
        if !entry
            .file_name()
            .to_str()
            .is_some_and(|name| name.starts_with(prefix))
        {
            continue;
        }
        if keep.is_some_and(|keep| same_path(&entry.path(), keep)) {
            continue;
        }
        let known_stale = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age >= std::time::Duration::from_hours(1));
        if known_stale {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

/// `IntelliJ` IDEA's layout, so both tools see the same JDKs.
fn base_dir_for(os: &str, home: &Path) -> PathBuf {
    match os {
        "macos" => home.join("Library/Java/JavaVirtualMachines"),
        _ => home.join(".jdks"),
    }
}

/// Whether `version` is strictly older than `newest`: the one comparison
/// behind every "superseded" and "outdated". Two spellings of one version are
/// equal; a name that does not parse is neither older nor newer.
pub(crate) fn is_older_than(version: &str, newest: &str) -> bool {
    compare(version, newest).is_ok_and(Ordering::is_lt)
}

/// `'a'`, `'a' or 'b'`, `'a', 'b' or 'c'`.
pub(crate) fn quoted_list(items: &[String]) -> String {
    let quoted: Vec<String> = items.iter().map(|item| format!("'{item}'")).collect();
    match quoted.split_last() {
        None => String::new(),
        Some((last, [])) => last.clone(),
        Some((last, rest)) => format!("{} or {last}", rest.join(", ")),
    }
}

/// A `jlo remove` target: a version name (`17`, `28-ea`) selects every build
/// of that name - so `26` leaves `26-ea` alone - and anything else one exact
/// directory name. `17.0` therefore matches nothing: there are no ranges.
enum Selector<'a> {
    Name(Request),
    Exact(&'a str),
}

impl<'a> Selector<'a> {
    fn parse(target: &'a str) -> Self {
        match Request::parse(target) {
            Ok(request) => Self::Name(request),
            Err(_) => Self::Exact(target),
        }
    }

    fn matches(&self, jdk: &InstalledJdk) -> bool {
        match self {
            Self::Name(request) => jdk.request == *request,
            Self::Exact(version) => jdk.version == *version,
        }
    }
}

/// Whether two paths name the same file or directory.
///
/// Canonicalised when both resolve, so a trailing slash or a symlink does not
/// let a live JDK slip past the `$JAVA_HOME` guard. The literal comparison
/// comes first and stands alone: a `$JAVA_HOME` that no longer exists cannot
/// be canonicalised, and that must not turn the guard off.
pub(crate) fn same_path(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// [`remove_install`], recording the version in `removed` or the reason in
/// `failures`.
fn remove_recorded(
    base: &Path,
    version: String,
    removed: &mut Vec<String>,
    failures: &mut Vec<String>,
) {
    match remove_install(base, &version) {
        Ok(()) => removed.push(version),
        Err(e) => failures.push(format!(
            "could not remove {:?}: {e:#}",
            base.join(entry_name(&version))
        )),
    }
}

/// Delete an install: **renamed out of its name first**, into a fresh staging
/// directory, then deleted there.
///
/// A deletion interrupted halfway would otherwise leave a gutted directory
/// that still carries the name, which the listing would resolve and `jlo env`
/// hand out as `JAVA_HOME`. The rename is atomic, so the store sees the JDK
/// whole or not at all; what an interrupted deletion leaves sits under
/// [`STAGING_PREFIX`], where the next install sweeps it.
///
/// The rename is also the last word between two runs deleting one install
/// from listings that may be stale: only one of them finds it still there.
fn remove_install(base: &Path, version: &str) -> anyhow::Result<()> {
    let trash = tempfile::Builder::new()
        .prefix(STAGING_PREFIX)
        .tempdir_in(base)?;
    std::fs::rename(
        base.join(entry_name(version)),
        trash.path().join(entry_name(version)),
    )?;
    let remnant = trash.path().to_path_buf();
    trash
        .close()
        .with_context(|| format!("moved aside, but could not delete {remnant:?}"))
}

/// The java home inside a store entry: `Contents/Home` for a macOS JDK
/// bundle, the entry itself for a flat install.
///
/// Probed rather than selected on `env::consts::OS`: jlo keeps the bundle,
/// but a hand-placed entry may come either way. The probe is `bin/java`, so a
/// `Contents/Home` that cannot run Java is not read as a bundle.
fn java_home_in(dir: &Path) -> PathBuf {
    let bundled = dir.join(BUNDLE_HOME[0]).join(BUNDLE_HOME[1]);
    if bundled.join("bin").join("java").exists() {
        bundled
    } else {
        dir.to_path_buf()
    }
}

/// Whether a live `$JAVA_HOME` names the store entry `dir`, as the entry
/// itself or its `Contents/Home`.
///
/// Deliberately *not* [`java_home_in`]: a deletion guard that asks the
/// filesystem what shape a directory is can be switched off by one that
/// cannot be stat'd. Both spellings are compared unconditionally.
fn owns(dir: &Path, active: &Path) -> bool {
    same_path(dir, active) || same_path(&dir.join(BUNDLE_HOME[0]).join(BUNDLE_HOME[1]), active)
}

/// What to move into the store: the root of the extracted archive.
///
/// On macOS that is the whole bundle, not its java home: the
/// `Contents/Info.plist` is what lets `/usr/libexec/java_home` (and
/// `/usr/bin/java`, Maven and Gradle toolchain discovery) see the install.
///
/// Found by looking, not by name: `release_name` does not match the archive's
/// top-level directory for early-access builds (`jdk-28+16-ea-beta` unpacks
/// into `jdk-28+16`).
fn find_jdk_path(temp_dest: &Path) -> anyhow::Result<PathBuf> {
    let read_failure = || format!("could not read the extracted archive in {temp_dest:?}");

    for entry in std::fs::read_dir(temp_dest).with_context(read_failure)? {
        let path = entry.with_context(read_failure)?.path();
        if path.is_dir() && java_home_in(&path).join("bin").join("java").exists() {
            return Ok(path);
        }
    }

    bail!("java executable is missing under {temp_dest:?}");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::request::request;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::tempdir;

    /// The store entry holding `version`.
    fn entry(base: &Path, version: &str) -> PathBuf {
        base.join(entry_name(version))
    }

    /// A flat JDK under `name`, exactly: `bin/java` and nothing else.
    fn create_named_jdk_dir(base: &Path, name: &str) {
        let dir = base.join(name);
        fs::create_dir_all(dir.join("bin")).unwrap();
        fs::write(dir.join("bin").join("java"), "").unwrap();
    }

    fn create_jdk_dir(base: &Path, version: &str) {
        create_named_jdk_dir(base, &entry_name(version));
    }

    /// A working JDK under its bare version, as an earlier jlo installed it
    /// or as a user might drop one in: not jlo's.
    fn create_foreign_jdk_dir(base: &Path, version: &str) {
        create_named_jdk_dir(base, version);
    }

    /// A `JdkMetadata` carrying only the field the store looks at.
    fn metadata(semver: &str) -> JdkMetadata {
        JdkMetadata {
            semver: semver.to_string(),
            package_name: String::new(),
            download_link: String::new(),
            checksum: String::new(),
        }
    }

    /// The java home inside a JDK directory on *this* platform: one level in
    /// on macOS, where the directory is a bundle, and the directory itself
    /// everywhere else. The expectation [`java_home_in`] has to meet, spelled
    /// out independently of it.
    fn expected_java_home(dir: &Path) -> PathBuf {
        if env::consts::OS == "macos" {
            dir.join("Contents").join("Home")
        } else {
            dir.to_path_buf()
        }
    }

    /// A JDK in the store as a macOS bundle, whatever the host: `bin/java`
    /// lives at `<version>/Contents/Home`, not at `<version>`. Written out
    /// rather than derived from [`expected_java_home`] so the bundle rules are
    /// exercised on Linux too - they are shape rules, not platform rules.
    fn create_bundle_jdk_dir(base: &Path, version: &str) -> PathBuf {
        let java_home = entry(base, version).join("Contents").join("Home");
        fs::create_dir_all(java_home.join("bin")).unwrap();
        fs::write(java_home.join("bin").join("java"), "").unwrap();
        java_home
    }

    /// Create a mock extracted JDK under `source`, in the layout
    /// [`find_jdk_path`] expects, and return the archive root - which is what
    /// `find_jdk_path` answers and `install` moves.
    fn create_extracted_jdk(source: &Path, release: &str) -> PathBuf {
        let java_home = expected_java_home(&source.join(release));
        fs::create_dir_all(java_home.join("bin")).unwrap();
        fs::write(java_home.join("bin").join("java"), "").unwrap();
        source.join(release)
    }

    // -- ownership is the name --

    /// Only `jlo-temurin-<semver>` is a JDK to jlo. An earlier jlo's bare
    /// version and an IDE's `temurin-` download both work as JDKs, which is
    /// what makes them worth pinning: listed, they would be resolved and
    /// deleted.
    #[test]
    fn only_a_jlo_entry_name_is_an_install() {
        let dir = tempdir().unwrap();
        create_jdk_dir(dir.path(), "21.0.3+9");
        create_foreign_jdk_dir(dir.path(), "21.0.5+11");
        create_named_jdk_dir(dir.path(), "temurin-21.0.5");
        create_named_jdk_dir(dir.path(), "jlo-temurin-latest");
        fs::write(entry(dir.path(), "21.0.7+6"), "a file").unwrap();

        let installed = JdkStore::at(dir.path()).list().unwrap();
        let versions: Vec<&str> = installed.iter().map(|jdk| jdk.version.as_str()).collect();
        assert_eq!(versions, ["21.0.3+9"]);
    }

    /// Everything the listing passes over, so `jlo list` can show it - except
    /// jlo's own staging directories, and files. Another hidden directory is
    /// somebody's, and shown.
    #[test]
    fn foreign_names_every_other_directory() {
        let dir = tempdir().unwrap();
        create_jdk_dir(dir.path(), "21.0.3+9");
        create_foreign_jdk_dir(dir.path(), "21.0.5+11");
        create_named_jdk_dir(dir.path(), "temurin-21.0.5");
        create_named_jdk_dir(dir.path(), "jlo-temurin-latest");
        fs::create_dir(dir.path().join(".tmpAbC123")).unwrap();
        fs::create_dir(dir.path().join(".backup-jdk")).unwrap();
        fs::write(dir.path().join("notes.txt"), "").unwrap();

        assert_eq!(
            JdkStore::at(dir.path()).foreign().unwrap(),
            [
                ".backup-jdk",
                "21.0.5+11",
                "jlo-temurin-latest",
                "temurin-21.0.5"
            ]
        );
    }

    #[test]
    fn foreign_of_a_missing_store_is_empty() {
        let dir = tempdir().unwrap();
        let store = JdkStore::at(dir.path().join("absent"));
        assert!(store.foreign().unwrap().is_empty());
    }

    /// A removal goes through a staging directory; a successful one must not
    /// leave that behind, or every removal would cost a stray `.tmp` entry
    /// until the next install sweeps it.
    #[test]
    fn removing_an_install_leaves_nothing_behind() {
        let dir = tempdir().unwrap();
        create_jdk_dir(dir.path(), "21.0.3+9");

        JdkStore::at(dir.path())
            .remove(&["21".to_string()], None)
            .expect("it is installed and not in use");

        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    /// Two runs deleting one install from listings taken before either
    /// started: the second must fail, not report a removal it did not make.
    #[test]
    fn removing_an_install_already_gone_fails() {
        let dir = tempdir().unwrap();
        create_jdk_dir(dir.path(), "21.0.3+9");
        remove_install(dir.path(), "21.0.3+9").unwrap();

        remove_install(dir.path(), "21.0.3+9").expect_err("the install is gone");
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    /// A deletion that fails partway must not leave a gutted JDK under its
    /// name, where `jlo env` would still hand it out. The remnant sits under
    /// the staging prefix instead, named in the failure, for the sweep to
    /// take. The failure is staged with a directory that cannot be emptied.
    #[test]
    fn a_deletion_that_fails_partway_leaves_nothing_under_the_name() {
        let dir = tempdir().unwrap();
        create_jdk_dir(dir.path(), "21.0.3+9");
        let locked = entry(dir.path(), "21.0.3+9").join("lib");
        fs::create_dir(&locked).unwrap();
        fs::write(locked.join("modules"), "").unwrap();
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o555)).unwrap();

        let err = remove_install(dir.path(), "21.0.3+9").expect_err("lib cannot be emptied");

        let store = JdkStore::at(dir.path());
        assert!(store.list().unwrap().is_empty());
        assert!(store.foreign().unwrap().is_empty());
        let remnants: Vec<PathBuf> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        assert_eq!(remnants.len(), 1, "{remnants:?}");
        assert!(
            format!("{err:#}").contains(&format!("{:?}", remnants[0])),
            "{err:#}"
        );

        let locked = remnants[0].join(entry_name("21.0.3+9")).join("lib");
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// An entry that is a symlink goes as a link: what it points at is not
    /// in the store and is not jlo's.
    #[test]
    fn removing_a_symlinked_entry_keeps_its_target() {
        let dir = tempdir().unwrap();
        let elsewhere = tempdir().unwrap();
        create_named_jdk_dir(elsewhere.path(), "jdk");
        std::os::unix::fs::symlink(elsewhere.path().join("jdk"), entry(dir.path(), "21.0.3+9"))
            .unwrap();

        JdkStore::at(dir.path())
            .remove(&["21".to_string()], None)
            .expect("the link is an install");

        assert!(!entry(dir.path(), "21.0.3+9").exists());
        assert!(elsewhere.path().join("jdk/bin/java").exists());
    }

    // -- the two shapes a store entry can have --
    //
    // One store holds both: a bundle jlo installed, and a hand-placed JDK in
    // either shape. Nothing here is
    // gated on the host platform, because the rules are about the directory,
    // not about the machine reading it.

    /// The whole point of keeping the bundle: what jlo hands out as
    /// `JAVA_HOME` has to be the java home inside it, not the bundle.
    #[test]
    fn find_matching_answers_a_bundle_with_its_contents_home() {
        let dir = tempdir().unwrap();
        let java_home = create_bundle_jdk_dir(dir.path(), "21.0.3+9");

        assert_eq!(
            JdkStore::at(dir.path()).find_matching(request("21")),
            Some(java_home)
        );
    }

    /// The other half of the same rule. A flat entry placed by hand has no
    /// `Contents/Home`, and must resolve to itself rather than to a path that
    /// is not there.
    #[test]
    fn find_matching_answers_a_flat_install_with_itself() {
        let dir = tempdir().unwrap();
        create_jdk_dir(dir.path(), "21.0.3+9");

        assert_eq!(
            JdkStore::at(dir.path()).find_matching(request("21")),
            Some(entry(dir.path(), "21.0.3+9"))
        );
    }

    /// A directory named `Contents/Home` that cannot run Java is not a java
    /// home. The launcher is the probe, so a JDK that merely happens to carry
    /// such a directory still resolves to itself.
    #[test]
    fn a_contents_home_without_a_launcher_is_not_a_bundle() {
        let dir = tempdir().unwrap();
        create_jdk_dir(dir.path(), "21.0.3+9");
        fs::create_dir_all(entry(dir.path(), "21.0.3+9").join("Contents/Home")).unwrap();

        assert_eq!(
            JdkStore::at(dir.path()).find_matching(request("21")),
            Some(entry(dir.path(), "21.0.3+9"))
        );
    }

    /// The live-build guard every deletion and `jlo current` go through,
    /// over every spelling a `$JAVA_HOME` can have for one build. `jlo env`
    /// exports a bundle's `Contents/Home`; a hand-set one may name the bundle
    /// root; a symlink (on macOS every path under `/var` is one) or a trailing
    /// separator must not walk past it. The bundle whose launcher is gone is
    /// why the guard compares paths rather than asking [`java_home_in`]: the
    /// shape probe would fall back to the entry and call a `Contents/Home`
    /// nobody's - a probe that fails is exactly the case the guard must
    /// survive.
    #[test]
    fn is_live_recognises_every_spelling_of_the_build_in_use() {
        let dir = tempdir().unwrap();
        let base = dir.path();
        create_jdk_dir(base, "17.0.2+8");
        let bundle_home = create_bundle_jdk_dir(base, "21.0.3+9");
        let broken_home = create_bundle_jdk_dir(base, "25.0.1+8");
        fs::remove_file(broken_home.join("bin").join("java")).unwrap();
        let link = base.join("link");
        std::os::unix::fs::symlink(entry(base, "17.0.2+8"), &link).unwrap();
        let store = JdkStore::at(base);
        let installed = store.list().unwrap();
        let jdk = |version: &str| installed.iter().find(|jdk| jdk.version == version).unwrap();

        let cases: [(&str, Option<PathBuf>, bool); 8] = [
            ("17.0.2+8", Some(entry(base, "17.0.2+8")), true),
            ("17.0.2+8", Some(link), true),
            (
                "17.0.2+8",
                Some(PathBuf::from(format!(
                    "{}/",
                    entry(base, "17.0.2+8").display()
                ))),
                true,
            ),
            ("21.0.3+9", Some(bundle_home), true),
            ("21.0.3+9", Some(entry(base, "21.0.3+9")), true),
            ("25.0.1+8", Some(broken_home), true),
            ("17.0.2+8", Some(entry(base, "21.0.3+9")), false),
            ("17.0.2+8", None, false),
        ];
        for (version, active, live) in cases {
            assert_eq!(
                store.is_live(jdk(version), active.as_deref()),
                live,
                "{version} with JAVA_HOME {active:?}"
            );
        }
    }

    // -- find_matching --

    #[test]
    fn find_matching_finds_latest() {
        let dir = tempdir().unwrap();
        create_jdk_dir(dir.path(), "21.0.1+12");
        create_jdk_dir(dir.path(), "21.0.3+9");
        create_jdk_dir(dir.path(), "17.0.2+8");

        let result = JdkStore::at(dir.path()).find_matching(request("21"));
        assert_eq!(
            result.unwrap().file_name().unwrap().to_str().unwrap(),
            entry_name("21.0.3+9")
        );
    }

    #[test]
    fn find_matching_no_match() {
        let dir = tempdir().unwrap();
        create_jdk_dir(dir.path(), "17.0.2+8");

        assert!(
            JdkStore::at(dir.path())
                .find_matching(request("21"))
                .is_none()
        );
    }

    /// The rule the whole design rests on: `26` and `26-ea` are two names, so
    /// an installed pre-release is invisible to a GA request.
    #[test]
    fn find_matching_never_crosses_streams() {
        let dir = tempdir().unwrap();
        create_jdk_dir(dir.path(), "26.0.1+9");
        create_jdk_dir(dir.path(), "26.0.2-beta+101.0.ea");
        let store = JdkStore::at(dir.path());

        assert_eq!(
            store.find_matching(request("26")),
            Some(entry(dir.path(), "26.0.1+9")),
            "a GA request must not be answered with the higher-sorting beta"
        );
        assert_eq!(
            store.find_matching(request("26-ea")),
            Some(entry(dir.path(), "26.0.2-beta+101.0.ea"))
        );
    }

    // -- newest_ga_request --

    /// Cascade stage 3's input. A machine that once tried a pre-release must
    /// not have that build become the answer to a bare `jlo env`.
    #[test]
    fn newest_ga_request_ignores_pre_releases() {
        let dir = tempdir().unwrap();
        create_jdk_dir(dir.path(), "21.0.5+11");
        create_jdk_dir(dir.path(), "28.0.0-beta+16.0.ea");

        assert_eq!(
            JdkStore::at(dir.path()).newest_ga_request(),
            Some(request("21"))
        );
    }

    #[test]
    fn newest_ga_request_is_none_when_only_pre_releases_are_installed() {
        let dir = tempdir().unwrap();
        create_jdk_dir(dir.path(), "28.0.0-beta+16.0.ea");

        assert_eq!(JdkStore::at(dir.path()).newest_ga_request(), None);
    }

    /// The version floor sits inside the selector, so `jlo current` cannot
    /// answer differently from the cascade.
    #[test]
    fn newest_ga_request_skips_a_store_below_the_floor() {
        let dir = tempdir().unwrap();
        create_jdk_dir(dir.path(), "7.0.4+101");

        assert_eq!(JdkStore::at(dir.path()).newest_ga_request(), None);
    }

    // -- installed_requests --

    /// What a bare `jlo update` iterates over. EA names are installed names
    /// too, so they appear here; the *skipping* is the caller's rule, decided
    /// by the command that has to explain it.
    #[test]
    fn installed_requests_names_both_streams() {
        let dir = tempdir().unwrap();
        create_jdk_dir(dir.path(), "21.0.3+9");
        create_jdk_dir(dir.path(), "21.0.5+11");
        create_jdk_dir(dir.path(), "17.0.2+8");
        create_jdk_dir(dir.path(), "28.0.0-beta+16.0.ea");

        let requests = JdkStore::at(dir.path()).installed_requests().unwrap();

        // One entry per name, however many builds it has, in `jlo list`'s
        // order.
        assert_eq!(
            requests,
            vec![request("28-ea"), request("21"), request("17")]
        );
    }

    /// Before the first install there is no base directory, and a bare
    /// `jlo update` then has nothing to update rather than an I/O error.
    #[test]
    fn installed_requests_missing_base_dir_is_empty() {
        let dir = tempdir().unwrap();
        let missing = dir.path().join("nothing-installed-here");

        let requests = JdkStore::at(&missing).installed_requests().unwrap();
        assert!(requests.is_empty(), "{requests:?}");
    }

    /// Only absence reads as empty: a base directory that exists but cannot
    /// be read is reported rather than quietly finding nothing to update.
    #[test]
    fn installed_requests_unreadable_base_dir_is_an_error() {
        let dir = tempdir().unwrap();
        let base = dir.path().join("store");
        std::fs::create_dir(&base).unwrap();
        std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o000)).unwrap();

        let result = JdkStore::at(&base).installed_requests();
        std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o755)).unwrap();

        let err = result.unwrap_err();
        assert!(
            format!("{err:#}").contains("could not read JDK base directory"),
            "{err:#}"
        );
    }

    // -- list --

    #[test]
    fn list_sorted_newest_first() {
        let dir = tempdir().unwrap();
        create_jdk_dir(dir.path(), "21.0.9+7");
        create_jdk_dir(dir.path(), "21.0.12+7");
        create_jdk_dir(dir.path(), "17.0.13+11");

        let jdks = JdkStore::at(dir.path()).list().unwrap();
        let versions: Vec<_> = jdks.iter().map(|j| j.version.as_str()).collect();
        assert_eq!(versions, vec!["21.0.12+7", "21.0.9+7", "17.0.13+11"]);
    }

    #[test]
    fn list_ignores_non_semver_and_files() {
        let dir = tempdir().unwrap();
        create_jdk_dir(dir.path(), "21.0.12+7");
        fs::create_dir(dir.path().join("not-a-jdk")).unwrap();
        fs::write(entry(dir.path(), "21.0.1+9"), "a file, not a directory").unwrap();

        let jdks = JdkStore::at(dir.path()).list().unwrap();
        assert_eq!(jdks.len(), 1);
        assert_eq!(jdks[0].version, "21.0.12+7");
    }

    #[test]
    fn list_missing_base_dir_is_empty() {
        let dir = tempdir().unwrap();
        let missing = dir.path().join("nothing-installed-here");
        assert!(JdkStore::at(&missing).list().unwrap().is_empty());
    }

    /// Only a *missing* store is empty. One that exists but cannot be read
    /// must not pass for "nothing installed" - every command would then act
    /// on a store it has not seen.
    #[test]
    fn list_unreadable_base_dir_is_an_error() {
        let dir = tempdir().unwrap();
        let not_a_dir = dir.path().join("store");
        fs::write(&not_a_dir, "").unwrap();
        assert!(
            JdkStore::at(&not_a_dir).list().is_err(),
            "the store is a file"
        );
    }

    /// The staging directory has to be a sibling of the installs: the install
    /// ends in a `rename` into the store, and a `rename` out of `$TMPDIR`
    /// fails with EXDEV wherever `/tmp` is a separate filesystem - which is
    /// the default on Fedora, Arch and Debian 13. The failure arrives after
    /// the download, so it costs the user the whole archive.
    #[test]
    fn installs_are_staged_inside_the_store_not_in_tmpdir() {
        let dir = tempdir().unwrap();
        let base = dir.path().join("never-created-yet");
        let store = JdkStore::at(&base);

        let staging = staging_dir(&store).expect("the store directory is created if missing");

        assert_eq!(staging.path().parent(), Some(base.as_path()));
    }

    /// An install killed with Ctrl-C runs no destructor, so its staging
    /// directory survives - holding the tarball and the unpacked JDK in the
    /// user's JDK directory rather than in `$TMPDIR`, where the system would
    /// have cleared it. Nothing else ever will, so the next install does.
    #[test]
    fn a_stale_staging_directory_is_swept_by_the_next_install() {
        let dir = tempdir().unwrap();
        let store = JdkStore::at(dir.path());
        let stale = dir.path().join(".tmpLEFTOVER");
        fs::create_dir_all(stale.join("jdk-21.0.5+11")).unwrap();
        // Two hours old, i.e. no install could still be using it.
        let long_ago = std::time::SystemTime::now() - std::time::Duration::from_hours(2);
        fs::File::options()
            .read(true)
            .open(&stale)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(long_ago))
            .unwrap();

        let fresh = staging_dir(&store).unwrap();

        assert!(!stale.exists(), "the abandoned staging directory is gone");
        assert!(fresh.path().exists(), "the new one is not");
    }

    /// ...but one an install running right now is using is left alone.
    #[test]
    fn a_staging_directory_in_use_survives_a_sweep() {
        let dir = tempdir().unwrap();
        let store = JdkStore::at(dir.path());
        let in_use = staging_dir(&store).unwrap();

        let second = staging_dir(&store).unwrap();

        assert!(in_use.path().exists(), "a live install was swept out");
        assert!(second.path().exists());
    }

    /// An mtime in the future (clock skew, a restored backup, NFS) says
    /// nothing about the directory being old, so the sweep must not read it
    /// as permission to delete.
    #[test]
    fn a_staging_directory_with_a_future_mtime_survives_a_sweep() {
        let dir = tempdir().unwrap();
        let future = dir.path().join(".tmpFUTURE");
        fs::create_dir_all(future.join("jdk-21.0.5+11")).unwrap();
        let later = std::time::SystemTime::now() + std::time::Duration::from_hours(2);
        fs::File::options()
            .read(true)
            .open(&future)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(later))
            .unwrap();

        sweep_stale_staging(dir.path(), STAGING_PREFIX, None);

        assert!(future.exists(), "a directory of unknown age was deleted");
    }

    /// The listing must pass over a staging directory, or an interrupted
    /// install would show up as a JDK.
    #[test]
    fn a_staging_directory_is_not_mistaken_for_an_install() {
        let dir = tempdir().unwrap();
        let store = JdkStore::at(dir.path());
        create_jdk_dir(dir.path(), "21.0.3+9");

        let staging = staging_dir(&store).unwrap();

        let listed: Vec<String> = store
            .list()
            .unwrap()
            .into_iter()
            .map(|j| j.version)
            .collect();
        assert_eq!(listed, ["21.0.3+9"]);
        assert!(staging.path().exists(), "the staging directory is real");
    }

    /// `jlo remove --superseded` can run while an install is mid-download in
    /// another shell: the staging directory is that install's, so pruning
    /// must not delete it.
    #[test]
    fn prune_leaves_an_in_flight_staging_directory_alone() {
        let dir = tempdir().unwrap();
        create_jdk_dir(dir.path(), "21.0.1+12");
        create_jdk_dir(dir.path(), "21.0.3+9");
        let store = JdkStore::at(dir.path());
        let staging = staging_dir(&store).unwrap();

        let report = store.prune(None).unwrap();

        assert!(
            staging.path().exists(),
            "prune deleted an in-flight install"
        );
        assert_eq!(report.removed.len(), 1);
    }

    // -- group_by_name --

    /// An install as `list` reports it, named the way `scan` names it.
    fn jdk(version: &str) -> InstalledJdk {
        InstalledJdk {
            version: version.to_string(),
            request: Request::of_build(&crate::version::parse(version).unwrap()).unwrap(),
        }
    }

    /// A group as `(name, head, others)`, with every build by its version.
    type Grouped<'a> = (String, &'a str, Vec<(&'a str, Status)>);

    fn grouped(installed: &[InstalledJdk]) -> Vec<Grouped<'_>> {
        group_by_name(installed)
            .into_iter()
            .map(|group| {
                (
                    group.request.to_string(),
                    group.head.version.as_str(),
                    group
                        .others
                        .into_iter()
                        .map(|(jdk, status)| (jdk.version.as_str(), status))
                        .collect(),
                )
            })
            .collect()
    }

    /// Two builds of one patch differ only in build metadata, which semver
    /// leaves out of precedence. `prune` used to sort them Equal and delete
    /// whichever `read_dir` happened to yield second - a coin toss over a JDK,
    /// and one that took the *newer* build about half the time. Either input
    /// order gives the same answer.
    #[test]
    fn the_lower_build_of_one_patch_is_superseded() {
        for order in [
            ["21.0.11+9.0.LTS", "21.0.11+10.0.LTS"],
            ["21.0.11+10.0.LTS", "21.0.11+9.0.LTS"],
        ] {
            let installed = order.map(jdk);
            assert_eq!(
                grouped(&installed),
                vec![(
                    "21".to_string(),
                    "21.0.11+10.0.LTS",
                    vec![("21.0.11+9.0.LTS", Status::Superseded)]
                )]
            );
        }
    }

    /// The leniency in `version::parse` means two directory names can spell
    /// one version. Nothing distinguishes them, so there is no basis for
    /// picking one to delete: neither is superseded, and both stay.
    #[test]
    fn two_spellings_of_one_version_are_neither_superseded() {
        let installed = [jdk("v21.0.11+9"), jdk("21.0.11+9")];
        assert_eq!(
            grouped(&installed),
            vec![(
                "21".to_string(),
                "v21.0.11+9",
                vec![("21.0.11+9", Status::Installed)]
            )]
        );
    }

    /// The two streams of one major are two names, so neither supersedes the
    /// other: a later-patch beta sorts above the installed GA build, and keyed
    /// on the major alone it would make the released build superseded. The released
    /// name comes first, as `jlo list` orders them.
    #[test]
    fn neither_stream_supersedes_the_other() {
        let installed = [
            jdk("26.0.3-beta+102.0.ea"),
            jdk("26.0.2-beta+101.0.ea"),
            jdk("26.0.1+9"),
        ];
        assert_eq!(
            grouped(&installed),
            vec![
                ("26".to_string(), "26.0.1+9", vec![]),
                (
                    "26-ea".to_string(),
                    "26.0.3-beta+102.0.ea",
                    vec![("26.0.2-beta+101.0.ea", Status::Superseded)]
                ),
            ]
        );
    }

    // -- replacement --

    /// What `jlo update` deletes after installing `21.0.5+11`: every older
    /// build of *that name* - and nothing of the sibling stream, nothing
    /// foreign (an earlier jlo's bare `21.0.2+13`), nothing newer.
    /// `21.0.7+6` stands for a concurrent run that installed past this one:
    /// measured against it, `21.0.5+11` - the build this run exports - would
    /// be deleted too.
    #[test]
    fn an_update_supersedes_the_older_builds_of_its_name_only() {
        let dir = tempdir().unwrap();
        let base = dir.path();
        create_jdk_dir(base, "21.0.1+12");
        create_jdk_dir(base, "21.0.3+9");
        create_foreign_jdk_dir(base, "21.0.2+13");
        create_jdk_dir(base, "21.0.5+11");
        create_jdk_dir(base, "21.0.7+6");
        create_jdk_dir(base, "21.0.0-beta+4.0.ea");

        let store = JdkStore::at(base);
        let installed = vec![(
            request("21"),
            "21.0.5+11".to_string(),
            entry(base, "21.0.5+11"),
        )];
        let plan = store.plan_replacement(installed, None);

        assert_eq!(plan.names[0].builds, vec!["21.0.3+9", "21.0.1+12"]);
    }

    /// A plan is a listing, and the store can change before it is applied.
    /// A build another run deleted in between is a failure, not a removal
    /// this run can claim.
    #[test]
    fn applying_a_plan_reports_a_build_already_gone() {
        let dir = tempdir().unwrap();
        let base = dir.path();
        create_jdk_dir(base, "21.0.3+9");
        create_jdk_dir(base, "21.0.5+11");
        let store = JdkStore::at(base);
        let installed = vec![(
            request("21"),
            "21.0.5+11".to_string(),
            entry(base, "21.0.5+11"),
        )];
        let plan = store.plan_replacement(installed, None);

        remove_install(base, "21.0.3+9").unwrap();
        let mut run = InstallRun::default();
        plan.apply::<std::io::Sink>(None, &mut run).unwrap();

        assert!(!entry(base, "21.0.3+9").exists());
        let (_, result) = &run.names[0];
        let NameResult::Installed { replaced, failures } = result else {
            panic!("{result:?}");
        };
        assert!(replaced.is_empty(), "{replaced:?}");
        assert_eq!(failures.len(), 1, "{failures:?}");
    }

    /// The guard `jlo remove <version>` applies to a named target, applied by
    /// the other selector of the same verb. Running it in a shell still on a
    /// superseded build is the ordinary flow, not a corner: without the guard
    /// the next command in that shell runs against a `$JAVA_HOME` that no
    /// longer exists.
    #[test]
    fn prune_leaves_the_jdk_java_home_points_at_alone() {
        let dir = tempdir().unwrap();
        create_jdk_dir(dir.path(), "21.0.1+12");
        create_jdk_dir(dir.path(), "21.0.3+9");

        let active = entry(dir.path(), "21.0.1+12");
        let report = JdkStore::at(dir.path()).prune(Some(&active)).unwrap();

        assert!(active.exists(), "the live JDK must survive");
        assert_eq!(report.skipped_in_use.as_deref(), Some("21.0.1+12"));
        assert_eq!(report.removed.len(), 0);
    }

    /// A skip, not a refusal: the live JDK stays and every other superseded
    /// build still goes.
    #[test]
    fn prune_removes_the_other_superseded_builds_around_the_live_one() {
        let dir = tempdir().unwrap();
        create_jdk_dir(dir.path(), "21.0.1+12");
        create_jdk_dir(dir.path(), "21.0.3+9");
        create_jdk_dir(dir.path(), "17.0.2+8");
        create_jdk_dir(dir.path(), "17.0.9+9");

        let active = entry(dir.path(), "21.0.1+12");
        JdkStore::at(dir.path()).prune(Some(&active)).unwrap();

        assert!(active.exists());
        assert!(
            !entry(dir.path(), "17.0.2+8").exists(),
            "17.0.2+8 still goes"
        );
    }

    /// In `jlo list`'s name order - newest major first, and of one major the
    /// released name before the pre-release - even where the two streams
    /// interleave by version; each name's builds newest first.
    #[test]
    fn prune_reports_builds_in_listing_order() {
        let dir = tempdir().unwrap();
        for version in [
            "17.0.0+1",
            "17.0.1+1",
            "17.0.2+8",
            "26.0.1-beta+1",
            "26.0.2+1",
            "26.0.3-beta+1",
            "26.0.4+1",
            "21.0.1+12",
            "21.0.3+9",
        ] {
            create_jdk_dir(dir.path(), version);
        }

        let report = JdkStore::at(dir.path()).prune(None).unwrap();

        assert_eq!(
            report.removed,
            vec![
                "26.0.2+1",
                "26.0.1-beta+1",
                "21.0.1+12",
                "17.0.1+1",
                "17.0.0+1"
            ]
        );
        assert!(report.failures.is_empty());
    }

    /// An earlier jlo's bare-version installs are older builds of a name
    /// jlo now holds, and still not jlo's to delete.
    #[test]
    fn prune_leaves_foreign_installs_alone() {
        let dir = tempdir().unwrap();
        create_foreign_jdk_dir(dir.path(), "21.0.1+12");
        create_jdk_dir(dir.path(), "21.0.3+9");

        let report = JdkStore::at(dir.path()).prune(None).unwrap();

        assert!(report.removed.is_empty());
        assert!(dir.path().join("21.0.1+12").exists());
    }

    /// `jlo remove --superseded` says so when the install directory cannot be read, rather
    /// than reporting an empty run.
    #[test]
    fn prune_missing_base_dir_is_an_error() {
        let dir = tempdir().unwrap();
        let missing = dir.path().join("never-installed");

        let err = JdkStore::at(&missing).prune(None).unwrap_err();
        assert!(
            format!("{err:#}").contains("could not read JDK base directory"),
            "{err:#}"
        );
    }

    // -- remove --

    fn targets(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn remove_deletes_every_build_of_a_major() {
        let dir = tempdir().unwrap();
        create_jdk_dir(dir.path(), "17.0.2+8");
        create_jdk_dir(dir.path(), "17.0.9+9");
        create_jdk_dir(dir.path(), "21.0.3+9");

        let report = JdkStore::at(dir.path())
            .remove(&targets(&["17"]), None)
            .unwrap();

        // Newest first, the order `list` and `jlo list --offline` use.
        assert_eq!(report.removed, vec!["17.0.9+9", "17.0.2+8"]);
        assert!(report.failures.is_empty());
        assert!(!entry(dir.path(), "17.0.2+8").exists());
        assert!(!entry(dir.path(), "17.0.9+9").exists());
        assert!(entry(dir.path(), "21.0.3+9").exists());
    }

    /// An exact target names one directory, so its siblings in the same major
    /// stay. This is the half of `remove` that reads like a pin but is not
    /// one - it selects an install that already exists rather than requesting
    /// a build.
    #[test]
    fn remove_deletes_only_the_exact_version_named() {
        let dir = tempdir().unwrap();
        create_jdk_dir(dir.path(), "17.0.2+8");
        create_jdk_dir(dir.path(), "17.0.9+9");

        let report = JdkStore::at(dir.path())
            .remove(&targets(&["17.0.2+8"]), None)
            .unwrap();

        assert_eq!(report.removed, vec!["17.0.2+8"]);
        assert!(!entry(dir.path(), "17.0.2+8").exists());
        assert!(entry(dir.path(), "17.0.9+9").exists());
    }

    #[test]
    fn remove_reports_a_target_that_is_not_installed() {
        let dir = tempdir().unwrap();
        create_jdk_dir(dir.path(), "21.0.3+9");

        let err = JdkStore::at(dir.path())
            .remove(&targets(&["17"]), None)
            .unwrap_err();

        assert!(
            matches!(err, RemoveError::NotInstalled(ref v) if v == &["17"]),
            "{err}"
        );
        assert_eq!(err.to_string(), "no installed JDK matches '17'");
    }

    /// An exact version that is not a directory name is "not installed", not
    /// a nearest-match: `remove` names a directory, it does not resolve one.
    #[test]
    fn remove_does_not_round_an_exact_target_to_a_neighbour() {
        let dir = tempdir().unwrap();
        create_jdk_dir(dir.path(), "17.0.9+9");

        let err = JdkStore::at(dir.path())
            .remove(&targets(&["17.0.2+8"]), None)
            .unwrap_err();

        assert!(matches!(err, RemoveError::NotInstalled(_)), "{err}");
        assert!(entry(dir.path(), "17.0.9+9").exists());
    }

    #[test]
    fn remove_takes_several_targets_at_once() {
        let dir = tempdir().unwrap();
        create_jdk_dir(dir.path(), "11.0.1+13");
        create_jdk_dir(dir.path(), "17.0.2+8");
        create_jdk_dir(dir.path(), "21.0.3+9");

        let report = JdkStore::at(dir.path())
            .remove(&targets(&["11", "17"]), None)
            .unwrap();

        // Newest first overall, whatever order the targets came in.
        assert_eq!(report.removed, vec!["17.0.2+8", "11.0.1+13"]);
        assert!(entry(dir.path(), "21.0.3+9").exists());
    }

    #[test]
    fn remove_mixes_major_and_exact_targets() {
        let dir = tempdir().unwrap();
        create_jdk_dir(dir.path(), "17.0.2+8");
        create_jdk_dir(dir.path(), "21.0.1+12");
        create_jdk_dir(dir.path(), "21.0.3+9");

        let report = JdkStore::at(dir.path())
            .remove(&targets(&["17", "21.0.1+12"]), None)
            .unwrap();

        assert_eq!(report.removed, vec!["21.0.1+12", "17.0.2+8"]);
        assert!(entry(dir.path(), "21.0.3+9").exists());
    }

    /// Overlapping targets select the same directory once. Deleting it twice
    /// would turn the second attempt into a spurious failure line.
    #[test]
    fn remove_does_not_select_an_overlapping_target_twice() {
        let dir = tempdir().unwrap();
        create_jdk_dir(dir.path(), "17.0.2+8");

        let report = JdkStore::at(dir.path())
            .remove(&targets(&["17", "17.0.2+8"]), None)
            .unwrap();

        assert_eq!(report.removed, vec!["17.0.2+8"]);
        assert!(report.failures.is_empty(), "{:?}", report.failures);
    }

    /// A version that matches nothing cannot have deleted anything, so it
    /// has no business stopping the versions that can. This is the case that
    /// made `jlo remove 3 4 5 17` throw away a perfectly good 17.
    #[test]
    fn remove_skips_a_version_that_matches_nothing_and_gets_on_with_the_rest() {
        let dir = tempdir().unwrap();
        create_jdk_dir(dir.path(), "17.0.2+8");

        let report = JdkStore::at(dir.path())
            .remove(&targets(&["3", "4", "5", "17"]), None)
            .unwrap();

        assert_eq!(report.removed, vec!["17.0.2+8"]);
        assert_eq!(report.not_installed, vec!["3", "4", "5"]);
        assert!(!entry(dir.path(), "17.0.2+8").exists());
    }

    /// Every miss in one message. Reporting only the first would make
    /// clearing out three stale majors a three-rerun exercise.
    #[test]
    fn remove_names_every_version_that_matched_nothing() {
        let dir = tempdir().unwrap();
        create_jdk_dir(dir.path(), "21.0.3+9");

        let err = JdkStore::at(dir.path())
            .remove(&targets(&["3", "4", "5"]), None)
            .unwrap_err();

        assert_eq!(err.to_string(), "no installed JDK matches '3', '4' or '5'");
    }

    /// The misses are collected across the whole list, not just its tail,
    /// and deduplicated in the order the user typed them.
    #[test]
    fn remove_collects_misses_from_either_side_of_a_match() {
        let dir = tempdir().unwrap();
        create_jdk_dir(dir.path(), "21.0.3+9");

        let report = JdkStore::at(dir.path())
            .remove(&targets(&["3", "21", "5", "3"]), None)
            .unwrap();

        assert_eq!(report.removed, vec!["21.0.3+9"]);
        assert_eq!(report.not_installed, vec!["3", "5"]);
    }

    // -- remove: the refusals --

    /// A foreign directory named exactly like the target is not an install:
    /// neither the name nor the exact build reaches it.
    #[test]
    fn remove_never_reaches_a_foreign_directory() {
        let dir = tempdir().unwrap();
        create_foreign_jdk_dir(dir.path(), "17.0.2+8");

        for target in ["17", "17.0.2+8"] {
            let err = JdkStore::at(dir.path())
                .remove(&targets(&[target]), None)
                .unwrap_err();
            assert!(matches!(err, RemoveError::NotInstalled(_)), "{err}");
        }
        assert!(dir.path().join("17.0.2+8").join("bin/java").exists());
    }

    /// The hazard that killed `jlo update --clean`: deleting the JDK the
    /// calling shell is on leaves `$JAVA_HOME` pointing at nothing.
    #[test]
    fn remove_refuses_the_jdk_java_home_points_at() {
        let dir = tempdir().unwrap();
        create_jdk_dir(dir.path(), "21.0.3+9");
        let active = entry(dir.path(), "21.0.3+9");

        let err = JdkStore::at(dir.path())
            .remove(&targets(&["21.0.3+9"]), Some(&active))
            .unwrap_err();

        assert!(matches!(err, RemoveError::InUse(_)), "{err}");
        assert_eq!(
            err.to_string(),
            "refusing to remove 21.0.3+9: JAVA_HOME points at it"
        );
        assert!(active.exists(), "the live JDK must survive the refusal");
    }

    /// The live JDK is set aside, not fatal to its siblings: `jlo remove 21`
    /// while the shell is on a 21 removes the other 21s and leaves that one.
    /// Skipping it is the whole of the guard - the other removals could never
    /// have stranded `$JAVA_HOME`.
    #[test]
    fn remove_skips_the_live_build_and_takes_its_siblings() {
        let dir = tempdir().unwrap();
        create_jdk_dir(dir.path(), "21.0.1+12");
        create_jdk_dir(dir.path(), "21.0.3+9");
        let active = entry(dir.path(), "21.0.3+9");

        let report = JdkStore::at(dir.path())
            .remove(&targets(&["21"]), Some(&active))
            .unwrap();

        assert_eq!(report.removed, vec!["21.0.1+12"]);
        assert_eq!(report.skipped_in_use.as_deref(), Some("21.0.3+9"));
        assert!(active.exists(), "the live JDK must survive");
    }

    /// The case that prompted the rule: a long cleanup list where one member
    /// happens to be the live JDK. The other removals are safe, and refusing
    /// them protected nothing.
    #[test]
    fn remove_takes_the_rest_of_a_long_list_past_the_live_jdk() {
        let dir = tempdir().unwrap();
        create_jdk_dir(dir.path(), "17.0.2+8");
        create_jdk_dir(dir.path(), "21.0.3+9");
        create_jdk_dir(dir.path(), "26.0.2+101");
        let active = entry(dir.path(), "26.0.2+101");

        let report = JdkStore::at(dir.path())
            .remove(&targets(&["21", "26", "3", "17"]), Some(&active))
            .unwrap();

        assert_eq!(report.removed, vec!["21.0.3+9", "17.0.2+8"]);
        assert_eq!(report.skipped_in_use.as_deref(), Some("26.0.2+101"));
        assert_eq!(report.not_installed, vec!["3"]);
        assert!(active.exists());
    }

    #[test]
    fn remove_proceeds_when_java_home_points_somewhere_else() {
        let dir = tempdir().unwrap();
        create_jdk_dir(dir.path(), "17.0.2+8");
        create_jdk_dir(dir.path(), "21.0.3+9");
        let active = entry(dir.path(), "21.0.3+9");

        let report = JdkStore::at(dir.path())
            .remove(&targets(&["17"]), Some(&active))
            .unwrap();

        assert_eq!(report.removed, vec!["17.0.2+8"]);
        assert!(active.exists());
    }

    /// `list` treats a missing base directory as "nothing installed", so an
    /// empty store answers the target rather than failing on the directory.
    #[test]
    fn remove_on_a_missing_base_dir_reports_nothing_installed() {
        let dir = tempdir().unwrap();
        let missing = dir.path().join("never-installed");

        let err = JdkStore::at(&missing)
            .remove(&targets(&["21"]), None)
            .unwrap_err();
        assert!(matches!(err, RemoveError::NotInstalled(_)), "{err}");
    }

    // -- quoted_list --

    #[test]
    fn quoted_list_reads_as_a_sentence() {
        assert_eq!(quoted_list(&targets(&["3"])), "'3'");
        assert_eq!(quoted_list(&targets(&["3", "4"])), "'3' or '4'");
        assert_eq!(quoted_list(&targets(&["3", "4", "5"])), "'3', '4' or '5'");
        assert_eq!(quoted_list(&[]), "");
    }

    // -- Selector --

    /// `jlo remove 26` must not take the beta with it, and `jlo remove 26-ea`
    /// must reach the beta - the selector is the *name*, like every other rule
    /// here. Exact-build targets are untouched by that: they name one
    /// directory and always did.
    #[test]
    fn selector_selects_by_name_not_by_major() {
        let ga = InstalledJdk {
            version: "26.0.1+9".to_string(),
            request: request("26"),
        };
        let ea = InstalledJdk {
            version: "26.0.2-beta+101.0.ea".to_string(),
            request: request("26-ea"),
        };

        assert!(Selector::parse("26").matches(&ga));
        assert!(!Selector::parse("26").matches(&ea));
        assert!(Selector::parse("26-ea").matches(&ea));
        assert!(!Selector::parse("26-ea").matches(&ga));
        // The exact build still names exactly one install.
        assert!(Selector::parse("26.0.2-beta+101.0.ea").matches(&ea));
        assert!(Selector::parse("26.0.1+9").matches(&ga));
        // A major is the whole number, not a prefix of it.
        assert!(!Selector::parse("2").matches(&ga));
        // Neither a major nor a directory name: a range, which jlo has no
        // notion of anywhere.
        assert!(!Selector::parse("26.0").matches(&ga));
    }

    // -- find_jdk_path --

    #[test]
    fn find_jdk_path_valid() {
        let dir = tempdir().unwrap();
        let jdk_dir = create_extracted_jdk(dir.path(), "jdk-21.0.3+9");

        let result = find_jdk_path(dir.path()).unwrap();
        assert_eq!(result, jdk_dir);
    }

    /// The archive's top-level directory is not the API's `release_name`, and
    /// for an early-access build the two differ: Adoptium labels the 28 EA
    /// build `jdk-28+16-ea-beta` and ships an archive that unpacks into
    /// `jdk-28+16`. Naming the directory instead of looking for it cost every
    /// EA install its last step, after the whole download.
    #[test]
    fn find_jdk_path_ignores_the_release_name() {
        let dir = tempdir().unwrap();
        let jdk_dir = create_extracted_jdk(dir.path(), "jdk-28+16");
        // The downloaded archive is extracted in place, so it is still here.
        fs::write(dir.path().join("OpenJDK28U-jdk_hotspot.tar.gz"), "").unwrap();

        assert_eq!(find_jdk_path(dir.path()).unwrap(), jdk_dir);
    }

    #[test]
    fn find_jdk_path_missing_java_binary() {
        let dir = tempdir().unwrap();

        // Create dir structure but no java binary
        fs::create_dir_all(expected_java_home(&dir.path().join("jdk-21.0.3+9")).join("bin"))
            .unwrap();

        let result = find_jdk_path(dir.path());
        assert!(result.is_err());
        let message = result.unwrap_err().to_string();
        assert!(message.contains("java executable is missing"), "{message}");
        // An install that cannot find its java must fail loudly, and naming
        // only a path jlo guessed at would point the reader at the wrong one.
        assert!(
            message.contains(dir.path().to_str().unwrap()),
            "names the directory it searched: {message}"
        );
    }

    // -- the install run --

    /// A package as far as the store looks at one: a tar.gz whose one root
    /// directory holds `bin/java`.
    fn jdk_archive(semver: &str) -> Vec<u8> {
        let mut builder = tar::Builder::new(flate2::write::GzEncoder::new(
            Vec::new(),
            flate2::Compression::fast(),
        ));
        let mut header = tar::Header::new_gnu();
        header.set_size(0);
        header.set_mode(0o755);
        header.set_cksum();
        builder
            .append_data(
                &mut header,
                format!("jdk-{semver}/bin/java"),
                std::io::empty(),
            )
            .unwrap();
        builder.into_inner().unwrap().finish().unwrap()
    }

    /// Adoptium on a local server: what it offers per major. By the time
    /// [`Self::assert`] runs, every major must have been looked up once and
    /// each package downloaded as often as its offer says.
    struct Adoptium {
        server: mockito::ServerGuard,
        mocks: Vec<mockito::Mock>,
    }

    impl Adoptium {
        fn new() -> Self {
            Self {
                server: mockito::Server::new(),
                mocks: Vec::new(),
            }
        }

        fn lookup(&mut self, major: &str) -> mockito::Mock {
            self.server
                .mock(
                    "GET",
                    mockito::Matcher::Regex(format!(r"^/v3/assets/latest/{major}/hotspot")),
                )
                .match_query(mockito::Matcher::Any)
        }

        /// `semver` as the latest build of `major`, downloaded `downloads`
        /// times. `intact: false` serves a package that fails its checksum.
        fn offer(mut self, major: &str, semver: &str, downloads: usize, intact: bool) -> Self {
            use sha2::Digest;
            let archive = jdk_archive(semver);
            let checksum = if intact {
                hex::encode(sha2::Sha256::digest(&archive))
            } else {
                "00".to_string()
            };
            let link = format!("{}/jdk-{major}.tar.gz", self.server.url());
            let lookup = self.lookup(major)
                .with_body(format!(
                    r#"[{{"version":{{"semver":"{semver}"}},"binary":{{"package":{{"name":"jdk.tar.gz","link":"{link}","checksum":"{checksum}"}}}}}}]"#
                ))
                .create();
            self.mocks.push(lookup);
            let download = self
                .server
                .mock("GET", format!("/jdk-{major}.tar.gz").as_str())
                .with_body(archive)
                .expect(downloads)
                .create();
            self.mocks.push(download);
            self
        }

        /// Adoptium's `200 []`: no build of `major` for this platform.
        fn not_offered(mut self, major: &str) -> Self {
            let lookup = self.lookup(major).with_body("[]").create();
            self.mocks.push(lookup);
            self
        }

        /// A lookup of `major` that fails outright.
        fn failing(mut self, major: &str) -> Self {
            let lookup = self.lookup(major).with_status(500).create();
            self.mocks.push(lookup);
            self
        }

        fn client(&self) -> AdoptiumClient {
            AdoptiumClient::new(self.server.url())
        }

        fn assert(&self) {
            for mock in &self.mocks {
                mock.assert();
            }
        }
    }

    fn names(names: &[&str]) -> Vec<Request> {
        names.iter().map(|name| request(name)).collect()
    }

    /// A run nothing evaluates: no payload is written.
    fn run_unwrapped(
        adoptium: &Adoptium,
        store: &JdkStore,
        requests: &[&str],
        active: Option<&Path>,
    ) -> InstallRun {
        install_each::<std::io::Sink>(&adoptium.client(), store, names(requests), active, None)
    }

    /// A wrapped run whose payload goes to `out`.
    fn run_wrapped(
        adoptium: &Adoptium,
        store: &JdkStore,
        requests: &[&str],
        active: Option<&Path>,
        out: &mut impl std::io::Write,
    ) -> InstallRun {
        install_each(
            &adoptium.client(),
            store,
            names(requests),
            active,
            Some(Payload::new(out)),
        )
    }

    /// A payload reader that looks at the store when the payload is flushed:
    /// whether `watched` was still there when the shell was told to move.
    struct Witness {
        watched: PathBuf,
        written: Vec<u8>,
        present_at_flush: Option<bool>,
    }

    impl Witness {
        fn new(watched: PathBuf) -> Self {
            Self {
                watched,
                written: Vec::new(),
                present_at_flush: None,
            }
        }

        fn payload(&self) -> String {
            String::from_utf8(self.written.clone()).unwrap()
        }
    }

    impl std::io::Write for Witness {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.written.write(buf)
        }

        fn flush(&mut self) -> std::io::Result<()> {
            self.present_at_flush = Some(self.watched.exists());
            Ok(())
        }
    }

    /// A payload reader that went away: every write fails, or with
    /// `writes: true` only the final flush.
    struct Gone {
        writes: bool,
    }

    impl std::io::Write for Gone {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            if self.writes {
                Ok(buf.len())
            } else {
                Err(std::io::ErrorKind::BrokenPipe.into())
            }
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Err(std::io::ErrorKind::BrokenPipe.into())
        }
    }

    /// A catalogue behind the store - a rolled-back release, or an install
    /// from elsewhere - is not followed: the older build would land beside
    /// the newer one, which it does not supersede.
    #[test]
    fn the_run_never_moves_a_name_back() {
        let dir = tempdir().unwrap();
        create_jdk_dir(dir.path(), "21.0.10+5");
        create_jdk_dir(dir.path(), "21.0.12+7");
        let adoptium = Adoptium::new().offer("21", "21.0.11+9", 0, true);

        let run = run_unwrapped(&adoptium, &JdkStore::at(dir.path()), &["21"], None);

        adoptium.assert();
        assert!(run.error.is_none(), "{:?}", run.error);
        assert_eq!(
            run.names,
            vec![(request("21"), NameResult::UpToDate("21.0.12+7".to_string()))]
        );
        assert!(!entry(dir.path(), "21.0.11+9").exists());
        assert!(entry(dir.path(), "21.0.10+5").exists());
    }

    /// Two spellings of one version are one version: a hand-placed
    /// `jlo-temurin-v21.0.11+9` is the offer already present.
    #[test]
    fn the_run_takes_another_spelling_of_the_offer_as_current() {
        let dir = tempdir().unwrap();
        create_jdk_dir(dir.path(), "v21.0.11+9");
        let adoptium = Adoptium::new().offer("21", "21.0.11+9", 0, true);

        let run = run_unwrapped(&adoptium, &JdkStore::at(dir.path()), &["21"], None);

        adoptium.assert();
        assert!(run.error.is_none(), "{:?}", run.error);
        assert_eq!(
            run.names,
            vec![(
                request("21"),
                NameResult::UpToDate("v21.0.11+9".to_string())
            )]
        );
        assert!(!entry(dir.path(), "21.0.11+9").exists());
    }

    /// Only builds of the name are measured against: a `21-ea` build newer
    /// than the offer must not make `21` read as up to date, nor be replaced
    /// by it.
    #[test]
    fn a_newer_pre_release_does_not_hold_back_its_release() {
        let dir = tempdir().unwrap();
        create_jdk_dir(dir.path(), "21.0.5+11");
        create_jdk_dir(dir.path(), "21.0.10-beta+3");
        let adoptium = Adoptium::new().offer("21", "21.0.9+10", 1, true);

        let run = run_unwrapped(&adoptium, &JdkStore::at(dir.path()), &["21"], None);

        adoptium.assert();
        assert!(run.error.is_none(), "{:?}", run.error);
        assert_eq!(run.names, vec![(request("21"), installed(&["21.0.5+11"]))]);
        assert!(entry(dir.path(), "21.0.9+10").exists());
        assert!(entry(dir.path(), "21.0.10-beta+3").exists());
    }

    /// `jlo install 8 21` on Apple silicon installs 21: a name Adoptium does
    /// not offer here is a fact about the name, not a reason to stop.
    #[test]
    fn a_name_adoptium_does_not_offer_is_skipped() {
        let dir = tempdir().unwrap();
        let adoptium = Adoptium::new()
            .not_offered("8")
            .offer("21", "21.0.9+10", 1, true);

        let run = run_unwrapped(&adoptium, &JdkStore::at(dir.path()), &["8", "21"], None);

        adoptium.assert();
        assert!(run.error.is_none(), "{:?}", run.error);
        assert_eq!(
            run.names,
            vec![
                (request("8"), NameResult::NotOffered),
                (request("21"), installed(&[]))
            ]
        );
        assert!(entry(dir.path(), "21.0.9+10").exists());
    }

    /// With no name left, a command told to install something must not
    /// succeed having done nothing - and nothing reaches the shell.
    #[test]
    fn only_names_adoptium_does_not_offer_is_an_error() {
        let dir = tempdir().unwrap();
        let adoptium = Adoptium::new().not_offered("8").not_offered("30");
        let mut out = Vec::new();

        let run = run_wrapped(
            &adoptium,
            &JdkStore::at(dir.path()),
            &["8", "30"],
            None,
            &mut out,
        );

        let error = run.error.expect("nothing was offered");
        assert!(
            format!("{:#}", error.error).contains("offers no build of '30' or '8'"),
            "{error:?}"
        );
        // Named in the error, so not skipped one by one as well.
        assert!(run.names.is_empty());
        assert!(out.is_empty());
        assert!(fs::read_dir(dir.path()).unwrap().next().is_none());
    }

    /// A lookup that fails says nothing about the name, so it stops the run -
    /// and every name is looked up first, so 21, processed ahead of the
    /// failing 17, is neither downloaded nor replaced.
    #[test]
    fn a_failed_lookup_stops_the_run_before_any_download() {
        let dir = tempdir().unwrap();
        create_jdk_dir(dir.path(), "17.0.5+8");
        create_jdk_dir(dir.path(), "21.0.5+11");
        let adoptium = Adoptium::new()
            .offer("21", "21.0.9+10", 0, true)
            .failing("17");
        let mut out = Vec::new();

        let run = run_wrapped(
            &adoptium,
            &JdkStore::at(dir.path()),
            &["17", "21"],
            None,
            &mut out,
        );

        adoptium.assert();
        let error = run.error.expect("the lookup failed");
        assert!(
            format!("{:#}", error.error).contains("HTTP 500"),
            "{error:?}"
        );
        assert!(run.names.is_empty());
        assert!(out.is_empty(), "the shell was told something");
        assert!(entry(dir.path(), "21.0.5+11").exists());
        assert!(!entry(dir.path(), "21.0.9+10").exists());
    }

    /// Unwrapped, nothing is known to evaluate stdout, so the shell cannot
    /// follow: the build `JAVA_HOME` points at stays, and is named.
    #[test]
    fn an_unwrapped_run_keeps_the_live_build() {
        let dir = tempdir().unwrap();
        create_jdk_dir(dir.path(), "21.0.5+11");
        let live = entry(dir.path(), "21.0.5+11");
        let adoptium = Adoptium::new().offer("21", "21.0.9+10", 1, true);

        let run = run_unwrapped(&adoptium, &JdkStore::at(dir.path()), &["21"], Some(&live));

        adoptium.assert();
        assert!(run.error.is_none(), "{:?}", run.error);
        assert!(live.exists());
        assert_eq!(run.kept_active.as_deref(), Some("21.0.5+11"));
        assert!(run.repointed.is_none());
        assert!(entry(dir.path(), "21.0.9+10").exists());
    }

    /// An entry symlinked to another is the same JDK: `JAVA_HOME` through the
    /// link is live in both, and keeping only one would delete the link's
    /// target or leave the link dangling.
    #[test]
    fn an_unwrapped_run_keeps_every_build_the_shell_is_on() {
        let dir = tempdir().unwrap();
        create_jdk_dir(dir.path(), "21.0.5+11");
        std::os::unix::fs::symlink(
            entry(dir.path(), "21.0.5+11"),
            entry(dir.path(), "21.0.6+7"),
        )
        .unwrap();
        let live = entry(dir.path(), "21.0.6+7");
        let adoptium = Adoptium::new().offer("21", "21.0.9+10", 1, true);

        let run = run_unwrapped(&adoptium, &JdkStore::at(dir.path()), &["21"], Some(&live));

        assert!(run.error.is_none(), "{:?}", run.error);
        assert!(live.join("bin/java").exists());
        assert!(entry(dir.path(), "21.0.5+11").exists());
    }

    /// Wrapped, the live build goes too - but only after the payload that
    /// moves the shell off it has been written and flushed.
    #[test]
    fn a_wrapped_run_deletes_the_live_build_only_after_the_payload() {
        let dir = tempdir().unwrap();
        create_jdk_dir(dir.path(), "21.0.5+11");
        let live = entry(dir.path(), "21.0.5+11");
        let adoptium = Adoptium::new().offer("21", "21.0.9+10", 1, true);
        let mut witness = Witness::new(live.clone());

        let run = run_wrapped(
            &adoptium,
            &JdkStore::at(dir.path()),
            &["21"],
            Some(&live),
            &mut witness,
        );

        adoptium.assert();
        assert!(run.error.is_none(), "{:?}", run.error);
        assert_eq!(witness.present_at_flush, Some(true));
        assert!(!live.exists());
        // The package is flat, so its java home is the entry itself.
        let new = entry(dir.path(), "21.0.9+10");
        assert_eq!(run.repointed.as_deref(), Some(new.as_path()));
        assert!(run.kept_active.is_none());
        let payload = witness.payload();
        assert!(
            payload.contains(&format!("export JAVA_HOME='{}'", new.display())),
            "{payload}"
        );
        assert!(payload.ends_with("# jlo'end\n"), "{payload}");
    }

    /// A payload that did not arrive whole is one the shell did not apply,
    /// so nothing is deleted - whether the writes fail or only the flush.
    #[test]
    fn a_payload_that_cannot_be_written_deletes_nothing() {
        for writes in [false, true] {
            let dir = tempdir().unwrap();
            create_jdk_dir(dir.path(), "17.0.5+8");
            create_jdk_dir(dir.path(), "21.0.5+11");
            let live = entry(dir.path(), "21.0.5+11");
            let adoptium = Adoptium::new()
                .offer("21", "21.0.9+10", 1, true)
                .offer("17", "17.0.9+1", 1, true);

            let run = run_wrapped(
                &adoptium,
                &JdkStore::at(dir.path()),
                &["17", "21"],
                Some(&live),
                &mut Gone { writes },
            );

            adoptium.assert();
            assert!(run.error.is_some(), "writes: {writes}");
            assert!(live.exists(), "writes: {writes}");
            assert!(entry(dir.path(), "17.0.5+8").exists(), "writes: {writes}");
            assert!(run.repointed.is_none(), "writes: {writes}");
        }
    }

    /// A failed download stops the run, but the names before it stay
    /// updated and replaced - and it is that failure the run reports, not a
    /// payload failure after it.
    #[test]
    fn a_failed_download_keeps_what_the_names_before_it_did() {
        let dir = tempdir().unwrap();
        create_jdk_dir(dir.path(), "17.0.5+8");
        create_jdk_dir(dir.path(), "21.0.5+11");
        let adoptium = Adoptium::new()
            .offer("21", "21.0.9+10", 1, true)
            .offer("17", "17.0.9+1", 1, false);
        let mut out = Vec::new();

        let run = run_wrapped(
            &adoptium,
            &JdkStore::at(dir.path()),
            &["17", "21"],
            None,
            &mut out,
        );

        adoptium.assert();
        let error = run.error.expect("the download failed");
        assert!(
            format!("{:#}", error.error).contains("checksum"),
            "{error:?}"
        );
        assert!(!entry(dir.path(), "21.0.5+11").exists());
        assert!(entry(dir.path(), "21.0.9+10").exists());
        assert!(entry(dir.path(), "17.0.5+8").exists());
        assert!(!entry(dir.path(), "17.0.9+1").exists());
        assert_eq!(String::from_utf8(out).unwrap(), "# jlo'end\n");
    }

    /// The download failure is the error the user sees, even when the
    /// payload after it cannot be written either.
    #[test]
    fn an_earlier_error_survives_a_failed_payload() {
        let dir = tempdir().unwrap();
        create_jdk_dir(dir.path(), "21.0.5+11");
        let adoptium = Adoptium::new()
            .offer("21", "21.0.9+10", 1, true)
            .offer("17", "17.0.9+1", 1, false);

        let run = run_wrapped(
            &adoptium,
            &JdkStore::at(dir.path()),
            &["17", "21"],
            None,
            &mut Gone { writes: false },
        );

        let error = run.error.expect("the download failed");
        assert!(
            format!("{:#}", error.error).contains("checksum"),
            "{error:?}"
        );
        assert!(entry(dir.path(), "21.0.5+11").exists());
    }

    /// A name given twice is one lookup and one download.
    #[test]
    fn the_run_takes_each_name_once() {
        let dir = tempdir().unwrap();
        let adoptium = Adoptium::new().offer("21", "21.0.9+10", 1, true);

        let run = run_unwrapped(&adoptium, &JdkStore::at(dir.path()), &["21", "21"], None);

        adoptium.assert();
        assert!(run.error.is_none(), "{:?}", run.error);
        assert_eq!(run.names, vec![(request("21"), installed(&[]))]);
    }

    fn installed(replaced: &[&str]) -> NameResult {
        NameResult::Installed {
            replaced: replaced.iter().map(ToString::to_string).collect(),
            failures: Vec::new(),
        }
    }

    // -- install --

    /// Install a mock JDK into a fresh store and hand back both halves of the
    /// answer: the store entry and what `install` returned.
    fn install_mock_jdk(dest_parent: &Path) -> (PathBuf, PathBuf) {
        let source_dir = tempdir().unwrap();
        let release = "jdk-21.0.3+9";
        create_extracted_jdk(source_dir.path(), release);

        let returned = JdkStore::at(dest_parent)
            .install(
                &metadata("21.0.3+9"),
                source_dir.path(),
                &InstallUi::hidden("test"),
            )
            .unwrap();

        (entry(dest_parent, "21.0.3+9"), returned)
    }

    /// The install lands under its entry name, and nothing else appears in
    /// the store: no marker beside it, no staging left over.
    #[test]
    fn install_moves_into_the_entry_name() {
        let dest_parent = tempdir().unwrap();
        let (entry, java_home) = install_mock_jdk(dest_parent.path());

        assert!(java_home.join("bin").join("java").exists());
        let names: Vec<_> = fs::read_dir(dest_parent.path())
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        assert_eq!(names, [entry]);
    }

    /// An entry already holding the name - a concurrent run's, or one placed
    /// by hand - is not overwritten: the install fails and leaves it as it
    /// was.
    #[test]
    fn an_install_onto_an_occupied_entry_leaves_it_alone() {
        let dest_parent = tempdir().unwrap();
        let source_dir = tempdir().unwrap();
        create_extracted_jdk(source_dir.path(), "jdk-21.0.3+9");
        create_jdk_dir(dest_parent.path(), "21.0.3+9");
        let theirs = entry(dest_parent.path(), "21.0.3+9");
        fs::write(theirs.join("release"), "theirs").unwrap();

        let result = JdkStore::at(dest_parent.path()).install(
            &metadata("21.0.3+9"),
            source_dir.path(),
            &InstallUi::hidden("test"),
        );

        assert!(result.is_err(), "installed over an existing entry");
        assert_eq!(
            fs::read_to_string(theirs.join("release")).unwrap(),
            "theirs"
        );
    }

    /// The caller uses this as `JAVA_HOME`, so on macOS it is the bundle's
    /// `Contents/Home` and not the store entry the bundle was moved to.
    #[test]
    fn install_returns_the_java_home_not_the_store_entry() {
        let dest_parent = tempdir().unwrap();
        let (entry, java_home) = install_mock_jdk(dest_parent.path());

        assert_eq!(java_home, expected_java_home(&entry));
    }

    #[test]
    fn active_version_names_the_install_java_home_points_at() {
        let dir = tempdir().unwrap();
        create_jdk_dir(dir.path(), "17.0.2+8");
        create_jdk_dir(dir.path(), "21.0.3+9");
        let store = JdkStore::at(dir.path());
        let installed = store.list().unwrap();

        let active = entry(dir.path(), "21.0.3+9");
        assert_eq!(
            store.active_version(&installed, Some(&active)).as_deref(),
            Some("21.0.3+9")
        );
    }

    #[test]
    fn active_version_is_none_when_java_home_points_outside_the_store() {
        let dir = tempdir().unwrap();
        create_jdk_dir(dir.path(), "21.0.3+9");
        let store = JdkStore::at(dir.path());
        let installed = store.list().unwrap();

        let outside = Path::new("/usr/lib/jvm/java-21-openjdk");
        assert_eq!(store.active_version(&installed, Some(outside)), None);
    }
}
