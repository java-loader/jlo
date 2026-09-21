use crate::jlo_home_dir;
use anyhow::anyhow;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

const JLO_CONFIG_FILE: &str = ".jlorc";
const JLO_DEFAULT_CONFIG_FILE: &str = "default.jlorc";

/// A resolved Java major version, and where it came from.
///
/// The provenance is the point: `jlo current` and `jlo env --verbose` both
/// report it, and a bare `String` forgets it the moment the walk finishes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Resolved {
    /// The Java major version, e.g. "21".
    pub version: String,
    pub source: Source,
}

/// Where a version in play came from.
///
/// Deliberately open: a later change that falls back to the newest installed
/// JDK - or to the latest release - adds a variant here rather than reshaping
/// the callers. The variant names are the vocabulary, and their tags are
/// fixed - `argument`, `project_config`, `default_config`, `foreign` - so the
/// machine-readable output still to come reports this fact under names that
/// are already settled rather than inventing a second spelling. Renaming a
/// variant is therefore a wire-format change, not a refactor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Source {
    /// An explicit CLI argument: `jlo env 21`.
    Argument,
    /// The nearest `.jlorc` at or above the cwd.
    ProjectConfig(PathBuf),
    /// `$JLO_HOME/default.jlorc`.
    DefaultConfig(PathBuf),
    /// Not a resolution at all: `$JAVA_HOME` was set outside jlo, so no
    /// config had any say. Never produced by [`find`] - it is how a command
    /// that starts from the live `$JAVA_HOME` reports a JDK jlo does not
    /// manage, without inventing a second vocabulary for "where this came
    /// from".
    Foreign,
}

impl Source {
    /// What to call this source in a status line.
    ///
    /// Pure: a project path is already shortened against the cwd by
    /// [`find_in`], so formatting never has to consult the filesystem.
    pub(crate) fn label(&self) -> String {
        match self {
            Self::Argument => "the command line".to_string(),
            Self::ProjectConfig(path) | Self::DefaultConfig(path) => path.display().to_string(),
            Self::Foreign => "$JAVA_HOME".to_string(),
        }
    }
}

/// The configured Java version, or `Ok(None)` when nothing is configured.
///
/// `None` is an ordinary state rather than a failure, because `jlo current`
/// asks this question of a shell that may well have a JDK active with no
/// `.jlorc` anywhere. The commands for which absence *is* a failure call
/// [`resolve`] instead.
pub(crate) fn find() -> anyhow::Result<Option<Resolved>> {
    let cwd = std::env::current_dir()
        .map_err(|e| anyhow!("could not determine the current directory: {e}"))?;
    find_in(
        &cwd,
        std::env::home_dir().as_deref(),
        &default_jlorc_path()?,
    )
}

/// The configured Java version, or today's "run 'jlo init'" error.
pub(crate) fn resolve() -> anyhow::Result<Resolved> {
    find()?.ok_or_else(|| {
        anyhow!(
            "No '{JLO_CONFIG_FILE}' found in the current directory or its parents, and no default config file. Please run 'jlo init' to create a configuration file."
        )
    })
}

/// The walk, with every input passed in so it can be tested without mutating
/// the process environment.
///
/// Project config first, searched upwards: `jlo env` is routinely run from a
/// subdirectory, and resolving the user default there would hand back a
/// different JDK without saying so.
fn find_in(
    cwd: &Path,
    home: Option<&Path>,
    default_path: &Path,
) -> anyhow::Result<Option<Resolved>> {
    if let Some(path) = find_project_config(cwd, home) {
        let version = load(&path).map_err(|e| anyhow!("could not load configuration: {e}"))?;
        return Ok(Some(Resolved {
            version,
            source: Source::ProjectConfig(shorten_against(&path, cwd)),
        }));
    }

    match load(default_path) {
        Ok(version) => Ok(Some(Resolved {
            version,
            source: Source::DefaultConfig(default_path.to_path_buf()),
        })),
        // Neither file exists. Whether that is a problem is the caller's call.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(anyhow!("could not load configuration: {e}")),
    }
}

/// Render a found config path the way the user typed their way to it: a
/// `.jlorc` in the current directory reads as `./.jlorc`, one further up keeps
/// its absolute path so it is clear the pin comes from somewhere else.
///
/// Done here, where the cwd is already in hand, so that formatting stays a
/// pure function of what `Source` carries. The result is still a path that
/// opens.
fn shorten_against(path: &Path, cwd: &Path) -> PathBuf {
    match path.strip_prefix(cwd) {
        Ok(relative) => Path::new(".").join(relative),
        Err(_) => path.to_path_buf(),
    }
}

