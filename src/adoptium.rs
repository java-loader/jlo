use crate::request::{Request, Stream};
use crate::ui::InstallUi;
use crate::version::cmp_desc;
use anyhow::{Context, bail};
use serde::de::DeserializeOwned;
use sha2::{Digest, Sha256};
use std::env;
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Component, Path};
use ureq::tls::{RootCerts, TlsConfig, TlsProvider};
use ureq::{Agent, Body};

const USER_AGENT: &str = concat!("J'Lo/", env!("CARGO_PKG_VERSION"));

/// The HTTP agent every remote jlo talks to is reached through.
///
/// Statuses are inspected by the callers, so keep ureq from turning a non-2xx
/// response into an error and losing the message wording. ureq defaults its
/// TLS provider to Rustls regardless of which TLS feature is enabled, and
/// panics on the first https request if that provider was not compiled in. It
/// also defaults to bundled Mozilla roots. Select native-tls with the platform
/// trust store, which is what jlo has always used and what TLS-intercepting
/// corporate proxies need.
pub(crate) fn agent() -> Agent {
    Agent::config_builder()
        .user_agent(USER_AGENT)
        .http_status_as_error(false)
        .tls_config(
            TlsConfig::builder()
                .provider(TlsProvider::NativeTls)
                .root_certs(RootCerts::PlatformVerifier)
                .build(),
        )
        .build()
        .into()
}

/// Stream a response body into `file`, reporting progress to `ui`, and return
/// the SHA256 of what was written as lowercase hex. Comparing it is left to the
/// caller, which knows what to name in the mismatch.
pub(crate) fn stream_hashed(
    body: &mut Body,
    file: &mut File,
    ui: &InstallUi,
) -> anyhow::Result<String> {
    let total_size = body
        .content_length()
        .context("could not determine the download size: no Content-Length header")?;
    ui.start_download(total_size);

    let mut hasher = Sha256::new();
    let mut downloaded: u64 = 0;
    let mut buffer = [0; 8192];
    let mut reader = body.as_reader();
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
    Ok(hex::encode(hasher.finalize()))
}

#[derive(Debug)]
pub(crate) struct JdkMetadata {
    pub semver: String,
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
            package_name: asset.binary.package.name,
            download_link: asset.binary.package.link,
            checksum: asset.binary.package.checksum,
        };
        if metadata.semver.is_empty()
            || metadata.package_name.is_empty()
            || metadata.download_link.is_empty()
            || metadata.checksum.is_empty()
        {
            bail!("incomplete metadata received from the Adoptium API");
        }
        // Both of these name a file or a directory jlo creates. Checked here,
        // at the edge, so no caller has to remember which fields are safe to
        // join onto a path.
        plain_name(&metadata.semver, "version.semver")?;
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

/// One entry of the response from `/v3/assets/feature_releases/{major}/ea`.
///
/// A different shape from [`Asset`] on purpose: that endpoint answers with a
/// *release*, which carries every binary of that build, and the query narrows
/// the array to this OS and architecture rather than the response shape doing
/// it.
#[derive(serde::Deserialize)]
struct Release {
    version_data: AssetVersion,
    binaries: Vec<AssetBinary>,
}

impl From<Asset> for Release {
    fn from(asset: Asset) -> Self {
        Release {
            version_data: asset.version,
            binaries: vec![asset.binary],
        }
    }
}

impl TryFrom<Release> for JdkMetadata {
    type Error = anyhow::Error;

    fn try_from(release: Release) -> anyhow::Result<Self> {
        let binary = release.binaries.into_iter().next().context(
            "the Adoptium API returned a release with no binary for this OS and architecture",
        )?;
        Asset {
            version: release.version_data,
            binary,
        }
        .try_into()
    }
}

/// The shape of `/v3/info/available_releases`.
#[derive(serde::Deserialize)]
struct ReleaseInfo {
    available_releases: Vec<i64>,
    // Optional because only the listing reads these two, and a response
    // without them still answers every other question this document is
    // fetched for.
    /// The newest major with a GA build.
    #[serde(default)]
    most_recent_feature_release: Option<i64>,
    /// The newest major with any build at all.
    #[serde(default)]
    tip_version: Option<i64>,
}

