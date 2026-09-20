use crate::ui::InstallUi;
use anyhow::{Context, bail};
use semver_rs::compare;
use sha2::{Digest, Sha256};
use std::cmp::Ordering;
use std::env;
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use ureq::Agent;
use ureq::tls::{RootCerts, TlsConfig, TlsProvider};

const MARKER_FILE: &str = ".jlo-managed";
const USER_AGENT: &str = concat!("J'Lo/", env!("CARGO_PKG_VERSION"));

fn sort_by_semver_desc(paths: &mut [PathBuf]) {
    paths.sort_by(|a, b| {
        let a_str = a.file_name().and_then(|name| name.to_str()).unwrap_or("");
        let b_str = b.file_name().and_then(|name| name.to_str()).unwrap_or("");
        compare(b_str, a_str, None).unwrap_or(Ordering::Equal)
    });
}

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

pub(crate) fn clean_jdks(jdk_base: &Path) -> anyhow::Result<CleanReport> {
    // collector major versions
    let mut installed_jdks: std::collections::HashMap<i64, Vec<PathBuf>> =
        std::collections::HashMap::new();
    let mut report = CleanReport::default();
    let entries = std::fs::read_dir(jdk_base)
        .with_context(|| format!("Can't read JDK base directory {jdk_base:?}"))?;

    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(file_name) = path.file_name().and_then(|n| n.to_str()) else {
            crate::ui::warning!("Ignoring directory with invalid name: {path:?}");
            continue;
        };
        if !path.join(MARKER_FILE).exists() {
            // skip directories not managed by jlo
            report.skipped_unmanaged += 1;
            continue;
        }
        let Ok(semver) = semver_rs::parse(file_name, None) else {
            crate::ui::warning!("Ignoring non-semver directory: {path:?}");
            continue;
        };
        installed_jdks.entry(semver.major).or_default().push(path);
    }

    // A `HashMap` hands back its keys in an arbitrary order, which made two runs
    // over the same directory print the majors differently. Sort so the output
    // is stable and matches `jlo list` (newest major first).
    let mut majors: Vec<i64> = installed_jdks.keys().copied().collect();
    majors.sort_unstable_by(|a, b| b.cmp(a));

    for major in majors {
        let Some(paths) = installed_jdks.get_mut(&major) else {
            continue;
        };
        sort_by_semver_desc(paths);

        if paths.len() <= 1 {
            continue;
        }

        // Record what was *actually* deleted. Announcing the removals up front
        // meant a failure below turned the line above it into a false claim.
        let mut removed = Vec::new();
        for old_jdk in &paths[1..] {
            let name = old_jdk
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("unknown")
                .to_string();
            match std::fs::remove_dir_all(old_jdk) {
                Ok(()) => removed.push(name),
                Err(e) => report
                    .failures
                    .push(format!("could not remove {old_jdk:?}: {e}")),
            }
        }

        if !removed.is_empty() {
            report.removed.push((major, removed));
        }
    }

    Ok(report)
}

pub(crate) fn find_suitable_jdk(jdk_base: &Path, required_version: &str) -> Option<PathBuf> {
    let entries = std::fs::read_dir(jdk_base).ok()?;

    let mut matching_versions: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.is_dir()
                && path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with(required_version))
        })
        .collect();

    sort_by_semver_desc(&mut matching_versions);

    matching_versions.first().cloned()
}

/// `semver_rs::parse` is lenient - it happily turns any junk into `0.0.0` - so a
/// directory only counts as a JDK when it parses to a real major version.
fn is_jdk_version_dir(name: &str) -> bool {
    semver_rs::parse(name, None).is_ok_and(|sv| sv.major > 0)
}

/// A JDK found in the install directory, identified by its semver directory name.
pub(crate) struct InstalledJdk {
    pub version: String,
    pub major: i64,
    /// Whether the JDK carries the `.jlo-managed` marker, i.e. whether `jlo
    /// clean` is allowed to remove it.
    pub managed: bool,
}

/// List every JDK in `jdk_base` whose directory name parses as a semver, newest
/// first. A missing base directory is not an error - it just means nothing has
/// been installed yet.
pub(crate) fn find_installed_jdks(jdk_base: &Path) -> anyhow::Result<Vec<InstalledJdk>> {
    let entries = match std::fs::read_dir(jdk_base) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => {
            return Err(e).with_context(|| format!("Can't read JDK base directory {jdk_base:?}"));
        }
    };

    let mut paths: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.is_dir()
                && path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(is_jdk_version_dir)
        })
        .collect();

    sort_by_semver_desc(&mut paths);

    Ok(paths
        .into_iter()
        .filter_map(|path| {
            let version = path.file_name().and_then(|name| name.to_str())?.to_string();
            let major = semver_rs::parse(&version, None).ok()?.major;
            Some(InstalledJdk {
                version,
                major,
                managed: path.join(MARKER_FILE).exists(),
            })
        })
        .collect())
}

