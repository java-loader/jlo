mod adoptium;
mod conf;
mod extract;
mod progress_bar;

use crate::adoptium::{
    AdoptiumClient, JdkMetadata, clean_jdks, find_installed_jdk, find_installed_major_versions,
    find_suitable_jdk,
};
use anyhow::Context;
use std::collections::HashSet;
use std::env;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::exit;
use tempfile::tempdir;

fn main() {
    if env::args().len() < 2 {
        eprintln!("Arguments missing.");
        print_usage_and_exit()
    }

    let api_url =
        env::var("JLO_ADOPTIUM_API_URL").unwrap_or_else(|_| adoptium::ADOPTIUM_API_URL.to_string());
    let client = AdoptiumClient::new(api_url).unwrap_or_else(|e| {
        eprintln!("Error: {:#}", e);
        exit(1);
    });

    // Get command
    let command = &env::args().nth(1).unwrap();
    match command.as_str() {
        "env" => {
            cmd_env(&client);
        }
        "home" => {
            cmd_home(&client);
        }
        "exec" => {
            cmd_exec(&client);
        }
        "clean" => {
            cmd_clean();
        }
        "default" => {
            cmd_default();
        }
        "init" => {
            cmd_init(&client);
        }
        "update" => {
            cmd_update(&client);
        }
        "selfupdate" => {
            eprintln!("Self-update is handled by the jlo shell function.");
            exit(1);
        }
        "sing" => {
            eprintln!("There are no Easter Eggs in this program. Trust me. 💃");
        }
        "version" => {
            println!(env!("CARGO_PKG_VERSION"));
        }
        _ => {
            eprintln!("Unknown command: {}", command);
            print_usage_and_exit()
        }
    }
}

fn print_usage_and_exit() -> ! {
    eprintln!(
        "Usage: jlo [ env | home | exec | clean | default | init | update | selfupdate | version ]"
    );
    exit(1);
}

/// Determine the requested major version: explicit CLI argument if present,
/// otherwise the project `.jlorc` / user default config.
fn resolve_java_version() -> String {
    let explicit = (env::args().len() > 2).then(|| env::args().nth(2).unwrap());
    resolve_java_version_from(explicit)
}

/// Like [`resolve_java_version`] but with the explicit version supplied by the
/// caller (used by `exec`, whose version is parsed out of its own arguments).
fn resolve_java_version_from(explicit: Option<String>) -> String {
    let java_version = explicit.unwrap_or_else(|| {
        conf::load_config_java_version().unwrap_or_else(|e| {
            eprintln!("{:#}", e);
            exit(1);
        })
    });

    assert_java_version(&java_version);
    java_version
}

fn cmd_env(client: &AdoptiumClient) {
    let java_version = resolve_java_version();
    if let Err(e) = setup(client, &java_version) {
        eprintln!("Error: {:#}", e);
        exit(1);
    }
}

fn cmd_home(client: &AdoptiumClient) {
    let java_version = resolve_java_version();
    let java_home = resolve_java_home(client, &java_version).unwrap_or_else(|e| {
        eprintln!("Error: {:#}", e);
        exit(1);
    });
    println!("{}", java_home.to_string_lossy());
}

fn cmd_exec(client: &AdoptiumClient) {
    let args: Vec<String> = env::args().skip(2).collect();
    let (version, command) = parse_exec_args(&args).unwrap_or_else(|e| {
        eprintln!("Error: {}", e);
        eprintln!("Usage: jlo exec [version] -- <command> [args...]");
        exit(1);
    });

    run_exec(client, version, command);
}

/// Resolve the JDK (installing on demand) and replace the current process with
/// the command. On non-Unix targets `exec` is unsupported, so bail out *before*
/// downloading anything.
#[cfg(unix)]
fn run_exec(client: &AdoptiumClient, version: Option<String>, command: Vec<String>) -> ! {
    let java_version = resolve_java_version_from(version);
    let java_home = resolve_java_home(client, &java_version).unwrap_or_else(|e| {
        eprintln!("Error: {:#}", e);
        exit(1);
    });

    exec_command(&java_home, &command);
}

