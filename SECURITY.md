# Security Policy

## Supported Versions

Only the latest release receives security fixes. `jlo selfupdate` (or `brew upgrade jlo` with Homebrew) brings an
installation up to date.

## Reporting a Vulnerability

**Please do not report security vulnerabilities through public GitHub issues, discussions, or pull requests.**

[**Report it privately via GitHub**](https://github.com/java-loader/jlo/security/advisories/new). This keeps the
report confidential, lets us discuss it in one place and produces an advisory that notifies users once a fix is
released.

Please include as much of the following as you can:

* The type of issue (e.g., path traversal while unpacking a JDK, a bypassed checksum check, shell code injected
  through the generated `export` lines, or unsafe file handling in the JDK store)
* The affected version, or the tag/commit you tested
* Your platform and shell
* Step-by-step instructions to reproduce the issue
* Proof-of-concept or exploit code (if possible)
* Impact of the issue, including how an attacker might exploit it

## Verifying Release Artifacts

Each release tarball has a build provenance attestation (from 0.4.0 on), and so has the release's `install.sh`
(from 0.6.0 on). It shows that the file was built by the release workflow of this repository on GitHub Actions,
from the tagged source. To verify a downloaded file with the GitHub CLI:

    gh attestation verify jlo-<platform>.tar.gz --repo java-loader/jlo \
        --signer-workflow java-loader/jlo/.github/workflows/release-please.yml

    gh attestation verify install.sh --repo java-loader/jlo \
        --signer-workflow java-loader/jlo/.github/workflows/release-please.yml

Each tarball also comes with a `.sha256` file. It catches a corrupt or truncated download, but as it is served from
the same place as the tarball, it does not prove where the tarball came from; the attestation does.
