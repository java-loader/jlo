use crate::USER_AGENT;
use anyhow::{Context, bail};
use reqwest::blocking::Client;
use semver_rs::compare;
use std::cmp::Ordering;
use std::env;
use std::path::{Path, PathBuf};

const MARKER_FILE: &str = ".jlo-managed";

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

        let kept = paths[0].file_name().unwrap().to_str().unwrap();
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

pub fn fetch_metadata(java_version: &String) -> anyhow::Result<JdkMetadata> {
    let api_url = format!(
        "https://api.adoptium.net/v3/assets/latest/{java_version}/hotspot?architecture={arch}&image_type=jdk&os={os}&vendor=eclipse",
        java_version = java_version,
        arch = jdk_arch(),
        os = jdk_os()
    );

    let client = Client::builder()
        .user_agent(USER_AGENT)
        .build()
        .context("Could not build HTTP client")?;
    let metadata_response = client
        .get(&api_url)
        .send()
        .context("Could not fetch metadata from API")?;

    if !metadata_response.status().is_success() {
        bail!(
            "Failed to fetch metadata from API: HTTP {}",
            metadata_response.status()
        );
    }

    let json: serde_json::Value = metadata_response
        .json()
        .context("Failed to parse JSON response")?;

    let json_array = json
        .as_array()
        .context("Unexpected JSON structure received from API.")?;

    if json_array.is_empty() {
        bail!(
            "No matching JDK found for the specified version and system architecture.\nTried to fetch metadata from: {}",
            api_url
        );
    }

    let root_node = json_array.first().unwrap();

    let semver = root_node["version"]["semver"].as_str().unwrap_or("");
    let release_name = root_node["release_name"].as_str().unwrap_or("");
    let package_name = root_node["binary"]["package"]["name"]
        .as_str()
        .unwrap_or("");
    let download_link = root_node["binary"]["package"]["link"]
        .as_str()
        .unwrap_or("");
    let checksum = root_node["binary"]["package"]["checksum"]
        .as_str()
        .unwrap_or("");
    if semver.is_empty()
        || release_name.is_empty()
        || package_name.is_empty()
        || download_link.is_empty()
        || checksum.is_empty()
    {
        bail!("Incomplete metadata received from API.");
    }
    Ok(JdkMetadata {
        semver: semver.to_string(),
        release_name: release_name.to_string(),
        package_name: package_name.to_string(),
        download_link: download_link.to_string(),
        checksum: checksum.to_string(),
    })
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

pub struct JdkMetadata {
    pub semver: String,
    pub release_name: String,
    pub package_name: String,
    pub download_link: String,
    pub checksum: String,
}

fn jdk_os() -> &'static str {
    match env::consts::OS {
        "linux" | "windows" | "solaris" | "aix" => env::consts::OS,
        "macos" => "mac",
        _ => panic!("Unknown OS: {}", env::consts::OS),
    }
}

fn jdk_arch() -> &'static str {
    match env::consts::ARCH {
        "x86_64" => "x64",
        "x86" => "x32",
        "powerpc64" => {
            if cfg!(target_endian = "little") {
                "ppc64le"
            } else {
                "ppc64"
            }
        }
        "s390x" | "arm" | "aarch64" => env::consts::ARCH,
        "sparc64" => "sparcv9",
        "riscv64" => "riscv64",
        _ => panic!("Unknown ARCH: {}", env::consts::ARCH),
    }
}

pub fn find_latest_jdk() -> anyhow::Result<String> {
    let client = Client::builder()
        .user_agent(USER_AGENT)
        .build()
        .context("Could not build HTTP client")?;
    let releases = client
        .get("https://api.adoptium.net/v3/info/available_releases")
        .send()
        .context("Could not fetch available releases from API")?;

    let json: serde_json::Value = releases.json().context("Failed to parse JSON response")?;
    let available_releases = json["available_releases"]
        .as_array()
        .context("Unexpected JSON structure received from API.")?;

    let latest = available_releases
        .iter()
        .filter_map(|v| v.as_i64())
        .max()
        .context("No available releases found.")?;

    Ok(latest.to_string())
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
        let os = jdk_os();
        assert!(
            ["linux", "mac", "windows", "solaris", "aix"].contains(&os),
            "unexpected os: {}",
            os
        );
    }

    #[test]
    fn jdk_arch_returns_known_value() {
        let arch = jdk_arch();
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
