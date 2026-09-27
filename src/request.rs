//! What version of Java was *asked for*: a major, and which stream of it.
//!
//! An EA build of a later patch sorts *above* the current GA build, so mixing
//! the streams would hand out a beta for `jlo env 26` and delete the GA build
//! as superseded. Every rule keyed on "a major" is keyed on a `Request` instead,
//! so no comparison sees both streams.

use anyhow::bail;
use std::cmp::Reverse;
use std::fmt;

/// One spelling: a second would let the requested and installed names
/// disagree.
const EA_SUFFIX: &str = "-ea";

/// The oldest major J'Lo will address. Adoptium offers 8 and up, and the floor
/// is also what keeps a prefix match from making `1` select `17`.
pub(crate) const OLDEST_MAJOR: i64 = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum Stream {
    /// A released build: `21.0.5+11`.
    Ga,
    /// A pre-release build: `28.0.0-beta+16.0.ea`.
    Ea,
}

/// A major version and one of its two streams - `21`, or `28-ea`.
///
/// Deliberately not `Ord`: names are put in order in one way only,
/// [`Self::listing_order`], and a derived `Ord` would be a second.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct Request {
    pub major: i64,
    pub stream: Stream,
}

impl Request {
    /// The sort key of every per-name output: newest major first, and the
    /// released name before the pre-release (`Ga` is declared before `Ea`).
    /// Reversing a plain major-then-stream order would lead with `27-ea`.
    pub(crate) fn listing_order(self) -> (Reverse<i64>, Stream) {
        (Reverse(self.major), self.stream)
    }

    /// Parse the one grammar J'Lo accepts for a version: a major of 8 or more,
    /// optionally suffixed `-ea`.
    ///
    /// Deliberately strict about surrounding text. This parses what a user
    /// typed or what a `.jlorc` holds, and a silently trimmed or case-folded
    /// answer is a different JDK than the one that was written down.
    pub(crate) fn parse(text: &str) -> anyhow::Result<Self> {
        let (digits, stream) = match text.strip_suffix(EA_SUFFIX) {
            Some(digits) => (digits, Stream::Ea),
            None => (text, Stream::Ga),
        };

        // Replaced, not wrapped: "invalid digit found in string" describes the
        // input, not the rule.
        //
        // `from_str`, not a digit scan: it accepts a leading `+`, as the
        // grammar always has, and the grammar may only widen.
        match digits.parse::<i64>() {
            Ok(major) if major >= OLDEST_MAJOR => Ok(Self { major, stream }),
            _ => bail!(Self::rejection(text)),
        }
    }

    /// The name a build answers to; `None` when its major does not fit an
    /// `i64`. The prerelease field is the whole stream test. No version floor,
    /// unlike [`Self::parse`]: a pre-8 JDK is still an install `jlo list` and
    /// `jlo remove` must see.
    pub(crate) fn of_build(version: &semver::Version) -> Option<Self> {
        let major = i64::try_from(version.major).ok()?;
        let stream = if version.pre.is_empty() {
            Stream::Ga
        } else {
            Stream::Ea
        };
        Some(Self { major, stream })
    }

    pub(crate) fn is_ea(self) -> bool {
        self.stream == Stream::Ea
    }

    /// One wording for every rejection, naming what *is* accepted.
    pub(crate) fn rejection(text: &str) -> String {
        format!(
            "unsupported version '{text}': expected a major version (8, 11, 21) \
             or a pre-release stream ('28-ea')"
        )
    }
}

/// A test fixture's version name, parsed - one spelling for every test module.
#[cfg(test)]
pub(crate) fn request(text: &str) -> Request {
    Request::parse(text).expect("the fixture names a valid version")
}

impl fmt::Display for Request {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.major)?;
        if self.is_ea() {
            write!(f, "{EA_SUFFIX}")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bare_major_is_the_ga_stream() {
        let request = Request::parse("21").expect("a bare major is valid");
        assert_eq!(
            request,
            Request {
                major: 21,
                stream: Stream::Ga
            }
        );
        assert_eq!(request.to_string(), "21");
    }

    #[test]
    fn the_ea_suffix_names_the_pre_release_stream() {
        let request = Request::parse("28-ea").expect("-ea is valid");
        assert_eq!(
            request,
            Request {
                major: 28,
                stream: Stream::Ea
            }
        );
        assert_eq!(request.to_string(), "28-ea");
        assert!(request.is_ea());
    }

    /// The suffix is one spelling, not a family of them. Accepting `-EA` or
    /// `-beta` would make the store name and the requested name disagree.
    #[test]
    fn only_lowercase_ea_is_accepted() {
        for rejected in [
            "28-EA", "28-Ea", "28-beta", "28-ea-1", "28ea", "28-", "-ea", "ea",
        ] {
            assert!(
                Request::parse(rejected).is_err(),
                "{rejected:?} should be refused"
            );
        }
    }

    /// The floor and the exact-version refusal are the rules that were already
    /// there; widening the grammar must not have loosened them.
    #[test]
    fn the_floor_and_the_major_only_rule_still_hold() {
        for rejected in [
            "7",
            "0",
            "-1",
            "21.0.9",
            "21.0.9+10",
            "21.0.9-ea",
            "",
            " 21",
            "abc",
        ] {
            // `+21` is deliberately absent: `u32::from_str` accepted it before
            // and still does. This widens the grammar; it does not narrow it.
            assert!(
                Request::parse(rejected).is_err(),
                "{rejected:?} should be refused"
            );
        }
        assert!(Request::parse("8").is_ok());
    }

    /// The error is what a user reads after a typo, so it has to name what is
    /// accepted rather than only what was rejected.
    #[test]
    fn the_error_names_both_accepted_forms() {
        let message = Request::parse("21.0.9")
            .expect_err("exact versions are refused")
            .to_string();
        assert!(
            message.contains("21.0.9"),
            "should quote the input: {message}"
        );
        assert!(
            message.contains("-ea"),
            "should name the -ea form: {message}"
        );
    }
}