// A real `execvp` is Unix-only. A native Windows build would replace this with a
// spawn-and-wait fallback that propagates the child's exit code.
#[cfg(not(unix))]
fn run_exec(_client: &AdoptiumClient, _version: Option<String>, _command: Vec<String>) -> ! {
    eprintln!("Error: 'jlo exec' is not supported on this platform.");
    exit(1);
}

/// Split the arguments following `exec` into an optional version and the command
/// to run. The literal `--` separates them; everything before it is the version
/// (zero or one token), everything after is the command.
fn parse_exec_args(args: &[String]) -> Result<(Option<String>, Vec<String>), String> {
    let sep = args
        .iter()
        .position(|a| a == "--")
        .ok_or("expected '--' before the command, e.g. jlo exec 21 -- java -version")?;

    let version = match &args[..sep] {
        [] => None,
        [v] => Some(v.clone()),
        _ => return Err("only one version may be given before '--'".to_string()),
    };

    let command = args[sep + 1..].to_vec();
    if command.is_empty() {
        return Err("no command given after '--'".to_string());
    }

    Ok((version, command))
}

/// Build the child `PATH` with the JDK's `bin` directory prepended.
fn child_path(java_bin: &str, current_path: &str) -> anyhow::Result<String> {
    if current_path.is_empty() {
        return Ok(java_bin.to_string());
    }

    let mut paths = vec![PathBuf::from(java_bin)];
    paths.extend(env::split_paths(current_path));

    Ok(env::join_paths(paths)
        .context("Could not join PATH components")?
        .to_str()
        .context("PATH contains non-UTF-8 characters")?
        .to_string())
}

/// Replace the current process with `command`, having set `JAVA_HOME` and
/// prepended the JDK's `bin` to `PATH`. On Unix this is a real `execvp`, so the
/// child's exit code and signals propagate transparently.
#[cfg(unix)]
fn exec_command(java_home: &Path, command: &[String]) -> ! {
    use std::os::unix::process::CommandExt;
    use std::process::Command;

    let (program, args) = command
        .split_first()
        .expect("command is non-empty (checked in parse_exec_args)");

    let java_bin = java_home.join("bin");
    let new_path = child_path(
        &java_bin.to_string_lossy(),
        &env::var("PATH").unwrap_or_default(),
    )
    .unwrap_or_else(|e| {
        eprintln!("Error: {:#}", e);
        exit(1);
    });

    // `exec` only returns if it failed to launch the program.
    let err = Command::new(program)
        .args(args)
        .env("JAVA_HOME", java_home)
        .env("PATH", new_path)
        .exec();

    eprintln!("Error: could not execute '{}': {}", program, err);
    exit(exec_failure_code(err.kind()));
}

/// Map a launch failure to a shell-conventional exit code: 126 for a command
/// that exists but can't be run (e.g. not executable), 127 otherwise.
fn exec_failure_code(kind: std::io::ErrorKind) -> i32 {
    match kind {
        std::io::ErrorKind::PermissionDenied => 126,
        _ => 127,
    }
}

fn cmd_clean() {
    let jdk_base = jdk_base_dir().unwrap_or_else(|e| {
        eprintln!("Error: {:#}", e);
        exit(1);
    });
    clean_jdks(&jdk_base).unwrap_or_else(|e| {
        eprintln!("Error: Could not clean JDKs: {:#}", e);
        exit(1);
    })
}

fn cmd_default() {
    let java_version = match env::args().nth(2) {
        Some(v) => v,
        None => {
            eprintln!("Error: Missing argument for default command.");
            print_usage_and_exit();
        }
    };

    assert_java_version(&java_version);
    conf::init_default_config(java_version).unwrap_or_else(|e| {
        eprintln!("Error: Could not create default config file: {:#}", e);
        exit(1);
    });
}