pub(crate) fn find_installed_major_versions(jdk_base: &Path) -> anyhow::Result<Vec<i64>> {
    let mut major_versions = std::collections::HashSet::new();

    let entries = std::fs::read_dir(jdk_base)
        .with_context(|| format!("Can't read JDK base directory {jdk_base:?}"))?;

    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(file_name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let Ok(semver) = semver_rs::parse(file_name, None) else {
            continue;
        };
        major_versions.insert(semver.major);
    }

    let mut major_versions_vec: Vec<i64> = major_versions.into_iter().collect();
    major_versions_vec.sort_unstable();
    Ok(major_versions_vec)
}

pub(crate) fn install_jdk(
    jdk_metadata: &JdkMetadata,
    source_dir: &Path,
    dest_dir: &Path,
    ui: &InstallUi,
) -> anyhow::Result<()> {
    // Validate extracted path
    let extracted_jdk_path =
        find_jdk_path(jdk_metadata, source_dir).context("Could not find JDK directory")?;

    // Create destination directory
    ui.start_install();
    std::fs::create_dir_all(
        dest_dir
            .parent()
            .context("destination directory has no parent")?,
    )
    .context("could not create destination directory")?;

    // Move extracted JDK to final location
    std::fs::rename(extracted_jdk_path, dest_dir).context("could not move JDK to destination")?;

    // touch a file to indicate that this directory is managed by jlo
    std::fs::File::create(dest_dir.join(MARKER_FILE)).context("could not create marker file")?;

    Ok(())
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
            bail!("Error: java executable is missing at: {java_bin:?}");
        }
    } else {
        let java_bin = extracted_jdk_path.join("bin").join("java");
        if !java_bin.exists() {
            bail!("Error: java executable is missing at: {java_bin:?}");
        }
    }

    Ok(extracted_jdk_path)
}

pub(crate) fn find_installed_jdk(
    jdk_metadata: &JdkMetadata,
    jdk_base_path: &Path,
) -> Option<PathBuf> {
    let extracted_jdk_path = jdk_base_path.join(&jdk_metadata.semver);
    if extracted_jdk_path.exists() {
        Some(extracted_jdk_path)
    } else {
        None
    }
}

#[derive(Debug)]
pub(crate) struct JdkMetadata {
    pub semver: String,
    pub release_name: String,
    pub package_name: String,
    pub download_link: String,
    pub checksum: String,
}

pub(crate) const ADOPTIUM_API_URL: &str = "https://api.adoptium.net";

/// One entry of the response from `/v3/assets/latest/...` — the shape the
/// Adoptium API promises for a JDK build.
#[derive(serde::Deserialize)]
struct Asset {
    version: AssetVersion,
    release_name: String,
    binary: AssetBinary,
}

#[derive(serde::Deserialize)]
struct AssetVersion {
    semver: String,
}

#[derive(serde::Deserialize)]
struct AssetBinary {
    package: AssetPackage,
}

#[derive(serde::Deserialize)]
struct AssetPackage {
    name: String,
    link: String,
    checksum: String,
}

impl TryFrom<Asset> for JdkMetadata {
    type Error = anyhow::Error;

    fn try_from(asset: Asset) -> anyhow::Result<Self> {
        let metadata = JdkMetadata {
            semver: asset.version.semver,
            release_name: asset.release_name,
            package_name: asset.binary.package.name,
            download_link: asset.binary.package.link,
            checksum: asset.binary.package.checksum,
        };
        if metadata.semver.is_empty()
            || metadata.release_name.is_empty()
            || metadata.package_name.is_empty()
            || metadata.download_link.is_empty()
            || metadata.checksum.is_empty()
        {
            bail!("Incomplete metadata received from API.");
        }
        Ok(metadata)
    }
}

/// The shape of `/v3/info/available_releases`.
#[derive(serde::Deserialize)]
struct AvailableReleases {
    available_releases: Vec<i64>,
    #[serde(default)]
    available_lts_releases: Vec<i64>,
}

/// A JDK release Adoptium offers for *this* OS and architecture.
#[derive(Debug)]
pub(crate) struct RemoteJdk {
    pub version: String,
    pub major: i64,
    pub lts: bool,
}

/// The single point of contact with Adoptium: discovering available releases,
/// fetching JDK metadata, and downloading packages. `base_url` covers the two
/// API endpoints; downloads follow whatever URL the metadata hands back.
pub(crate) struct AdoptiumClient {
    agent: Agent,
    base_url: String,
}

impl AdoptiumClient {
    pub(crate) fn new(base_url: impl Into<String>) -> Self {
        // Statuses are inspected explicitly below, so keep ureq from turning a
        // non-2xx response into an error and losing the message wording.
        // ureq defaults its TLS provider to Rustls regardless of which TLS
        // feature is enabled, and panics on the first https request if that
        // provider was not compiled in. It also defaults to bundled Mozilla
        // roots. Select native-tls with the platform trust store, which is what
        // jlo has always used and what TLS-intercepting corporate proxies need.
        let agent: Agent = Agent::config_builder()
            .user_agent(USER_AGENT)
            .http_status_as_error(false)
            .tls_config(
                TlsConfig::builder()
                    .provider(TlsProvider::NativeTls)
                    .root_certs(RootCerts::PlatformVerifier)
                    .build(),
            )
            .build()
            .into();
        Self {
            agent,
            base_url: base_url.into(),
        }
    }

