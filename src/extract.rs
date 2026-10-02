use crate::cancel::Cancel;
use anyhow::{Context, bail};
use flate2::bufread::GzDecoder;
use std::fs::File;
use std::io::BufReader;
use std::path::Path;
use tar::Archive;

pub(crate) fn extract(file: &Path, dest: &Path, cancel: Cancel) -> anyhow::Result<()> {
    match file.extension().and_then(|s| s.to_str()) {
        Some("gz") => extract_tar_gz(file, dest, cancel),
        // Adoptium ships .zip only for Windows: reaching here is a platform
        // detection fault, so the message names that, not the extension.
        Some("zip") => bail!(
            "{file:?} is a Windows JDK archive; jlo supports Linux and macOS only, \
             so the platform it asked Adoptium for is one it cannot install"
        ),
        _ => bail!("unsupported archive format: {file:?} (only .tar.gz is supported)"),
    }
}

/// The archive is read through `cancel`, so a signal stops the unpacking at its
/// next read without restructuring `tar::Archive::unpack`.
fn extract_tar_gz(source: &Path, dest: &Path, cancel: Cancel) -> anyhow::Result<()> {
    let file = File::open(source).with_context(|| format!("could not open archive {source:?}"))?;

    let decompressor = GzDecoder::new(BufReader::new(cancel.reader(file)));
    let mut archive = Archive::new(decompressor);

    archive
        .unpack(dest)
        .with_context(|| format!("could not extract archive {source:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    /// The one arm that is not a format question. A `.zip` from Adoptium means
    /// the platform detection picked Windows, so the message has to say that
    /// rather than "unsupported archive format" - which would send the reader
    /// looking for a missing decompressor.
    #[test]
    fn a_zip_names_the_unsupported_platform_not_the_extension() {
        let dir = tempdir().unwrap();
        let err = extract(&dir.path().join("jdk.zip"), dir.path(), Cancel::process()).unwrap_err();
        let message = format!("{err:#}");
        assert!(message.contains("Windows"), "{message}");
        assert!(!message.contains("unsupported archive format"), "{message}");
    }

    #[test]
    fn rejects_unsupported_archive_format() {
        let dir = tempdir().unwrap();
        let err = extract(&dir.path().join("jdk.7z"), dir.path(), Cancel::process()).unwrap_err();
        assert!(format!("{err:#}").contains("unsupported archive format"));
    }
}
