use crate::CommandError;
use crate::adoptium::{AdoptiumClient, JdkMetadata};
use crate::extract;
use crate::request::{Request, Stream};
use crate::ui::{self, InstallUi};
use crate::version::compare;
use anyhow::{Context, anyhow, bail};
use std::cmp::Ordering;
use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};
use std::env;
use std::fs::File;
use std::path::{Path, PathBuf};

/// The ownership marker jlo *used* to write, inside the JDK directory.
///
/// Still honoured, never written. By construction it can only appear on an
/// install made before the macOS bundle was kept, so it is only ever found on
/// a flat directory - where a file costs nothing. Dropping the fallback would
/// silently orphan those installs: no marker means `jlo remove` declines, and
/// declining is the failure mode the user cannot see until they ask for a
/// deletion that does not happen.
const LEGACY_MARKER_FILE: &str = ".jlo-managed";

/// The ownership marker jlo writes, *beside* the JDK directory rather than in
/// it.
///
/// A macOS JDK directory is a signed bundle, and a file at its root unseals
/// it: `codesign --verify` and `spctl --assess` both report "unsealed contents
/// present in the bundle root" on an install that verifies without one. So the
/// bundle goes down exactly as Eclipse shipped it and the marker sits next to
/// it. `scan` filters to directories, so the file never appears as an install,
/// and neither `/usr/libexec/java_home` (which counts bundles) nor `IntelliJ`
/// (which scans subdirectories) sees it either.
fn sibling_marker(base: &Path, version: &str) -> PathBuf {
    base.join(format!("{version}.jlo-managed"))
}

/// Where a macOS JDK bundle keeps its java home, relative to the bundle
/// directory. Named once because two rules depend on it and they must not
/// drift: [`java_home_in`] hands this path out, [`owns`] refuses to delete it.
const BUNDLE_HOME: [&str; 2] = ["Contents", "Home"];

/// What a `jlo remove --superseded` run did, so the caller owns the presentation and
/// [`JdkStore::prune`] owns only the filesystem work.
#[derive(Debug, Default)]
pub(crate) struct PruneReport {
    /// `(name, removed version names)`, newest name first. Only versions
    /// actually deleted appear here. Keyed on the name rather than the major
    /// so a pre-release and the release it previews are reported apart, the
    /// way they are deleted apart.
    pub(crate) removed: Vec<(Request, Vec<String>)>,
    /// One message per JDK that could not be deleted.
    pub(crate) failures: Vec<String>,
    /// Installs without a `.jlo-managed` marker. Counted rather than listed:
    /// on a machine that also uses sdkman or Homebrew this is every other JDK,
    /// and a line each would bury the removals.
    pub(crate) skipped_unmanaged: usize,
    /// The superseded build left alone because `$JAVA_HOME` points at it. At
    /// most one, there being only one `$JAVA_HOME`. Named rather than counted
    /// for the same reason as [`RemoveReport::skipped_in_use`]: it is the one
    /// skip the user can act on, by switching shells and running the command
    /// again.
    pub(crate) skipped_in_use: Option<String>,
}

impl PruneReport {
    pub(crate) fn removed_count(&self) -> usize {
        self.removed.iter().map(|(_, v)| v.len()).sum()
    }
}

/// What a `jlo remove` run did. The counterpart to [`PruneReport`] for the
/// command that names its target instead of deriving it from a rule.
#[derive(Debug, Default)]
pub(crate) struct RemoveReport {
    /// The versions actually deleted, newest first.
    pub(crate) removed: Vec<String>,
    /// One message per JDK that could not be deleted.
    pub(crate) failures: Vec<String>,
    /// Versions matching a target that were left alone for want of a
    /// `.jlo-managed` marker. Listed rather than counted, unlike
    /// [`PruneReport::skipped_unmanaged`]: the user named these, so every
    /// install they did *not* get is worth a line.
    pub(crate) skipped_unmanaged: Vec<String>,
    /// Targets that matched nothing installed. Not a failure - the JDK is
    /// already absent, which is what was asked for - but worth saying, since
    /// it is usually a typo.
    pub(crate) not_installed: Vec<String>,
    /// The version left alone because `$JAVA_HOME` points at it. At most one,
    /// there being only one `$JAVA_HOME`. Unlike the two above this is worth
    /// a warning rather than a note: it is the one skip the user can act on,
    /// by switching shells and running the command again.
    pub(crate) skipped_in_use: Option<String>,
}

/// Why [`JdkStore::remove`] deleted nothing.
///
/// A typed refusal rather than an `anyhow::Error` because each variant owns
/// the advice line that belongs under it, and the caller must not have to
/// match on message text to find it. Every variant here means the store is
/// exactly as it was - each one fires only when there was nothing left to
/// remove, or (for [`RemoveError::InUse`]) before anything has been.
#[derive(Debug)]
pub(crate) enum RemoveError {
    /// *Nothing at all* was left to remove, and these versions are why:
    /// none of them matched an install. A version that matches nothing
    /// alongside one that does is not an error - see [`JdkStore::remove`] -
    /// so this fires only when the whole command would have done nothing.
    /// Every such version is named, not just the first.
    NotInstalled(Vec<String>),
    /// The only thing left to remove was the JDK `$JAVA_HOME` points at, and
    /// deleting that leaves the calling shell pointing at a path that no
    /// longer exists - the hazard that killed `jlo update --clean`. Like the
    /// other two, it is a skip when there is other work to do and an error
    /// only when there is not.
    InUse(String),
    /// Everything that matched lacks the `.jlo-managed` marker, so J'Lo did
    /// not install it and will not delete it. Like
    /// [`Self::NotInstalled`], this fires only when it leaves nothing to do.
    Unmanaged(Vec<String>),
    /// The install directory itself could not be read.
    Store(anyhow::Error),
}

impl RemoveError {
    /// The advice line that belongs under this refusal, if any.
    pub(crate) fn hint(&self) -> Option<String> {
        match self {
            Self::NotInstalled(_) => Some(
                "Nothing was removed. Run 'jlo list --offline' to see what is installed."
                    .to_string(),
            ),
            Self::InUse(_) => Some(
                "Nothing was removed. Switch the shell to another JDK first, \
                 e.g. 'jlo env 21', then remove it."
                    .to_string(),
            ),
            Self::Unmanaged(_) => Some(
                "Nothing was removed. J'Lo only deletes installs carrying its \
                 .jlo-managed marker; remove the directory by hand if you are sure."
                    .to_string(),
            ),
            Self::Store(_) => None,
        }
    }
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
            Self::Unmanaged(versions) => write!(
                f,
                "refusing to remove {}: not installed by jlo",
                versions.join(", ")
            ),
            // `{:#}` so the whole `anyhow` chain survives into the one place
            // that prints it, matching `ui::error!("{:#}", ...)` in `main`.
            Self::Store(e) => write!(f, "{e:#}"),
        }
    }
}

/// A JDK found in the install directory, identified by its semver directory name.
pub(crate) struct InstalledJdk {
    pub version: String,
    pub major: i64,
    /// Which of the major's two streams this build belongs to, read off its
    /// name: a pre-release is early access, anything else is released.
    pub stream: Stream,
    /// Whether the JDK carries the `.jlo-managed` marker, i.e. whether
    /// `jlo remove` is allowed to delete it, by either of its selectors.
    pub managed: bool,
}

impl InstalledJdk {
    /// The name this install answers to - what every "one build per ..." rule
    /// groups by.
    pub(crate) fn request(&self) -> Request {
        Request {
            major: self.major,
            stream: self.stream,
        }
    }
}

/// Whether `offered` is newer than every one of `builds` - the installs of
/// one name. The single rule behind both `jlo list` calling an offer an
/// `update` and `install`/`update` downloading it, so the listing and the
/// command cannot disagree about what counts as moving a name forward.
///
/// The catalogue can sit *behind* the store - an install that came from
/// somewhere else, or a major Adoptium has since rolled back - and following
/// it would be a downgrade. Unmanaged installs count too: an offer no newer
/// than one of them is not an improvement on what is already on disk. Two
/// spellings of one version (`v21.0.11+9`, `21.0.11+9`) compare equal, so
/// neither supersedes the other.
///
/// True when `builds` is empty: nothing installed is superseded by anything.
/// The listing does not call that an update, and checks for it itself.
pub(crate) fn supersedes_every_install(offered: &str, builds: &[&InstalledJdk]) -> bool {
    builds
        .iter()
        .all(|jdk| compare(offered, &jdk.version).is_ok_and(Ordering::is_gt))
}

/// The install cascade stage 3 picks: the newest *released* build at or above
/// the version floor.
///
/// GA only, because nobody asked for a pre-release: a machine that once tried
/// `28-ea` must not start answering a bare `jlo env` with a beta. The floor is
/// the grammar's own - a store of nothing but pre-8 JDKs is one every other
/// part of jlo would reject, so stage 3 walks past it rather than resolving to
/// a version that cannot be asked for.
///
/// Takes the list rather than reading the store, because `jlo current` has it
/// already and must not answer this question differently from the cascade.
/// `installed` is newest first, as [`JdkStore::list`] leaves it.
pub(crate) fn newest_ga(installed: &[InstalledJdk]) -> Option<&InstalledJdk> {
    installed
        .iter()
        .find(|jdk| jdk.stream == Stream::Ga && Request::parse(&jdk.major.to_string()).is_ok())
}

/// One directory found in the store, with everything a single walk can say
/// about it. Each caller applies its own notion of what counts as a JDK here,
/// which is why nothing is filtered out yet.
struct Candidate {
    path: PathBuf,
    /// The directory name, or `None` when it is not valid UTF-8.
    name: Option<String>,
    /// The name as a *request*: its major and its stream, or `None` when the
    /// name is not a semver - see [`is_jdk_version_dir`]. This is the key
    /// every "one build per ..." rule groups by - keyed on the major alone,
    /// a pre-release would supersede the released build it previews, because
    /// it sorts above it.
    request: Option<Request>,
    /// Whether the directory carries the `.jlo-managed` marker.
    managed: bool,
}

/// The directory J'Lo installs JDKs into, and everything it knows about what
/// lives there.
pub(crate) struct JdkStore {
    base: PathBuf,
}

impl JdkStore {
    /// The real store for this machine. The location is not configurable.
    pub(crate) fn discover() -> anyhow::Result<Self> {
        let home = env::home_dir().context("could not determine home directory")?;
        Ok(Self::at(base_dir_for(env::consts::OS, &home)))
    }

    /// A store rooted at an arbitrary directory. The test adapter.
    pub(crate) fn at(base: impl Into<PathBuf>) -> Self {
        Self { base: base.into() }
    }

    /// The install directory itself, for the two places that have to name it:
    /// PATH rewriting, which has to know which PATH entries J'Lo owns, and the
    /// empty `jlo list --offline` line, which says where it looked.
    pub(crate) fn base(&self) -> &Path {
        &self.base
    }

    /// Every JDK in the store whose directory name parses as a semver, newest
    /// first. A missing base directory is not an error - it just means nothing
    /// has been installed yet.
    pub(crate) fn list(&self) -> anyhow::Result<Vec<InstalledJdk>> {
        let mut candidates = match self.scan() {
            Ok(candidates) => candidates,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e).with_context(|| self.read_failure()),
        };

        candidates.retain(|candidate| candidate.name.as_deref().is_some_and(is_jdk_version_dir));
        sort_by_semver_desc(&mut candidates);

