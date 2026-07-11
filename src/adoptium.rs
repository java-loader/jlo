use crate::progress_bar::setup_progress_bar;
use anyhow::{Context, bail};
use reqwest::blocking::Client;
use semver_rs::compare;
use sha2::{Digest, Sha256};
use std::cmp::Ordering;
use std::env;
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

const MARKER_FILE: &str = ".jlo-managed";
const USER_AGENT: &str = concat!("J'Lo/", env!("CARGO_PKG_VERSION"));

fn sort_by_semver_desc(paths: &mut [PathBuf]) {
    paths.sort_by(|a, b| {
        let a_str = a.file_name().and_then(|name| name.to_str()).unwrap_or("");
        let b_str = b.file_name().and_then(|name| name.to_str()).unwrap_or("");
        compare(b_str, a_str, None).unwrap_or(Ordering::Equal)
    });
}

pub fn clean_jdks(jdk_base: &Path) -> anyhow::Result<()> {
    // collector major versions
    let mut installed_jdks: std::collections::HashMap<i64, Vec<PathBuf>> =
        std::collections::HashMap::new();
    let entries = std::fs::read_dir(jdk_base)
        .with_context(|| format!("Can't read JDK base directory {:?}", jdk_base))?;

    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        if !path.is_dir() {
            eprintln!("{:?} is not a directory", path);
            continue;
        }
        let file_name = match path.file_name().and_then(|n| n.to_str()) {
            Some(name) => name,
            None => {
                eprintln!("Ignoring directory with invalid name: {:?}", path);
                continue;
            }
        };
        if !path.join(MARKER_FILE).exists() {
            // skip directories not managed by jlo
            eprintln!("Ignoring non-jlo-managed directory: {:?}", path);
            continue;
        }
        let semver = match semver_rs::parse(file_name, None) {
            Ok(sv) => sv,
            Err(_) => {
                eprintln!("Ignoring non-semver directory: {:?}", path);
                continue;
            }
        };
        installed_jdks.entry(semver.major).or_default().push(path);
    }

    for (major, mut paths) in installed_jdks {
        sort_by_semver_desc(&mut paths);

        if paths.len() <= 1 {
            continue;
        }

        let kept = paths[0]
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown");
        let removed = paths[1..]
            .iter()
            .filter_map(|p| p.file_name().and_then(|n| n.to_str()))
            .collect::<Vec<_>>()
            .join(", ");

        eprintln!(
            "Keeping {} for JDK {}, but removing: {}",
            kept, major, removed
        );

        for old_jdk in &paths[1..] {
            if let Err(e) = std::fs::remove_dir_all(old_jdk) {
                eprintln!("Error removing old JDK {:?}: {}", old_jdk, e);
            }
        }
    }

    Ok(())
}

pub fn find_suitable_jdk(jdk_base: &Path, required_version: &str) -> Option<PathBuf> {
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

pub fn find_installed_major_versions(jdk_base: &Path) -> anyhow::Result<Vec<i64>> {
    let mut major_versions = std::collections::HashSet::new();

    let entries = std::fs::read_dir(jdk_base)
        .with_context(|| format!("Can't read JDK base directory {:?}", jdk_base))?;

    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let file_name = match path.file_name().and_then(|n| n.to_str()) {
            Some(name) => name,
            None => continue,
        };
        let semver = match semver_rs::parse(file_name, None) {
            Ok(sv) => sv,
            Err(_) => continue,
        };
        major_versions.insert(semver.major);
    }

    let mut major_versions_vec: Vec<i64> = major_versions.into_iter().collect();
    major_versions_vec.sort_unstable();
    Ok(major_versions_vec)
}

pub fn install_jdk(
    jdk_metadata: &JdkMetadata,
    source_dir: &Path,
    dest_dir: &Path,
) -> anyhow::Result<()> {
    // Validate extracted path
    let extracted_jdk_path =
        find_jdk_path(jdk_metadata, source_dir).context("Could not find JDK directory")?;

    // Create destination directory
    eprintln!("Installing JDK to {:?}", dest_dir);
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
            bail!("Error: java executable is missing at: {:?}", java_bin);
        }
    } else {
        let java_bin = extracted_jdk_path.join("bin").join("java");
        if !java_bin.exists() {
            bail!("Error: java executable is missing at: {:?}", java_bin);
        }
    }

    Ok(extracted_jdk_path)
}

pub fn find_installed_jdk(jdk_metadata: &JdkMetadata, jdk_base_path: &Path) -> Option<PathBuf> {
    let extracted_jdk_path = jdk_base_path.join(&jdk_metadata.semver);
    match extracted_jdk_path.exists() {
        true => Some(extracted_jdk_path),
        false => None,
    }
}

