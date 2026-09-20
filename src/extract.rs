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
        Some("zip") => extract_zip(file, dest, ui),
        _ => bail!("Unsupported archive format: {file:?}. Only .tar.gz and .zip are supported."),
    }
}

fn extract_tar_gz(source: &Path, dest: &Path, ui: &InstallUi) -> anyhow::Result<()> {
    let file = File::open(source).with_context(|| format!("Error opening archive {source:?}"))?;
    ui.start_extract();

    let progress_reader = ui.wrap_read(BufReader::new(file));
    let decompressor = GzDecoder::new(progress_reader);
    let mut archive = Archive::new(decompressor);

    archive
        .unpack(dest)
        .with_context(|| format!("Error extracting archive {source:?}"))
}

fn extract_zip(source: &Path, dest: &Path, ui: &InstallUi) -> anyhow::Result<()> {
    let file = File::open(source).with_context(|| format!("Error opening archive {source:?}"))?;
    ui.start_extract();

    let progress_reader = ui.wrap_read(BufReader::new(file));
    let mut archive = zip::ZipArchive::new(progress_reader)
        .with_context(|| format!("Error reading zip archive {source:?}"))?;

    archive
        .extract(dest)
        .with_context(|| format!("Error extracting archive {source:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::tempdir;
    use zip::write::SimpleFileOptions;

    /// jlo builds `zip` without its default features, so the deflate codec has
    /// to come from the explicitly selected `deflate-flate2`. A round trip
    /// catches a feature trim that silently drops decompression support.
    #[test]
    fn extracts_deflated_and_stored_zip_entries() {
        let dir = tempdir().unwrap();
        let archive_path = dir.path().join("jdk.zip");

        let file = File::create(&archive_path).unwrap();
        let mut zip = zip::ZipWriter::new(file);

        zip.start_file(
            "jdk/bin/java",
            SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated),
        )
        .unwrap();
        // Repetitive content so the deflate path actually compresses.
        zip.write_all(&b"java".repeat(256)).unwrap();

        zip.start_file(
            "jdk/release",
            SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored),
        )
        .unwrap();
        zip.write_all(b"JAVA_VERSION=\"21\"").unwrap();

        zip.finish().unwrap();

        let dest = dir.path().join("out");
        extract(&archive_path, &dest, &InstallUi::hidden("test")).unwrap();

        assert_eq!(
            std::fs::read(dest.join("jdk/bin/java")).unwrap(),
            b"java".repeat(256)
        );
        assert_eq!(
            std::fs::read_to_string(dest.join("jdk/release")).unwrap(),
            "JAVA_VERSION=\"21\""
        );
    }

    #[test]
    fn rejects_unsupported_archive_format() {
        let dir = tempdir().unwrap();
        let err = extract(
            &dir.path().join("jdk.7z"),
            dir.path(),
            &InstallUi::hidden("test"),
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("Unsupported archive format"));
    }
}
