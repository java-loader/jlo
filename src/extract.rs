use crate::ui::InstallUi;
use anyhow::{Context, bail};
use flate2::bufread::GzDecoder;
use std::fs::File;
use std::io::BufReader;
use std::path::Path;
use tar::Archive;

pub(crate) fn extract(file: &Path, dest: &Path, ui: &InstallUi) -> anyhow::Result<()> {
    match file.extension().and_then(|s| s.to_str()) {
        Some("gz") => extract_tar_gz(file, dest, ui),
        // Adoptium ships .zip only for Windows: reaching here is a platform
        // detection fault, so the message names that, not the extension.
        Some("zip") => bail!(
            "{file:?} is a Windows JDK archive; jlo supports Linux and macOS only, \
             so the platform it asked Adoptium for is one it cannot install"
        ),
        _ => bail!("unsupported archive format: {file:?} (only .tar.gz is supported)"),
    }
}

fn extract_tar_gz(source: &Path, dest: &Path, ui: &InstallUi) -> anyhow::Result<()> {
    let file = File::open(source).with_context(|| format!("could not open archive {source:?}"))?;
    ui.start_extract();

    let decompressor = GzDecoder::new(BufReader::new(file));
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
        let err = extract(
            &dir.path().join("jdk.zip"),
            dir.path(),
            &InstallUi::hidden(),
        )
        .unwrap_err();
        let message = format!("{err:#}");
        assert!(message.contains("Windows"), "{message}");
        assert!(!message.contains("unsupported archive format"), "{message}");
    }

    /// The bar counts downloaded bytes only: unpacking reads the archive
    /// again, and counting that too would run the bar past its total.
    #[test]
    fn extracting_leaves_the_download_count_alone() {
        let dir = tempdir().unwrap();
        let archive = dir.path().join("jdk.tar.gz");
        let mut builder = tar::Builder::new(flate2::write::GzEncoder::new(
            File::create(&archive).unwrap(),
            flate2::Compression::default(),
        ));
        let mut header = tar::Header::new_gnu();
        header.set_size(4);
        header.set_mode(0o755);
        header.set_cksum();
        builder
            .append_data(&mut header, "jdk/bin/java", &b"java"[..])
            .unwrap();
        builder.into_inner().unwrap().finish().unwrap();

        let ui = InstallUi::hidden();
        let dest = dir.path().join("out");
        extract(&archive, &dest, &ui).unwrap();
        assert!(dest.join("jdk/bin/java").exists());
        assert_eq!(ui.position(), 0);
    }

    #[test]
    fn rejects_unsupported_archive_format() {
        let dir = tempdir().unwrap();
        let err =
            extract(&dir.path().join("jdk.7z"), dir.path(), &InstallUi::hidden()).unwrap_err();
        assert!(format!("{err:#}").contains("unsupported archive format"));
    }
}