#[derive(Debug)]
pub struct JdkMetadata {
    pub semver: String,
    pub release_name: String,
    pub package_name: String,
    pub download_link: String,
    pub checksum: String,
}

pub const ADOPTIUM_API_URL: &str = "https://api.adoptium.net";

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
}

/// The single point of contact with Adoptium: discovering available releases,
/// fetching JDK metadata, and downloading packages. `base_url` covers the two
/// API endpoints; downloads follow whatever URL the metadata hands back.
pub struct AdoptiumClient {
    client: Client,
    base_url: String,
}

impl AdoptiumClient {
    pub fn new(base_url: impl Into<String>) -> anyhow::Result<Self> {
        let client = Client::builder()
            .user_agent(USER_AGENT)
            .build()
            .context("Could not build HTTP client")?;
        Ok(Self {
            client,
            base_url: base_url.into(),
        })
    }

    pub fn fetch_metadata(&self, java_version: &str) -> anyhow::Result<JdkMetadata> {
        let api_url = format!(
            "{base_url}/v3/assets/latest/{java_version}/hotspot?architecture={arch}&image_type=jdk&os={os}&vendor=eclipse",
            base_url = self.base_url,
            arch = jdk_arch()?,
            os = jdk_os()?
        );

        let response = self
            .client
            .get(&api_url)
            .send()
            .context("Could not fetch metadata from API")?;

        if !response.status().is_success() {
            bail!(
                "Failed to fetch metadata from API: HTTP {}",
                response.status()
            );
        }

        let assets: Vec<Asset> = response.json().context("Failed to parse JSON response")?;

        let asset = assets.into_iter().next().with_context(|| {
            format!(
                "No matching JDK found for the specified version and system architecture.\nTried to fetch metadata from: {}",
                api_url
            )
        })?;

        asset.try_into()
    }

    pub fn latest_major(&self) -> anyhow::Result<String> {
        let response = self
            .client
            .get(format!("{}/v3/info/available_releases", self.base_url))
            .send()
            .context("Could not fetch available releases from API")?;

        if !response.status().is_success() {
            bail!(
                "Failed to fetch available releases from API: HTTP {}",
                response.status()
            );
        }

        let releases: AvailableReleases =
            response.json().context("Failed to parse JSON response")?;

        let latest = releases
            .available_releases
            .into_iter()
            .max()
            .context("No available releases found.")?;

        Ok(latest.to_string())
    }

    pub fn download(&self, metadata: &JdkMetadata, file: &mut File) -> anyhow::Result<()> {
        let mut response = self.client.get(&metadata.download_link).send()?;

        if !response.status().is_success() {
            bail!(
                "Failed to download {} from {}: HTTP {}",
                metadata.package_name,
                metadata.download_link,
                response.status()
            );
        }

        let total_size = response
            .content_length()
            .context("Failed to get content length")?;

        let pb = setup_progress_bar(
            &format!(
                "Downloading JDK {} ({})",
                metadata.semver, metadata.package_name
            ),
            total_size,
        );

        let mut hasher = Sha256::new();

        let mut downloaded: u64 = 0;
        let mut buffer = [0; 8192];
        loop {
            let n = response
                .read(&mut buffer)
                .context("Could not read package data from response")?;
            if n == 0 {
                break;
            }
            file.write_all(&buffer[..n])?;
            downloaded += n as u64;
            pb.set_position(downloaded);
            hasher.update(&buffer[..n]);
        }

        pb.finish_and_clear();

        let hash = hex::encode(hasher.finalize());
        if hash != metadata.checksum {
            bail!(
                "Checksum mismatch: expected {}, got {}.",
                metadata.checksum,
                hash
            );
        }

        eprintln!("✅ Download complete, checksum passed.");

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
            "unexpected os: {}",
            os
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
            "unexpected arch: {}",
            arch
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
        install_jdk(&metadata, source_dir.path(), &dest).unwrap();

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

        let client = AdoptiumClient::new(server.url()).unwrap();
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

        let client = AdoptiumClient::new(server.url()).unwrap();
        let err = client.fetch_metadata("21").unwrap_err();

        assert!(format!("{:#}", err).contains("HTTP 500"), "got: {:#}", err);
    }

    #[test]
    fn fetch_metadata_malformed_json_errors() {
        let mut server = mockito::Server::new();
        let _m = metadata_mock(&mut server, 200, "this is not json");

        let client = AdoptiumClient::new(server.url()).unwrap();
        let err = client.fetch_metadata("21").unwrap_err();

        assert!(
            format!("{:#}", err).contains("Failed to parse JSON response"),
            "got: {:#}",
            err
        );
    }