impl ReleaseInfo {
    /// The majors that exist only as a pre-release stream: above the newest
    /// release, up to the tip.
    ///
    /// Not every major's EA stream: after a GA, Adoptium keeps the stream
    /// running as a preview of the next *patch*, which is not a new name worth
    /// offering in a listing, and asking for all of them would be one paged
    /// request per major.
    fn unreleased_majors(&self) -> impl Iterator<Item = i64> {
        self.most_recent_feature_release
            .zip(self.tip_version)
            .into_iter()
            .flat_map(|(released, tip)| released + 1..=tip)
    }
}

/// The catalogue as one answer: the builds on offer for this machine, and the
/// majors Adoptium has released.
///
/// The two are not the same list. A major with no build for this OS, or one
/// whose lookup failed, has no row here but is still released - so "has 26
/// shipped?" has to be asked of `released_majors`, not of the rows.
#[derive(Debug)]
pub(crate) struct Catalogue {
    pub jdks: Vec<RemoteJdk>,
    pub released_majors: Vec<i64>,
}

/// A JDK build Adoptium offers for *this* OS and architecture: the newest of
/// one name.
#[derive(Debug)]
pub(crate) struct RemoteJdk {
    pub version: String,
    /// The name this build is the newest of - what the listing compares
    /// installs against.
    pub request: Request,
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
        Self {
            agent: agent(),
            base_url: base_url.into(),
        }
    }

    /// The newest build of `request` for this OS and architecture, or `None`
    /// when Adoptium offers none.
    ///
    /// "Not offered" is a value, not an error: Adoptium answers it with `200`
    /// and an empty array, and it is a fact about the name - no JDK 8 for
    /// macOS on Apple silicon - that a caller handling several names skips
    /// past, where a network or HTTP failure is one it has to stop at.
    pub(crate) fn fetch_metadata(&self, request: Request) -> anyhow::Result<Option<JdkMetadata>> {
        self.fetch_newest(request)?
            .map(JdkMetadata::try_from)
            .transpose()
    }

    /// The newest build of `request`, in release shape whichever endpoint
    /// answered: the GA endpoint's asset is a release with its one binary.
    /// Both endpoints answer "nothing on offer" with `200` and an empty array.
    fn fetch_newest(&self, request: Request) -> anyhow::Result<Option<Release>> {
        Ok(match request.stream {
            Stream::Ga => {
                let url = self.latest_asset_url(&request.major.to_string())?;
                let assets: Vec<Asset> = self.get_json(&url, "metadata")?;
                assets.into_iter().next().map(Release::from)
            }
            Stream::Ea => {
                let url = self.ea_release_url(request.major)?;
                let releases: Vec<Release> = self.get_json(&url, "metadata")?;
                releases.into_iter().next()
            }
        })
    }

    /// `sort_order=DESC` with `page_size=1` asks the API for the newest
    /// pre-release and nothing else: the stream is paged, and every page after
    /// the first is a build J'Lo would discard.
    fn ea_release_url(&self, major: i64) -> anyhow::Result<String> {
        Ok(format!(
            "{base_url}/v3/assets/feature_releases/{major}/ea?architecture={arch}&image_type=jdk&os={os}&vendor=eclipse&page_size=1&sort_order=DESC",
            base_url = self.base_url,
            arch = jdk_arch()?,
            os = jdk_os()?
        ))
    }

    fn latest_asset_url(&self, java_version: &str) -> anyhow::Result<String> {
        Ok(format!(
            "{base_url}/v3/assets/latest/{java_version}/hotspot?architecture={arch}&image_type=jdk&os={os}&vendor=eclipse",
            base_url = self.base_url,
            arch = jdk_arch()?,
            os = jdk_os()?
        ))
    }

    /// Every JDK Adoptium can install on this machine, newest first: the
    /// newest build of every released major, and of the pre-release stream of
    /// every major not yet released.
    ///
    /// Costs one request for the major-version list plus one per name. Done
    /// serially that is ~4s, so the per-name lookups are fanned out across
    /// threads sharing the pooled client.
    pub(crate) fn available_jdks(&self) -> anyhow::Result<Catalogue> {
        let releases = self.fetch_available_releases()?;
        let names: Vec<Request> = releases
            .available_releases
            .iter()
            .map(|&major| Request {
                major,
                stream: Stream::Ga,
            })
            .chain(releases.unreleased_majors().map(|major| Request {
                major,
                stream: Stream::Ea,
            }))
            .collect();

        let looked_up: Vec<(Request, anyhow::Result<Option<String>>)> =
            std::thread::scope(|scope| {
                let handles: Vec<_> = names
                    .iter()
                    .map(|&name| (name, scope.spawn(move || self.latest_version(name))))
                    .collect();

                handles
                    .into_iter()
                    .map(|(name, handle)| {
                        let result = handle
                            .join()
                            .unwrap_or_else(|_| bail!("lookup thread panicked"));
                        (name, result)
                    })
                    .collect()
            });

        let mut jdks = Vec::new();
        for (name, result) in looked_up {
            match result {
                Ok(Some(version)) => jdks.push(RemoteJdk {
                    version,
                    request: name,
                }),
                // No build for this OS/architecture - nothing to offer.
                Ok(None) => {}
                // One name failing should not cost the user the whole listing.
                Err(e) => crate::ui::warning!("could not look up JDK {name}: {e:#}"),
            }
        }

        jdks.sort_by(|a, b| cmp_desc(&a.version, &b.version));
        Ok(Catalogue {
            jdks,
            released_majors: releases.available_releases,
        })
    }

    /// Only the version: a pre-release with no binary for this platform still
    /// has one to list, where [`Self::fetch_metadata`] would refuse it.
    fn latest_version(&self, name: Request) -> anyhow::Result<Option<String>> {
        Ok(self
            .fetch_newest(name)?
            .map(|release| release.version_data.semver))
    }

    fn fetch_available_releases(&self) -> anyhow::Result<ReleaseInfo> {
        self.get_json(
            &format!("{}/v3/info/available_releases", self.base_url),
            "available releases",
        )
    }

    /// GET `url` and parse the JSON body. `what` names the document in the
    /// failure messages.
    fn get_json<T: DeserializeOwned>(&self, url: &str, what: &str) -> anyhow::Result<T> {
        let mut response = self
            .agent
            .get(url)
            .call()
            .with_context(|| format!("could not fetch {what} from the Adoptium API"))?;

        if !response.status().is_success() {
            bail!(
                "Failed to fetch {what} from API: HTTP {}",
                response.status()
            );
        }

        response
            .body_mut()
            .read_json()
            .context("could not parse the Adoptium API response")
    }

    /// The majors Adoptium has shipped a GA build of.
    ///
    /// One small JSON document, the same one `latest_major` reads. Fetched at
    /// most once per command, and only when a pre-release name is in play, so
    /// an ordinary `jlo update` makes exactly the requests it made before.
    pub(crate) fn released_majors(&self) -> anyhow::Result<Vec<i64>> {
        Ok(self.fetch_available_releases()?.available_releases)
    }

    /// The newest major Adoptium has shipped, as the name `jlo init` writes
    /// and the cascade's last stage downloads. `available_releases` holds
    /// majors that have shipped, so this is a GA name by construction; it is
    /// parsed rather than assumed so the one grammar, floor included, stays in
    /// one place.
    pub(crate) fn latest_major(&self) -> anyhow::Result<Request> {
        let releases = self.fetch_available_releases()?;

        let latest = releases
            .available_releases
            .into_iter()
            .max()
            .context("no available releases found")?;

        Request::parse(&latest.to_string())
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

        let hash = stream_hashed(response.body_mut(), file, ui)?;
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

/// This machine as Adoptium names it - `mac/aarch64` - for the messages that
/// say a build is not offered here. Falls back to Rust's own names for a
/// platform Adoptium has no name for, which no request can have reached.
pub(crate) fn platform() -> String {
    format!(
        "{}/{}",
        jdk_os().unwrap_or(env::consts::OS),
        jdk_arch().unwrap_or(env::consts::ARCH)
    )
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

    fn asset(semver: &str, package_name: &str) -> Asset {
        Asset {
            version: AssetVersion {
                semver: semver.to_string(),
            },
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
        let metadata: JdkMetadata = asset("21.0.5+11", "OpenJDK21U.tar.gz")
            .try_into()
            .expect("a normal Adoptium response must pass");
        assert_eq!(metadata.semver, "21.0.5+11");
    }

    /// Both of these are joined onto a path: `package name` names the temp
    /// file the download is written to, and `version.semver` the install
    /// directory. A value that walks out of the directory it is joined onto
    /// has to be refused before the join, not noticed after it. A leading `./` goes with them:
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
            for (semver, package_name) in [(escape, "jdk.tar.gz"), ("21.0.5+11", escape)] {
                let err = JdkMetadata::try_from(asset(semver, package_name))
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
        let metadata = client
            .fetch_metadata(Request {
                major: 21,
                stream: Stream::Ga,
            })
            .unwrap()
            .expect("the fixture offers a build");

        assert_eq!(metadata.semver, "21.0.11+10.0.LTS");
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
        let err = client
            .fetch_metadata(Request {
                major: 21,
                stream: Stream::Ga,
            })
            .unwrap_err();

        assert!(format!("{err:#}").contains("HTTP 500"), "got: {err:#}");
    }

    #[test]
    fn fetch_metadata_malformed_json_errors() {
        let mut server = mockito::Server::new();
        let _m = metadata_mock(&mut server, 200, "this is not json");

        let client = AdoptiumClient::new(server.url());
        let err = client
            .fetch_metadata(Request {
                major: 21,
                stream: Stream::Ga,
            })
            .unwrap_err();

        assert!(
            format!("{err:#}").contains("could not parse the Adoptium API response"),
            "got: {err:#}"
        );
    }

    /// `200 []` is how Adoptium says it has no build of a name for this
    /// platform - JDK 8 on Apple silicon. A value rather than an error, so a
    /// run over several names can skip it and stop only at a real failure.
    #[test]
    fn fetch_metadata_empty_array_means_not_offered() {
        let mut server = mockito::Server::new();
        let _m = metadata_mock(&mut server, 200, "[]");

        let client = AdoptiumClient::new(server.url());
        let metadata = client
            .fetch_metadata(Request {
                major: 21,
                stream: Stream::Ga,
            })
            .expect("an empty array is not a failure");

        assert!(metadata.is_none(), "{metadata:?}");
    }

    #[test]
    fn fetch_metadata_missing_field_names_the_field() {
        let mut server = mockito::Server::new();
        let body = fixture_without_package_field("checksum");
        let _m = metadata_mock(&mut server, 200, &body);

        let client = AdoptiumClient::new(server.url());
        let err = client
            .fetch_metadata(Request {
                major: 21,
                stream: Stream::Ga,
            })
            .unwrap_err();

        assert!(format!("{err:#}").contains("checksum"), "got: {err:#}");
    }

    #[test]
    fn fetch_metadata_empty_field_is_incomplete() {
        let mut server = mockito::Server::new();
        let body = fixture_with_package_field("checksum", serde_json::Value::String(String::new()));
        let _m = metadata_mock(&mut server, 200, &body);

        let client = AdoptiumClient::new(server.url());
        let err = client
            .fetch_metadata(Request {
                major: 21,
                stream: Stream::Ga,
            })
            .unwrap_err();

        assert!(
            format!("{err:#}").contains("incomplete metadata"),
            "got: {err:#}"
        );
    }

    // -- available_jdks --

    fn releases_body(majors: &[i64]) -> String {
        serde_json::json!({
            "available_releases": majors,
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
            .with_body(releases_body(&[17, 21, 25]))
            .create();
        let _a17 = major_mock(&mut server, 17, 200, &asset_body("17.0.20+101"));
        let _a21 = major_mock(&mut server, 21, 200, &asset_body("21.0.12+101.0.LTS"));
        let _a25 = major_mock(&mut server, 25, 200, &asset_body("25.0.4+101.0.LTS"));

        let client = AdoptiumClient::new(server.url());
        let jdks = client.available_jdks().unwrap().jdks;

        let rows: Vec<_> = jdks
            .iter()
            .map(|j| (j.version.as_str(), j.request.major))
            .collect();
        assert_eq!(
            rows,
            vec![
                ("25.0.4+101.0.LTS", 25),
                ("21.0.12+101.0.LTS", 21),
                ("17.0.20+101", 17),
            ]
        );
    }

    #[test]
    fn available_jdks_skips_majors_without_a_build_for_this_platform() {
        let mut server = mockito::Server::new();
        let _r = server
            .mock("GET", "/v3/info/available_releases")
            .with_body(releases_body(&[16, 21]))
            .create();
        // Adoptium answers 200 with an empty array when it has no build for
        // this OS/architecture.
        let _a16 = major_mock(&mut server, 16, 200, "[]");
        let _a21 = major_mock(&mut server, 21, 200, &asset_body("21.0.12+101.0.LTS"));

        let client = AdoptiumClient::new(server.url());
        let jdks = client.available_jdks().unwrap().jdks;

        assert_eq!(jdks.len(), 1);
        assert_eq!(jdks[0].version, "21.0.12+101.0.LTS");
    }

    #[test]
    fn available_jdks_survives_a_single_major_failing() {
        let mut server = mockito::Server::new();
        let _r = server
            .mock("GET", "/v3/info/available_releases")
            .with_body(releases_body(&[17, 21]))
            .create();
        let _a17 = major_mock(&mut server, 17, 500, "boom");
        let _a21 = major_mock(&mut server, 21, 200, &asset_body("21.0.12+101.0.LTS"));

        let client = AdoptiumClient::new(server.url());
        let jdks = client.available_jdks().unwrap().jdks;

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

    const EA_FIXTURE: &str = include_str!("../tests/fixtures/feature_releases_28_ea.json");

    fn ea_body(semver: &str) -> String {
        let mut json: serde_json::Value = serde_json::from_str(EA_FIXTURE).unwrap();
        json[0]["version_data"]["semver"] = serde_json::Value::String(semver.to_string());
        json.to_string()
    }

    fn ea_mock(
        server: &mut mockito::ServerGuard,
        major: i64,
        status: usize,
        body: &str,
    ) -> mockito::Mock {
        server
            .mock(
                "GET",
                mockito::Matcher::Regex(format!(r"^/v3/assets/feature_releases/{major}/ea")),
            )
            .match_query(mockito::Matcher::Any)
            .with_status(status)
            .with_body(body)
            .create()
    }

    fn releases_with_tip(majors: &[i64], released: i64, tip: i64) -> String {
        serde_json::json!({
            "available_releases": majors,
            "most_recent_feature_release": released,
            "tip_version": tip,
        })
        .to_string()
    }

    /// Only majors above the newest release are offered as a pre-release
    /// stream. 26's stream is live too, but it previews the next patch of a
    /// released major, so it is not a name the listing offers.
    #[test]
    fn available_jdks_offers_the_pre_release_stream_of_every_unreleased_major() {
        let mut server = mockito::Server::new();
        let _r = server
            .mock("GET", "/v3/info/available_releases")
            .with_body(releases_with_tip(&[25, 26], 26, 28))
            .create();
        let _a25 = major_mock(&mut server, 25, 200, &asset_body("25.0.4+101.0.LTS"));
        let _a26 = major_mock(&mut server, 26, 200, &asset_body("26.0.1+9"));
        let ea26 = ea_mock(&mut server, 26, 200, &ea_body("26.0.2-beta+101.0.ea")).expect(0);
        let _ea27 = ea_mock(&mut server, 27, 200, &ea_body("27.0.0-beta+30.0.ea"));
        let _ea28 = ea_mock(&mut server, 28, 200, EA_FIXTURE);

        let client = AdoptiumClient::new(server.url());
        let jdks = client.available_jdks().unwrap().jdks;

        let rows: Vec<_> = jdks
            .iter()
            .map(|j| (j.version.as_str(), j.request.to_string()))
            .collect();
        assert_eq!(
            rows,
            vec![
                ("28.0.0-beta+16.0.ea", "28-ea".to_string()),
                ("27.0.0-beta+30.0.ea", "27-ea".to_string()),
                ("26.0.1+9", "26".to_string()),
                ("25.0.4+101.0.LTS", "25".to_string()),
            ]
        );
        ea26.assert();
    }

    /// The same two outcomes a released major has: no build for this platform
    /// is no row, and a failed lookup is a warning and no row - neither costs
    /// the rest of the listing.
    #[test]
    fn available_jdks_drops_only_the_pre_release_stream_it_cannot_list() {
        let mut server = mockito::Server::new();
        let _r = server
            .mock("GET", "/v3/info/available_releases")
            .with_body(releases_with_tip(&[26], 26, 28))
            .create();
        let _a26 = major_mock(&mut server, 26, 200, &asset_body("26.0.1+9"));
        let _ea27 = ea_mock(&mut server, 27, 500, "boom");
        let _ea28 = ea_mock(&mut server, 28, 200, "[]");

        let client = AdoptiumClient::new(server.url());
        let jdks = client.available_jdks().unwrap().jdks;

        let versions: Vec<_> = jdks.iter().map(|j| j.version.as_str()).collect();
        assert_eq!(versions, vec!["26.0.1+9"]);
    }

    // -- released_majors --

    /// The notice a `-ea` pin earns is keyed on this list, so it has to be the
    /// released majors themselves rather than anything derived from them.
    #[test]
    fn released_majors_reads_the_available_releases_list() {
        let mut server = mockito::Server::new();
        let _mock = server
            .mock("GET", "/v3/info/available_releases")
            .with_status(200)
            .with_body(RELEASES_FIXTURE)
            .create();

        let released = AdoptiumClient::new(server.url()).released_majors().unwrap();

        assert!(released.contains(&21), "21 is a released major");
        assert!(!released.contains(&99), "99 is not");
    }

    #[test]
    fn latest_major_happy_path() {
        let mut server = mockito::Server::new();
        let _m = server
            .mock("GET", "/v3/info/available_releases")
            .with_body(RELEASES_FIXTURE)
            .create();

        let client = AdoptiumClient::new(server.url());
        assert_eq!(client.latest_major().unwrap().to_string(), "26");
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

    // -- fetch_metadata: EA stream --

    /// The EA endpoint answers with *releases* (a `binaries` array), not with
    /// the flat *assets* the latest endpoint returns. Two shapes, one
    /// `JdkMetadata`.
    #[test]
    fn fetch_metadata_reads_the_ea_release_shape() {
        let mut server = mockito::Server::new();
        let mock = server
            .mock(
                "GET",
                mockito::Matcher::Regex(r"^/v3/assets/feature_releases/28/ea".to_string()),
            )
            .with_status(200)
            .with_body(include_str!(
                "../tests/fixtures/feature_releases_28_ea.json"
            ))
            .create();

        let client = AdoptiumClient::new(server.url());
        let metadata = client
            .fetch_metadata(Request {
                major: 28,
                stream: Stream::Ea,
            })
            .expect("the EA fixture is a complete release")
            .expect("the EA fixture offers a build");

        mock.assert();
        assert!(
            metadata.semver.contains('-'),
            "an EA build carries a prerelease: {}",
            metadata.semver
        );
        assert!(!metadata.checksum.is_empty());
        assert!(!metadata.download_link.is_empty());
    }

    /// A GA request must still go to the latest endpoint - the two URLs are
    /// the only thing that keeps the streams apart at the network seam.
    #[test]
    fn fetch_metadata_still_uses_the_latest_endpoint_for_ga() {
        let mut server = mockito::Server::new();
        let mock = server
            .mock(
                "GET",
                mockito::Matcher::Regex(r"^/v3/assets/latest/21/hotspot".to_string()),
            )
            .with_status(200)
            .with_body(ASSETS_FIXTURE)
            .create();

        let client = AdoptiumClient::new(server.url());
        client
            .fetch_metadata(Request {
                major: 21,
                stream: Stream::Ga,
            })
            .expect("the GA fixture is a complete asset");
        mock.assert();
    }

    /// An empty array is a 200, not an error: it means Adoptium has no EA
    /// build for this major on this OS/arch. Saying so beats a silent GA
    /// substitution, which would hand back a different JDK than was asked for.
    #[test]
    fn an_empty_ea_stream_is_not_offered_rather_than_the_ga_build() {
        let mut server = mockito::Server::new();
        let _mock = server
            .mock(
                "GET",
                mockito::Matcher::Regex(r"^/v3/assets/feature_releases/11/ea".to_string()),
            )
            .with_status(200)
            .with_body("[]")
            .create();

        // Only the EA endpoint is mocked, so a fallback to the GA one would
        // come back as an error rather than as `None`.
        let client = AdoptiumClient::new(server.url());
        let metadata = client
            .fetch_metadata(Request {
                major: 11,
                stream: Stream::Ea,
            })
            .expect("an empty stream is not a failure");
        assert!(metadata.is_none(), "{metadata:?}");
    }
}
