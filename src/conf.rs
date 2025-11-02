use crate::{jlo_home_dir};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

const JLO_CONFIG_FILE: &str = ".jlorc";
const JLO_DEFAULT_CONFIG_FILE: &str = "default.jlorc";

pub fn load_config_java_version() -> Result<String, String> {
    // Try project config first; if any error other than NotFound, return it.
    match load(Path::new(JLO_CONFIG_FILE)) {
        Ok(v) => return Ok(v),
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            return Err(format!("Error: Could not load configuration: {}", e))
        }
        Err(_) => {} // NotFound -> fall through to default config
    }

    // Try the default config path.
    let default_path = default_jlorc_path()?;
    load(&default_path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            format!(
                "Neither '{}' nor the default config file found. Please run 'jlo init' to create a configuration file.",
                JLO_CONFIG_FILE
            )
        } else {
            format!("Error: Could not load configuration: {}", e)
        }
    })
}

fn default_jlorc_path() -> Result<PathBuf, String> {
    jlo_home_dir().map(|p| p.join(JLO_DEFAULT_CONFIG_FILE))
}

fn load(path: &Path) -> Result<String, std::io::Error> {
    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(e) => {
            if e.kind() == std::io::ErrorKind::NotFound {
                return Err(std::io::Error::new(
                    e.kind(),
                    format!("File '{}' not found: {}", path.display(), e),
                ));
            }
            return Err(std::io::Error::new(
                e.kind(),
                format!("Could not read file '{}': {}", path.display(), e),
            ));
        }
    };

    let java_version = content
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.starts_with('#'))
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("Could not find java version in file '{}'", path.display()),
            )
        })?
        .to_string();

    if !is_valid_version(&java_version) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "Unsupported Java version specified in '{}': '{}'.",
                path.display(),
                java_version
            ),
        ));
    }

    Ok(java_version)
}

pub fn init_project_config(java_version: String) -> Result<(), String> {
    let path = Path::new(JLO_CONFIG_FILE);
    init_config(path, java_version)
}

pub fn init_default_config(java_version: String) -> Result<(), String> {
    let path = default_jlorc_path()?;
    init_config(&path, java_version)
}

fn init_config(path: &Path, latest_release: String) -> Result<(), String> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::AlreadyExists {
                format!("File '{}' already exists!", path.display())
            } else {
                e.to_string()
            }
        })?;

    writeln!(
        file,
        "# Java version configured by J'Lo - https://github.com/java-loader/jlo"
    )
    .map_err(|e| e.to_string())?;
    writeln!(file, "{}", latest_release).map_err(|e| e.to_string())?;

    println!(
        "Created config file '{}' with Java {}",
        path.display(),
        latest_release
    );
    Ok(())
}

pub fn is_valid_version(version: &str) -> bool {
    if let Ok(ver) = version.parse::<u32>() {
        ver >= 8
    } else {
        false
    }
}