fn cmd_init(client: &AdoptiumClient) {
    let java_version = if env::args().len() > 2 {
        env::args().nth(2).unwrap()
    } else {
        client.latest_major().unwrap_or_else(|e| {
            eprintln!("Error: Could not fetch latest JDK version: {:#}", e);
            exit(1);
        })
    };

    assert_java_version(&java_version);

    conf::init_project_config(java_version).unwrap_or_else(|e| {
        eprintln!("Error: Could not create config file: {:#}", e);
        exit(1);
    });
}

fn cmd_update(client: &AdoptiumClient) {
    let mut versions_to_install: HashSet<String> = HashSet::new();

    let args: Vec<String> = env::args().skip(2).collect();

    if args.is_empty() {
        let java_version = conf::load_config_java_version().unwrap_or_else(|e| {
            eprintln!("Error: Could not load configuration: {:#}", e);
            exit(1);
        });
        versions_to_install.insert(java_version);
    } else {
        if args.iter().any(|arg| arg == "all") {
            find_installed_major_versions(&jdk_base_dir().unwrap_or_else(|e| {
                eprintln!("Error: {:#}", e);
                exit(1);
            }))
            .unwrap_or_else(|e| {
                eprintln!("Error: Could not determine installed JDK versions: {:#}", e);
                exit(1);
            })
            .into_iter()
            .for_each(|v| {
                versions_to_install.insert(v.to_string());
            });
        }

        args.into_iter().filter(|arg| arg != "all").for_each(|v| {
            if !conf::is_valid_version(&v) {
                eprintln!("Skipping invalid version: '{}'.", v)
            } else {
                versions_to_install.insert(v);
            }
        });

        if versions_to_install.is_empty() {
            eprintln!("No valid Java versions provided to update.");
            exit(1);
        }
    }

    // Sort versions_to_install alphabetically for consistent processing order
    let mut versions_to_install: Vec<_> = versions_to_install.into_iter().collect();
    versions_to_install.sort();

    for java_version in versions_to_install {
        update(client, &java_version);
    }
}

fn update(client: &AdoptiumClient, java_version: &str) {
    let jdk_metadata = client.fetch_metadata(java_version).unwrap_or_else(|e| {
        eprintln!("Error: Could not fetch JDK metadata: {:#}", e);
        exit(1);
    });

    let jdk_base = jdk_base_dir().unwrap_or_else(|e| {
        eprintln!("Error: {:#}", e);
        exit(1);
    });

    if let Some(path) = find_installed_jdk(&jdk_metadata, &jdk_base) {
        eprintln!(
            "Most recent version of JDK {} is already installed at: {}",
            java_version,
            path.to_string_lossy()
        );
    } else {
        install_jdk(client, &jdk_base, &jdk_metadata).unwrap_or_else(|e| {
            eprintln!("Error: Could not install JDK: {:#}", e);
            exit(1);
        });
    }
}

/// Resolve the JAVA_HOME for the requested major version, installing the JDK on
/// demand if it is not already present. Diagnostics go to stderr; this returns
/// the path so callers decide what (if anything) to print to stdout.
fn resolve_java_home(client: &AdoptiumClient, java_version: &str) -> anyhow::Result<PathBuf> {
    let jdk_base = jdk_base_dir()?;

    match find_suitable_jdk(&jdk_base, java_version) {
        Some(path) => Ok(path),
        None => {
            let metadata = client.fetch_metadata(java_version)?;
            install_jdk(client, &jdk_base, &metadata)
        }
    }
}

