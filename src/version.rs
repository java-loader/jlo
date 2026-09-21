//! Ordering for the version strings jlo handles: JDK directory and catalogue
//! names like `21.0.11+10.0.LTS`, and J'Lo's own release tags.
//!
//! Both are proper semver, so this is a thin wrapper over [`semver`] that
//! exists only so the parse-parse-compare dance is written once. The strictness
//! is the point: a name that is not semver is an error here, which is how
//! `temurin-21` and friends get filtered out of the store.
//!
//! Build metadata (`+10.0.LTS`) is ignored when ordering, so two Adoptium
//! builds of the same patch compare `Equal`. That is what the semver spec says
//! precedence means, and it is what jlo has always done. Note that it is *not*
//! what `Ord for Version` does - that derive includes the `build` field and
//! gives it a total order - so every comparison here goes through
//! `cmp_precedence` instead.

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

/// Order `a` against `b` by semver precedence, failing if either side is not
/// semver. Build metadata does not participate - see the module comment.
pub(crate) fn compare(a: &str, b: &str) -> Result<Ordering> {
    Ok(parse(a)?.cmp_precedence(&parse(b)?))
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
        }
    }

    /// A hand-registered `v21.0.11+9` parsed under `semver_rs` and still has
    /// to: the directory name is the whole registration for a JDK jlo did not
    /// install, so refusing it would make that install vanish without a word.
    #[test]
    fn one_leading_v_is_tolerated() {
        assert_eq!(parse("v21.0.11+9").expect("parse").major, 21);
        assert_eq!(
            compare("v21.0.12+9", "21.0.11+9").expect("compare"),
            Ordering::Greater
        );
        assert_eq!(
            compare("v21.0.11+9", "21.0.11+9").expect("compare"),
            Ordering::Equal
        );
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

    /// Build metadata is not part of the precedence order, so the two builds
    /// of a patch are interchangeable as far as every caller is concerned.
    /// `jlo prune` and the `superseded` status both depend on this: neither
    /// may claim one build supersedes the other.
    #[test]
    fn build_metadata_does_not_order() {
        assert_eq!(
            compare("17.0.11+10", "17.0.11+9").expect("compare"),
            Ordering::Equal
        );
        assert_eq!(
            compare("21.0.11+10.0.LTS", "21.0.11+9.0.LTS").expect("compare"),
            Ordering::Equal
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

    #[test]
    fn a_non_semver_side_is_an_error() {
        assert!(compare("temurin-21", "21.0.1+1").is_err());
        assert!(compare("21.0.1+1", "temurin-21").is_err());
    }
}
