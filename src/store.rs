use crate::adoptium::JdkMetadata;
use crate::ui::InstallUi;
use anyhow::{Context, bail};
use semver_rs::compare;
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::env;
use std::path::{Path, PathBuf};

const MARKER_FILE: &str = ".jlo-managed";

/// What a `jlo prune` run did, so the caller owns the presentation and
/// [`JdkStore::prune`] owns only the filesystem work.
#[derive(Debug, Default)]
pub(crate) struct PruneReport {
    /// `(major, removed version names)`, newest major first. Only versions
    /// actually deleted appear here.
    pub(crate) removed: Vec<(i64, Vec<String>)>,
    /// One message per JDK that could not be deleted.
    pub(crate) failures: Vec<String>,
    /// Installs without a `.jlo-managed` marker. Counted rather than listed:
    /// on a machine that also uses sdkman or Homebrew this is every other JDK,
    /// and a line each would bury the removals.
    pub(crate) skipped_unmanaged: usize,
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
    /// Whether the JDK carries the `.jlo-managed` marker, i.e. whether
    /// `jlo prune` and `jlo remove` are allowed to delete it.
    pub managed: bool,
}

/// One directory found in the store, with everything a single walk can say
/// about it. Each caller applies its own notion of what counts as a JDK here,
/// which is why nothing is filtered out yet.
struct Candidate {
    path: PathBuf,
    /// The directory name, or `None` when it is not valid UTF-8.
    name: Option<String>,
    /// The major version the name parses to, or `None` when it is not a
    /// semver. `semver_rs::parse` is lenient, so this can be `0` - see
    /// [`is_jdk_version_dir`].
    major: Option<i64>,
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
                Some(InstalledJdk {
                    version: candidate.name?,
                    major: candidate.major?,
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
            .find(|jdk| same_dir(&self.base.join(&jdk.version), active))
            .map(|jdk| jdk.version.clone())
    }

    /// The newest installed JDK whose major version is `major`, if any.
    ///
    /// Matched on the parsed major rather than on a name prefix: a prefix
    /// match makes `1` select `17`, and the only reason that is unreachable
    /// today is the `>= 8` floor in [`crate::conf::is_valid_version`]. A
    /// `major` that is not an integer matches nothing.
    pub(crate) fn find_matching(&self, major: &str) -> Option<PathBuf> {
        let major: i64 = major.parse().ok()?;
        let mut matching_versions = self.scan().ok()?;
        matching_versions.retain(|candidate| candidate.major == Some(major));

        sort_by_semver_desc(&mut matching_versions);

        matching_versions
            .into_iter()
            .next()
            .map(|candidate| candidate.path)
    }

    /// The path of the exact build `metadata` describes, if it is installed.
    pub(crate) fn find_exact(&self, metadata: &JdkMetadata) -> Option<PathBuf> {
        let extracted_jdk_path = self.base.join(&metadata.semver);
        if extracted_jdk_path.exists() {
            Some(extracted_jdk_path)
        } else {
            None
        }
    }

    /// The major versions present in the store, ascending.
    pub(crate) fn installed_majors(&self) -> anyhow::Result<Vec<i64>> {
        let major_versions: HashSet<i64> = self
            .scan_required()?
            .into_iter()
            .filter_map(|candidate| candidate.major)
            .collect();

        let mut major_versions_vec: Vec<i64> = major_versions.into_iter().collect();
        major_versions_vec.sort_unstable();
        Ok(major_versions_vec)
    }

    /// How many installs `jlo prune` would remove: every managed JDK that is
    /// not the newest of its major.
    ///
    /// The read-only counterpart to [`Self::prune`], so `jlo update` can point
    /// at `jlo prune` after superseding a minor without deleting anything
    /// itself.
    pub(crate) fn superseded_count(&self) -> anyhow::Result<usize> {
        let mut newest_seen: HashSet<i64> = HashSet::new();
        let mut superseded = 0;

        // `list` yields newest first, so the first managed JDK of a major is
        // the one `prune` keeps and every later one is superseded.
        for jdk in self.list()?.into_iter().filter(|jdk| jdk.managed) {
            if !newest_seen.insert(jdk.major) {
                superseded += 1;
            }
        }

        Ok(superseded)
    }

    /// Remove every managed JDK that is not the newest of its major.
    pub(crate) fn prune(&self) -> anyhow::Result<PruneReport> {
        // collector major versions
        let mut installed_jdks: HashMap<i64, Vec<Candidate>> = HashMap::new();
        let mut report = PruneReport::default();

        for candidate in self.scan_required()? {
            let path = &candidate.path;
            if candidate.name.is_none() {
                crate::ui::warning!("ignoring directory with invalid name {path:?}");
                continue;
            }
            if !candidate.managed {
                // skip directories not managed by jlo
                report.skipped_unmanaged += 1;
                continue;
            }
            let Some(major) = candidate.major else {
                crate::ui::warning!("ignoring non-semver directory {path:?}");
                continue;
            };
            installed_jdks.entry(major).or_default().push(candidate);
        }

        // A `HashMap` hands back its keys in an arbitrary order, which made two runs
        // over the same directory print the majors differently. Sort so the output
        // is stable and matches `jlo list` (newest major first).
        let mut majors: Vec<i64> = installed_jdks.keys().copied().collect();
        majors.sort_unstable_by(|a, b| b.cmp(a));

        for major in majors {
            let Some(candidates) = installed_jdks.get_mut(&major) else {
                continue;
            };
            sort_by_semver_desc(candidates);

            if candidates.len() <= 1 {
                continue;
            }

            // Record what was *actually* deleted. Announcing the removals up front
            // meant a failure below turned the line above it into a false claim.
            let mut removed = Vec::new();
            for old_jdk in &candidates[1..] {
                let name = old_jdk.name.as_deref().unwrap_or("unknown").to_string();
                let path = &old_jdk.path;
                match std::fs::remove_dir_all(path) {
                    Ok(()) => removed.push(name),
                    Err(e) => report
                        .failures
                        .push(format!("could not remove {path:?}: {e}")),
                }
            }

            if !removed.is_empty() {
                report.removed.push((major, removed));
            }
        }

        Ok(report)
    }

    /// Delete the JDKs `targets` name: for each one, every installed build of
    /// a major version (`17`), or the one exact build (`17.0.11+10`).
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
                .partition(|jdk| same_dir(&self.base.join(&jdk.version), active)),
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
            match std::fs::remove_dir_all(&path) {
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

        // Validate extracted path
        let extracted_jdk_path = find_jdk_path(metadata, source_dir)
            .context("could not find the extracted JDK directory")?;

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

        // touch a file to indicate that this directory is managed by jlo
        std::fs::File::create(dest_dir.join(MARKER_FILE))
            .context("could not create marker file")?;

        Ok(dest_dir)
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
                let major = name
                    .as_deref()
                    .and_then(|name| semver_rs::parse(name, None).ok())
                    .map(|semver| semver.major);
                let managed = path.join(MARKER_FILE).exists();
                Candidate {
                    path,
                    name,
                    major,
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

/// JDK install location, matching `IntelliJ` IDEA's layout so both tools see the
/// same JDKs. Split out from [`JdkStore::discover`] so every platform is
/// testable from any host.
fn base_dir_for(os: &str, home: &Path) -> PathBuf {
    match os {
        "macos" => home.join("Library/Java/JavaVirtualMachines"),
        _ => home.join(".jdks"),
    }
}

fn sort_by_semver_desc(candidates: &mut [Candidate]) {
    candidates.sort_by(|a, b| {
        let a_str = a.name.as_deref().unwrap_or("");
        let b_str = b.name.as_deref().unwrap_or("");
        compare(b_str, a_str, None).unwrap_or(Ordering::Equal)
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

/// Whether `jdk` is what one `jlo remove` target names.
///
/// A bare integer is a major version and selects every build of that major;
/// anything else has to equal the directory name exactly. Nothing in between
/// is accepted - `17.0` would have to mean "some 17.0.x", which is a version
/// range, and ranges are what `.jlorc` deliberately does not have.
fn matches_target(jdk: &InstalledJdk, target: &str) -> bool {
    match target.parse::<i64>() {
        Ok(major) => jdk.major == major,
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

/// `semver_rs::parse` is lenient - it happily turns any junk into `0.0.0` - so a
/// directory only counts as a JDK when it parses to a real major version.
fn is_jdk_version_dir(name: &str) -> bool {
    semver_rs::parse(name, None).is_ok_and(|sv| sv.major > 0)
}

fn find_jdk_path(jdk_metadata: &JdkMetadata, temp_dest: &Path) -> anyhow::Result<PathBuf> {
    let mut extracted_jdk_path = temp_dest.join(&jdk_metadata.release_name);

    // On macOS, the JDK is inside Contents/Home
    if env::consts::OS == "macos" {
        extracted_jdk_path = extracted_jdk_path.join("Contents").join("Home");
    }

    if env::consts::OS == "windows" {
        let java_bin = extracted_jdk_path.join("bin").join("java.exe");
        if !java_bin.exists() {
            bail!("java executable is missing at {java_bin:?}");
        }
    } else {
        let java_bin = extracted_jdk_path.join("bin").join("java");
        if !java_bin.exists() {
            bail!("java executable is missing at {java_bin:?}");
        }
    }

    Ok(extracted_jdk_path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn create_jdk_dir(base: &Path, version: &str, managed: bool) {
        let dir = base.join(version);
        fs::create_dir_all(dir.join("bin")).unwrap();
        // Create a fake java binary
        fs::write(dir.join("bin").join("java"), "").unwrap();
        if managed {
            fs::File::create(dir.join(MARKER_FILE)).unwrap();
        }
    }

    /// A `JdkMetadata` carrying only the two fields the store looks at.
    fn metadata(semver: &str, release_name: &str) -> JdkMetadata {
        JdkMetadata {
            semver: semver.to_string(),
            release_name: release_name.to_string(),
            package_name: String::new(),
            download_link: String::new(),
            checksum: String::new(),
        }
    }

    /// Where an extracted Adoptium archive puts the JDK on this platform.
    fn extracted_jdk_dir(source: &Path, release: &str) -> PathBuf {
        if env::consts::OS == "macos" {
            source.join(release).join("Contents").join("Home")
        } else {
            source.join(release)
        }
    }

    fn java_binary_name() -> &'static str {
        if env::consts::OS == "windows" {
            "java.exe"
        } else {
            "java"
        }
    }

    /// Create a mock extracted JDK under `source`, in the layout
    /// [`find_jdk_path`] expects, and return its directory.
    fn create_extracted_jdk(source: &Path, release: &str) -> PathBuf {
        let jdk_dir = extracted_jdk_dir(source, release);
        fs::create_dir_all(jdk_dir.join("bin")).unwrap();
        fs::write(jdk_dir.join("bin").join(java_binary_name()), "").unwrap();
        jdk_dir
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

    #[test]
    fn base_dir_matches_intellij_layout_on_windows() {
        let home = Path::new("/Users/u");
        assert_eq!(base_dir_for("windows", home), home.join(".jdks"));
    }

    // -- find_matching --

    #[test]
    fn find_matching_finds_latest() {
        let dir = tempdir().unwrap();
        create_jdk_dir(dir.path(), "21.0.1+12", true);
        create_jdk_dir(dir.path(), "21.0.3+9", true);
        create_jdk_dir(dir.path(), "17.0.2+8", true);

        let result = JdkStore::at(dir.path()).find_matching("21");
        assert_eq!(
            result.unwrap().file_name().unwrap().to_str().unwrap(),
            "21.0.3+9"
        );
    }

    #[test]
    fn find_matching_no_match() {
        let dir = tempdir().unwrap();
        create_jdk_dir(dir.path(), "17.0.2+8", true);

        assert!(JdkStore::at(dir.path()).find_matching("21").is_none());
    }

    #[test]
    fn find_matching_empty_dir() {
        let dir = tempdir().unwrap();
        assert!(JdkStore::at(dir.path()).find_matching("21").is_none());
    }

    #[test]
    fn find_matching_respects_version_boundaries() {
        let dir = tempdir().unwrap();
        create_jdk_dir(dir.path(), "17.0.2+8", true);
        create_jdk_dir(dir.path(), "1.8.0+402", true);

        let store = JdkStore::at(dir.path());
        // The boundary case a prefix match got wrong: "1" is a prefix of both
        // "17.0.2+8" and "1.8.0+402", but only the latter is major 1.
        assert_eq!(
            store
                .find_matching("1")
                .unwrap()
                .file_name()
                .unwrap()
                .to_str()
                .unwrap(),
            "1.8.0+402"
        );
        assert_eq!(
            store
                .find_matching("17")
                .unwrap()
                .file_name()
                .unwrap()
                .to_str()
                .unwrap(),
            "17.0.2+8"
        );
        // A major that is not an integer selects nothing rather than whatever
        // happens to share its leading characters.
        assert!(store.find_matching("17.0").is_none());
    }

    // -- find_exact --

    #[test]
    fn find_exact_exists() {
        let dir = tempdir().unwrap();
        create_jdk_dir(dir.path(), "21.0.3+9", true);

        assert!(
            JdkStore::at(dir.path())
                .find_exact(&metadata("21.0.3+9", ""))
                .is_some()
        );
    }

    #[test]
    fn find_exact_not_exists() {
        let dir = tempdir().unwrap();

        assert!(
            JdkStore::at(dir.path())
                .find_exact(&metadata("21.0.3+9", ""))
                .is_none()
        );
    }

    // -- installed_majors --

    #[test]
    fn installed_majors_discovers_majors() {
        let dir = tempdir().unwrap();
        create_jdk_dir(dir.path(), "21.0.1+12", true);
        create_jdk_dir(dir.path(), "21.0.3+9", true);
        create_jdk_dir(dir.path(), "17.0.2+8", true);
        create_jdk_dir(dir.path(), "11.0.1+13", true);

        let versions = JdkStore::at(dir.path()).installed_majors().unwrap();
        assert_eq!(versions, vec![11, 17, 21]);
    }

    #[test]
    fn installed_majors_ignores_non_dirs() {
        let dir = tempdir().unwrap();
        create_jdk_dir(dir.path(), "21.0.1+12", true);
        // Plain file should be skipped
        fs::write(dir.path().join("some-file.txt"), "").unwrap();

        let versions = JdkStore::at(dir.path()).installed_majors().unwrap();
        assert_eq!(versions, vec![21]);
    }

    #[test]
    fn installed_majors_empty_dir() {
        let dir = tempdir().unwrap();
        let versions = JdkStore::at(dir.path()).installed_majors().unwrap();
        assert!(versions.is_empty());
    }

    /// `jlo update --all` reports an unreadable install directory rather than
    /// quietly finding nothing to update.
    #[test]
    fn installed_majors_missing_base_dir_is_an_error() {
        let dir = tempdir().unwrap();
        let missing = dir.path().join("nothing-installed-here");

        let err = JdkStore::at(&missing).installed_majors().unwrap_err();
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
        // point at a `jlo prune` that then removes nothing.
        assert_eq!(JdkStore::at(dir.path()).superseded_count().unwrap(), 0);
    }

    #[test]
    fn superseded_count_on_missing_base_dir() {
        let dir = tempdir().unwrap();
        let missing = dir.path().join("never-installed");

        assert_eq!(JdkStore::at(&missing).superseded_count().unwrap(), 0);
    }

    // -- prune --

    #[test]
    fn prune_removes_older_versions() {
        let dir = tempdir().unwrap();
        create_jdk_dir(dir.path(), "21.0.1+12", true);
        create_jdk_dir(dir.path(), "21.0.3+9", true);
        create_jdk_dir(dir.path(), "17.0.2+8", true);

        JdkStore::at(dir.path()).prune().unwrap();

        // 21.0.3+9 kept, 21.0.1+12 removed, 17.0.2+8 kept (only version for major 17)
        assert!(dir.path().join("21.0.3+9").exists());
        assert!(!dir.path().join("21.0.1+12").exists());
        assert!(dir.path().join("17.0.2+8").exists());
    }

    #[test]
    fn prune_ignores_unmanaged() {
        let dir = tempdir().unwrap();
        create_jdk_dir(dir.path(), "21.0.1+12", false); // no marker
        create_jdk_dir(dir.path(), "21.0.3+9", true);

        JdkStore::at(dir.path()).prune().unwrap();

        // Unmanaged dir should not be touched
        assert!(dir.path().join("21.0.1+12").exists());
        assert!(dir.path().join("21.0.3+9").exists());
    }

    /// A `HashMap` yields its keys in an arbitrary order, so the majors used to
    /// print differently from one run to the next over the same directory.
    #[test]
    fn prune_reports_majors_newest_first() {
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

        let report = JdkStore::at(dir.path()).prune().unwrap();

        let majors: Vec<i64> = report.removed.iter().map(|(major, _)| *major).collect();
        assert_eq!(majors, vec![25, 21, 17]);
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

        let report = JdkStore::at(dir.path()).prune().unwrap();

        assert_eq!(report.skipped_unmanaged, 2);
        assert_eq!(report.removed_count(), 0);
        assert!(dir.path().join("21.0.1+12").exists());
    }

    #[test]
    fn prune_single_version_kept() {
        let dir = tempdir().unwrap();
        create_jdk_dir(dir.path(), "21.0.3+9", true);

        JdkStore::at(dir.path()).prune().unwrap();
        assert!(dir.path().join("21.0.3+9").exists());
    }

    /// `jlo prune` says so when the install directory cannot be read, rather
    /// than reporting an empty run.
    #[test]
    fn prune_missing_base_dir_is_an_error() {
        let dir = tempdir().unwrap();
        let missing = dir.path().join("never-installed");

        let err = JdkStore::at(&missing).prune().unwrap_err();
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

    #[test]
    fn matches_target_reads_a_bare_integer_as_a_major() {
        let jdk = InstalledJdk {
            version: "17.0.2+8".to_string(),
            major: 17,
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
        let release = "jdk-21.0.3+9";
        let jdk_dir = create_extracted_jdk(dir.path(), release);

        let result = find_jdk_path(&metadata("", release), dir.path()).unwrap();
        assert_eq!(result, jdk_dir);
    }

    #[test]
    fn find_jdk_path_missing_java_binary() {
        let dir = tempdir().unwrap();
        let release = "jdk-21.0.3+9";

        // Create dir structure but no java binary
        fs::create_dir_all(extracted_jdk_dir(dir.path(), release).join("bin")).unwrap();

        let result = find_jdk_path(&metadata("", release), dir.path());
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("java executable is missing")
        );
    }

    // -- install --

    #[test]
    fn install_moves_and_marks() {
        let source_dir = tempdir().unwrap();
        let dest_parent = tempdir().unwrap();
        let release = "jdk-21.0.3+9";
        create_extracted_jdk(source_dir.path(), release);

        let dest = JdkStore::at(dest_parent.path())
            .install(
                &metadata("21.0.3+9", release),
                source_dir.path(),
                &InstallUi::hidden("test"),
            )
            .unwrap();

        assert!(dest.exists());
        assert!(dest.join(MARKER_FILE).exists());
        assert!(dest.join("bin").join(java_binary_name()).exists());
    }

    /// The caller needs the installed path (for `ui.finish` and its own return
    /// value), and it is the store - not the caller - that decides the layout.
    #[test]
    fn install_returns_the_path_it_installed_to() {
        let source_dir = tempdir().unwrap();
        let dest_parent = tempdir().unwrap();
        let release = "jdk-21.0.3+9";
        create_extracted_jdk(source_dir.path(), release);

        let dest = JdkStore::at(dest_parent.path())
            .install(
                &metadata("21.0.3+9", release),
                source_dir.path(),
                &InstallUi::hidden("test"),
            )
            .unwrap();

        assert_eq!(dest, dest_parent.path().join("21.0.3+9"));
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