    #[test]
    fn fetch_metadata_empty_array_means_no_matching_jdk() {
        let mut server = mockito::Server::new();
        let _m = metadata_mock(&mut server, 200, "[]");

        let client = AdoptiumClient::new(server.url()).unwrap();
        let err = client.fetch_metadata("21").unwrap_err();

        assert!(
            format!("{:#}", err).contains("No matching JDK found"),
            "got: {:#}",
            err
        );
    }

    #[test]
    fn fetch_metadata_missing_field_names_the_field() {
        let mut server = mockito::Server::new();
        let body = fixture_without_package_field("checksum");
        let _m = metadata_mock(&mut server, 200, &body);

        let client = AdoptiumClient::new(server.url()).unwrap();
        let err = client.fetch_metadata("21").unwrap_err();

        assert!(format!("{:#}", err).contains("checksum"), "got: {:#}", err);
    }

    #[test]
    fn fetch_metadata_empty_field_is_incomplete() {
        let mut server = mockito::Server::new();
        let body = fixture_with_package_field("checksum", serde_json::Value::String(String::new()));
        let _m = metadata_mock(&mut server, 200, &body);

        let client = AdoptiumClient::new(server.url()).unwrap();
        let err = client.fetch_metadata("21").unwrap_err();

        assert!(
            format!("{:#}", err).contains("Incomplete metadata"),
            "got: {:#}",
            err
        );
    }

    #[test]
    fn latest_major_happy_path() {
        let mut server = mockito::Server::new();
        let _m = server
            .mock("GET", "/v3/info/available_releases")
            .with_body(RELEASES_FIXTURE)
            .create();

        let client = AdoptiumClient::new(server.url()).unwrap();
        assert_eq!(client.latest_major().unwrap(), "26");
    }

    #[test]
    fn latest_major_missing_key_errors() {
        let mut server = mockito::Server::new();
        let _m = server
            .mock("GET", "/v3/info/available_releases")
            .with_body("{}")
            .create();

        let client = AdoptiumClient::new(server.url()).unwrap();
        let err = client.latest_major().unwrap_err();

        assert!(
            format!("{:#}", err).contains("available_releases"),
            "got: {:#}",
            err
        );
    }

    #[test]
    fn download_happy_path_writes_verified_file() {
        let mut server = mockito::Server::new();
        let _m = server
            .mock("GET", "/pkg.tar.gz")
            .with_body(FAKE_PACKAGE)
            .create();

        let client = AdoptiumClient::new(server.url()).unwrap();
        let metadata = fake_metadata(
            format!("{}/pkg.tar.gz", server.url()),
            FAKE_PACKAGE_CHECKSUM,
        );

        let mut file = tempfile::tempfile().unwrap();
        client.download(&metadata, &mut file).unwrap();

        use std::io::Seek;
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

        let client = AdoptiumClient::new(server.url()).unwrap();
        let metadata = fake_metadata(format!("{}/pkg.tar.gz", server.url()), "deadbeef");

        let mut file = tempfile::tempfile().unwrap();
        let err = client.download(&metadata, &mut file).unwrap_err();

        assert!(
            format!("{:#}", err).contains("Checksum mismatch"),
            "got: {:#}",
            err
        );
    }

    #[test]
    fn download_without_content_length_errors() {
        let mut server = mockito::Server::new();
        let _m = server
            .mock("GET", "/pkg.tar.gz")
            .with_chunked_body(|w| w.write_all(FAKE_PACKAGE))
            .create();

        let client = AdoptiumClient::new(server.url()).unwrap();
        let metadata = fake_metadata(
            format!("{}/pkg.tar.gz", server.url()),
            FAKE_PACKAGE_CHECKSUM,
        );

        let mut file = tempfile::tempfile().unwrap();
        let err = client.download(&metadata, &mut file).unwrap_err();

        assert!(
            format!("{:#}", err).contains("content length"),
            "got: {:#}",
            err
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

        let client = AdoptiumClient::new(server.url()).unwrap();
        let err = client.latest_major().unwrap_err();

        assert!(format!("{:#}", err).contains("HTTP 500"), "got: {:#}", err);
    }

    #[test]
    fn download_http_error_reports_status_not_checksum() {
        let mut server = mockito::Server::new();
        let _m = server
            .mock("GET", "/pkg.tar.gz")
            .with_status(404)
            .with_body("not found")
            .create();

        let client = AdoptiumClient::new(server.url()).unwrap();
        let metadata = fake_metadata(
            format!("{}/pkg.tar.gz", server.url()),
            FAKE_PACKAGE_CHECKSUM,
        );

        let mut file = tempfile::tempfile().unwrap();
        let err = client.download(&metadata, &mut file).unwrap_err();

        let msg = format!("{:#}", err);
        assert!(msg.contains("HTTP 404"), "got: {}", msg);
        assert!(!msg.contains("Checksum mismatch"), "got: {}", msg);
    }
}