fn setup(client: &AdoptiumClient, java_version: &str) -> anyhow::Result<()> {
    let java_home = resolve_java_home(client, java_version)?;

    let mut updates = false;

    let current_java_home = env::var("JAVA_HOME").unwrap_or_default();
    if current_java_home != java_home.to_string_lossy() {
        updates = true;
        println!("export JAVA_HOME=\"{}\"", java_home.to_string_lossy());
    }

    let java_bin_path = java_home.join("bin").to_string_lossy().into_owned();
    let current_path = env::var("PATH").unwrap_or_default();
    let jlo_base = jlo_home_dir()?;
    if let Some(updated_path) = update_path(&java_bin_path, &current_path, &jlo_base)? {
        updates = true;
        println!("export PATH=\"{}\"", updated_path);
    }

    if updates {
        eprintln!("Use Java from {}", java_home.to_string_lossy());
    }

    Ok(())
}

fn install_jdk(
    client: &AdoptiumClient,
    jdk_base: &Path,
    jdk_metadata: &JdkMetadata,
) -> anyhow::Result<PathBuf> {
    // Download JDK
    let temp_dir = tempdir().context("could not create temporary directory")?;
    let temp_file = temp_dir.path().join(&jdk_metadata.package_name);
    let file = &mut File::create(&temp_file).context("could not create temporary file")?;
    client.download(jdk_metadata, file)?;

    // Extract JDK to temp dir
    extract::extract(&temp_file, temp_dir.path())?;

    let dest_dir = jdk_base.join(&jdk_metadata.semver);
    adoptium::install_jdk(jdk_metadata, temp_dir.path(), dest_dir.as_path())?;

    temp_dir.close().unwrap_or_else(|err| {
        eprintln!("Warning: Could not delete temporary directory: {}", err);
    });

    Ok(dest_dir)
}

fn jlo_home_dir() -> anyhow::Result<PathBuf> {
    let path = env::var_os("JLO_HOME")
        .map(PathBuf::from)
        .or_else(env::home_dir)
        .context("Could not determine home directory.")?;
    Ok(path)
}

fn jdk_base_dir() -> anyhow::Result<PathBuf> {
    let home = env::home_dir().context("Could not determine home directory")?;
    Ok(match env::consts::OS {
        "macos" => home.join("Library/Java/JavaVirtualMachines"),
        _ => home.join("jdks"),
    })
}

fn update_path(
    java_path: &str,
    current_path: &str,
    jlo_base: &Path,
) -> anyhow::Result<Option<String>> {
    // Remove any existing J'Lo paths to avoid duplicates
    let mut path_vector: Vec<_> = env::split_paths(current_path)
        .filter(|p| !p.starts_with(jlo_base))
        .collect();

    // Insert the new path at the beginning
    path_vector.insert(0, java_path.into());

    // Join paths back into a single string
    let new_path = env::join_paths(path_vector)
        .context("Could not join PATH components")?
        .to_str()
        .context("PATH contains non-UTF-8 characters")?
        .to_string();

    // Only return if the path has changed
    if new_path == current_path {
        Ok(None)
    } else {
        Ok(Some(new_path))
    }
}

