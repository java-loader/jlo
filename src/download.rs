use crate::USER_AGENT;
use crate::progress_bar::setup_progress_bar;
use reqwest::blocking::Client;
use sha2::{Digest, Sha256};
use std::error::Error;
use std::fs::File;
use std::io::{Read, Write};

pub fn download(
    name: &str,
    url: &str,
    expected_checksum: &str,
    file: &mut File,
) -> Result<(), Box<dyn Error>> {
    let client = Client::builder().user_agent(USER_AGENT).build()?;
    let response = client.get(url).send()?;

    let total_size = response
        .content_length()
        .ok_or("Failed to get content length")?;

    let mut source = response;

    let pb = setup_progress_bar(format!("Downloading {}", name).as_str(), total_size);

    let mut hasher = Sha256::new();

    let mut downloaded: u64 = 0;
    let mut buffer = [0; 8192];
    while let Ok(n) = source.read(&mut buffer) {
        if n == 0 {
            break;
        }
        file.write_all(&buffer[..n])?;
        downloaded += n as u64;
        pb.set_position(downloaded);
        hasher.update(&buffer[..n]);
    }

    pb.finish_and_clear();

    let hash = hex::encode(hasher.finalize());
    if hash != expected_checksum {
        return Err(format!(
            "Checksum mismatch: expected {}, got {}.",
            expected_checksum, hash
        )
        .into());
    }

    eprintln!("✅ Download complete, checksum passed.");

    Ok(())
}