        Ok(candidates
            .into_iter()
            .filter_map(|candidate| {
                let request = candidate.request?;
                Some(InstalledJdk {
                    version: candidate.name?,
                    major: request.major,
                    stream: request.stream,
                    managed: candidate.managed,
                })
            })
            .collect())
    }

    /// The installed version `$JAVA_HOME` currently points at, if that is one
    /// of ours.
    ///
    /// Resolved here rather than in `ui` so the listing works on version
    /// names and never has to know where the store lives. Returns `None` when
    /// `$JAVA_HOME` is unset or points outside the store - a system JDK or
    /// one another tool manages - which the caller reports rather than hides.
    pub(crate) fn active_version(
        &self,
        installed: &[InstalledJdk],
        active_java_home: Option<&Path>,
    ) -> Option<String> {
        let active = active_java_home?;
        installed
            .iter()
            .find(|jdk| owns(&self.base.join(&jdk.version), active))
            .map(|jdk| jdk.version.clone())
    }

    /// The newest installed JDK answering to `request`, if any.
    ///
    /// Matched on the parsed name rather than on a name prefix: a prefix
    /// match makes `1` select `17`, and the only reason that is unreachable
    /// is the `>= 8` floor in [`Request::parse`], which is also what refuses
    /// `17.0` before it can ever mean "some 17.0.x". Matching the whole name
    /// rather than the major is what keeps a pre-release out of the answer to
    /// a GA request: it sorts above the build it previews, so a major-only
    /// filter would hand `jlo env 26` a beta.
    pub(crate) fn find_matching(&self, request: Request) -> Option<PathBuf> {
        let mut matching_versions = self.scan().ok()?;
        matching_versions.retain(|candidate| candidate.request == Some(request));

        sort_by_semver_desc(&mut matching_versions);

        matching_versions
            .into_iter()
            .next()
            .map(|candidate| java_home_in(&candidate.path))
    }

    /// The version name when `java_home` is one of ours still in the
    /// pre-bundle macOS layout, and `None` otherwise.
    ///
    /// Such an install works in every way jlo cares about - it resolves, it
    /// runs - but `/usr/libexec/java_home` cannot see it, which is the whole
    /// of what keeping the bundle bought. There is no migration (see
    /// [`java_home_in`]), so without a word from jlo the user would never
    /// learn that a JDK they already have is the one still missing out.
    ///
    /// The test is the shape, not the name: a bundle's java home is
    /// `<entry>/Contents/Home`, whose parent is `Contents`, so only a flat
    /// install has the store itself as its parent. Unmanaged installs are
    /// excluded deliberately - jlo did not put them there and cannot offer
    /// `jlo remove` as the fix, and README already says the bundle is the
    /// better shape to drop in by hand.
    /// Returns the version and its name, the name being what `jlo install`
    /// takes in the advice line - read off the install rather than reparsed
    /// from the directory name, which has already been parsed once to get
    /// here. The whole name, not the major: under a flat early-access install
    /// a major-only hint would advise installing the released stream.
    pub(crate) fn legacy_layout(&self, java_home: &Path) -> Option<(String, Request)> {
        if env::consts::OS != "macos" || java_home.parent() != Some(self.base.as_path()) {
            return None;
        }

        let name = java_home.file_name()?.to_str()?;
        self.list()
            .ok()?
            .into_iter()
            .find(|jdk| jdk.version == name && jdk.managed)
            .map(|jdk| {
                let request = jdk.request();
                (jdk.version, request)
            })
    }

    /// The name of the newest *released* JDK in the store, or `None` when
    /// nothing released is installed.
    ///
    /// Stage 3 of `resolve`'s version cascade: what a bare `jlo env` resolves to
    /// when no config anywhere names a version. "Newest" is by semver across
    /// every major, so a store holding 17.0.11 and 21.0.5 answers 21.
    ///
    /// An unreadable store reads as "nothing installed", matching
    /// [`Self::find_matching`]: both answer "is there one here", and neither
    /// is the place to fail over a directory that cannot be read.
    pub(crate) fn newest_ga_request(&self) -> Option<Request> {
        newest_ga(&self.list().ok()?).map(InstalledJdk::request)
    }

    /// The version names present in the store, ascending. A major with a
    /// build of each stream installed contributes both of its names.
    pub(crate) fn installed_requests(&self) -> anyhow::Result<Vec<Request>> {
        let requests: HashSet<Request> = self
            .scan_required()?
            .into_iter()
            .filter_map(|candidate| candidate.request)
            .collect();

        let mut requests: Vec<Request> = requests.into_iter().collect();
        requests.sort_unstable();
        Ok(requests)
    }

    /// How many installs `jlo remove --superseded` would remove: every managed JDK that is
    /// not the newest of its name.
    ///
    /// The read-only counterpart to [`Self::prune`], so `jlo install` and
    /// `jlo update` can point at `jlo remove --superseded` for the leftovers
    /// they did not touch - names this run did not move, or deletions that
    /// failed.
    pub(crate) fn superseded_count(&self) -> anyhow::Result<usize> {
        let mut newest: HashMap<Request, String> = HashMap::new();
        let mut superseded = 0;

        // `list` yields newest first, so the first managed JDK of a name is
        // the one `prune` keeps. What follows it is counted only when it is
        // *older* - "not the newest" would over-count, because two names can
        // spell one version and [`Self::prune`] leaves both of those alone.
        for jdk in self.list()?.into_iter().filter(|jdk| jdk.managed) {
            match newest.entry(jdk.request()) {
                Entry::Vacant(slot) => {
                    slot.insert(jdk.version);
                }
                Entry::Occupied(newest) => {
                    if is_older_than(&jdk.version, newest.get()) {
                        superseded += 1;
                    }
                }
            }
        }

        Ok(superseded)
    }

    /// Remove every managed JDK that is not the newest of its name.
    ///
    /// `active_java_home` is the directory `$JAVA_HOME` points at, if any, and
    /// it is skipped by exactly the rule [`Self::remove`] applies to a named
    /// target: deleting the JDK the calling shell is on leaves that shell
    /// pointing at a path that no longer exists - the hazard that killed `jlo
    /// update --clean`. The two selectors of one verb must not disagree about
    /// it. `jlo install` and `jlo update` may delete the live build only
    /// when the wrapper evaluates them, because then they move the shell
    /// first (see [`install_each`]); this verb is not evaluated by the
    /// wrapper and cannot.
    ///
    /// A skip, not a refusal: the other superseded builds still go. It is
    /// passed in rather than read here for the same reason as in
    /// [`Self::remove`] - so the guard is testable without mutating the
    /// process environment.
    pub(crate) fn prune(&self, active_java_home: Option<&Path>) -> anyhow::Result<PruneReport> {
        // Grouped by name, not by major: a pre-release sorts above the
        // release it previews, so a major-keyed group would make the released
        // build superseded by a beta of the same major.
        let mut installed_jdks: HashMap<Request, Vec<Candidate>> = HashMap::new();
        let mut report = PruneReport::default();

        for candidate in self.scan_required()? {
            let path = &candidate.path;
            if candidate.name.is_none() {
                crate::ui::warning!("ignoring directory with invalid name {path:?}");
                continue;
            }
            // The name is parsed before the marker is consulted, so
            // `skipped_unmanaged` counts only directories `jlo list` would
            // show. A vendor-named `IntelliJ` download (`temurin-21.0.1`) is not
            // an install jlo declined to touch - it is not an install jlo can
            // see at all, and counting it would report "left 1 install alone"
            // about something the listing never mentioned.
            let Some(request) = candidate.request else {
                // A staging directory is jlo's own and is expected to be
                // here; warning about it would put a line under every
                // `jlo remove --superseded` for the rest of the machine's
                // life. It is swept in `staging_dir` instead.
                if !is_staging_dir(candidate.name.as_deref()) {
                    crate::ui::warning!("ignoring non-semver directory {path:?}");
                }
                continue;
            };
            if !candidate.managed {
                // skip directories not managed by jlo
                report.skipped_unmanaged += 1;
                continue;
            }
            installed_jdks.entry(request).or_default().push(candidate);
        }

        // A `HashMap` hands back its keys in an arbitrary order, which made two runs
        // over the same directory print the names differently. Sort so the output
        // is stable and matches `jlo list` (newest major first); the derived
        // `Ord` also orders the two streams of one major stably.
        let mut requests: Vec<Request> = installed_jdks.keys().copied().collect();
        requests.sort_unstable_by(|a, b| b.cmp(a));

        for request in requests {
            let Some(candidates) = installed_jdks.get_mut(&request) else {
                continue;
            };
            sort_by_semver_desc(candidates);

            // Sorted newest first, so the head is the build to keep - but "not
            // the head" is not the same as "older". Two names can spell one
            // version (`21.0.11+9` and `v21.0.11+9`), and between those there
            // is nothing to choose, so deleting by position would be the same
            // coin toss the sort used to be. Only a strictly older build goes.
            let Some(newest) = candidates.first().and_then(|c| c.name.clone()) else {
                continue;
            };

            // Record what was *actually* deleted. Announcing the removals up front
            // meant a failure below turned the line above it into a false claim.
            let mut removed = Vec::new();
            for old_jdk in candidates
                .iter()
                .filter(|c| c.name.as_deref().is_some_and(|n| is_older_than(n, &newest)))
            {
                let name = old_jdk.name.as_deref().unwrap_or("unknown").to_string();
                let path = &old_jdk.path;
                // Both spellings of the entry are compared, as in
                // `Self::remove`: `owns` is deliberately not `java_home_in`,
                // so a bundle whose `Contents/Home` has gone unreadable is
                // still protected.
                if active_java_home.is_some_and(|active| owns(&self.base.join(&name), active)) {
                    report.skipped_in_use = Some(name);
                    continue;
                }
                match remove_install(&self.base, &name) {
                    Ok(()) => removed.push(name),
                    Err(e) => report
                        .failures
                        .push(format!("could not remove {path:?}: {e}")),
                }
            }

            if !removed.is_empty() {
                report.removed.push((request, removed));
            }
        }

        Ok(report)
    }

    /// The managed builds of `request` older than `newest` - the build
    /// `jlo install` or `jlo update` has just installed - newest first.
    ///
    /// One name, never its sibling stream: `newest` is compared only against
    /// builds of `request`, so a GA update never deletes a pre-release of the
    /// same major, nor the other way round. Unmanaged builds are left alone,
    /// as by every other deletion.
    fn superseded_by(&self, request: Request, newest: &str) -> anyhow::Result<Vec<Candidate>> {
        let mut superseded: Vec<Candidate> = self
            .scan_required()?
            .into_iter()
            .filter(|c| c.managed && c.request == Some(request))
            .filter(|c| c.name.as_deref().is_some_and(|n| is_older_than(n, newest)))
            .collect();
        sort_by_semver_desc(&mut superseded);
        Ok(superseded)
    }

    /// Delete the JDKs `targets` name: for each one, every installed build of
    /// a version name (`17`, `28-ea`), or the one exact build (`17.0.11+10`).
    ///
    /// The explicit counterpart to [`Self::prune`] - the targets the user
    /// named, rather than a set derived from a rule.
    ///
    /// One rule, three reasons: an install that cannot be removed is set
    /// aside with the reason why, and never stops the ones that can. The
    /// reasons are the three [`RemoveError`] variants -
    /// [`RemoveError::NotInstalled`] (the JDK is already absent, which is
    /// what was asked for), [`RemoveError::Unmanaged`] (J'Lo did not install
    /// it) and [`RemoveError::InUse`] (`$JAVA_HOME` points at it) -
    /// and each becomes an *error* only when it leaves nothing to remove at
    /// all, because a command told exactly what to delete must not report
    /// success having deleted nothing.
    ///
    /// Setting aside rather than refusing is the whole point. None of the
    /// three can delete anything, so aborting the other versions on their
    /// account protects nothing - it only makes the user retype the command
    /// once per problem. The guard the hazards actually need is "never delete
    /// this one", and skipping it is exactly that. `jlo update` has the same
    /// shape: it warns past a version it cannot use and gets on with the
    /// others.
    ///
    /// `active_java_home` is the directory `$JAVA_HOME` points at, if any. It
    /// is passed in rather than read here so the refusal is testable without
    /// mutating the process environment, the way [`Self::at`] keeps the
    /// install directory injectable.
    pub(crate) fn remove(
        &self,
        targets: &[String],
        active_java_home: Option<&Path>,
    ) -> Result<RemoveReport, RemoveError> {
        let installed = self.list().map_err(RemoveError::Store)?;

        // Resolved by index into `installed` so overlapping targets - `jlo
        // remove 17 17.0.2+8` names the same directory twice - select it
        // once. Deleting it twice would turn the second attempt into a
        // spurious "could not remove" line.
        let mut selected: Vec<usize> = Vec::new();
        let mut missing: Vec<String> = Vec::new();
        for target in targets {
            let matches = installed
                .iter()
                .enumerate()
                .filter(|(_, jdk)| matches_target(jdk, target))
                .map(|(index, _)| index);

            let before = selected.len();
            selected.extend(matches);
            if selected.len() == before && !missing.contains(target) {
                missing.push(target.clone());
            }
        }

        // `installed` is newest first, so ascending indices report the
        // removals in the order `jlo list --offline` shows them, whatever
        // order the targets were given in.
        selected.sort_unstable();
        selected.dedup();

        let matching: Vec<&InstalledJdk> = selected.into_iter().map(|i| &installed[i]).collect();

        // Set aside before the marker check, so a live install that is also
        // unmanaged is reported as live: that is the one the user can act on.
        let (in_use, removable): (Vec<_>, Vec<_>) = match active_java_home {
            Some(active) => matching
                .into_iter()
                .partition(|jdk| owns(&self.base.join(&jdk.version), active)),
            None => (Vec::new(), matching),
        };
        let in_use = in_use.first().map(|jdk| jdk.version.clone());

        let (managed, unmanaged): (Vec<_>, Vec<_>) =
            removable.into_iter().partition(|jdk| jdk.managed);

        let unmanaged: Vec<String> = unmanaged
            .into_iter()
            .map(|jdk| jdk.version.clone())
            .collect();

        // Nothing left to delete. Exiting 0 here would report success on a
        // command that did not do what it was asked, so say which of the
        // three reasons it was, most actionable first: the live JDK can be
        // had by switching shells, the unmanaged one is J'Lo declining, and a
        // version that matched nothing is simply not there.
        if managed.is_empty() {
            return Err(match (in_use, unmanaged.is_empty()) {
                (Some(version), _) => RemoveError::InUse(version),
                (None, false) => RemoveError::Unmanaged(unmanaged),
                (None, true) => RemoveError::NotInstalled(missing),
            });
        }

        let mut report = RemoveReport {
            skipped_unmanaged: unmanaged,
            not_installed: missing,
            skipped_in_use: in_use,
            ..RemoveReport::default()
        };

        // `list` yields newest first, so the removals are reported that way
        // too - the same order as `jlo list --offline`.
        for jdk in managed {
            let path = self.base.join(&jdk.version);
            match remove_install(&self.base, &jdk.version) {
                Ok(()) => report.removed.push(jdk.version.clone()),
                Err(e) => report
                    .failures
                    .push(format!("could not remove {path:?}: {e}")),
            }
        }

        Ok(report)
    }

    /// Move an extracted JDK from `source_dir` into the store and mark it
    /// managed. Returns the installed path.
    pub(crate) fn install(
        &self,
        metadata: &JdkMetadata,
        source_dir: &Path,
        ui: &InstallUi,
    ) -> anyhow::Result<PathBuf> {
        let dest_dir = self.base.join(&metadata.semver);

        let extracted_jdk_path =
            find_jdk_path(source_dir).context("could not find the extracted JDK directory")?;

        // Create destination directory
        ui.start_install();
        std::fs::create_dir_all(
            dest_dir
                .parent()
                .context("destination directory has no parent")?,
        )
        .context("could not create destination directory")?;

        // Move extracted JDK to final location
        std::fs::rename(extracted_jdk_path, &dest_dir)
            .context("could not move JDK to destination")?;

        // touch a file to indicate that this directory is managed by jlo -
        // beside it, never inside it. See [`sibling_marker`].
        std::fs::File::create(sibling_marker(&self.base, &metadata.semver))
            .context("could not create marker file")?;

        // The java home, not the entry: every caller uses this as JAVA_HOME,
        // and on macOS the two are no longer the same directory.
        Ok(java_home_in(&dest_dir))
    }

    /// Every directory in the base directory, paired with what a single walk
    /// can tell about it. The raw `io::Error` survives so each caller can keep
    /// its own answer to "what does a missing base directory mean".
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
                let parsed = name
                    .as_deref()
                    .and_then(|name| crate::version::parse(name).ok());
                // A major that does not fit an `i64` is not a JDK; the rest
                // of the crate counts majors in `i64` because that is what
                // the Adoptium API hands back.
                let major = parsed
                    .as_ref()
                    .and_then(|semver| i64::try_from(semver.major).ok());
                let request = parsed.as_ref().zip(major).map(|(semver, major)| Request {
                    major,
                    stream: crate::request::stream_of(semver),
                });
                // `is_file`, not `exists`: a *directory* named
                // `21.0.3+9.jlo-managed` is itself a store entry with a
                // semver-shaped name, and letting it confer ownership on its
                // neighbour `21.0.3+9` would put a JDK jlo never installed
                // within reach of `jlo remove`.
                let managed = name
                    .as_deref()
                    .is_some_and(|name| sibling_marker(&self.base, name).is_file())
                    || path.join(LEGACY_MARKER_FILE).is_file();
                Candidate {
                    path,
                    name,
                    request,
                    managed,
                }
            })
            .collect())
    }

    /// [`Self::scan`] for the callers that treat an unreadable - or absent -
    /// base directory as a failure.
    fn scan_required(&self) -> anyhow::Result<Vec<Candidate>> {
        self.scan().with_context(|| self.read_failure())
    }

    fn read_failure(&self) -> String {
        let base = &self.base;
        format!("could not read JDK base directory {base:?}")
    }
}

