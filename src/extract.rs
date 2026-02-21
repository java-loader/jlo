use crate::progress_bar::setup_progress_bar;
use anyhow::{Context, bail};
use flate2::bufread::GzDecoder;
use std::fs::File;
use std::io::BufReader;
use std::path::Path;
use tar::Archive;

pub fn extract(file: &Path, dest: &Path) -> anyhow::Result<()> {
    match file.extension().and_then(|s| s.to_str()) {
        Some("gz") => extract_tar_gz(file, dest),
        Some("zip") => extract_zip(file, dest),
        _ => bail!(
            "Unsupported archive format: {:?}. Only .tar.gz and .zip are supported.",
            file
        ),
    }
}

fn extract_tar_gz(source: &Path, dest: &Path) -> anyhow::Result<()> {
    let file = File::open(source).with_context(|| format!("Error opening archive {:?}", source))?;
    let metadata = file
        .metadata()
        .with_context(|| format!("Error reading metadata of {:?}", source))?;
    let pb = setup_progress_bar("Extracting", metadata.len());

    let buffered_file = BufReader::new(file);
    let progress_reader = pb.wrap_read(buffered_file);
    let decompressor = GzDecoder::new(progress_reader);
    let mut archive = Archive::new(decompressor);

    let result = archive
        .unpack(dest)
        .with_context(|| format!("Error extracting archive {:?}", source));

    match &result {
        Ok(_) => {
            pb.finish_and_clear();
            eprintln!("✅ Extraction complete.");
        }
        Err(e) => {
            pb.abandon_with_message("❌ Extraction failed!");
            eprintln!("Error: {}", e);
        }
    }

    result
}

fn extract_zip(source: &Path, dest: &Path) -> anyhow::Result<()> {
    let file = File::open(source).with_context(|| format!("Error opening archive {:?}", source))?;
    let metadata = file
        .metadata()
        .with_context(|| format!("Error reading metadata of {:?}", source))?;
    let pb = setup_progress_bar("Extracting", metadata.len());

    let buffered_file = BufReader::new(file);
    let progress_reader = pb.wrap_read(buffered_file);
    let mut archive = zip::ZipArchive::new(progress_reader)
        .with_context(|| format!("Error reading zip archive {:?}", source))?;

    let result = archive
        .extract(dest)
        .with_context(|| format!("Error extracting archive {:?}", source));

    match &result {
        Ok(_) => {
            pb.finish_and_clear();
            eprintln!("✅ Extraction complete.");
        }
        Err(e) => {
            pb.abandon_with_message("❌ Extraction failed!");
            eprintln!("Error: {}", e);
        }
    }

    result
}
