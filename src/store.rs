use crate::adoptium::JdkMetadata;
use crate::ui::InstallUi;
use anyhow::{Context, bail};
use semver_rs::compare;
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::env;
use std::path::{Path, PathBuf};

const MARKER_FILE: &str = ".jlo-managed";

/// What a `jlo clean` run did, so the caller owns the presentation and this
/// function owns only the filesystem work.
#[derive(Debug, Default)]
pub(crate) struct CleanReport {
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

impl CleanReport {
    pub(crate) fn removed_count(&self) -> usize {
        self.removed.iter().map(|(_, v)| v.len()).sum()
    }
}

/// A JDK found in the install directory, identified by its semver directory name.
pub(crate) struct InstalledJdk {
    pub version: String,
    pub major: i64,
    /// Whether the JDK carries the `.jlo-managed` marker, i.e. whether `jlo
    /// clean` is allowed to remove it.
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
    /// The real store for this machine (see ADR 0005).
    pub(crate) fn discover() -> anyhow::Result<Self> {
        let home = env::home_dir().context("could not determine home directory")?;
        Ok(Self::at(base_dir_for(env::consts::OS, &home)))
    }

    /// A store rooted at an arbitrary directory. The test adapter.
    pub(crate) fn at(base: impl Into<PathBuf>) -> Self {
        Self { base: base.into() }
    }

    /// The install directory itself. Needed only by PATH rewriting, which has
    /// to know which PATH entries J'Lo owns.
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

    /// The newest installed JDK whose version matches `major`, if any.
    pub(crate) fn find_matching(&self, major: &str) -> Option<PathBuf> {
        let mut matching_versions = self.scan().ok()?;
        matching_versions.retain(|candidate| {
            candidate
                .name
                .as_deref()
                .is_some_and(|name| name.starts_with(major))
        });

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

    /// How many installs `jlo clean` would remove: every managed JDK that is
    /// not the newest of its major.
    ///
    /// The read-only counterpart to [`Self::clean`], so `jlo update` can point
    /// at `jlo clean` after superseding a minor without deleting anything
    /// itself.
    pub(crate) fn superseded_count(&self) -> anyhow::Result<usize> {
        let mut newest_seen: HashSet<i64> = HashSet::new();
        let mut superseded = 0;

        // `list` yields newest first, so the first managed JDK of a major is
        // the one `clean` keeps and every later one is superseded.
        for jdk in self.list()?.into_iter().filter(|jdk| jdk.managed) {
            if !newest_seen.insert(jdk.major) {
                superseded += 1;
            }
        }

        Ok(superseded)
    }

    /// Remove every managed JDK that is not the newest of its major.
    pub(crate) fn clean(&self) -> anyhow::Result<CleanReport> {
        // collector major versions
        let mut installed_jdks: HashMap<i64, Vec<Candidate>> = HashMap::new();
        let mut report = CleanReport::default();

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

        // Exactly what `clean` would remove: two old 21s, no 17.
        assert_eq!(JdkStore::at(dir.path()).superseded_count().unwrap(), 2);
    }

    #[test]
    fn superseded_count_ignores_unmanaged() {
        let dir = tempdir().unwrap();
        create_jdk_dir(dir.path(), "21.0.1+12", false);
        create_jdk_dir(dir.path(), "21.0.3+9", true);

        // `clean` never touches an unmanaged install, so counting one would
        // point at a `jlo clean` that then removes nothing.
        assert_eq!(JdkStore::at(dir.path()).superseded_count().unwrap(), 0);
    }

    #[test]
    fn superseded_count_on_missing_base_dir() {
        let dir = tempdir().unwrap();
        let missing = dir.path().join("never-installed");

        assert_eq!(JdkStore::at(&missing).superseded_count().unwrap(), 0);
    }

    // -- clean --

    #[test]
    fn clean_removes_older_versions() {
        let dir = tempdir().unwrap();
        create_jdk_dir(dir.path(), "21.0.1+12", true);
        create_jdk_dir(dir.path(), "21.0.3+9", true);
        create_jdk_dir(dir.path(), "17.0.2+8", true);

        JdkStore::at(dir.path()).clean().unwrap();

        // 21.0.3+9 kept, 21.0.1+12 removed, 17.0.2+8 kept (only version for major 17)
        assert!(dir.path().join("21.0.3+9").exists());
        assert!(!dir.path().join("21.0.1+12").exists());
        assert!(dir.path().join("17.0.2+8").exists());
    }

    #[test]
    fn clean_ignores_unmanaged() {
        let dir = tempdir().unwrap();
        create_jdk_dir(dir.path(), "21.0.1+12", false); // no marker
        create_jdk_dir(dir.path(), "21.0.3+9", true);

        JdkStore::at(dir.path()).clean().unwrap();

        // Unmanaged dir should not be touched
        assert!(dir.path().join("21.0.1+12").exists());
        assert!(dir.path().join("21.0.3+9").exists());
    }

    /// A `HashMap` yields its keys in an arbitrary order, so the majors used to
    /// print differently from one run to the next over the same directory.
    #[test]
    fn clean_reports_majors_newest_first() {
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

        let report = JdkStore::at(dir.path()).clean().unwrap();

        let majors: Vec<i64> = report.removed.iter().map(|(major, _)| *major).collect();
        assert_eq!(majors, vec![25, 21, 17]);
        assert_eq!(report.removed_count(), 3);
        assert_eq!(report.removed[0].1, vec!["25.0.1+1"]);
        assert!(report.failures.is_empty());
    }

    #[test]
    fn clean_counts_unmanaged_without_removing_them() {
        let dir = tempdir().unwrap();
        create_jdk_dir(dir.path(), "21.0.1+12", false);
        create_jdk_dir(dir.path(), "21.0.3+9", false);
        create_jdk_dir(dir.path(), "17.0.2+8", true);

        let report = JdkStore::at(dir.path()).clean().unwrap();

        assert_eq!(report.skipped_unmanaged, 2);
        assert_eq!(report.removed_count(), 0);
        assert!(dir.path().join("21.0.1+12").exists());
    }

    #[test]
    fn clean_single_version_kept() {
        let dir = tempdir().unwrap();
        create_jdk_dir(dir.path(), "21.0.3+9", true);

        JdkStore::at(dir.path()).clean().unwrap();
        assert!(dir.path().join("21.0.3+9").exists());
    }

    /// `jlo clean` says so when the install directory cannot be read, rather
    /// than reporting an empty run.
    #[test]
    fn clean_missing_base_dir_is_an_error() {
        let dir = tempdir().unwrap();
        let missing = dir.path().join("never-installed");

        let err = JdkStore::at(&missing).clean().unwrap_err();
        assert!(
            format!("{err:#}").contains("could not read JDK base directory"),
            "{err:#}"
        );
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
}