    pub(crate) fn fetch_metadata(&self, java_version: &str) -> anyhow::Result<JdkMetadata> {
        let api_url = self.latest_asset_url(java_version)?;

        let asset = self.fetch_latest_asset(&api_url)?.with_context(|| {
            format!(
                "No matching JDK found for the specified version and system architecture.\nTried to fetch metadata from: {api_url}"
            )
        })?;

        asset.try_into()
    }

    fn latest_asset_url(&self, java_version: &str) -> anyhow::Result<String> {
        Ok(format!(
            "{base_url}/v3/assets/latest/{java_version}/hotspot?architecture={arch}&image_type=jdk&os={os}&vendor=eclipse",
            base_url = self.base_url,
            arch = jdk_arch()?,
            os = jdk_os()?
        ))
    }

    /// Fetch the newest asset for a major version, or `None` when Adoptium has
    /// no build for this OS/architecture. That case is *not* an HTTP error: the
    /// API answers `200` with an empty array.
    fn fetch_latest_asset(&self, api_url: &str) -> anyhow::Result<Option<Asset>> {
        let mut response = self
            .agent
            .get(api_url)
            .call()
            .context("Could not fetch metadata from API")?;

        if !response.status().is_success() {
            bail!(
                "Failed to fetch metadata from API: HTTP {}",
                response.status()
            );
        }

        let assets: Vec<Asset> = response
            .body_mut()
            .read_json()
            .context("Failed to parse JSON response")?;

        Ok(assets.into_iter().next())
    }

    /// Every JDK Adoptium can install on this machine, newest first.
    ///
    /// Costs one request for the major-version list plus one per major. Done
    /// serially that is ~4s, so the per-major lookups are fanned out across
    /// threads sharing the pooled client.
    pub(crate) fn available_jdks(&self) -> anyhow::Result<Vec<RemoteJdk>> {
        let releases = self.fetch_available_releases()?;
        let lts: std::collections::HashSet<i64> =
            releases.available_lts_releases.into_iter().collect();

        let looked_up: Vec<(i64, anyhow::Result<Option<String>>)> = std::thread::scope(|scope| {
            let handles: Vec<_> = releases
                .available_releases
                .iter()
                .map(|&major| (major, scope.spawn(move || self.latest_version(major))))
                .collect();

            handles
                .into_iter()
                .map(|(major, handle)| {
                    let result = handle
                        .join()
                        .unwrap_or_else(|_| bail!("lookup thread panicked"));
                    (major, result)
                })
                .collect()
        });

        let mut jdks = Vec::new();
        for (major, result) in looked_up {
            match result {
                Ok(Some(version)) => jdks.push(RemoteJdk {
                    version,
                    major,
                    lts: lts.contains(&major),
                }),
                // No build for this OS/architecture - nothing to offer.
                Ok(None) => {}
                // One major failing should not cost the user the whole listing.
                Err(e) => crate::ui::warning!("could not look up JDK {major}: {e:#}"),
            }
        }

        jdks.sort_by(|a, b| compare(&b.version, &a.version, None).unwrap_or(Ordering::Equal));
        Ok(jdks)
    }

    fn latest_version(&self, major: i64) -> anyhow::Result<Option<String>> {
        let api_url = self.latest_asset_url(&major.to_string())?;
        Ok(self
            .fetch_latest_asset(&api_url)?
            .map(|asset| asset.version.semver))
    }

    fn fetch_available_releases(&self) -> anyhow::Result<AvailableReleases> {
        let mut response = self
            .agent
            .get(format!("{}/v3/info/available_releases", self.base_url))
            .call()
            .context("Could not fetch available releases from API")?;

        if !response.status().is_success() {
            bail!(
                "Failed to fetch available releases from API: HTTP {}",
                response.status()
            );
        }

        response
            .body_mut()
            .read_json()
            .context("Failed to parse JSON response")
    }

    pub(crate) fn latest_major(&self) -> anyhow::Result<String> {
        let releases = self.fetch_available_releases()?;

        let latest = releases
            .available_releases
            .into_iter()
            .max()
            .context("No available releases found.")?;

        Ok(latest.to_string())
    }

    pub(crate) fn download(
        &self,
        metadata: &JdkMetadata,
        file: &mut File,
        ui: &InstallUi,
    ) -> anyhow::Result<()> {
        let mut response = self.agent.get(&metadata.download_link).call()?;

        if !response.status().is_success() {
            bail!(
                "Failed to download {} from {}: HTTP {}",
                metadata.package_name,
                metadata.download_link,
                response.status()
            );
        }

        let total_size = response
            .body()
            .content_length()
            .context("Failed to get content length")?;

        ui.start_download(total_size);

        let mut hasher = Sha256::new();

        let mut downloaded: u64 = 0;
        let mut buffer = [0; 8192];
        let mut reader = response.body_mut().as_reader();
        loop {
            let n = reader
                .read(&mut buffer)
                .context("Could not read package data from response")?;
            if n == 0 {
                break;
            }
            file.write_all(&buffer[..n])?;
            downloaded += n as u64;
            ui.set_downloaded(downloaded);
            hasher.update(&buffer[..n]);
        }

        let hash = hex::encode(hasher.finalize());
        if hash != metadata.checksum {
            bail!(
                "Checksum mismatch: expected {}, got {}.",
                metadata.checksum,
                hash
            );
        }

        Ok(())
    }
}