/// What [`install_each`] did, handed back whole rather than as a `Result`: a
/// failure on the third name must not hide that the first one deleted the
/// build the shell was on.
#[derive(Debug, Default)]
pub(crate) struct InstallRun {
    /// `(name, removed version names)`, one entry per name whose superseded
    /// builds were deleted, in processing order.
    pub(crate) replaced: Vec<(Request, Vec<String>)>,
    /// One message per superseded build that could not be deleted.
    pub(crate) failures: Vec<String>,
    /// The java home of the new build, when the build `$JAVA_HOME` pointed
    /// at was among those deleted and the shell was told to follow.
    pub(crate) repointed: Option<PathBuf>,
    /// The superseded build left in place because `$JAVA_HOME` points at it
    /// and the shell could not be told to follow.
    pub(crate) kept_active: Option<String>,
    /// What stopped the run: a lookup that failed, which stops it before
    /// anything is downloaded; no name left that Adoptium offers; a download
    /// or install, after which the names that follow were not attempted; or
    /// the shell's payload that could not be written, after which nothing is
    /// deleted.
    pub(crate) error: Option<CommandError>,
}

impl InstallRun {
    pub(crate) fn removed_count(&self) -> usize {
        self.replaced.iter().map(|(_, v)| v.len()).sum()
    }
}

/// The one operation behind both `install` and `update`: per name, download
/// the latest build if it supersedes every install of the name, then delete
/// the builds of that name it supersedes. A name moves forward, never back.
/// The two verbs differ only in how they arrive at this set of names. The
/// on-demand install behind `env`, `home` and `exec` does not come through
/// here: it only fills a missing name, so it has nothing to supersede.
///
/// `active` is `$JAVA_HOME` of the calling shell, if set. Its build is
/// deleted only when `shell_follows`; otherwise it is kept and named in
/// [`InstallRun::kept_active`]. `emit` is called once, after every download
/// and before any deletion, with the java home the shell has to move to, if
/// any: a run killed before it returns has deleted nothing, and one killed
/// after has already told the shell where to go. When it fails, nothing is
/// deleted.
pub(crate) fn install_each(
    client: &AdoptiumClient,
    store: &JdkStore,
    requests: HashSet<Request>,
    active: Option<&Path>,
    shell_follows: bool,
    emit: impl FnOnce(Option<&Path>) -> anyhow::Result<()>,
) -> InstallRun {
    // Sorted for a stable processing order, rather than whatever order the
    // hash set happens to iterate in.
    let mut requests: Vec<Request> = requests.into_iter().collect();
    requests.sort_unstable();

    let mut run = InstallRun::default();
    let offered = match resolve_offered(client, &requests) {
        Ok(offered) => offered,
        Err(e) => {
            run.error = Some(e);
            return run;
        }
    };

    let mut installed = Vec::new();
    for (request, metadata) in offered {
        match install_latest(client, store, request, metadata) {
            Ok(Some(build)) => installed.push((request, build)),
            Ok(None) => {}
            Err(e) => {
                // Stop, as a failed download always has - but keep what the
                // names before it did.
                run.error = Some(e.into());
                break;
            }
        }
    }

    let mut repointed = None;
    let mut doomed = Vec::new();
    for (request, (version, java_home)) in &installed {
        let superseded = store.superseded_by(*request, version).map(|mut builds| {
            let live = builds
                .iter()
                .position(|old| active.is_some_and(|active| owns(&old.path, active)));
            if let Some(at) = live {
                if shell_follows {
                    repointed = Some(java_home.clone());
                } else {
                    run.kept_active = builds.remove(at).name;
                }
            }
            builds
        });
        doomed.push((*request, superseded));
    }

    if let Err(e) = emit(repointed.as_deref()) {
        run.error.get_or_insert(e.into());
        return run;
    }
    run.repointed = repointed;

    for (request, superseded) in doomed {
        replace(store, request, superseded, &mut run);
    }

    if run.error.is_some() {
        return run;
    }

    // Only when a pre-release name is in play, and then once for the whole
    // run: it is one document, the same for every major. A failed lookup is
    // swallowed for the reason `count_superseded` swallows its own - a note is
    // not worth failing an otherwise successful command over.
    if requests.iter().any(|request| request.is_ea()) {
        let released = client.released_majors().unwrap_or_default();
        ui::announce_released_ea(&requests, &released);
    }

    // Every name this run moved has had its superseded builds deleted, so
    // what is counted here are leftovers from before this run, or builds a
    // deletion failed on. A kept live build has a hint of its own.
    if run.kept_active.is_none()
        && let Some(hint) = ui::superseded_hint(!installed.is_empty(), count_superseded(store))
    {
        ui::hint!("{hint}");
    }

    run
}

/// Delete the builds of `request` its new build has superseded, and record
/// the outcome in `run`.
fn replace(
    store: &JdkStore,
    request: Request,
    superseded: anyhow::Result<Vec<Candidate>>,
    run: &mut InstallRun,
) {
    let mut removed = Vec::new();
    let mut failures = Vec::new();
    match superseded {
        Err(e) => failures.push(format!("{e:#}")),
        Ok(builds) => {
            for old in builds {
                // Filtered on the name in `superseded_by`, so it is present.
                let Some(name) = old.name else { continue };
                match remove_install(&store.base, &name) {
                    Ok(()) => removed.push(name),
                    Err(e) => failures.push(format!("could not remove {:?}: {e}", old.path)),
                }
            }
        }
    }
    ui::replaced(request, &removed, &failures);
    if !removed.is_empty() {
        run.replaced.push((request, removed));
    }
    run.failures.extend(failures);
}

/// How many installs `jlo remove --superseded` would remove, or 0 if that
/// cannot be determined. A hint is not worth failing an otherwise successful
/// run, so an unreadable JDK directory just means no hint.
fn count_superseded(store: &JdkStore) -> usize {
    store.superseded_count().unwrap_or(0)
}

/// The first phase of [`install_each`]: what Adoptium offers for every name,
/// asked before anything is downloaded or deleted.
///
/// A name it does not offer is skipped with a warning, the rule `remove` and
/// `requested_versions` already follow: it cannot be acted on, so stopping
/// the others on its account protects nothing - and names are processed
/// sorted, so `jlo install 8 21` on Apple silicon would otherwise install
/// nothing. It is an error only when it leaves nothing at all, and then the
/// store is untouched.
///
/// A lookup that *fails* - network, HTTP, a response that does not parse -
/// is different: it says nothing about the name, so it stops the run, and
/// asking every name first is what lets it stop before anything changed.
/// No extra request either way: this is the lookup the download needs.
fn resolve_offered(
    client: &AdoptiumClient,
    requests: &[Request],
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
    // Said only when something is left to do: with nothing left, the error
    // above names every skipped name in one line instead.
    for &request in &not_offered {
        ui::skipping_not_offered(request);
    }
    if !not_offered.is_empty() {
        ui::hint!("{}", ui::NOT_OFFERED_HINT);
    }
    Ok(offered)
}

