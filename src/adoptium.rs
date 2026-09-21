use crate::ui::InstallUi;
use crate::version::compare;
use anyhow::{Context, bail};
use sha2::{Digest, Sha256};
use std::cmp::Ordering;
use std::env;
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Component, Path};
use ureq::Agent;
use ureq::tls::{RootCerts, TlsConfig, TlsProvider};

const USER_AGENT: &str = concat!("J'Lo/", env!("CARGO_PKG_VERSION"));

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
            bail!("incomplete metadata received from the Adoptium API");
        }
        // Three of these name a file or a directory jlo creates. Checked here,
        // at the edge, so no caller has to remember which fields are safe to
        // join onto a path.
        plain_name(&metadata.semver, "version.semver")?;
        plain_name(&metadata.release_name, "release_name")?;
        plain_name(&metadata.package_name, "package name")?;
        Ok(metadata)
    }
}

/// Require a field to be a single ordinary path component.
///
/// `Path::join` neither resolves `..` nor resists a leading `/` - an absolute
/// value discards the base it is joined onto entirely. These strings arrive
/// over the wire, and the checksum cannot vouch for them: it is fetched from
/// the same response, and the temp file is created and written before the
/// digest is compared. So the shape is checked instead, once, before any of
/// them reaches a path.
fn plain_name(value: &str, field: &str) -> anyhow::Result<()> {
    let mut components = Path::new(value).components();
    match (components.next(), components.next()) {
        (Some(Component::Normal(_)), None) => Ok(()),
        _ => bail!("Adoptium API returned an unusable {field}: {value:?}"),
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
            .context("could not fetch metadata from the Adoptium API")?;

        if !response.status().is_success() {
            bail!(
                "Failed to fetch metadata from API: HTTP {}",
                response.status()
            );
        }

        let assets: Vec<Asset> = response
            .body_mut()
            .read_json()
            .context("could not parse the Adoptium API response")?;

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

        jdks.sort_by(|a, b| compare(&b.version, &a.version).unwrap_or(Ordering::Equal));
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
            .context("could not fetch available releases from the Adoptium API")?;

        if !response.status().is_success() {
            bail!(
                "Failed to fetch available releases from API: HTTP {}",
                response.status()
            );
        }

        response
            .body_mut()
            .read_json()
            .context("could not parse the Adoptium API response")
    }

    pub(crate) fn latest_major(&self) -> anyhow::Result<String> {
        let releases = self.fetch_available_releases()?;

        let latest = releases
            .available_releases
            .into_iter()
            .max()
            .context("no available releases found")?;

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
                "could not download {} from {}: HTTP {}",
                metadata.package_name,
                metadata.download_link,
                response.status()
            );
        }

        let total_size = response
            .body()
            .content_length()
            .context("could not determine the download size: no Content-Length header")?;

        ui.start_download(total_size);

        let mut hasher = Sha256::new();

        let mut downloaded: u64 = 0;
        let mut buffer = [0; 8192];
        let mut reader = response.body_mut().as_reader();
        loop {
            let n = reader
                .read(&mut buffer)
                .context("could not read package data from the response")?;
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
                "checksum mismatch: expected {}, got {}",
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
        _ => bail!("unsupported OS: {}", env::consts::OS),
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
        _ => bail!("unsupported architecture: {}", env::consts::ARCH),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- metadata validation --

    fn asset(semver: &str, release_name: &str, package_name: &str) -> Asset {
        Asset {
            version: AssetVersion {
                semver: semver.to_string(),
            },
            release_name: release_name.to_string(),
            binary: AssetBinary {
                package: AssetPackage {
                    name: package_name.to_string(),
                    link: "https://example.invalid/jdk.tar.gz".to_string(),
                    checksum: "0".repeat(64),
                },
            },
        }
    }

    #[test]
    fn ordinary_metadata_is_accepted() {
        let metadata: JdkMetadata = asset("21.0.5+11", "jdk-21.0.5+11", "OpenJDK21U.tar.gz")
            .try_into()
            .expect("a normal Adoptium response must pass");
        assert_eq!(metadata.semver, "21.0.5+11");
    }

    /// Each of these is joined onto a path: `package name` names the temp file
    /// the download is written to, `version.semver` the install directory, and
    /// `release_name` the extracted directory that gets moved into it. A value
    /// that walks out of the directory it is joined onto has to be refused
    /// before the join, not noticed after it. A leading `./` goes with them:
    /// `Path::components` keeps it, Adoptium never sends it, and a check that
    /// refuses it is the one that is easy to read.
    #[test]
    fn metadata_that_escapes_its_directory_is_refused() {
        for escape in [
            "../../../../.zshrc",
            "..",
            "/etc/passwd",
            "sub/dir",
            "a/../../b",
            ".",
            "./jdk.tar.gz",
        ] {
            for (semver, release_name, package_name) in [
                (escape, "jdk-21", "jdk.tar.gz"),
                ("21.0.5+11", escape, "jdk.tar.gz"),
                ("21.0.5+11", "jdk-21", escape),
            ] {
                let err = JdkMetadata::try_from(asset(semver, release_name, package_name))
                    .expect_err("an escaping field must be refused");
                assert!(
                    format!("{err:#}").contains("unusable"),
                    "{escape:?} was refused for the wrong reason: {err:#}"
                );
            }
        }
    }

    // -- jdk_os / jdk_arch smoke tests --
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
            format!("{err:#}").contains("could not parse the Adoptium API response"),
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
            format!("{err:#}").contains("incomplete metadata"),
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
            format!("{err:#}").contains("checksum mismatch"),
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
            format!("{err:#}").contains("Content-Length"),
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
        assert!(!msg.contains("checksum mismatch"), "got: {msg}");
    }
}