fn jdk_os() -> anyhow::Result<&'static str> {
    match env::consts::OS {
        "linux" | "windows" | "solaris" | "aix" => Ok(env::consts::OS),
        "macos" => Ok("mac"),
        _ => bail!("Unsupported OS: {}", env::consts::OS),
    }
}

fn jdk_arch() -> anyhow::Result<&'static str> {
    match env::consts::ARCH {
        "x86_64" => Ok("x64"),
        "x86" => Ok("x32"),
        "powerpc64" => {
            if cfg!(target_endian = "little") {
                Ok("ppc64le")
            } else {
                Ok("ppc64")
            }
        }
        "s390x" | "arm" | "aarch64" => Ok(env::consts::ARCH),
        "sparc64" => Ok("sparcv9"),
        "riscv64" => Ok("riscv64"),
        _ => bail!("Unsupported architecture: {}", env::consts::ARCH),
    }
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

    // -- jdk_os / jdk_arch smoke tests --

    #[test]
    fn jdk_os_returns_known_value() {
        let os = jdk_os().unwrap();
        assert!(
            ["linux", "mac", "windows", "solaris", "aix"].contains(&os),
            "unexpected os: {os}"
        );
    }

    #[test]
    fn jdk_arch_returns_known_value() {
        let arch = jdk_arch().unwrap();
        assert!(
            [
                "x64", "x32", "aarch64", "arm", "s390x", "ppc64", "ppc64le", "sparcv9", "riscv64"
            ]
            .contains(&arch),
            "unexpected arch: {arch}"
        );
    }

    // -- find_suitable_jdk --

    #[test]
    fn find_suitable_jdk_finds_latest() {
        let dir = tempdir().unwrap();
        create_jdk_dir(dir.path(), "21.0.1+12", true);
        create_jdk_dir(dir.path(), "21.0.3+9", true);
        create_jdk_dir(dir.path(), "17.0.2+8", true);

        let result = find_suitable_jdk(dir.path(), "21");
        assert_eq!(
            result.unwrap().file_name().unwrap().to_str().unwrap(),
            "21.0.3+9"
        );
    }

    #[test]
    fn find_suitable_jdk_no_match() {
        let dir = tempdir().unwrap();
        create_jdk_dir(dir.path(), "17.0.2+8", true);

        assert!(find_suitable_jdk(dir.path(), "21").is_none());
    }

    #[test]
    fn find_suitable_jdk_empty_dir() {
        let dir = tempdir().unwrap();
        assert!(find_suitable_jdk(dir.path(), "21").is_none());
    }

    // -- find_installed_jdk --

    #[test]
    fn find_installed_jdk_exists() {
        let dir = tempdir().unwrap();
        create_jdk_dir(dir.path(), "21.0.3+9", true);

        let metadata = JdkMetadata {
            semver: "21.0.3+9".to_string(),
            release_name: String::new(),
            package_name: String::new(),
            download_link: String::new(),
            checksum: String::new(),
        };
        assert!(find_installed_jdk(&metadata, dir.path()).is_some());
    }

    #[test]
    fn find_installed_jdk_not_exists() {
        let dir = tempdir().unwrap();

        let metadata = JdkMetadata {
            semver: "21.0.3+9".to_string(),
            release_name: String::new(),
            package_name: String::new(),
            download_link: String::new(),
            checksum: String::new(),
        };
        assert!(find_installed_jdk(&metadata, dir.path()).is_none());
    }

    // -- find_installed_major_versions --

    #[test]
    fn find_installed_major_versions_discovers_majors() {
        let dir = tempdir().unwrap();
        create_jdk_dir(dir.path(), "21.0.1+12", true);
        create_jdk_dir(dir.path(), "21.0.3+9", true);
        create_jdk_dir(dir.path(), "17.0.2+8", true);
        create_jdk_dir(dir.path(), "11.0.1+13", true);

        let versions = find_installed_major_versions(dir.path()).unwrap();
        assert_eq!(versions, vec![11, 17, 21]);
    }

    #[test]
    fn find_installed_major_versions_ignores_non_dirs() {
        let dir = tempdir().unwrap();
        create_jdk_dir(dir.path(), "21.0.1+12", true);
        // Plain file should be skipped
        fs::write(dir.path().join("some-file.txt"), "").unwrap();

        let versions = find_installed_major_versions(dir.path()).unwrap();
        assert_eq!(versions, vec![21]);
    }

    #[test]
    fn find_installed_major_versions_empty_dir() {
        let dir = tempdir().unwrap();
        let versions = find_installed_major_versions(dir.path()).unwrap();
        assert!(versions.is_empty());
    }

    // -- find_installed_jdks --

    #[test]
    fn find_installed_jdks_sorted_newest_first() {
        let dir = tempdir().unwrap();
        create_jdk_dir(dir.path(), "21.0.9+7", true);
        create_jdk_dir(dir.path(), "21.0.12+7", true);
        create_jdk_dir(dir.path(), "17.0.13+11", true);

        let jdks = find_installed_jdks(dir.path()).unwrap();
        let versions: Vec<_> = jdks.iter().map(|j| j.version.as_str()).collect();
        assert_eq!(versions, vec!["21.0.12+7", "21.0.9+7", "17.0.13+11"]);
    }

    #[test]
    fn find_installed_jdks_reports_managed_flag() {
        let dir = tempdir().unwrap();
        create_jdk_dir(dir.path(), "21.0.12+7", true);
        create_jdk_dir(dir.path(), "17.0.13+11", false);

        let jdks = find_installed_jdks(dir.path()).unwrap();
        assert_eq!(jdks[0].version, "21.0.12+7");
        assert_eq!(jdks[0].major, 21);
        assert!(jdks[0].managed);
        assert_eq!(jdks[1].version, "17.0.13+11");
        assert_eq!(jdks[1].major, 17);
        assert!(!jdks[1].managed);
    }

    #[test]
    fn find_installed_jdks_ignores_non_semver_and_files() {
        let dir = tempdir().unwrap();
        create_jdk_dir(dir.path(), "21.0.12+7", true);
        fs::create_dir(dir.path().join("not-a-jdk")).unwrap();
        fs::write(dir.path().join("21.0.1+9"), "a file, not a directory").unwrap();

        let jdks = find_installed_jdks(dir.path()).unwrap();
        assert_eq!(jdks.len(), 1);
        assert_eq!(jdks[0].version, "21.0.12+7");
    }

    #[test]
    fn find_installed_jdks_missing_base_dir_is_empty() {
        let dir = tempdir().unwrap();
        let missing = dir.path().join("nothing-installed-here");
        assert!(find_installed_jdks(&missing).unwrap().is_empty());
    }

    // -- clean_jdks --

    #[test]
    fn clean_jdks_removes_older_versions() {
        let dir = tempdir().unwrap();
        create_jdk_dir(dir.path(), "21.0.1+12", true);
        create_jdk_dir(dir.path(), "21.0.3+9", true);
        create_jdk_dir(dir.path(), "17.0.2+8", true);

        clean_jdks(dir.path()).unwrap();

        // 21.0.3+9 kept, 21.0.1+12 removed, 17.0.2+8 kept (only version for major 17)
        assert!(dir.path().join("21.0.3+9").exists());
        assert!(!dir.path().join("21.0.1+12").exists());
        assert!(dir.path().join("17.0.2+8").exists());
    }

    #[test]
    fn clean_jdks_ignores_unmanaged() {
        let dir = tempdir().unwrap();
        create_jdk_dir(dir.path(), "21.0.1+12", false); // no marker
        create_jdk_dir(dir.path(), "21.0.3+9", true);

        clean_jdks(dir.path()).unwrap();

        // Unmanaged dir should not be touched
        assert!(dir.path().join("21.0.1+12").exists());
        assert!(dir.path().join("21.0.3+9").exists());
    }

    /// A `HashMap` yields its keys in an arbitrary order, so the majors used to
    /// print differently from one run to the next over the same directory.
    #[test]
    fn clean_jdks_reports_majors_newest_first() {
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

        let report = clean_jdks(dir.path()).unwrap();

        let majors: Vec<i64> = report.removed.iter().map(|(major, _)| *major).collect();
        assert_eq!(majors, vec![25, 21, 17]);
        assert_eq!(report.removed_count(), 3);
        assert_eq!(report.removed[0].1, vec!["25.0.1+1"]);
        assert!(report.failures.is_empty());
    }

    #[test]
    fn clean_jdks_counts_unmanaged_without_removing_them() {
        let dir = tempdir().unwrap();
        create_jdk_dir(dir.path(), "21.0.1+12", false);
        create_jdk_dir(dir.path(), "21.0.3+9", false);
        create_jdk_dir(dir.path(), "17.0.2+8", true);

        let report = clean_jdks(dir.path()).unwrap();

        assert_eq!(report.skipped_unmanaged, 2);
        assert_eq!(report.removed_count(), 0);
        assert!(dir.path().join("21.0.1+12").exists());
    }

    #[test]
    fn clean_jdks_single_version_kept() {
        let dir = tempdir().unwrap();
        create_jdk_dir(dir.path(), "21.0.3+9", true);

        clean_jdks(dir.path()).unwrap();
        assert!(dir.path().join("21.0.3+9").exists());
    }

    // -- find_jdk_path --

    #[test]
    fn find_jdk_path_valid() {
        let dir = tempdir().unwrap();
        let release = "jdk-21.0.3+9";

        let jdk_dir = if env::consts::OS == "macos" {
            dir.path().join(release).join("Contents").join("Home")
        } else {
            dir.path().join(release)
        };
        fs::create_dir_all(jdk_dir.join("bin")).unwrap();
        let java_name = if env::consts::OS == "windows" {
            "java.exe"
        } else {
            "java"
        };
        fs::write(jdk_dir.join("bin").join(java_name), "").unwrap();

        let metadata = JdkMetadata {
            semver: String::new(),
            release_name: release.to_string(),
            package_name: String::new(),
            download_link: String::new(),
            checksum: String::new(),
        };

        let result = find_jdk_path(&metadata, dir.path()).unwrap();
        assert_eq!(result, jdk_dir);
    }

    #[test]
    fn find_jdk_path_missing_java_binary() {
        let dir = tempdir().unwrap();
        let release = "jdk-21.0.3+9";

        let jdk_dir = if env::consts::OS == "macos" {
            dir.path().join(release).join("Contents").join("Home")
        } else {
            dir.path().join(release)
        };
        // Create dir structure but no java binary
        fs::create_dir_all(jdk_dir.join("bin")).unwrap();

        let metadata = JdkMetadata {
            semver: String::new(),
            release_name: release.to_string(),
            package_name: String::new(),
            download_link: String::new(),
            checksum: String::new(),
        };

        let result = find_jdk_path(&metadata, dir.path());
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("java executable is missing")
        );
    }

    // -- install_jdk --

    #[test]
    fn install_jdk_moves_and_marks() {
        let source_dir = tempdir().unwrap();
        let dest_parent = tempdir().unwrap();
        let release = "jdk-21.0.3+9";

        // Create mock extracted JDK in source
        let extracted = if env::consts::OS == "macos" {
            source_dir
                .path()
                .join(release)
                .join("Contents")
                .join("Home")
        } else {
            source_dir.path().join(release)
        };
        fs::create_dir_all(extracted.join("bin")).unwrap();
        let java_name = if env::consts::OS == "windows" {
            "java.exe"
        } else {
            "java"
        };
        fs::write(extracted.join("bin").join(java_name), "").unwrap();

        let metadata = JdkMetadata {
            semver: "21.0.3+9".to_string(),
            release_name: release.to_string(),
            package_name: String::new(),
            download_link: String::new(),
            checksum: String::new(),
        };

        let dest = dest_parent.path().join("21.0.3+9");
        install_jdk(
            &metadata,
            source_dir.path(),
            &dest,
            &InstallUi::hidden("test"),
        )
        .unwrap();

        assert!(dest.exists());
        assert!(dest.join(MARKER_FILE).exists());
        assert!(dest.join("bin").join(java_name).exists());
    }
}