/// The version and java home of the build installed, or `None` when the name
/// was already current - so the caller can tell a real update from a no-op.
///
/// Downloads only an offer that supersedes every install of the name. Asking
/// "is this exact build on disk?" instead would follow a catalogue that sits
/// behind the store: the older build would land beside the newer one, which
/// it does not supersede, so nothing would be replaced and the name would
/// hold two builds.
fn install_latest(
    client: &AdoptiumClient,
    store: &JdkStore,
    request: Request,
    jdk_metadata: JdkMetadata,
) -> anyhow::Result<Option<(String, PathBuf)>> {
    let installed = store.list()?;
    // Only this name: a pre-release sorts above the release it previews, so
    // measured against `21-ea` an offer for `21` would never be newer.
    let builds: Vec<&InstalledJdk> = installed
        .iter()
        .filter(|jdk| jdk.request() == request)
        .collect();

    match builds.first() {
        // `list` is newest first.
        Some(newest) if !supersedes_every_install(&jdk_metadata.semver, &builds) => {
            let older = compare(&jdk_metadata.semver, &newest.version).is_ok_and(Ordering::is_lt);
            ui::up_to_date(
                &request.to_string(),
                &newest.version,
                older.then_some(jdk_metadata.semver.as_str()),
            );
            Ok(None)
        }
        _ => {
            let java_home =
                install_jdk(client, store, &jdk_metadata).context("could not install JDK")?;
            Ok(Some((jdk_metadata.semver, java_home)))
        }
    }
}

