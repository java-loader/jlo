use crate::progress_bar::setup_progress_bar;
use flate2::bufread::GzDecoder;
use std::error::Error;
use std::fs::File;
use std::io::BufReader;
use std::path::Path;
use tar::Archive;

pub fn extract(file: &Path, dest: &Path) -> Result<(), Box<dyn Error>> {
    match file.extension().and_then(|s| s.to_str()) {
        Some("gz") => extract_tar_gz(file, dest),
        Some("zip") => extract_zip(file, dest),
        _ => Err(format!(
            "Unsupported archive format: {:?}. Only .tar.gz and .zip are supported.",
            file
        )
        .into()),
    }
}

fn extract_tar_gz(source: &Path, dest: &Path) -> Result<(), Box<dyn Error>> {
    let file =
        File::open(source).map_err(|err| format!("Error opening archive {:?}: {}", source, err))?;
    let metadata = file
        .metadata()
        .map_err(|err| format!("Error reading metadata of {:?}: {}", source, err))?;
    let pb = setup_progress_bar("Extracting", metadata.len());

    let buffered_file = BufReader::new(file);
    let progress_reader = pb.wrap_read(buffered_file);
    let decompressor = GzDecoder::new(progress_reader);
    let mut archive = Archive::new(decompressor);

    let result = archive
        .unpack(dest)
        .map_err(|err| format!("Error extracting archive {:?}: {}", source, err));

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

    result.map_err(|e| e.into())
}

fn extract_zip(source: &Path, dest: &Path) -> Result<(), Box<dyn Error>> {
    let file =
        File::open(source).map_err(|err| format!("Error opening archive {:?}: {}", source, err))?;
    let metadata = file
        .metadata()
        .map_err(|err| format!("Error reading metadata of {:?}: {}", source, err))?;
    let pb = setup_progress_bar("Extracting", metadata.len());

    let buffered_file = BufReader::new(file);
    let progress_reader = pb.wrap_read(buffered_file);
    let mut archive = zip::ZipArchive::new(progress_reader)
        .map_err(|err| format!("Error reading zip archive {:?}: {}", source, err))?;

    let result = archive
        .extract(dest)
        .map_err(|err| format!("Error extracting archive {:?}: {}", source, err));

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

    result.map_err(|e| e.into())
}