#[cfg(test)]
mod client_tests {
    use super::*;
    use std::io::Read;

    const ASSETS_FIXTURE: &str = include_str!("../tests/fixtures/assets_latest.json");
    const RELEASES_FIXTURE: &str = include_str!("../tests/fixtures/available_releases.json");
    const FAKE_PACKAGE: &[u8] = b"fake-jdk-package-bytes";
    const FAKE_PACKAGE_CHECKSUM: &str =
        "c24e5c702f84a86d7be63da2e942872b1cc66a2a35c0168a18042170119201b0";

    fn metadata_mock(
        server: &mut mockito::ServerGuard,
        status: usize,
        body: &str,
    ) -> mockito::Mock {
        server
            .mock(
                "GET",
                mockito::Matcher::Regex(r"^/v3/assets/latest/21/hotspot".to_string()),
            )
            .match_query(mockito::Matcher::Any)
            .with_status(status)
            .with_body(body)
            .create()
    }

    fn fixture_with_package_field(field: &str, value: serde_json::Value) -> String {
        let mut json: serde_json::Value = serde_json::from_str(ASSETS_FIXTURE).unwrap();
        json[0]["binary"]["package"][field] = value;
        json.to_string()
    }

    fn fixture_without_package_field(field: &str) -> String {
        let mut json: serde_json::Value = serde_json::from_str(ASSETS_FIXTURE).unwrap();
        json[0]["binary"]["package"]
            .as_object_mut()
            .unwrap()
            .remove(field);
        json.to_string()
    }