pub(crate) fn install_jdk(
    client: &AdoptiumClient,
    store: &JdkStore,
    jdk_metadata: &JdkMetadata,
) -> anyhow::Result<PathBuf> {
    // One progress region spans all three phases, so the terminal shows a
    // single line that changes rather than three bars stacking up.
    let ui = InstallUi::new(&jdk_metadata.semver);

    match install_jdk_inner(client, store, jdk_metadata, &ui) {
        Ok(dest_dir) => {
            ui.finish(&dest_dir);
            Ok(dest_dir)
        }
        Err(e) => {
            // Clear the live region first: a half-drawn bar above the error
            // only gets in the way of reading it.
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

    // Extract JDK to temp dir
    extract::extract(&temp_file, temp_dir.path(), ui)?;

    let dest_dir = store.install(jdk_metadata, temp_dir.path(), ui)?;

    temp_dir.close().unwrap_or_else(|err| {
        ui::warning!("could not delete temporary directory: {err}");
    });

    Ok(dest_dir)
}

/// Where an install is downloaded and unpacked: inside the store, not in
/// `$TMPDIR`.
///
/// The last step of an install is a `rename` into the store, and `rename` is
/// only atomic - only *possible* - within one filesystem. `$TMPDIR` is a
/// different one routinely: every distribution that mounts `/tmp` as tmpfs
/// (Fedora, Arch, Debian 13) turns every install into an `EXDEV` failure
/// raised after the whole archive has been downloaded and unpacked.
/// `install.rs::write_atomic` stages beside its target for exactly this
/// reason; the store had never been given the same treatment.
///
/// A sibling of the installs, and one `scan` passes over: its name does not
/// parse as a version, so an interrupted install leaves a `.tmpXXXXXX` the
/// listing and both `remove` selectors ignore, rather than a half-moved JDK.
fn staging_dir(store: &JdkStore) -> anyhow::Result<tempfile::TempDir> {
    std::fs::create_dir_all(store.base())
        .with_context(|| format!("could not create {}", store.base().display()))?;

    // An install killed with Ctrl-C runs no destructor, so its staging
    // directory survives - holding the tarball and the unpacked JDK, half a
    // gigabyte of it, in the user's JDK directory rather than in `$TMPDIR`
    // where the system would eventually clear it. Nothing else will ever
    // remove it, so the next install does, before adding one of its own.
    sweep_stale_staging(store.base());

    tempfile::tempdir_in(store.base())
        .context("could not create a staging directory in the JDK install directory")
}

/// The prefix `tempfile` gives the directories [`staging_dir`] makes. A
/// leading dot is what keeps them out of the listing: `version::parse` refuses
/// it, so `scan` drops them the way it drops any other non-version name.
const STAGING_PREFIX: &str = ".tmp";

fn is_staging_dir(name: Option<&str>) -> bool {
    name.is_some_and(|name| name.starts_with(STAGING_PREFIX))
}

/// Delete staging directories left by an earlier, interrupted install.
///
/// Best effort in both directions: a failure is not worth a word (the install
/// that follows is what the user asked for, and this is housekeeping), and a
/// staging directory belonging to an install running *right now* is left
/// alone - it is in use, so removing its contents would break a command that
/// is working. There is no pid in the name to test, so "in use" is read as
/// "modified in the last hour", which is far longer than any install takes.
fn sweep_stale_staging(base: &Path) {
    let Ok(entries) = std::fs::read_dir(base) else {
        return;
    };
    for entry in entries.flatten() {
        if !is_staging_dir(entry.file_name().to_str()) {
            continue;
        }
        let recently_touched = entry
            .metadata()
            .and_then(|m| m.modified())
            .and_then(|t| t.elapsed().map_err(|_| std::io::ErrorKind::Other.into()))
            .is_ok_and(|age| age < std::time::Duration::from_hours(1));
        if !recently_touched {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

/// JDK install location, matching `IntelliJ` IDEA's layout so both tools see the
/// same JDKs. Split out from [`JdkStore::discover`] so every platform is
/// testable from any host.
fn base_dir_for(os: &str, home: &Path) -> PathBuf {
    match os {
        "macos" => home.join("Library/Java/JavaVirtualMachines"),
        _ => home.join(".jdks"),
    }
}

/// Whether `version` is superseded by `newest`, the build of its name that
/// [`JdkStore::prune`] keeps. The single definition behind both `prune` and
/// [`JdkStore::superseded_count`], so the hint that offers the deletion and
/// the deletion itself can never disagree about how many there are.
///
/// A name that does not parse never reaches here: both callers filter on the
/// parsed name first.
fn is_older_than(version: &str, newest: &str) -> bool {
    compare(version, newest).is_ok_and(Ordering::is_lt)
}

fn sort_by_semver_desc(candidates: &mut [Candidate]) {
    candidates.sort_by(|a, b| {
        let a_str = a.name.as_deref().unwrap_or("");
        let b_str = b.name.as_deref().unwrap_or("");
        compare(b_str, a_str).unwrap_or(Ordering::Equal)
    });
}

/// `'a'`, `'a' or 'b'`, `'a', 'b' or 'c'` - so a refusal naming several
/// versions reads as a sentence rather than as a dumped vector.
pub(crate) fn quoted_list(items: &[String]) -> String {
    let quoted: Vec<String> = items.iter().map(|item| format!("'{item}'")).collect();
    match quoted.split_last() {
        None => String::new(),
        Some((last, [])) => last.clone(),
        Some((last, rest)) => format!("{} or {last}", rest.join(", ")),
    }
}

/// Whether `target` names this install: a version name (`17`, `28-ea`)
/// selects every build of that name, an exact directory name selects one.
///
/// The name, not the major: `jlo remove 26` must leave `26-ea` alone, for the
/// same reason `jlo env 26` must not resolve to it. Anything that is not a
/// name falls through to the exact spelling, which is how `17.0.11+10` still
/// works - and why `17.0` still matches nothing, a range being what `.jlorc`
/// deliberately does not have.
fn matches_target(jdk: &InstalledJdk, target: &str) -> bool {
    match Request::parse(target) {
        Ok(request) => jdk.request() == request,
        Err(_) => jdk.version == target,
    }
}

/// Whether two paths name the same directory.
///
/// Canonicalised when both resolve, so a trailing slash or a symlinked home
/// does not let a live JDK slip past the `$JAVA_HOME` refusal. The literal
/// comparison comes first and stands alone as the fallback: a `$JAVA_HOME`
/// pointing at a path that no longer exists cannot be canonicalised, and that
/// must not silently turn the refusal off.
fn same_dir(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// Delete an install and the marker that claims it.
///
/// One function because the two must not drift, and **the marker goes first**.
/// Either order can be interrupted; the question is which half is safe to be
/// left with.
///
/// A marker outliving its directory is the dangerous half. Nothing can clean
/// it up - `scan` walks directories, so jlo cannot even see it - and it claims
/// the next thing to appear under that name. A user who then drops a JDK of
/// their own into `<store>/<version>` has it read as jlo's, and `jlo remove`
/// deletes a JDK jlo never installed. That is the one thing CONTEXT.md's
/// robustness order forbids outright.
///
/// A directory outliving its marker is the safe half: the install reads as
/// unmanaged, jlo declines to touch it, and the user removes it by hand. An
/// orphan the user can delete beats a trap that deletes for them.
///
/// A legacy in-directory marker needs no attention: it goes with the
/// directory it lives in.
fn remove_install(base: &Path, version: &str) -> std::io::Result<()> {
    match std::fs::remove_file(sibling_marker(base, version)) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e),
        _ => {}
    }
    std::fs::remove_dir_all(base.join(version))
}

/// The java home inside a store entry: `Contents/Home` for a macOS JDK
/// bundle, the entry itself for a flat install. This is the value `jlo home`,
/// `jlo env` and `jlo exec` hand out.
///
/// Probed rather than selected on `env::consts::OS`, because one store holds
/// both shapes at once: installs made by jlo versions that unwrapped the
/// bundle sit beside bundles, and a hand-placed JDK (README, "Using a JDK
/// J'Lo Did Not Install") may arrive as either.
///
/// The probe is `bin/java`, not the directory: a `Contents/Home` that cannot
/// run Java is not a java home, whatever its name, and requiring the launcher
/// is also what keeps a Linux JDK that happens to carry a `Contents/Home` from
/// being read as a bundle.
fn java_home_in(dir: &Path) -> PathBuf {
    let bundled = dir.join(BUNDLE_HOME[0]).join(BUNDLE_HOME[1]);
    if bundled.join("bin").join("java").exists() {
        bundled
    } else {
        dir.to_path_buf()
    }
}

/// Whether `active` - a live `$JAVA_HOME` - names the store entry `dir`, by
/// either of the two spellings a JDK directory has: the entry itself (a flat
/// install) or its `Contents/Home` (a macOS bundle).
///
/// Deliberately *not* [`java_home_in`]: this is the guard that stops `jlo
/// remove` deleting the JDK the calling shell is using, and a guard that asks
/// the filesystem what shape a directory is can be switched off by a directory
/// that cannot be stat'd. Both spellings are compared unconditionally instead,
/// so a bundle whose `Contents/Home` has gone unreadable is still protected.
///
/// The same rule answers `jlo current`: a `$JAVA_HOME` pointing at a bundle
/// root resolves to its version rather than falling through to "that install
/// is no longer there".
fn owns(dir: &Path, active: &Path) -> bool {
    same_dir(dir, active) || same_dir(&dir.join(BUNDLE_HOME[0]).join(BUNDLE_HOME[1]), active)
}

/// A directory counts as a JDK when its name parses as a version. That is what
/// jlo names its installs, and it is what keeps a hand-placed `temurin-21.0.5`
/// out of the listing.
fn is_jdk_version_dir(name: &str) -> bool {
    crate::version::parse(name).is_ok()
}

/// What to move into the store: the root of the extracted archive.
///
/// On macOS that root is a JDK bundle, and the whole of it is kept - the
/// `Contents/Info.plist` beside `Contents/Home` is what makes
/// `/usr/libexec/java_home` (and everything that shells out to it: Maven
/// Toolchains' macOS discovery, some Gradle toolchain detectors,
/// `/usr/bin/java`) able to see the install at all. Unwrapping it to the java
/// home, which is what jlo used to do, left those tools reporting no Java
/// runtime on a machine with five JDKs on it.
///
/// Found by looking, not by name. The archive's own top-level directory is the
/// fact; `release_name` is an API label that happens to match it for released
/// builds (`jdk-21.0.12+101`) and does not for early-access ones - Adoptium
/// labels the 28 EA build `jdk-28+16-ea-beta` and ships an archive that unpacks
/// into `jdk-28+16`. Joining the label was only ever right by luck, and the
/// luck ran out at the last step of an install that had already downloaded the
/// whole archive.
///
/// The downloaded archive sits in this directory too, hence the filter to
/// directories; the `java` launcher is what tells the extracted tree from
/// anything else, one level in on a macOS bundle. Probed via [`java_home_in`]
/// rather than selected on `env::consts::OS`, for the reason stated there.
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
    use std::fs;
    use tempfile::tempdir;

    fn request(name: &str) -> Request {
        Request::parse(name).expect("the fixture names a valid version")
    }

    fn create_jdk_dir(base: &Path, version: &str, managed: bool) {
        let dir = base.join(version);
        fs::create_dir_all(dir.join("bin")).unwrap();
        // Create a fake java binary
        fs::write(dir.join("bin").join("java"), "").unwrap();
        if managed {
            fs::File::create(sibling_marker(base, version)).unwrap();
        }
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
    fn create_bundle_jdk_dir(base: &Path, version: &str, managed: bool) -> PathBuf {
        let entry = base.join(version);
        let java_home = entry.join("Contents").join("Home");
        fs::create_dir_all(java_home.join("bin")).unwrap();
        fs::write(java_home.join("bin").join("java"), "").unwrap();
        if managed {
            fs::File::create(sibling_marker(base, version)).unwrap();
        }
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

    // -- base_dir_for --

    #[test]
    fn base_dir_matches_intellij_layout_on_macos() {
        let home = Path::new("/Users/u");
        assert_eq!(
            base_dir_for("macos", home),
            home.join("Library/Java/JavaVirtualMachines")
        );
    }

    #[test]
    fn base_dir_matches_intellij_layout_on_linux() {
        let home = Path::new("/home/u");
        assert_eq!(base_dir_for("linux", home), home.join(".jdks"));
    }

    // -- legacy_layout --

    /// The platform guard, which is load-bearing in a way the expression does
    /// not show. On Linux *every* install is flat and its parent *is* the
    /// store, so the second condition never rejects anything: drop or reorder
    /// the `macos` check and every Linux user gets the warning on every `jlo
    /// home`, for a layout that is correct there. Asserted per platform rather
    /// than on a `cfg!` constant - the behaviour is what must not change.
    #[test]
    fn only_macos_calls_a_flat_install_a_legacy_layout() {
        let dir = tempdir().unwrap();
        create_jdk_dir(dir.path(), "21.0.3+9", true);
        let store = JdkStore::at(dir.path());

        let verdict = store.legacy_layout(&dir.path().join("21.0.3+9"));

        if cfg!(target_os = "macos") {
            assert_eq!(verdict, Some(("21.0.3+9".to_string(), request("21"))));
        } else {
            assert_eq!(verdict, None, "a flat install is the norm off macOS");
        }
    }

    /// An install jlo did not make is not one jlo can offer to reinstall, so
    /// it is left alone. README already says the bundle is the better shape
    /// to drop in by hand.
    #[test]
    fn an_unmanaged_flat_install_is_not_warned_about() {
        let dir = tempdir().unwrap();
        create_jdk_dir(dir.path(), "21.0.3+9", false);

        let verdict = JdkStore::at(dir.path()).legacy_layout(&dir.path().join("21.0.3+9"));
        assert_eq!(verdict, None);
    }

    /// A bundle is the current layout, so it is never the thing being warned
    /// about - its java home is one level in, and the guard turns on exactly
    /// that difference.
    #[test]
    fn a_bundle_is_not_a_legacy_layout() {
        let dir = tempdir().unwrap();
        let java_home = create_bundle_jdk_dir(dir.path(), "21.0.3+9", true);

        assert_eq!(JdkStore::at(dir.path()).legacy_layout(&java_home), None);
    }

    // -- the two marker spellings --

    /// An install made before the marker moved out of the JDK directory is
    /// still jlo's. Dropping this fallback would not break loudly: the
    /// install would simply read as unmanaged and `jlo remove` would decline
    /// to touch it, which the user finds out only when a deletion silently
    /// does nothing.
    #[test]
    fn a_legacy_in_directory_marker_still_means_managed() {
        let dir = tempdir().unwrap();
        create_jdk_dir(dir.path(), "21.0.3+9", false);
        fs::File::create(dir.path().join("21.0.3+9").join(LEGACY_MARKER_FILE)).unwrap();

        let installed = JdkStore::at(dir.path()).list().unwrap();
        // The count and the name are asserted too: "every listed JDK is
        // managed" is vacuously true of a listing that dropped the fixture.
        assert_eq!(installed.len(), 1);
        assert_eq!(installed[0].version, "21.0.3+9");
        assert!(installed[0].managed, "legacy marker ignored");
    }

    /// The marker is a file, and `scan` walks directories, so it must not be
    /// mistaken for an install of its own - which would put a phantom row in
    /// `jlo list` named after a real JDK.
    #[test]
    fn the_sibling_marker_is_not_itself_an_install() {
        let dir = tempdir().unwrap();
        create_jdk_dir(dir.path(), "21.0.3+9", true);

        let installed = JdkStore::at(dir.path()).list().unwrap();
        assert_eq!(installed.len(), 1);
        assert_eq!(installed[0].version, "21.0.3+9");
    }

    /// A marker outliving its install would claim the next install of that
    /// version before jlo had written anything - so an unmanaged JDK the user
    /// dropped in by hand under a name jlo once used would read as jlo's, and
    /// `jlo remove` would delete it.
    #[test]
    fn removing_an_install_takes_its_marker_with_it() {
        let dir = tempdir().unwrap();
        create_jdk_dir(dir.path(), "21.0.3+9", true);

        JdkStore::at(dir.path())
            .remove(&["21".to_string()], None)
            .expect("it is managed and not in use");

        assert!(!sibling_marker(dir.path(), "21.0.3+9").exists());
        assert!(!dir.path().join("21.0.3+9").exists());
    }

    /// The same rule on the path that deletes by rule rather than by name.
    #[test]
    fn pruning_takes_the_markers_of_what_it_removed() {
        let dir = tempdir().unwrap();
        create_jdk_dir(dir.path(), "21.0.1+12", true);
        create_jdk_dir(dir.path(), "21.0.3+9", true);

        let report = JdkStore::at(dir.path()).prune(None).unwrap();

        assert_eq!(report.removed_count(), 1);
        assert!(!sibling_marker(dir.path(), "21.0.1+12").exists());
        assert!(sibling_marker(dir.path(), "21.0.3+9").exists());
    }

    /// A *directory* whose name happens to end in `.jlo-managed` is an entry
    /// like any other, and must not confer ownership on the entry it appears
    /// to name - that would put a JDK jlo never installed within reach of
    /// `jlo remove`, which is the one thing the marker exists to prevent.
    #[test]
    fn a_directory_named_like_a_marker_does_not_claim_its_neighbour() {
        let dir = tempdir().unwrap();
        create_jdk_dir(dir.path(), "21.0.3+9", false);
        fs::create_dir_all(dir.path().join("21.0.3+9.jlo-managed")).unwrap();

        let installed = JdkStore::at(dir.path()).list().unwrap();
        let jdk = installed
            .iter()
            .find(|jdk| jdk.version == "21.0.3+9")
            .expect("the JDK is listed");
        assert!(
            !jdk.managed,
            "a directory was accepted as an ownership marker"
        );
    }

    /// The order inside [`remove_install`], pinned from the outside: after a
    /// removal whose directory deletion fails, the install must read as
    /// *unmanaged* rather than as still-owned. A marker outliving its
    /// directory cannot be cleaned up - `scan` walks directories - and would
    /// claim whatever the user next puts under that name.
    ///
    /// The failure is staged by making the entry a *file*, which
    /// `remove_dir_all` refuses: the closest a test can get to an interruption
    /// without racing one.
    #[test]
    fn a_failed_removal_leaves_the_install_unowned_not_claimed() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("21.0.3+9"), "not a directory").unwrap();
        fs::File::create(sibling_marker(dir.path(), "21.0.3+9")).unwrap();

        remove_install(dir.path(), "21.0.3+9").expect_err("the entry is not a directory");

        assert!(
            !sibling_marker(dir.path(), "21.0.3+9").exists(),
            "the marker outlived the removal and would claim the next install"
        );
    }

    // -- the two shapes a store entry can have --
    //
    // One store holds both: a bundle jlo installed, a flat directory an older
    // jlo left behind, and a hand-placed JDK in either shape. Nothing here is
    // gated on the host platform, because the rules are about the directory,
    // not about the machine reading it.

    /// The whole point of keeping the bundle: what jlo hands out as
    /// `JAVA_HOME` has to be the java home inside it, not the bundle.
    #[test]
    fn find_matching_answers_a_bundle_with_its_contents_home() {
        let dir = tempdir().unwrap();
        let java_home = create_bundle_jdk_dir(dir.path(), "21.0.3+9", true);

        assert_eq!(
            JdkStore::at(dir.path()).find_matching(request("21")),
            Some(java_home)
        );
    }

    /// The other half of the same rule. An install made before jlo kept the
    /// bundle has no `Contents/Home`, and must keep resolving to itself
    /// rather than to a path that is not there - there is no migration step.
    #[test]
    fn find_matching_answers_a_flat_install_with_itself() {
        let dir = tempdir().unwrap();
        create_jdk_dir(dir.path(), "21.0.3+9", true);

        assert_eq!(
            JdkStore::at(dir.path()).find_matching(request("21")),
            Some(dir.path().join("21.0.3+9"))
        );
    }

    /// A directory named `Contents/Home` that cannot run Java is not a java
    /// home. The launcher is the probe, so a JDK that merely happens to carry
    /// such a directory still resolves to itself.
    #[test]
    fn a_contents_home_without_a_launcher_is_not_a_bundle() {
        let dir = tempdir().unwrap();
        create_jdk_dir(dir.path(), "21.0.3+9", true);
        fs::create_dir_all(dir.path().join("21.0.3+9/Contents/Home")).unwrap();

        assert_eq!(
            JdkStore::at(dir.path()).find_matching(request("21")),
            Some(dir.path().join("21.0.3+9"))
        );
    }

    /// `jlo current` asks this, and `jlo env` set the `$JAVA_HOME` it is
    /// asking about - so for a bundle that is the `Contents/Home` path, not
    /// the store entry. Getting this wrong makes `current` report a live
    /// install as one that is no longer there.
    #[test]
    fn active_version_recognises_a_bundles_contents_home() {
        let dir = tempdir().unwrap();
        let java_home = create_bundle_jdk_dir(dir.path(), "21.0.3+9", true);
        let store = JdkStore::at(dir.path());
        let installed = store.list().unwrap();

        assert_eq!(
            store
                .active_version(&installed, Some(&java_home))
                .as_deref(),
            Some("21.0.3+9")
        );
    }

    /// The bundle root is the other spelling of the same install. Nothing jlo
    /// prints produces it, but a `$JAVA_HOME` set by hand can, and answering
    /// "not one of ours" about our own directory would be wrong.
    #[test]
    fn active_version_recognises_a_bundle_root() {
        let dir = tempdir().unwrap();
        create_bundle_jdk_dir(dir.path(), "21.0.3+9", true);
        let store = JdkStore::at(dir.path());
        let installed = store.list().unwrap();

        let entry = dir.path().join("21.0.3+9");
        assert_eq!(
            store.active_version(&installed, Some(&entry)).as_deref(),
            Some("21.0.3+9")
        );
    }

    /// The refusal that protects the calling shell, in bundle shape. `jlo env`
    /// exported the `Contents/Home` path, so that is what the guard is handed,
    /// and a guard that only knew the store entry would delete the JDK the
    /// shell is running on.
    #[test]
    fn remove_refuses_a_bundle_whose_contents_home_is_in_use() {
        let dir = tempdir().unwrap();
        let java_home = create_bundle_jdk_dir(dir.path(), "21.0.3+9", true);

        let err = JdkStore::at(dir.path())
            .remove(&["21".to_string()], Some(&java_home))
            .expect_err("JAVA_HOME points at it");

        assert!(matches!(err, RemoveError::InUse(v) if v == "21.0.3+9"));
        assert!(dir.path().join("21.0.3+9").exists());
    }

    /// The reason the guard compares paths instead of asking the filesystem
    /// what shape the directory is. Here the bundle's launcher is gone, so the
    /// shape probe would fall back to the store entry and conclude that a
    /// `$JAVA_HOME` of `Contents/Home` belongs to nobody - and delete the
    /// directory the calling shell is pointing into. A probe that fails is
    /// exactly the case a removal guard must survive.
    #[test]
    fn remove_refuses_a_bundle_whose_launcher_is_missing() {
        let dir = tempdir().unwrap();
        let java_home = create_bundle_jdk_dir(dir.path(), "21.0.3+9", true);
        fs::remove_file(java_home.join("bin").join("java")).unwrap();
        assert_eq!(
            java_home_in(&dir.path().join("21.0.3+9")),
            dir.path().join("21.0.3+9")
        );

        let err = JdkStore::at(dir.path())
            .remove(&["21".to_string()], Some(&java_home))
            .expect_err("JAVA_HOME points into it");

        assert!(matches!(err, RemoveError::InUse(v) if v == "21.0.3+9"));
        assert!(dir.path().join("21.0.3+9").exists());
    }

    /// The other spelling, at the other end of the guard: a `$JAVA_HOME` set
    /// by hand to the bundle root is still the JDK in use.
    #[test]
    fn remove_refuses_a_bundle_whose_root_is_in_use() {
        let dir = tempdir().unwrap();
        create_bundle_jdk_dir(dir.path(), "21.0.3+9", true);
        let entry = dir.path().join("21.0.3+9");

        let err = JdkStore::at(dir.path())
            .remove(&["21".to_string()], Some(&entry))
            .expect_err("JAVA_HOME points at it");

        assert!(matches!(err, RemoveError::InUse(v) if v == "21.0.3+9"));
        assert!(entry.exists());
    }

    // -- find_matching --

    #[test]
    fn find_matching_finds_latest() {
        let dir = tempdir().unwrap();
        create_jdk_dir(dir.path(), "21.0.1+12", true);
        create_jdk_dir(dir.path(), "21.0.3+9", true);
        create_jdk_dir(dir.path(), "17.0.2+8", true);

        let result = JdkStore::at(dir.path()).find_matching(request("21"));
        assert_eq!(
            result.unwrap().file_name().unwrap().to_str().unwrap(),
            "21.0.3+9"
        );
    }

    #[test]
    fn find_matching_no_match() {
        let dir = tempdir().unwrap();
        create_jdk_dir(dir.path(), "17.0.2+8", true);

        assert!(
            JdkStore::at(dir.path())
                .find_matching(request("21"))
                .is_none()
        );
    }

    #[test]
    fn find_matching_empty_dir() {
        let dir = tempdir().unwrap();
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
        create_jdk_dir(dir.path(), "26.0.1+9", true);
        create_jdk_dir(dir.path(), "26.0.2-beta+101.0.ea", true);
        let store = JdkStore::at(dir.path());

        assert_eq!(
            store.find_matching(Request {
                major: 26,
                stream: Stream::Ga
            }),
            Some(dir.path().join("26.0.1+9")),
            "a GA request must not be answered with the higher-sorting beta"
        );
        assert_eq!(
            store.find_matching(Request {
                major: 26,
                stream: Stream::Ea
            }),
            Some(dir.path().join("26.0.2-beta+101.0.ea"))
        );
    }

    // -- newest_ga_request --

    /// Cascade stage 3's input. A machine that once tried a pre-release must
    /// not have that build become the answer to a bare `jlo env`.
    #[test]
    fn newest_ga_request_ignores_pre_releases() {
        let dir = tempdir().unwrap();
        create_jdk_dir(dir.path(), "21.0.5+11", true);
        create_jdk_dir(dir.path(), "28.0.0-beta+16.0.ea", true);

        assert_eq!(
            JdkStore::at(dir.path()).newest_ga_request(),
            Some(Request {
                major: 21,
                stream: Stream::Ga
            })
        );
    }

    #[test]
    fn newest_ga_request_is_none_when_only_pre_releases_are_installed() {
        let dir = tempdir().unwrap();
        create_jdk_dir(dir.path(), "28.0.0-beta+16.0.ea", true);

        assert_eq!(JdkStore::at(dir.path()).newest_ga_request(), None);
    }

    /// The floor the old `newest_installed` filter applied, now inside the
    /// selector so `jlo current` cannot answer differently from the cascade.
    #[test]
    fn newest_ga_request_skips_a_store_below_the_floor() {
        let dir = tempdir().unwrap();
        create_jdk_dir(dir.path(), "7.0.4+101", true);

        assert_eq!(JdkStore::at(dir.path()).newest_ga_request(), None);
    }

    // -- installed_requests --

    #[test]
    fn installed_requests_discovers_names() {
        let dir = tempdir().unwrap();
        create_jdk_dir(dir.path(), "21.0.1+12", true);
        create_jdk_dir(dir.path(), "21.0.3+9", true);
        create_jdk_dir(dir.path(), "17.0.2+8", true);
        create_jdk_dir(dir.path(), "11.0.1+13", true);

        let versions = JdkStore::at(dir.path()).installed_requests().unwrap();
        assert_eq!(versions, vec![request("11"), request("17"), request("21")]);
    }

    #[test]
    fn installed_requests_ignores_non_dirs() {
        let dir = tempdir().unwrap();
        create_jdk_dir(dir.path(), "21.0.1+12", true);
        // Plain file should be skipped
        fs::write(dir.path().join("some-file.txt"), "").unwrap();

        let versions = JdkStore::at(dir.path()).installed_requests().unwrap();
        assert_eq!(versions, vec![request("21")]);
    }

    #[test]
    fn installed_requests_empty_dir() {
        let dir = tempdir().unwrap();
        let versions = JdkStore::at(dir.path()).installed_requests().unwrap();
        assert!(versions.is_empty());
    }

    /// What a bare `jlo update` iterates over. EA names are installed names
    /// too, so they appear here; the *skipping* is the caller's rule, decided
    /// by the command that has to explain it.
    #[test]
    fn installed_requests_names_both_streams() {
        let dir = tempdir().unwrap();
        create_jdk_dir(dir.path(), "21.0.5+11", true);
        create_jdk_dir(dir.path(), "28.0.0-beta+16.0.ea", true);

        let requests = JdkStore::at(dir.path()).installed_requests().unwrap();

        assert_eq!(
            requests,
            vec![
                Request {
                    major: 21,
                    stream: Stream::Ga
                },
                Request {
                    major: 28,
                    stream: Stream::Ea
                },
            ]
        );
    }

    /// A bare `jlo update` reports an unreadable install directory rather than
    /// quietly finding nothing to update.
    #[test]
    fn installed_requests_missing_base_dir_is_an_error() {
        let dir = tempdir().unwrap();
        let missing = dir.path().join("nothing-installed-here");

        let err = JdkStore::at(&missing).installed_requests().unwrap_err();
        assert!(
            format!("{err:#}").contains("could not read JDK base directory"),
            "{err:#}"
        );
    }

    // -- list --

    #[test]
    fn list_sorted_newest_first() {
        let dir = tempdir().unwrap();
        create_jdk_dir(dir.path(), "21.0.9+7", true);
        create_jdk_dir(dir.path(), "21.0.12+7", true);
        create_jdk_dir(dir.path(), "17.0.13+11", true);

        let jdks = JdkStore::at(dir.path()).list().unwrap();
        let versions: Vec<_> = jdks.iter().map(|j| j.version.as_str()).collect();
        assert_eq!(versions, vec!["21.0.12+7", "21.0.9+7", "17.0.13+11"]);
    }

    #[test]
    fn list_reports_managed_flag() {
        let dir = tempdir().unwrap();
        create_jdk_dir(dir.path(), "21.0.12+7", true);
        create_jdk_dir(dir.path(), "17.0.13+11", false);

        let jdks = JdkStore::at(dir.path()).list().unwrap();
        assert_eq!(jdks[0].version, "21.0.12+7");
        assert_eq!(jdks[0].major, 21);
        assert!(jdks[0].managed);
        assert_eq!(jdks[1].version, "17.0.13+11");
        assert_eq!(jdks[1].major, 17);
        assert!(!jdks[1].managed);
    }

    #[test]
    fn list_ignores_non_semver_and_files() {
        let dir = tempdir().unwrap();
        create_jdk_dir(dir.path(), "21.0.12+7", true);
        fs::create_dir(dir.path().join("not-a-jdk")).unwrap();
        fs::write(dir.path().join("21.0.1+9"), "a file, not a directory").unwrap();

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

    // -- superseded_count --

    #[test]
    fn superseded_count_counts_all_but_newest_per_major() {
        let dir = tempdir().unwrap();
        create_jdk_dir(dir.path(), "21.0.1+12", true);
        create_jdk_dir(dir.path(), "21.0.3+9", true);
        create_jdk_dir(dir.path(), "21.0.12+7", true);
        create_jdk_dir(dir.path(), "17.0.2+8", true);

        // Exactly what `prune` would remove: two old 21s, no 17.
        assert_eq!(JdkStore::at(dir.path()).superseded_count().unwrap(), 2);
    }

    #[test]
    fn superseded_count_ignores_unmanaged() {
        let dir = tempdir().unwrap();
        create_jdk_dir(dir.path(), "21.0.1+12", false);
        create_jdk_dir(dir.path(), "21.0.3+9", true);

        // `prune` never touches an unmanaged install, so counting one would
        // point at a `jlo remove --superseded` that then removes nothing.
        assert_eq!(JdkStore::at(dir.path()).superseded_count().unwrap(), 0);
    }

    #[test]
    fn superseded_count_on_missing_base_dir() {
        let dir = tempdir().unwrap();
        let missing = dir.path().join("never-installed");

        assert_eq!(JdkStore::at(&missing).superseded_count().unwrap(), 0);
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

    /// The staging directory is expected to be here, so `jlo remove
    /// --superseded` must not put a line under itself about it - once would
    /// be noise, and it would be every run for the rest of the machine's
    /// life. A directory that is genuinely unexpected still gets one.
    #[test]
    fn prune_says_nothing_about_a_staging_directory() {
        let dir = tempdir().unwrap();
        create_jdk_dir(dir.path(), "21.0.3+9", true);
        let store = JdkStore::at(dir.path());
        let staging = staging_dir(&store).unwrap();

        let report = store.prune(None).unwrap();

        assert_eq!(report.skipped_unmanaged, 0);
        assert!(staging.path().exists(), "prune must not delete it either");
    }

    /// ...and the listing must pass over it, or an interrupted install would
    /// show up as a JDK.
    #[test]
    fn a_staging_directory_is_not_mistaken_for_an_install() {
        let dir = tempdir().unwrap();
        let store = JdkStore::at(dir.path());
        create_jdk_dir(dir.path(), "21.0.3+9", true);

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

    // -- prune --

    #[test]
    fn prune_removes_older_versions() {
        let dir = tempdir().unwrap();
        create_jdk_dir(dir.path(), "21.0.1+12", true);
        create_jdk_dir(dir.path(), "21.0.3+9", true);
        create_jdk_dir(dir.path(), "17.0.2+8", true);

        JdkStore::at(dir.path()).prune(None).unwrap();

        // 21.0.3+9 kept, 21.0.1+12 removed, 17.0.2+8 kept (only version for major 17)
        assert!(dir.path().join("21.0.3+9").exists());
        assert!(!dir.path().join("21.0.1+12").exists());
        assert!(dir.path().join("17.0.2+8").exists());
    }

    // -- superseded_by --

    /// What `jlo update` deletes after installing `21.0.5+11`: every older
    /// managed build of *that name* - and nothing of the sibling stream,
    /// nothing unmanaged, nothing newer.
    #[test]
    fn an_update_supersedes_the_older_builds_of_its_name_only() {
        let dir = tempdir().unwrap();
        let base = dir.path();
        create_jdk_dir(base, "21.0.1+12", true);
        create_jdk_dir(base, "21.0.3+9", true);
        create_jdk_dir(base, "21.0.2+13", false);
        create_jdk_dir(base, "21.0.5+11", true);
        create_jdk_dir(base, "21.0.0-beta+4.0.ea", true);

        let superseded = JdkStore::at(base)
            .superseded_by(request("21"), "21.0.5+11")
            .unwrap();

        let names: Vec<_> = superseded
            .iter()
            .filter_map(|c| c.name.as_deref())
            .collect();
        assert_eq!(names, vec!["21.0.3+9", "21.0.1+12"]);
    }

    /// The guard `jlo remove <version>` applies to a named target, applied by
    /// the other selector of the same verb. `jlo install` and `jlo update`
    /// print the `jlo remove --superseded` hint into the shell they just ran
    /// in, so this is the ordinary flow, not a corner: without it the next
    /// command in that shell runs against a `$JAVA_HOME` that no longer
    /// exists.
    #[test]
    fn prune_leaves_the_jdk_java_home_points_at_alone() {
        let dir = tempdir().unwrap();
        create_jdk_dir(dir.path(), "21.0.1+12", true);
        create_jdk_dir(dir.path(), "21.0.3+9", true);

        let active = dir.path().join("21.0.1+12");
        let report = JdkStore::at(dir.path()).prune(Some(&active)).unwrap();

        assert!(active.exists(), "the live JDK must survive");
        assert_eq!(report.skipped_in_use.as_deref(), Some("21.0.1+12"));
        assert_eq!(report.removed_count(), 0);
    }

    /// `$JAVA_HOME` on a macOS bundle names `<version>/Contents/Home`, not the
    /// store entry, so a guard comparing only the entry would protect nothing
    /// on the platform the bundle exists for.
    #[test]
    fn prune_recognises_the_bundle_spelling_of_the_live_jdk() {
        let dir = tempdir().unwrap();
        let active = create_bundle_jdk_dir(dir.path(), "21.0.1+12", true);
        create_bundle_jdk_dir(dir.path(), "21.0.3+9", true);

        let report = JdkStore::at(dir.path()).prune(Some(&active)).unwrap();

        assert!(dir.path().join("21.0.1+12").exists());
        assert_eq!(report.skipped_in_use.as_deref(), Some("21.0.1+12"));
    }

    /// A skip, not a refusal: the live JDK stays and every other superseded
    /// build still goes.
    #[test]
    fn prune_removes_the_other_superseded_builds_around_the_live_one() {
        let dir = tempdir().unwrap();
        create_jdk_dir(dir.path(), "21.0.1+12", true);
        create_jdk_dir(dir.path(), "21.0.3+9", true);
        create_jdk_dir(dir.path(), "17.0.2+8", true);
        create_jdk_dir(dir.path(), "17.0.9+9", true);

        let active = dir.path().join("21.0.1+12");
        JdkStore::at(dir.path()).prune(Some(&active)).unwrap();

        assert!(active.exists());
        assert!(!dir.path().join("17.0.2+8").exists(), "17.0.2+8 still goes");
    }

    /// A vendor-named `IntelliJ` download is not an install jlo declined to
    /// touch - it is one jlo cannot see. Counting it would make
    /// `jlo remove --superseded` report "left 1 install alone" about
    /// something `jlo list` never mentioned.
    #[test]
    fn prune_does_not_count_a_vendor_named_directory_as_an_unmanaged_install() {
        let dir = tempdir().unwrap();
        create_jdk_dir(dir.path(), "21.0.3+9", true);
        std::fs::create_dir_all(dir.path().join("temurin-17.0.9")).unwrap();

        let report = JdkStore::at(dir.path()).prune(None).unwrap();

        assert_eq!(report.skipped_unmanaged, 0);
    }

    #[test]
    fn prune_ignores_unmanaged() {
        let dir = tempdir().unwrap();
        create_jdk_dir(dir.path(), "21.0.1+12", false); // no marker
        create_jdk_dir(dir.path(), "21.0.3+9", true);

        JdkStore::at(dir.path()).prune(None).unwrap();

        // Unmanaged dir should not be touched
        assert!(dir.path().join("21.0.1+12").exists());
        assert!(dir.path().join("21.0.3+9").exists());
    }

    /// Two builds of one patch differ only in build metadata, which semver
    /// leaves out of precedence. `prune` used to sort them Equal and delete
    /// whichever `read_dir` happened to yield second - a coin toss over a JDK,
    /// and one that took the *newer* build about half the time.
    #[test]
    fn prune_keeps_the_higher_build_of_one_patch() {
        for order in [
            ["21.0.11+9.0.LTS", "21.0.11+10.0.LTS"],
            ["21.0.11+10.0.LTS", "21.0.11+9.0.LTS"],
        ] {
            let dir = tempdir().unwrap();
            for version in order {
                create_jdk_dir(dir.path(), version, true);
            }

            let report = JdkStore::at(dir.path()).prune(None).unwrap();

            assert_eq!(
                report.removed,
                vec![(request("21"), vec!["21.0.11+9.0.LTS".to_string()])]
            );
            assert!(dir.path().join("21.0.11+10.0.LTS").exists());
            assert!(!dir.path().join("21.0.11+9.0.LTS").exists());
        }
    }

    /// The leniency in `version::parse` means two directory names can spell
    /// one version. Nothing distinguishes them, so `prune` has no basis for
    /// picking one, and picking by `read_dir` order would be the same coin
    /// toss. It leaves both alone, which is what `jlo list` already shows.
    #[test]
    fn prune_keeps_both_when_two_names_spell_one_version() {
        let dir = tempdir().unwrap();
        create_jdk_dir(dir.path(), "21.0.11+9", true);
        create_jdk_dir(dir.path(), "v21.0.11+9", true);

        let report = JdkStore::at(dir.path()).prune(None).unwrap();

        assert_eq!(report.removed_count(), 0);
        assert!(dir.path().join("21.0.11+9").exists());
        assert!(dir.path().join("v21.0.11+9").exists());
    }

    /// The count behind the superseded hint has to be the number `prune`
    /// would actually remove, or the hint offers work that will not happen.
    #[test]
    fn superseded_count_agrees_with_prune_on_one_patch() {
        let dir = tempdir().unwrap();
        create_jdk_dir(dir.path(), "21.0.11+9.0.LTS", true);
        create_jdk_dir(dir.path(), "21.0.11+10.0.LTS", true);
        create_jdk_dir(dir.path(), "17.0.11+9", true);
        create_jdk_dir(dir.path(), "v17.0.11+9", true);

        let store = JdkStore::at(dir.path());
        let before = store.superseded_count().unwrap();
        assert_eq!(before, 1);
        assert_eq!(store.prune(None).unwrap().removed_count(), before);
    }

    /// `remove --superseded` keeps the newest of each *name*. Without the
    /// stream in the key, the beta - which sorts above the GA build it
    /// previews - would make the released build superseded.
    #[test]
    fn superseded_counts_within_a_stream_only() {
        let dir = tempdir().unwrap();
        create_jdk_dir(dir.path(), "26.0.1+9", true);
        create_jdk_dir(dir.path(), "26.0.2-beta+101.0.ea", true);
        let store = JdkStore::at(dir.path());

        assert_eq!(
            store.superseded_count().unwrap(),
            0,
            "one build of each stream supersedes nothing"
        );

        create_jdk_dir(dir.path(), "26.0.3-beta+102.0.ea", true);
        assert_eq!(
            store.superseded_count().unwrap(),
            1,
            "the older beta is superseded by the newer beta, and only by it"
        );
    }

    #[test]
    fn prune_keeps_the_newest_of_each_stream() {
        let dir = tempdir().unwrap();
        create_jdk_dir(dir.path(), "26.0.1+9", true);
        create_jdk_dir(dir.path(), "26.0.2-beta+101.0.ea", true);
        create_jdk_dir(dir.path(), "26.0.3-beta+102.0.ea", true);

        let report = JdkStore::at(dir.path()).prune(None).unwrap();

        assert_eq!(report.removed_count(), 1);
        assert!(dir.path().join("26.0.1+9").exists(), "the GA build stays");
        assert!(
            dir.path().join("26.0.3-beta+102.0.ea").exists(),
            "the newest beta stays"
        );
        assert!(!dir.path().join("26.0.2-beta+101.0.ea").exists());
    }

    /// A `HashMap` yields its keys in an arbitrary order, so the majors used to
    /// print differently from one run to the next over the same directory.
    #[test]
    fn prune_reports_names_newest_first() {
        let dir = tempdir().unwrap();
        for version in [
            "17.0.1+1",
            "17.0.2+8",
            "25.0.1+1",
            "25.0.2+1",
            "21.0.1+12",
            "21.0.3+9",
        ] {
            create_jdk_dir(dir.path(), version, true);
        }

        let report = JdkStore::at(dir.path()).prune(None).unwrap();

        let names: Vec<Request> = report.removed.iter().map(|(name, _)| *name).collect();
        assert_eq!(names, vec![request("25"), request("21"), request("17")]);
        assert_eq!(report.removed_count(), 3);
        assert_eq!(report.removed[0].1, vec!["25.0.1+1"]);
        assert!(report.failures.is_empty());
    }

    #[test]
    fn prune_counts_unmanaged_without_removing_them() {
        let dir = tempdir().unwrap();
        create_jdk_dir(dir.path(), "21.0.1+12", false);
        create_jdk_dir(dir.path(), "21.0.3+9", false);
        create_jdk_dir(dir.path(), "17.0.2+8", true);

        let report = JdkStore::at(dir.path()).prune(None).unwrap();

        assert_eq!(report.skipped_unmanaged, 2);
        assert_eq!(report.removed_count(), 0);
        assert!(dir.path().join("21.0.1+12").exists());
    }

    #[test]
    fn prune_single_version_kept() {
        let dir = tempdir().unwrap();
        create_jdk_dir(dir.path(), "21.0.3+9", true);

        JdkStore::at(dir.path()).prune(None).unwrap();
        assert!(dir.path().join("21.0.3+9").exists());
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
        create_jdk_dir(dir.path(), "17.0.2+8", true);
        create_jdk_dir(dir.path(), "17.0.9+9", true);
        create_jdk_dir(dir.path(), "21.0.3+9", true);

        let report = JdkStore::at(dir.path())
            .remove(&targets(&["17"]), None)
            .unwrap();

        // Newest first, the order `list` and `jlo list --offline` use.
        assert_eq!(report.removed, vec!["17.0.9+9", "17.0.2+8"]);
        assert!(report.failures.is_empty());
        assert!(!dir.path().join("17.0.2+8").exists());
        assert!(!dir.path().join("17.0.9+9").exists());
        assert!(dir.path().join("21.0.3+9").exists());
    }

    /// An exact target names one directory, so its siblings in the same major
    /// stay. This is the half of `remove` that reads like a pin but is not
    /// one - it selects an install that already exists rather than requesting
    /// a build.
    #[test]
    fn remove_deletes_only_the_exact_version_named() {
        let dir = tempdir().unwrap();
        create_jdk_dir(dir.path(), "17.0.2+8", true);
        create_jdk_dir(dir.path(), "17.0.9+9", true);

        let report = JdkStore::at(dir.path())
            .remove(&targets(&["17.0.2+8"]), None)
            .unwrap();

        assert_eq!(report.removed, vec!["17.0.2+8"]);
        assert!(!dir.path().join("17.0.2+8").exists());
        assert!(dir.path().join("17.0.9+9").exists());
    }

    #[test]
    fn remove_reports_a_target_that_is_not_installed() {
        let dir = tempdir().unwrap();
        create_jdk_dir(dir.path(), "21.0.3+9", true);

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
        create_jdk_dir(dir.path(), "17.0.9+9", true);

        let err = JdkStore::at(dir.path())
            .remove(&targets(&["17.0.2+8"]), None)
            .unwrap_err();

        assert!(matches!(err, RemoveError::NotInstalled(_)), "{err}");
        assert!(dir.path().join("17.0.9+9").exists());
    }

    #[test]
    fn remove_takes_several_targets_at_once() {
        let dir = tempdir().unwrap();
        create_jdk_dir(dir.path(), "11.0.1+13", true);
        create_jdk_dir(dir.path(), "17.0.2+8", true);
        create_jdk_dir(dir.path(), "21.0.3+9", true);

        let report = JdkStore::at(dir.path())
            .remove(&targets(&["11", "17"]), None)
            .unwrap();

        // Newest first overall, whatever order the targets came in.
        assert_eq!(report.removed, vec!["17.0.2+8", "11.0.1+13"]);
        assert!(dir.path().join("21.0.3+9").exists());
    }

    #[test]
    fn remove_mixes_major_and_exact_targets() {
        let dir = tempdir().unwrap();
        create_jdk_dir(dir.path(), "17.0.2+8", true);
        create_jdk_dir(dir.path(), "21.0.1+12", true);
        create_jdk_dir(dir.path(), "21.0.3+9", true);

        let report = JdkStore::at(dir.path())
            .remove(&targets(&["17", "21.0.1+12"]), None)
            .unwrap();

        assert_eq!(report.removed, vec!["21.0.1+12", "17.0.2+8"]);
        assert!(dir.path().join("21.0.3+9").exists());
    }

    /// Overlapping targets select the same directory once. Deleting it twice
    /// would turn the second attempt into a spurious failure line.
    #[test]
    fn remove_does_not_select_an_overlapping_target_twice() {
        let dir = tempdir().unwrap();
        create_jdk_dir(dir.path(), "17.0.2+8", true);

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
        create_jdk_dir(dir.path(), "17.0.2+8", true);

        let report = JdkStore::at(dir.path())
            .remove(&targets(&["3", "4", "5", "17"]), None)
            .unwrap();

        assert_eq!(report.removed, vec!["17.0.2+8"]);
        assert_eq!(report.not_installed, vec!["3", "4", "5"]);
        assert!(!dir.path().join("17.0.2+8").exists());
    }

    /// Same for an unmanaged install named alongside a removable one: it is
    /// reported and skipped, not a refusal.
    #[test]
    fn remove_skips_an_unmanaged_version_named_alongside_a_removable_one() {
        let dir = tempdir().unwrap();
        create_jdk_dir(dir.path(), "17.0.2+8", true);
        create_jdk_dir(dir.path(), "21.0.3+9", false);

        let report = JdkStore::at(dir.path())
            .remove(&targets(&["17", "21"]), None)
            .unwrap();

        assert_eq!(report.removed, vec!["17.0.2+8"]);
        assert_eq!(report.skipped_unmanaged, vec!["21.0.3+9"]);
        assert!(dir.path().join("21.0.3+9").exists());
    }

    /// When nothing is left to do, the unmanaged install is the better
    /// answer: it is the one J'Lo found and declined, where the other version
    /// simply is not there.
    #[test]
    fn remove_names_the_unmanaged_install_ahead_of_a_missing_version() {
        let dir = tempdir().unwrap();
        create_jdk_dir(dir.path(), "21.0.3+9", false);

        let err = JdkStore::at(dir.path())
            .remove(&targets(&["3", "21"]), None)
            .unwrap_err();

        assert!(matches!(err, RemoveError::Unmanaged(_)), "{err}");
        assert!(dir.path().join("21.0.3+9").exists());
    }

    /// Every miss in one message. Reporting only the first would make
    /// clearing out three stale majors a three-rerun exercise.
    #[test]
    fn remove_names_every_version_that_matched_nothing() {
        let dir = tempdir().unwrap();
        create_jdk_dir(dir.path(), "21.0.3+9", true);

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
        create_jdk_dir(dir.path(), "21.0.3+9", true);

        let report = JdkStore::at(dir.path())
            .remove(&targets(&["3", "21", "5", "3"]), None)
            .unwrap();

        assert_eq!(report.removed, vec!["21.0.3+9"]);
        assert_eq!(report.not_installed, vec!["3", "5"]);
    }

    // -- remove: the two refusals --

    /// No marker, no deletion. The user named this install
    /// explicitly, so silently skipping it and exiting 0 would claim a
    /// removal that did not happen.
    #[test]
    fn remove_refuses_an_unmanaged_install() {
        let dir = tempdir().unwrap();
        create_jdk_dir(dir.path(), "17.0.2+8", false);

        let err = JdkStore::at(dir.path())
            .remove(&targets(&["17"]), None)
            .unwrap_err();

        assert!(matches!(err, RemoveError::Unmanaged(_)), "{err}");
        assert_eq!(
            err.to_string(),
            "refusing to remove 17.0.2+8: not installed by jlo"
        );
        assert!(
            dir.path().join("17.0.2+8").exists(),
            "the unmanaged install must survive the refusal"
        );
    }

    /// A managed match alongside an unmanaged one is not a refusal: the
    /// managed install goes, and the one left behind is reported by name so
    /// the user is not left wondering why a version never goes away.
    #[test]
    fn remove_skips_an_unmanaged_sibling_without_refusing() {
        let dir = tempdir().unwrap();
        create_jdk_dir(dir.path(), "17.0.2+8", true);
        create_jdk_dir(dir.path(), "17.0.9+9", false);

        let report = JdkStore::at(dir.path())
            .remove(&targets(&["17"]), None)
            .unwrap();

        assert_eq!(report.removed, vec!["17.0.2+8"]);
        assert_eq!(report.skipped_unmanaged, vec!["17.0.9+9"]);
        assert!(dir.path().join("17.0.9+9").exists());
    }

    /// The hazard that killed `jlo update --clean`: deleting the JDK the
    /// calling shell is on leaves `$JAVA_HOME` pointing at nothing.
    #[test]
    fn remove_refuses_the_jdk_java_home_points_at() {
        let dir = tempdir().unwrap();
        create_jdk_dir(dir.path(), "21.0.3+9", true);
        let active = dir.path().join("21.0.3+9");

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
        create_jdk_dir(dir.path(), "21.0.1+12", true);
        create_jdk_dir(dir.path(), "21.0.3+9", true);
        let active = dir.path().join("21.0.3+9");

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
        create_jdk_dir(dir.path(), "17.0.2+8", true);
        create_jdk_dir(dir.path(), "21.0.3+9", true);
        create_jdk_dir(dir.path(), "26.0.2+101", true);
        let active = dir.path().join("26.0.2+101");

        let report = JdkStore::at(dir.path())
            .remove(&targets(&["21", "26", "3", "17"]), Some(&active))
            .unwrap();

        assert_eq!(report.removed, vec!["21.0.3+9", "17.0.2+8"]);
        assert_eq!(report.skipped_in_use.as_deref(), Some("26.0.2+101"));
        assert_eq!(report.not_installed, vec!["3"]);
        assert!(active.exists());
    }

    /// Checked before the marker: when a target trips both refusals, the
    /// live-JDK one is the more useful thing to say.
    #[test]
    fn remove_reports_the_live_jdk_ahead_of_the_missing_marker() {
        let dir = tempdir().unwrap();
        create_jdk_dir(dir.path(), "21.0.3+9", false);
        let active = dir.path().join("21.0.3+9");

        let err = JdkStore::at(dir.path())
            .remove(&targets(&["21"]), Some(&active))
            .unwrap_err();

        assert!(matches!(err, RemoveError::InUse(_)), "{err}");
    }

    #[test]
    fn remove_proceeds_when_java_home_points_somewhere_else() {
        let dir = tempdir().unwrap();
        create_jdk_dir(dir.path(), "17.0.2+8", true);
        create_jdk_dir(dir.path(), "21.0.3+9", true);
        let active = dir.path().join("21.0.3+9");

        let report = JdkStore::at(dir.path())
            .remove(&targets(&["17"]), Some(&active))
            .unwrap();

        assert_eq!(report.removed, vec!["17.0.2+8"]);
        assert!(active.exists());
    }

    /// A `$JAVA_HOME` with a trailing separator names the same directory, and
    /// must not walk past the refusal.
    #[test]
    fn remove_sees_through_a_trailing_separator_on_java_home() {
        let dir = tempdir().unwrap();
        create_jdk_dir(dir.path(), "21.0.3+9", true);
        let active = PathBuf::from(format!("{}/21.0.3+9/", dir.path().display()));

        let err = JdkStore::at(dir.path())
            .remove(&targets(&["21"]), Some(&active))
            .unwrap_err();

        assert!(matches!(err, RemoveError::InUse(_)), "{err}");
    }

    /// Every refusal leaves the caller a usable next step; only the
    /// unreadable-directory variant is a plain failure with nothing to advise.
    #[test]
    fn remove_refusals_carry_advice() {
        assert!(RemoveError::NotInstalled(targets(&["17"])).hint().is_some());
        assert!(RemoveError::InUse("21.0.3+9".to_string()).hint().is_some());
        assert!(
            RemoveError::Unmanaged(vec!["21.0.3+9".to_string()])
                .hint()
                .is_some()
        );
        assert!(RemoveError::Store(anyhow::anyhow!("boom")).hint().is_none());
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

    // -- matches_target --

    /// `jlo remove 26` must not take the beta with it, and `jlo remove 26-ea`
    /// must reach the beta - the selector is the *name*, like every other rule
    /// here. Exact-build targets are untouched by that: they name one
    /// directory and always did.
    #[test]
    fn matches_target_selects_by_name_not_by_major() {
        let ga = InstalledJdk {
            version: "26.0.1+9".to_string(),
            major: 26,
            stream: Stream::Ga,
            managed: true,
        };
        let ea = InstalledJdk {
            version: "26.0.2-beta+101.0.ea".to_string(),
            major: 26,
            stream: Stream::Ea,
            managed: true,
        };

        assert!(matches_target(&ga, "26"));
        assert!(!matches_target(&ea, "26"));
        assert!(matches_target(&ea, "26-ea"));
        assert!(!matches_target(&ga, "26-ea"));
        // The exact build still names exactly one install.
        assert!(matches_target(&ea, "26.0.2-beta+101.0.ea"));
    }

    #[test]
    fn matches_target_reads_a_bare_integer_as_a_major() {
        let jdk = InstalledJdk {
            version: "17.0.2+8".to_string(),
            major: 17,
            stream: Stream::Ga,
            managed: true,
        };

        assert!(matches_target(&jdk, "17"));
        assert!(matches_target(&jdk, "17.0.2+8"));
        assert!(!matches_target(&jdk, "1"));
        // Neither a major nor a directory name: a range, which jlo has no
        // notion of anywhere.
        assert!(!matches_target(&jdk, "17.0"));
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

        (dest_parent.join("21.0.3+9"), returned)
    }

    /// The marker goes *beside* the install, never inside it: on macOS the
    /// install is a signed bundle and a file at its root unseals it. Nothing
    /// is written into the directory at all.
    #[test]
    fn install_moves_and_marks() {
        let dest_parent = tempdir().unwrap();
        let (entry, java_home) = install_mock_jdk(dest_parent.path());

        assert!(sibling_marker(dest_parent.path(), "21.0.3+9").exists());
        assert!(!entry.join(LEGACY_MARKER_FILE).exists());
        assert!(java_home.join("bin").join("java").exists());
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
        create_jdk_dir(dir.path(), "17.0.2+8", true);
        create_jdk_dir(dir.path(), "21.0.3+9", true);
        let store = JdkStore::at(dir.path());
        let installed = store.list().unwrap();

        let active = dir.path().join("21.0.3+9");
        assert_eq!(
            store.active_version(&installed, Some(&active)).as_deref(),
            Some("21.0.3+9")
        );
    }

    #[test]
    fn active_version_is_none_when_java_home_points_outside_the_store() {
        let dir = tempdir().unwrap();
        create_jdk_dir(dir.path(), "21.0.3+9", true);
        let store = JdkStore::at(dir.path());
        let installed = store.list().unwrap();

        let outside = Path::new("/usr/lib/jvm/java-21-openjdk");
        assert_eq!(store.active_version(&installed, Some(outside)), None);
    }

    #[test]
    fn active_version_is_none_without_java_home() {
        let dir = tempdir().unwrap();
        create_jdk_dir(dir.path(), "21.0.3+9", true);
        let store = JdkStore::at(dir.path());
        let installed = store.list().unwrap();

        assert_eq!(store.active_version(&installed, None), None);
    }
}
