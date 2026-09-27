//! `jlo selfupdate`: J'Lo replaced by its newest release.
//!
//! The binary does not download itself: when the latest release is newer, it
//! runs that release's `install.sh`, pinned to the tag, so there is one
//! publisher of a new binary. A second copy of the installer could strand an
//! old install on a version that cannot update itself.
//!
//! When J'Lo is already current it rewrites its shell files instead. Either
//! way the reload lines follow: the stale piece may be the wrapper resident in
//! the shell that asked.

use crate::CommandError;
use crate::install::{self, Layout};
use crate::ui;
use anyhow::{Context, Result, anyhow, bail};
use std::cmp::Ordering;
use std::process::{Command, Stdio};
use std::time::Duration;
use ureq::Agent;

/// The release root. `JLO_RELEASE_API_URL` overrides it, the way
/// `JLO_ADOPTIUM_API_URL` does for Adoptium, so the path is testable offline.
pub(crate) const RELEASES_URL: &str = "https://github.com/java-loader/jlo/releases";

/// release-please tags the crate, not the repository, so the tag is
/// `jlo-bin-v0.3.0` and not a bare `v0.3.0`. A tag without this prefix is an
/// error rather than a guess: the version it yields decides whether the
/// running J'Lo is replaced.
const TAG_PREFIX: &str = "jlo-bin-v";

const VERSION: &str = env!("CARGO_PKG_VERSION");

/// What a user runs when `selfupdate` cannot finish the job.
const INSTALL_LINE: &str = "/bin/bash -c \"$(curl -fsSL https://github.com/java-loader/jlo/releases/latest/download/install.sh)\"";

/// How long `jlo update` waits for the tag. The hint is a courtesy on a command
/// that has already done its work; a slow release host must not hold the prompt.
const HINT_TIMEOUT: Duration = Duration::from_secs(2);

/// The one contact with the GitHub release host.
pub(crate) struct ReleaseClient {
    agent: Agent,
    base_url: String,
    timeout: Option<Duration>,
}

impl std::fmt::Debug for ReleaseClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReleaseClient")
            .field("base_url", &self.base_url)
            .finish_non_exhaustive()
    }
}

impl ReleaseClient {
    pub(crate) fn new(base_url: impl Into<String>) -> Self {
        Self {
            agent: crate::adoptium::agent(),
            base_url: base_url.into(),
            timeout: None,
        }
    }

    fn from_env() -> Self {
        Self::new(std::env::var("JLO_RELEASE_API_URL").unwrap_or_else(|_| RELEASES_URL.to_string()))
    }

    fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    /// The tag of the latest release, read from the `Location` header of
    /// `/releases/latest`.
    ///
    /// Not the REST API: no token, no 60/hr unauthenticated rate limit to hit
    /// from a shared CI runner, no JSON contract to keep fixtures for.
    ///
    /// `max_redirects(0)` is what makes that possible at all - ureq follows
    /// redirects by default, so without it the 302 is consumed and the
    /// `Location` header never reaches this code.
    fn latest_tag(&self) -> Result<String> {
        let url = format!("{}/latest", self.base_url);
        let response = self
            .agent
            .get(&url)
            .config()
            .max_redirects(0)
            .timeout_global(self.timeout)
            .build()
            .call()
            .with_context(|| format!("could not reach {url}"))?;

        let status = response.status();
        if !status.is_redirection() {
            bail!("{url} did not redirect to the latest release: HTTP {status}");
        }
        let location = response
            .headers()
            .get("location")
            .and_then(|value| value.to_str().ok())
            .ok_or_else(|| anyhow!("{url} answered HTTP {status} without a Location header"))?;

        tag_from_location(location)
            .ok_or_else(|| anyhow!("could not read a release tag from {location:?}"))
    }

    /// Where `tag`'s assets live - and what `install.sh` is pinned to, so the
    /// tarball and its checksum come from the tag resolved here rather than
    /// from a `latest` that a release published in between would move.
    fn download_base(&self, tag: &str) -> String {
        format!("{}/download/{tag}", self.base_url)
    }