    fn fake_metadata(download_link: String, checksum: &str) -> JdkMetadata {
        JdkMetadata {
            semver: "21.0.11+10.0.LTS".to_string(),
            release_name: "jdk-21.0.11+10".to_string(),
            package_name: "fake.tar.gz".to_string(),
            download_link,
            checksum: checksum.to_string(),
        }
    }

    #[test]
    fn fetch_metadata_happy_path() {
        let mut server = mockito::Server::new();
        let _m = metadata_mock(&mut server, 200, ASSETS_FIXTURE);

        let client = AdoptiumClient::new(server.url());
        let metadata = client.fetch_metadata("21").unwrap();

        assert_eq!(metadata.semver, "21.0.11+10.0.LTS");
        assert_eq!(metadata.release_name, "jdk-21.0.11+10");
        assert_eq!(
            metadata.package_name,
            "OpenJDK21U-jdk_aarch64_mac_hotspot_21.0.11_10.tar.gz"
        );
        assert!(
            metadata
                .download_link
                .starts_with("https://github.com/adoptium/temurin21-binaries/")
        );
        assert_eq!(
            metadata.checksum,
            "6ebcf221c9b41507b14c098e93c6ead6440b8d9bd154f8ec666c4c73abbdb201"
        );
    }

    #[test]
    fn fetch_metadata_http_error_reports_status() {
        let mut server = mockito::Server::new();
        let _m = metadata_mock(&mut server, 500, "boom");

        let client = AdoptiumClient::new(server.url());
        let err = client.fetch_metadata("21").unwrap_err();

        assert!(format!("{err:#}").contains("HTTP 500"), "got: {err:#}");
    }