/// The nearest `.jlorc` at or above `start`.
///
/// The search stops after `home` and after a VCS root, both inclusive: a
/// `.jlorc` outside the repository - or above the user's home directory -
/// belongs to some other project, and inheriting it silently is the failure
/// mode this search exists to prevent. `home` is the last directory examined
/// rather than a skipped one, so a `.jlorc` sitting in `$HOME` keeps applying
/// as it did when only the current directory was consulted.
fn find_project_config(start: &Path, home: Option<&Path>) -> Option<PathBuf> {
    let mut dir = start;
    loop {
        let candidate = dir.join(JLO_CONFIG_FILE);
        if candidate.is_file() {
            return Some(candidate);
        }
        if home == Some(dir) || is_vcs_root(dir) {
            return None;
        }
        dir = dir.parent()?;
    }
}

/// `.git` is matched with `exists`, not `is_dir`: worktrees and submodules
/// record it as a file, and both are still repository roots.
fn is_vcs_root(dir: &Path) -> bool {
    dir.join(".git").exists()
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
                "unsupported version '{}' in '{}': only major versions 8, 11, ... are supported",
                java_version,
                path.display()
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
        // Same wording as `assert_java_version` in main.rs: whichever path
        // rejects the version, the user is told what would be accepted.
        assert!(
            err.to_string()
                .contains("only major versions 8, 11, ... are supported"),
            "{err}"
        );
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

    /// Walk-up tests. Paths are canonicalized because `tempdir()` hands back
    /// `/var/...` on macOS while the walk sees `/private/var/...`, and the
    /// `$HOME` boundary is a path comparison.
    fn canon(p: &Path) -> PathBuf {
        p.canonicalize().unwrap()
    }

    #[test]
    fn find_project_config_finds_jlorc_in_start_dir() {
        let home = tempdir().unwrap();
        let home = canon(home.path());
        let project = home.join("project");
        fs::create_dir_all(&project).unwrap();
        fs::write(project.join(".jlorc"), "21\n").unwrap();

        assert_eq!(
            find_project_config(&project, Some(&home)),
            Some(project.join(".jlorc"))
        );
    }

    #[test]
    fn find_project_config_walks_up_from_subdirectory() {
        // The bug: `jlo env` in project/src/main used to miss project/.jlorc
        // and silently fall through to the user default.
        let home = tempdir().unwrap();
        let home = canon(home.path());
        let project = home.join("project");
        let deep = project.join("src").join("main");
        fs::create_dir_all(&deep).unwrap();
        fs::write(project.join(".jlorc"), "21\n").unwrap();

        assert_eq!(
            find_project_config(&deep, Some(&home)),
            Some(project.join(".jlorc"))
        );
    }

    #[test]
    fn find_project_config_prefers_nearest() {
        let home = tempdir().unwrap();
        let home = canon(home.path());
        let project = home.join("project");
        let module = project.join("module");
        fs::create_dir_all(&module).unwrap();
        fs::write(project.join(".jlorc"), "17\n").unwrap();
        fs::write(module.join(".jlorc"), "21\n").unwrap();

        assert_eq!(
            find_project_config(&module, Some(&home)),
            Some(module.join(".jlorc"))
        );
    }

    #[test]
    fn find_project_config_stops_at_vcs_root() {
        // A .jlorc outside the repository belongs to someone else's project.
        let home = tempdir().unwrap();
        let home = canon(home.path());
        let outer = home.join("outer");
        let repo = outer.join("repo");
        let deep = repo.join("src");
        fs::create_dir_all(&deep).unwrap();
        fs::create_dir_all(repo.join(".git")).unwrap();
        fs::write(outer.join(".jlorc"), "17\n").unwrap();

        assert_eq!(find_project_config(&deep, Some(&home)), None);
    }

    #[test]
    fn find_project_config_finds_jlorc_at_vcs_root() {
        // Stopping at the VCS root still checks the root itself.
        let home = tempdir().unwrap();
        let home = canon(home.path());
        let repo = home.join("repo");
        let deep = repo.join("src");
        fs::create_dir_all(&deep).unwrap();
        fs::create_dir_all(repo.join(".git")).unwrap();
        fs::write(repo.join(".jlorc"), "21\n").unwrap();

        assert_eq!(
            find_project_config(&deep, Some(&home)),
            Some(repo.join(".jlorc"))
        );
    }

    #[test]
    fn find_project_config_stops_at_home() {
        let outside = tempdir().unwrap();
        let outside = canon(outside.path());
        let home = outside.join("home");
        let project = home.join("project");
        fs::create_dir_all(&project).unwrap();
        fs::write(outside.join(".jlorc"), "17\n").unwrap();

        assert_eq!(find_project_config(&project, Some(&home)), None);
    }

    #[test]
    fn find_project_config_checks_home_itself() {
        // `$HOME` is the outermost directory searched, not one skipped: a
        // `.jlorc` in `$HOME` applied when CWD was `$HOME` before walk-up too.
        let home = tempdir().unwrap();
        let home = canon(home.path());
        let project = home.join("project");
        fs::create_dir_all(&project).unwrap();
        fs::write(home.join(".jlorc"), "17\n").unwrap();

        assert_eq!(
            find_project_config(&project, Some(&home)),
            Some(home.join(".jlorc"))
        );
    }

    // -- find_in: the walk, plus where the answer came from --
    //
    // Same temp-dir fixtures as the `find_project_config` tests below; the
    // only addition is that the `Source` variant is asserted, because that is
    // the fact the bare `String` used to drop.

    #[test]
    fn find_in_reports_a_project_config_as_its_source() {
        let home = tempdir().unwrap();
        let home = canon(home.path());
        let project = home.join("project");
        fs::create_dir_all(&project).unwrap();
        fs::write(project.join(".jlorc"), "21\n").unwrap();
        let default = home.join("missing").join("default.jlorc");

        let resolved = find_in(&project, Some(&home), &default)
            .unwrap()
            .expect("the project .jlorc answers");
        assert_eq!(resolved.version, "21");
        // Shortened against the cwd, so a status line reads `./.jlorc`
        // rather than an absolute path the user never typed.
        assert_eq!(
            resolved.source,
            Source::ProjectConfig(PathBuf::from("./.jlorc"))
        );
    }

    /// A `.jlorc` in a parent keeps its absolute path: the pin comes from
    /// somewhere other than where the user is standing, and the line says so.
    #[test]
    fn find_in_keeps_an_absolute_path_for_a_parent_config() {
        let home = tempdir().unwrap();
        let home = canon(home.path());
        let project = home.join("project");
        let deep = project.join("src");
        fs::create_dir_all(&deep).unwrap();
        fs::write(project.join(".jlorc"), "17\n").unwrap();
        let default = home.join("missing").join("default.jlorc");

        let resolved = find_in(&deep, Some(&home), &default).unwrap().unwrap();
        assert_eq!(resolved.version, "17");
        assert_eq!(
            resolved.source,
            Source::ProjectConfig(project.join(".jlorc"))
        );
    }

    #[test]
    fn find_in_reports_the_default_config_as_its_source() {
        let home = tempdir().unwrap();
        let home = canon(home.path());
        let project = home.join("project");
        fs::create_dir_all(&project).unwrap();
        let default = home.join(".jlo").join("default.jlorc");
        fs::create_dir_all(default.parent().unwrap()).unwrap();
        fs::write(&default, "25\n").unwrap();

        let resolved = find_in(&project, Some(&home), &default).unwrap().unwrap();
        assert_eq!(resolved.version, "25");
        assert_eq!(resolved.source, Source::DefaultConfig(default));
    }

    /// Nothing configured is `Ok(None)`, not an error: `jlo current` asks this
    /// of shells that have a JDK active with no `.jlorc` anywhere.
    #[test]
    fn find_in_returns_none_when_nothing_is_configured() {
        let home = tempdir().unwrap();
        let home = canon(home.path());
        let project = home.join("project");
        fs::create_dir_all(&project).unwrap();
        let default = home.join(".jlo").join("default.jlorc");

        assert_eq!(find_in(&project, Some(&home), &default).unwrap(), None);
    }

    /// A `.jlorc` that exists but is unreadable is still an error - absence is
    /// the only thing `Ok(None)` means.
    #[test]
    fn find_in_still_fails_on_an_invalid_config() {
        let home = tempdir().unwrap();
        let home = canon(home.path());
        let project = home.join("project");
        fs::create_dir_all(&project).unwrap();
        fs::write(project.join(".jlorc"), "7\n").unwrap();
        let default = home.join("missing").join("default.jlorc");

        let err = find_in(&project, Some(&home), &default).unwrap_err();
        assert!(
            err.to_string().contains("could not load configuration"),
            "{err}"
        );
    }

    #[test]
    fn find_project_config_returns_none_when_absent() {
        let home = tempdir().unwrap();
        let home = canon(home.path());
        let project = home.join("project");
        fs::create_dir_all(&project).unwrap();

        assert_eq!(find_project_config(&project, Some(&home)), None);
    }
}