    /// `tag`'s `install.sh`, written to `file`. An HTTP error is an error:
    /// an empty body run by `sh` would succeed at doing nothing.
    fn fetch_installer(&self, tag: &str, file: &mut std::fs::File) -> Result<()> {
        let url = format!("{}/install.sh", self.download_base(tag));
        let mut response = self
            .agent
            .get(&url)
            .call()
            .with_context(|| format!("could not reach {url}"))?;
        if !response.status().is_success() {
            bail!("could not download {url}: HTTP {}", response.status());
        }
        std::io::copy(&mut response.body_mut().as_reader(), file)
            .with_context(|| format!("could not download {url}"))?;
        Ok(())
    }
}

/// The last non-empty path segment of a redirect target, with any query or
/// fragment dropped. The header may carry a relative reference, so this parses
/// a path rather than a URL.
fn tag_from_location(location: &str) -> Option<String> {
    let path = location
        .split(['?', '#'])
        .next()
        .unwrap_or(location)
        .trim_end_matches('/');
    let tag = path.rsplit('/').next()?;
    // "latest" back again means the redirect did not resolve to a release -
    // an empty repository, say.
    if tag.is_empty() || tag == "latest" {
        return None;
    }
    Some(tag.to_string())
}

fn version_from_tag(tag: &str) -> Result<&str> {
    tag.strip_prefix(TAG_PREFIX)
        .filter(|version| !version.is_empty())
        .ok_or_else(|| {
            anyhow!(
                "unrecognised release tag {tag:?}: J'Lo's releases are tagged {TAG_PREFIX}<version>."
            )
        })
}

fn is_newer(candidate: &str, current: &str) -> Result<bool> {
    let ordering = crate::version::compare(candidate, current).with_context(|| {
        format!("could not compare J'Lo versions {candidate:?} and {current:?}")
    })?;
    Ok(ordering == Ordering::Greater)
}

pub(crate) fn cmd_selfupdate(wrapped: bool) -> Result<(), CommandError> {
    let layout = Layout::new(crate::jlo_home_dir()?);
    // Before the network: a J'Lo this command may not replace has nothing to ask.
    check_owned(&layout)?;

    let client = ReleaseClient::from_env();
    let tag = client.latest_tag()?;
    let latest = version_from_tag(&tag)?;
    if is_newer(latest, VERSION)? {
        eprintln!("Updating J'Lo {VERSION} → {latest}");
        run_installer(&client, &layout, &tag)?;
    } else {
        install::refresh_layout(&layout)?;
        ui::created!("J'Lo {VERSION} is already the latest version.");
    }
    Ok(install::print_reload(&layout, wrapped)?)
}

/// The one line `jlo update` adds when a newer J'Lo is out. Silent on every
/// failure: an error here would read as the update failing. Asked only for the
/// J'Lo `selfupdate` may replace.
pub(crate) fn announce_newer_release() {
    let Ok(home) = crate::jlo_home_dir() else {
        return;
    };
    if !install::is_current_exe(&Layout::new(home).binary()) {
        return;
    }
    let Ok(tag) = ReleaseClient::from_env()
        .with_timeout(HINT_TIMEOUT)
        .latest_tag()
    else {
        return;
    };
    let Ok(latest) = version_from_tag(&tag) else {
        return;
    };
    if is_newer(latest, VERSION).unwrap_or(false) {
        ui::hint!("{}", ui::newer_jlo_hint(latest));
    }
}

/// Whether this is the J'Lo `selfupdate` may replace: the binary at
/// `$JLO_HOME/bin/jlo-bin`, nothing else. Homebrew's keg and a build in
/// `target/` are updated the way they were installed; running the installer
/// from either would publish a second J'Lo beside the one the user runs.
fn check_owned(layout: &Layout) -> Result<(), CommandError> {
    if install::is_current_exe(&layout.binary()) {
        return Ok(());
    }
    Err(CommandError::with_hint(
        anyhow!(
            "this J'Lo is not the one installed at {:?}, the only one selfupdate replaces.",
            layout.binary()
        ),
        "Update it the way it was installed: 'brew upgrade jlo' for Homebrew, ./install-local.sh for a local build.",
    ))
}