    #[test]
    fn fetch_metadata_malformed_json_errors() {
        let mut server = mockito::Server::new();
        let _m = metadata_mock(&mut server, 200, "this is not json");

        let client = AdoptiumClient::new(server.url());
        let err = client.fetch_metadata("21").unwrap_err();

        assert!(
            format!("{err:#}").contains("Failed to parse JSON response"),
            "got: {err:#}"
        );
    }

    #[test]
    fn fetch_metadata_empty_array_means_no_matching_jdk() {
        let mut server = mockito::Server::new();
        let _m = metadata_mock(&mut server, 200, "[]");

        let client = AdoptiumClient::new(server.url());
        let err = client.fetch_metadata("21").unwrap_err();

        assert!(
            format!("{err:#}").contains("No matching JDK found"),
            "got: {err:#}"
        );
    }

    #[test]
    fn fetch_metadata_missing_field_names_the_field() {
        let mut server = mockito::Server::new();
        let body = fixture_without_package_field("checksum");
        let _m = metadata_mock(&mut server, 200, &body);

        let client = AdoptiumClient::new(server.url());
        let err = client.fetch_metadata("21").unwrap_err();

        assert!(format!("{err:#}").contains("checksum"), "got: {err:#}");
    }

    #[test]
    fn fetch_metadata_empty_field_is_incomplete() {
        let mut server = mockito::Server::new();
        let body = fixture_with_package_field("checksum", serde_json::Value::String(String::new()));
        let _m = metadata_mock(&mut server, 200, &body);

        let client = AdoptiumClient::new(server.url());
        let err = client.fetch_metadata("21").unwrap_err();

        assert!(
            format!("{err:#}").contains("Incomplete metadata"),
            "got: {err:#}"
        );
    }

    // -- available_jdks --

    fn releases_body(majors: &[i64], lts: &[i64]) -> String {
        serde_json::json!({
            "available_releases": majors,
            "available_lts_releases": lts,
        })
        .to_string()
    }

    fn asset_body(semver: &str) -> String {
        let mut json: serde_json::Value = serde_json::from_str(ASSETS_FIXTURE).unwrap();
        json[0]["version"]["semver"] = serde_json::Value::String(semver.to_string());
        json.to_string()
    }

    fn major_mock(
        server: &mut mockito::ServerGuard,
        major: i64,
        status: usize,
        body: &str,
    ) -> mockito::Mock {
        server
            .mock(
                "GET",
                mockito::Matcher::Regex(format!(r"^/v3/assets/latest/{major}/hotspot")),
            )
            .match_query(mockito::Matcher::Any)
            .with_status(status)
            .with_body(body)
            .expect_at_least(1)
            .create()
    }

    #[test]
    fn available_jdks_lists_latest_per_major_newest_first() {
        let mut server = mockito::Server::new();
        let _r = server
            .mock("GET", "/v3/info/available_releases")
            .with_body(releases_body(&[17, 21, 25], &[17, 21]))
            .create();
        let _a17 = major_mock(&mut server, 17, 200, &asset_body("17.0.20+101"));
        let _a21 = major_mock(&mut server, 21, 200, &asset_body("21.0.12+101.0.LTS"));
        let _a25 = major_mock(&mut server, 25, 200, &asset_body("25.0.4+101.0.LTS"));

        let client = AdoptiumClient::new(server.url());
        let jdks = client.available_jdks().unwrap();

        let rows: Vec<_> = jdks
            .iter()
            .map(|j| (j.version.as_str(), j.major, j.lts))
            .collect();
        assert_eq!(
            rows,
            vec![
                ("25.0.4+101.0.LTS", 25, false),
                ("21.0.12+101.0.LTS", 21, true),
                ("17.0.20+101", 17, true),
            ]
        );
    }

    #[test]
    fn available_jdks_skips_majors_without_a_build_for_this_platform() {
        let mut server = mockito::Server::new();
        let _r = server
            .mock("GET", "/v3/info/available_releases")
            .with_body(releases_body(&[16, 21], &[21]))
            .create();
        // Adoptium answers 200 with an empty array when it has no build for
        // this OS/architecture.
        let _a16 = major_mock(&mut server, 16, 200, "[]");
        let _a21 = major_mock(&mut server, 21, 200, &asset_body("21.0.12+101.0.LTS"));

        let client = AdoptiumClient::new(server.url());
        let jdks = client.available_jdks().unwrap();

        assert_eq!(jdks.len(), 1);
        assert_eq!(jdks[0].version, "21.0.12+101.0.LTS");
    }

    #[test]
    fn available_jdks_survives_a_single_major_failing() {
        let mut server = mockito::Server::new();
        let _r = server
            .mock("GET", "/v3/info/available_releases")
            .with_body(releases_body(&[17, 21], &[17, 21]))
            .create();
        let _a17 = major_mock(&mut server, 17, 500, "boom");
        let _a21 = major_mock(&mut server, 21, 200, &asset_body("21.0.12+101.0.LTS"));

        let client = AdoptiumClient::new(server.url());
        let jdks = client.available_jdks().unwrap();

        assert_eq!(jdks.len(), 1);
        assert_eq!(jdks[0].version, "21.0.12+101.0.LTS");
    }