fn assert_java_version(java_version: &str) {
    if !conf::is_valid_version(java_version) {
        eprintln!(
            "Unsupported version: '{}'. Only major versions 8, 11, ... are supported.",
            java_version
        );
        exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn owned(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parse_exec_args_version_and_command() {
        let (version, command) =
            parse_exec_args(&owned(&["21", "--", "java", "-version"])).unwrap();
        assert_eq!(version, Some("21".to_string()));
        assert_eq!(command, owned(&["java", "-version"]));
    }

    #[test]
    fn parse_exec_args_no_version_uses_none() {
        let (version, command) = parse_exec_args(&owned(&["--", "java", "-version"])).unwrap();
        assert_eq!(version, None);
        assert_eq!(command, owned(&["java", "-version"]));
    }

    #[test]
    fn parse_exec_args_missing_separator_errors() {
        assert!(parse_exec_args(&owned(&["21", "java", "-version"])).is_err());
    }

    #[test]
    fn parse_exec_args_empty_command_errors() {
        assert!(parse_exec_args(&owned(&["21", "--"])).is_err());
    }

    #[test]
    fn parse_exec_args_multiple_versions_error() {
        assert!(parse_exec_args(&owned(&["21", "25", "--", "java"])).is_err());
    }

    #[test]
    fn exec_failure_code_distinguishes_not_found_and_not_executable() {
        use std::io::ErrorKind;
        assert_eq!(exec_failure_code(ErrorKind::NotFound), 127);
        assert_eq!(exec_failure_code(ErrorKind::PermissionDenied), 126);
        assert_eq!(exec_failure_code(ErrorKind::Other), 127);
    }

    #[test]
    fn parse_exec_args_no_args_errors() {
        assert!(parse_exec_args(&owned(&[])).is_err());
    }

    #[test]
    fn parse_exec_args_only_separator_errors() {
        // "--" alone: no version, no command
        assert!(parse_exec_args(&owned(&["--"])).is_err());
    }

    #[test]
    fn parse_exec_args_double_dash_in_command_is_preserved() {
        // only the first "--" separates; later ones belong to the command
        let (version, command) =
            parse_exec_args(&owned(&["21", "--", "sh", "-c", "--", "x"])).unwrap();
        assert_eq!(version, Some("21".to_string()));
        assert_eq!(command, owned(&["sh", "-c", "--", "x"]));
    }

    #[test]
    fn child_path_prepends_java_bin() {
        assert_eq!(
            child_path("/jdk/21/bin", "/usr/bin:/bin").unwrap(),
            "/jdk/21/bin:/usr/bin:/bin"
        );
    }

    #[test]
    fn child_path_handles_empty_path() {
        assert_eq!(child_path("/jdk/21/bin", "").unwrap(), "/jdk/21/bin");
    }

    #[test]
    fn update_path_inserts_at_front() {
        let jlo_base = Path::new("/home/user/.jlo");
        let result = update_path("/new/java/bin", "/usr/bin:/usr/local/bin", jlo_base).unwrap();
        assert_eq!(result.unwrap(), "/new/java/bin:/usr/bin:/usr/local/bin");
    }

    #[test]
    fn update_path_removes_existing_jlo_paths() {
        let jlo_base = Path::new("/home/user/.jlo");
        let current = "/home/user/.jlo/old/bin:/usr/bin";
        let result = update_path("/new/java/bin", current, jlo_base).unwrap();
        assert_eq!(result.unwrap(), "/new/java/bin:/usr/bin");
    }

    #[test]
    fn update_path_returns_none_when_unchanged() {
        let jlo_base = Path::new("/home/user/.jlo");
        // java_path starts with jlo_base, so it gets filtered then re-inserted — net no change
        let current = "/home/user/.jlo/jdks/21/bin:/usr/bin";
        let result = update_path("/home/user/.jlo/jdks/21/bin", current, jlo_base).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn update_path_handles_empty_path() {
        let jlo_base = Path::new("/home/user/.jlo");
        let result = update_path("/new/java/bin", "", jlo_base).unwrap();
        assert_eq!(result.unwrap(), "/new/java/bin:");
    }

    #[test]
    fn jdk_base_dir_returns_plausible_path() {
        let base = jdk_base_dir().unwrap();
        let path_str = base.to_string_lossy();
        if cfg!(target_os = "macos") {
            assert!(path_str.contains("Library/Java/JavaVirtualMachines"));
        } else {
            assert!(path_str.ends_with("jdks"));
        }
    }

    #[test]
    #[serial_test::serial]
    fn jlo_home_dir_uses_env_var() {
        let dir = tempdir().unwrap();
        unsafe {
            env::set_var("JLO_HOME", dir.path());
        }
        let result = jlo_home_dir().unwrap();
        assert_eq!(result, dir.path());
        unsafe {
            env::remove_var("JLO_HOME");
        }
    }
}
