use crate::jlo_home_dir;
use anyhow::anyhow;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

const JLO_CONFIG_FILE: &str = ".jlorc";
const JLO_DEFAULT_CONFIG_FILE: &str = "default.jlorc";

pub(crate) fn load_config_java_version() -> anyhow::Result<String> {
    // Try project config first; if any error other than NotFound, return it.
    match load(Path::new(JLO_CONFIG_FILE)) {
        Ok(v) => return Ok(v),
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            return Err(anyhow!("could not load configuration: {e}"));
        }
        Err(_) => {} // NotFound -> fall through to default config
    }

    // Try the default config path.
    let default_path = default_jlorc_path()?;
    load(&default_path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            anyhow!(
                "Neither '{JLO_CONFIG_FILE}' nor the default config file found. Please run 'jlo init' to create a configuration file."
            )
        } else {
            anyhow!("could not load configuration: {e}")
        }
    })
}

fn default_jlorc_path() -> anyhow::Result<PathBuf> {
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

pub(crate) fn init_project_config(java_version: &str, force: bool) -> anyhow::Result<()> {
    let path = Path::new(JLO_CONFIG_FILE);
    init_config(path, java_version, force)
}

pub(crate) fn init_default_config(java_version: &str, force: bool) -> anyhow::Result<()> {
    let path = default_jlorc_path()?;
    init_config(&path, java_version, force)
}

fn init_config(path: &Path, latest_release: &str, force: bool) -> anyhow::Result<()> {
    // $JLO_HOME need not exist: jlo-bin can be run straight from a build,
    // without install.sh ever having created ~/.jlo.
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)
            .map_err(|e| anyhow!("could not create directory '{}': {e}", parent.display()))?;
    }

    // Two opens rather than one, because the message has to distinguish
    // "created" from "replaced": create_new is the only way to learn whether
    // the file was already there without a racy pre-check.
    let mut replaced = false;
    let mut file = match OpenOptions::new().write(true).create_new(true).open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && force => {
            replaced = true;
            OpenOptions::new()
                .write(true)
                .truncate(true)
                .open(path)
                .map_err(|e| anyhow!("could not open file '{}': {e}", path.display()))?
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            return Err(anyhow!("file '{}' already exists", path.display()));
        }
        Err(e) => return Err(anyhow!(e)),
    };

    writeln!(
        file,
        "# Java version configured by J'Lo - https://github.com/java-loader/jlo"
    )?;
    writeln!(file, "{latest_release}")?;

    // stderr, like every other status message: this module's contract is that
    // stdout carries only shell code the caller may `eval`. Nothing sources
    // `jlo init` today, which is exactly why the inconsistency was easy to miss.
    crate::ui::created!(
        "{} config file '{}' with Java {}",
        if replaced { "Updated" } else { "Created" },
        path.display(),
        latest_release
    );
    Ok(())
}

pub(crate) fn is_valid_version(version: &str) -> bool {
    if let Ok(ver) = version.parse::<u32>() {
        ver >= 8
    } else {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn valid_versions() {
        assert!(is_valid_version("8"));
        assert!(is_valid_version("11"));
        assert!(is_valid_version("17"));
        assert!(is_valid_version("21"));
        assert!(is_valid_version("25"));
    }

    #[test]
    fn invalid_versions() {
        assert!(!is_valid_version("7"));
        assert!(!is_valid_version("0"));
        assert!(!is_valid_version(""));
        assert!(!is_valid_version("abc"));
        assert!(!is_valid_version("-1"));
        assert!(!is_valid_version("8.0"));
    }

    #[test]
    fn load_valid_version() {
        let dir = tempdir().unwrap();
        let file = dir.path().join(".jlorc");
        fs::write(&file, "21\n").unwrap();
        assert_eq!(load(&file).unwrap(), "21");
    }

    #[test]
    fn load_with_comments_and_blanks() {
        let dir = tempdir().unwrap();
        let file = dir.path().join(".jlorc");
        fs::write(&file, "# comment\n\n  # another comment\n  17  \n").unwrap();
        assert_eq!(load(&file).unwrap(), "17");
    }

    #[test]
    fn load_empty_file() {
        let dir = tempdir().unwrap();
        let file = dir.path().join(".jlorc");
        fs::write(&file, "").unwrap();
        let err = load(&file).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn load_only_comments() {
        let dir = tempdir().unwrap();
        let file = dir.path().join(".jlorc");
        fs::write(&file, "# just a comment\n# another\n").unwrap();
        let err = load(&file).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn load_invalid_version_in_file() {
        let dir = tempdir().unwrap();
        let file = dir.path().join(".jlorc");
        fs::write(&file, "7\n").unwrap();
        let err = load(&file).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    }

    #[test]
    fn load_missing_file() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("nonexistent");
        let err = load(&file).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
    }

    #[test]
    fn init_config_creates_file() {
        let dir = tempdir().unwrap();
        let file = dir.path().join(".jlorc");
        init_config(&file, "21", false).unwrap();

        let content = fs::read_to_string(&file).unwrap();
        let lines: Vec<_> = content.lines().collect();
        assert_eq!(
            lines[0],
            "# Java version configured by J'Lo - https://github.com/java-loader/jlo"
        );
        assert_eq!(lines[1], "21");
    }

    #[test]
    fn init_config_creates_missing_parent_directory() {
        // `jlo init --global` writes into $JLO_HOME, which does not exist yet when
        // jlo-bin is run without the installer having created ~/.jlo.
        let dir = tempdir().unwrap();
        let file = dir.path().join(".jlo").join("default.jlorc");
        init_config(&file, "21", false).unwrap();

        assert_eq!(
            fs::read_to_string(&file).unwrap().lines().nth(1),
            Some("21")
        );
    }

    #[test]
    fn init_config_fails_if_exists() {
        let dir = tempdir().unwrap();
        let file = dir.path().join(".jlorc");
        fs::write(&file, "17\n").unwrap();

        let err = init_config(&file, "21", false).unwrap_err();
        assert!(err.to_string().contains("already exists"));
    }

    #[test]
    fn init_config_overwrites_if_forced() {
        let dir = tempdir().unwrap();
        let file = dir.path().join(".jlorc");
        fs::write(&file, "17\nleftover\n").unwrap();

        init_config(&file, "21", true).unwrap();

        let content = fs::read_to_string(&file).unwrap();
        assert_eq!(content.lines().nth(1), Some("21"));
        // Truncated, not patched in place.
        assert!(!content.contains("leftover"));
    }
}