    #[test]
    fn available_jdks_http_error_on_release_list_is_fatal() {
        let mut server = mockito::Server::new();
        let _r = server
            .mock("GET", "/v3/info/available_releases")
            .with_status(503)
            .with_body("nope")
            .create();

        let client = AdoptiumClient::new(server.url());
        let err = client.available_jdks().unwrap_err();

        assert!(format!("{err:#}").contains("HTTP 503"), "got: {err:#}");
    }

    #[test]
    fn available_jdks_without_lts_field_marks_nothing_lts() {
        let mut server = mockito::Server::new();
        let _r = server
            .mock("GET", "/v3/info/available_releases")
            .with_body(r#"{"available_releases":[21]}"#)
            .create();
        let _a21 = major_mock(&mut server, 21, 200, &asset_body("21.0.12+101.0.LTS"));

        let client = AdoptiumClient::new(server.url());
        let jdks = client.available_jdks().unwrap();

        assert_eq!(jdks.len(), 1);
        assert!(!jdks[0].lts);
    }

    #[test]
    fn latest_major_happy_path() {
        let mut server = mockito::Server::new();
        let _m = server
            .mock("GET", "/v3/info/available_releases")
            .with_body(RELEASES_FIXTURE)
            .create();

        let client = AdoptiumClient::new(server.url());
        assert_eq!(client.latest_major().unwrap(), "26");
    }

    #[test]
    fn latest_major_missing_key_errors() {
        let mut server = mockito::Server::new();
        let _m = server
            .mock("GET", "/v3/info/available_releases")
            .with_body("{}")
            .create();

        let client = AdoptiumClient::new(server.url());
        let err = client.latest_major().unwrap_err();

        assert!(
            format!("{err:#}").contains("available_releases"),
            "got: {err:#}"
        );
    }

    #[test]
    fn download_happy_path_writes_verified_file() {
        use std::io::Seek;

        let mut server = mockito::Server::new();
        let _m = server
            .mock("GET", "/pkg.tar.gz")
            .with_body(FAKE_PACKAGE)
            .create();

        let client = AdoptiumClient::new(server.url());
        let metadata = fake_metadata(
            format!("{}/pkg.tar.gz", server.url()),
            FAKE_PACKAGE_CHECKSUM,
        );

        let mut file = tempfile::tempfile().unwrap();
        client
            .download(&metadata, &mut file, &InstallUi::hidden("test"))
            .unwrap();

        file.rewind().unwrap();
        let mut content = Vec::new();
        file.read_to_end(&mut content).unwrap();
        assert_eq!(content, FAKE_PACKAGE);
    }

    #[test]
    fn download_checksum_mismatch_errors() {
        let mut server = mockito::Server::new();
        let _m = server
            .mock("GET", "/pkg.tar.gz")
            .with_body(FAKE_PACKAGE)
            .create();

        let client = AdoptiumClient::new(server.url());
        let metadata = fake_metadata(format!("{}/pkg.tar.gz", server.url()), "deadbeef");

        let mut file = tempfile::tempfile().unwrap();
        let err = client
            .download(&metadata, &mut file, &InstallUi::hidden("test"))
            .unwrap_err();

        assert!(
            format!("{err:#}").contains("Checksum mismatch"),
            "got: {err:#}"
        );
    }

    #[test]
    fn download_without_content_length_errors() {
        let mut server = mockito::Server::new();
        let _m = server
            .mock("GET", "/pkg.tar.gz")
            .with_chunked_body(|w| w.write_all(FAKE_PACKAGE))
            .create();

        let client = AdoptiumClient::new(server.url());
        let metadata = fake_metadata(
            format!("{}/pkg.tar.gz", server.url()),
            FAKE_PACKAGE_CHECKSUM,
        );

        let mut file = tempfile::tempfile().unwrap();
        let err = client
            .download(&metadata, &mut file, &InstallUi::hidden("test"))
            .unwrap_err();

        assert!(
            format!("{err:#}").contains("content length"),
            "got: {err:#}"
        );
    }

    #[test]
    fn latest_major_http_error_reports_status() {
        let mut server = mockito::Server::new();
        // valid body — the status alone must fail the call
        let _m = server
            .mock("GET", "/v3/info/available_releases")
            .with_status(500)
            .with_body(RELEASES_FIXTURE)
            .create();

        let client = AdoptiumClient::new(server.url());
        let err = client.latest_major().unwrap_err();

        assert!(format!("{err:#}").contains("HTTP 500"), "got: {err:#}");
    }

    #[test]
    fn download_http_error_reports_status_not_checksum() {
        let mut server = mockito::Server::new();
        let _m = server
            .mock("GET", "/pkg.tar.gz")
            .with_status(404)
            .with_body("not found")
            .create();

        let client = AdoptiumClient::new(server.url());
        let metadata = fake_metadata(
            format!("{}/pkg.tar.gz", server.url()),
            FAKE_PACKAGE_CHECKSUM,
        );

        let mut file = tempfile::tempfile().unwrap();
        let err = client
            .download(&metadata, &mut file, &InstallUi::hidden("test"))
            .unwrap_err();

        let msg = format!("{err:#}");
        assert!(msg.contains("HTTP 404"), "got: {msg}");
        assert!(!msg.contains("Checksum mismatch"), "got: {msg}");
    }
}
