//! `$JLO_HOME`: where J'Lo keeps the user's `default.jlorc` and, for the curl
//! install, its own layout. Its own module because `conf` reads it and
//! `install` writes under it, and neither should import the other for it.

use anyhow::{Context, anyhow};
use std::env;
use std::path::PathBuf;

/// Used when `JLO_HOME` is unset. Must stay in sync with `install.sh`.
pub(crate) const JLO_HOME_DIR_NAME: &str = ".jlo";

/// The fallback must match what `install.sh` exports: scripts calling
/// `jlo-bin` directly have no `JLO_HOME` and must resolve the same files.
pub(crate) fn jlo_home_dir() -> anyhow::Result<PathBuf> {
    // Empty is unset: taken literally it roots the whole layout at `/`.
    let path = env::var_os("JLO_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| env::home_dir().map(|home| home.join(JLO_HOME_DIR_NAME)))
        .context("could not determine home directory.")?;
    checked_path("JLO_HOME", path)
}

/// Refuse a path that could not be written into generated shell code and mean
/// the same thing wherever that code runs. `what` names the path in the error.
pub(crate) fn checked_path(what: &str, path: PathBuf) -> anyhow::Result<PathBuf> {
    // The layout is full of paths that outlive this process (the symlink
    // target, the stubs), and a relative one names a different directory
    // from everywhere they are used.
    if !path.is_absolute() {
        return Err(anyhow!(
            "{what} must be an absolute path, but is '{}'",
            path.display()
        ));
    }

    // The stubs spell this path with `display()`, which substitutes U+FFFD:
    // the files would name a directory that does not exist.
    if path.to_str().is_none() {
        return Err(anyhow!(
            "{what} is not valid UTF-8, so jlo cannot write it into the shell code it generates: '{}'",
            path.display()
        ));
    }

    // A newline would split a copied profile line or end a generated comment
    // early, and no quoting survives that.
    if path.to_string_lossy().chars().any(char::is_control) {
        return Err(anyhow!(
            "{what} must not contain control characters (a newline, say): '{}'",
            path.display().to_string().escape_debug()
        ));
    }

    Ok(path)
}

#[cfg(test)]
// `env::set_var` requires `unsafe` under edition 2024; the mutations here are
// guarded by `serial_test`.
#[allow(unsafe_code)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    #[serial_test::serial]
    fn jlo_home_dir_uses_env_var() {
        let dir = tempdir().unwrap();
        let result = with_jlo_home(dir.path().as_os_str(), jlo_home_dir).unwrap();
        assert_eq!(result, dir.path());
    }

    /// The values that cannot be a home, refused here rather than at
    /// each of the places that write the layout out. `install` spells this
    /// path into the generated stubs and into the `~/.local/bin/jlo` symlink
    /// target, so a value that survives to there produces an install that is
    /// wrong in a different way for each of them.
    #[test]
    #[serial_test::serial]
    fn jlo_home_dir_refuses_a_value_it_could_not_write_down() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        // Empty: a shell spells "unset" this way, and taking it literally
        // roots the whole layout at `/`.
        let empty = with_jlo_home(&OsString::new(), jlo_home_dir);
        assert_eq!(
            empty.expect("empty falls back to $HOME/.jlo"),
            env::home_dir().unwrap().join(".jlo")
        );

        // Relative: a different directory from every working directory.
        let relative = with_jlo_home(&OsString::from("jlo-home"), jlo_home_dir)
            .expect_err("a relative home is refused");
        assert!(relative.to_string().contains("absolute"), "{relative}");

        // Undecodable: written into the stubs as U+FFFD, naming a directory
        // that does not exist.
        let undecodable = OsString::from_vec(vec![b'/', b't', b'm', b'p', b'/', 0xff]);
        let err = with_jlo_home(&undecodable, jlo_home_dir).expect_err("refused");
        assert!(err.to_string().contains("not valid UTF-8"), "{err}");

        // A newline: splits a copied profile line, or ends a generated comment early.
        let newline = with_jlo_home(&OsString::from("/tmp/jlo\nx"), jlo_home_dir)
            .expect_err("a newline is refused");
        assert!(
            newline.to_string().contains("control characters"),
            "{newline}"
        );
    }

    /// Run `f` with `JLO_HOME` set to `value`, restoring the variable after.
    fn with_jlo_home<T>(value: &std::ffi::OsStr, f: impl FnOnce() -> T) -> T {
        let previous = env::var_os("JLO_HOME");
        unsafe {
            env::set_var("JLO_HOME", value);
        }
        let out = f();
        unsafe {
            match previous {
                Some(previous) => env::set_var("JLO_HOME", previous),
                None => env::remove_var("JLO_HOME"),
            }
        }
        out
    }
}
