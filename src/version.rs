//! Ordering for the version strings jlo handles: JDK directory and catalogue
//! names like `21.0.11+10.0.LTS`, and J'Lo's own release tags.
//!
//! Both are proper semver, so this is a thin wrapper over [`semver`] that
//! exists only so the parse-parse-compare dance is written once. The strictness
//! is the point: a name that is not semver is an error here, which is how
//! `temurin-21` and friends get filtered out of the store.
//!
//! Build metadata (`+10.0.LTS`) participates in the ordering, which the semver
//! spec does not ask for: it says precedence ignores build metadata, so
//! `21.0.11+10.0.LTS` and `21.0.11+9.0.LTS` are equally new.
//!
//! jlo needs more than precedence. Adoptium's build metadata carries the JDK
//! build number, two builds of one patch are two directories in the store, and
//! `jlo remove --superseded` has to say which of them it keeps. Left equal they sorted
//! arbitrarily and `prune` deleted whichever `read_dir` yielded second, which
//! took the newer build about half the time. `Ord for Version` orders the
//! `build` field after the rest, so `+10.0.LTS` is newer than `+9.0.LTS` and
//! the answer is both stable and right.
//!
//! This is an ordering over Adoptium's naming, not over semver in general.
//! Within one major Adoptium keeps a single shape - `+101.0.LTS` for an LTS
//! line, `+12` for the rest - so the identifiers being compared are always
//! alike, and the first of them is the build number.

use anyhow::{Context, Result};
use semver::Version;
use std::cmp::Ordering;

/// Parse a semver version, tolerating surrounding whitespace and one leading
/// `v`.
///
/// Neither is in the spec and `semver` rejects both, but `semver_rs` - which
/// this replaced - was node-semver's parser: it trimmed its input and carried
/// `^v?` in its version regex. A JDK registered by hand as `v21.0.11+9` has
/// therefore always been found, and refusing it now would make that install
/// vanish from `jlo list` and from `jlo env 21` without a word. The directory
/// name is the whole registration, so both are honoured here on purpose.
pub(crate) fn parse(name: &str) -> Result<Version> {
    let trimmed = name.trim();
    Version::parse(trimmed.strip_prefix('v').unwrap_or(trimmed))
        .with_context(|| format!("{name:?} is not a semver version"))
}

/// Order `a` against `b`, failing if either side is not semver. Build metadata
/// breaks a tie rather than being ignored - see the module comment.
pub(crate) fn compare(a: &str, b: &str) -> Result<Ordering> {
    Ok(parse(a)?.cmp(&parse(b)?))
}

/// [`compare`] reversed, for sorting newest first. A name that is not semver
/// compares equal, so a stable sort leaves it where it was.
pub(crate) fn cmp_desc(a: &str, b: &str) -> Ordering {
    compare(b, a).unwrap_or(Ordering::Equal)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adoptium_build_metadata_parses() {
        for name in [
            "21.0.11+10.0.LTS",
            "11.0.32+101",
            "25.0.4+101.0.LTS",
            "8.0.442+6",
        ] {
            assert!(parse(name).is_ok(), "{name}");
        }
    }

    #[test]
    fn non_semver_names_are_refused() {
        for name in [
            "temurin-21.0.5",
            "21",
            "21.0",
            "",
            "jdk-21.0.5+11",
            "latest",
            "v",
            "version-21.0.5+11",
        ] {
            assert!(parse(name).is_err(), "{name}");
            // Either side: `is_older_than` reads the error as "never older".
            assert!(compare(name, "21.0.1+1").is_err(), "{name} vs semver");
            assert!(compare("21.0.1+1", name).is_err(), "semver vs {name}");
        }
    }

    /// A hand-registered `v21.0.11+9` parsed under `semver_rs` and still has
    /// to: the directory name is the whole registration for a JDK jlo did not
    /// install, so refusing it would make that install vanish without a word.
    #[test]
    fn one_leading_v_is_tolerated() {
        assert_eq!(parse("v21.0.11+9").expect("parse").major, 21);
        // Only one, and only at the front.
        assert!(parse("vv21.0.11+9").is_err());
    }

    /// `semver_rs` trimmed before parsing. A directory name with stray
    /// whitespace is a mistake, but it was a *findable* one, and this swap is
    /// not the place to turn it into a JDK that silently went missing.
    #[test]
    fn surrounding_whitespace_is_tolerated() {
        for name in [
            " 21.0.11+9",
            "21.0.11+9\t",
            "  v21.0.11+9  ",
            "\n21.0.11+9\n",
        ] {
            assert_eq!(parse(name).expect(name).major, 21, "{name:?}");
        }
        // Trimming is not the same as ignoring interior whitespace.
        assert!(parse("v 21.0.11+9").is_err());
        assert!(parse("21.0. 11+9").is_err());
    }

    /// The build number breaks a tie between two builds of one patch. `jlo
    /// remove --superseded` deletes on this answer, so it has to be the higher build that
    /// wins and it has to be numeric: `+10` is newer than `+9`, not older the
    /// way a string comparison would have it.
    #[test]
    fn the_build_number_breaks_a_tie() {
        assert_eq!(
            compare("17.0.11+10", "17.0.11+9").expect("compare"),
            Ordering::Greater
        );
        assert_eq!(
            compare("21.0.11+10.0.LTS", "21.0.11+9.0.LTS").expect("compare"),
            Ordering::Greater
        );
    }

    /// A tie is still reachable, because two names can spell one version.
    /// Callers that delete have to treat that as "nothing to choose between
    /// these" rather than as an order.
    #[test]
    fn two_names_for_one_version_stay_equal() {
        assert_eq!(
            compare("v21.0.11+9", "21.0.11+9").expect("compare"),
            Ordering::Equal
        );
        assert_eq!(
            compare(" 21.0.11+9 ", "21.0.11+9").expect("compare"),
            Ordering::Equal
        );
    }

    /// The patch still outranks the build number, or a rebuild of an old patch
    /// would look newer than the patch that superseded it.
    #[test]
    fn the_patch_outranks_the_build_number() {
        assert_eq!(
            compare("21.0.12+1", "21.0.11+99").expect("compare"),
            Ordering::Greater
        );
    }

    #[test]
    fn numeric_fields_order_numerically_not_lexically() {
        assert_eq!(
            compare("21.0.10+7", "21.0.9+7").expect("c"),
            Ordering::Greater
        );
        assert_eq!(compare("9.0.1+1", "10.0.1+1").expect("c"), Ordering::Less);
    }
}