/// Run `tag`'s `install.sh`, pinned to `tag`.
///
/// `JLO_HOME` goes explicitly: this process may have resolved it from `$HOME`,
/// and the installer must write where this binary reads. The child's stdout
/// goes to stderr, because stdout is the channel the wrapper evaluates.
fn run_installer(client: &ReleaseClient, layout: &Layout, tag: &str) -> Result<(), CommandError> {
    let mut script =
        tempfile::NamedTempFile::new().context("could not create a file for the installer")?;
    client
        .fetch_installer(tag, script.as_file_mut())
        .map_err(|e| CommandError::with_hint(e, installer_hint()))?;
    let status = Command::new("sh")
        .arg(script.path())
        .env("JLO_INSTALL_BASE_URL", client.download_base(tag))
        .env("JLO_HOME", layout.home())
        .stdout(Stdio::from(std::io::stderr()))
        .status()
        .context("could not run the installer with sh")?;
    if !status.success() {
        return Err(CommandError::with_hint(
            anyhow!("the installer of {tag} failed ({status})."),
            installer_hint(),
        ));
    }
    Ok(())
}

fn installer_hint() -> String {
    format!("Run the installer directly: {INSTALL_LINE}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tag_yields_its_version() {
        assert_eq!(
            version_from_tag("jlo-bin-v0.4.0").expect("version"),
            "0.4.0"
        );
    }

    /// The tag format is a contract with release-please, and reading it wrong
    /// decides whether the running binary gets replaced. Fail loudly.
    #[test]
    fn an_unknown_tag_format_is_an_error_not_a_guess() {
        for tag in ["v0.4.0", "0.4.0", "jlo-v0.4.0", "jlo-bin-v"] {
            let err = version_from_tag(tag).expect_err(tag);
            assert!(
                format!("{err:#}").contains("unrecognised release tag"),
                "{tag}: {err:#}"
            );
        }
    }

    #[test]
    fn a_location_header_yields_the_tag() {
        assert_eq!(
            tag_from_location("https://github.com/java-loader/jlo/releases/tag/jlo-bin-v0.4.0")
                .as_deref(),
            Some("jlo-bin-v0.4.0")
        );
        assert_eq!(
            tag_from_location("/java-loader/jlo/releases/tag/jlo-bin-v0.4.0?x=1").as_deref(),
            Some("jlo-bin-v0.4.0")
        );
    }

    #[test]
    fn a_location_that_names_no_release_is_unreadable() {
        assert_eq!(
            tag_from_location("https://github.com/x/y/releases/latest"),
            None
        );
        assert_eq!(tag_from_location("/"), None);
    }

    #[test]
    fn version_comparison_is_semver_not_string() {
        assert!(is_newer("0.10.0", "0.9.0").expect("compare"));
        assert!(!is_newer("0.4.0", "0.4.0").expect("compare"));
        assert!(!is_newer("0.3.0", "0.4.0").expect("compare"));
    }
}

#[cfg(test)]
mod http_tests {
    use super::*;

    /// Discovery depends on *not* following the redirect. If a future ureq
    /// changes the default, or the per-request override is dropped, this
    /// notices.
    #[test]
    fn latest_tag_does_not_follow_the_redirect() {
        let mut server = mockito::Server::new();
        let redirect = server
            .mock("GET", "/latest")
            .with_status(302)
            .with_header("location", &format!("{}/tag/jlo-bin-v1.2.3", server.url()))
            .create();
        let target = server
            .mock("GET", "/tag/jlo-bin-v1.2.3")
            .with_status(200)
            .with_body("the release page")
            .expect(0)
            .create();

        let client = ReleaseClient::new(server.url());
        assert_eq!(client.latest_tag().expect("tag"), "jlo-bin-v1.2.3");
        redirect.assert();
        target.assert();
    }

    #[test]
    fn latest_tag_reports_a_non_redirect() {
        let mut server = mockito::Server::new();
        let _mock = server.mock("GET", "/latest").with_status(404).create();

        let err = ReleaseClient::new(server.url())
            .latest_tag()
            .expect_err("404");
        assert!(format!("{err:#}").contains("HTTP 404"), "{err:#}");
    }

    #[test]
    fn latest_tag_reports_a_redirect_without_a_location() {
        let mut server = mockito::Server::new();
        let _mock = server.mock("GET", "/latest").with_status(302).create();

        let err = ReleaseClient::new(server.url())
            .latest_tag()
            .expect_err("no location");
        assert!(format!("{err:#}").contains("Location"), "{err:#}");
    }
}
