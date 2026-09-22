//! What version of Java was *asked for*: a major, and which stream of it.
//!
//! Adoptium runs two streams per major - the released builds, and an
//! early-access stream that keeps running after the major goes GA (asking for
//! the EA of 26 today yields `26.0.2-beta`, a preview of the next patch). A
//! pre-release sorts *above* the GA build it previews, so a store that mixed
//! the two would hand out a beta for `jlo env 26` and delete the GA build as
//! superseded.
//!
//! So the two are not mixed: `26-ea` is a name that stands beside `26`, and
//! this type is that name. Every rule keyed on "a major" is keyed on a
//! `Request` instead, which is why no comparison anywhere sees both streams
//! and no filter has to remember to exclude one.

use anyhow::bail;
use std::fmt;

/// The suffix that names the pre-release stream. One spelling: a store
/// directory is named by Adoptium's semver, and a second accepted spelling
/// would let the requested name and the installed name disagree.
const EA_SUFFIX: &str = "-ea";

/// The oldest major J'Lo will address. Adoptium offers 8 and up, and the floor
/// is also what keeps a prefix match from making `1` select `17`.
const OLDEST_MAJOR: i64 = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum Stream {
    /// A released build: `21.0.5+11`.
    Ga,
    /// A pre-release build: `28.0.0-beta+16.0.ea`.
    Ea,
}

/// A major version and one of its two streams - `21`, or `28-ea`.
///
/// `Ord` is derived and is relied on: major first, then stream, with `Ga`
/// before `Ea` because that is the declaration order. `prune` and
/// `installed_requests` sort by it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct Request {
    pub major: i64,
    pub stream: Stream,
}

impl Request {
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

        // The parse failure's own wording ("invalid digit found in string")
        // describes the input, not the rule, so it is replaced rather than
        // wrapped: one message answers the only question a rejection raises.
        //
        // `i64::from_str` is used rather than a hand-rolled digit scan so the
        // set of accepted spellings is exactly what `u32::from_str` accepted
        // before - a leading `+` included. Widening the grammar is the change
        // being made here; narrowing it is not.
        let Ok(major) = digits.parse::<i64>() else {
            bail!(Self::rejection(text));
        };

        if major < OLDEST_MAJOR {
            bail!(Self::rejection(text));
        }

        Ok(Self { major, stream })
    }

    pub(crate) fn is_ea(self) -> bool {
        self.stream == Stream::Ea
    }

    /// The message a rejected version earns. One wording for every rejection,
    /// because the reader's question is the same in each case: what *is*
    /// accepted? `conf::load` reports it too, wrapped with the file it came
    /// from, so that a typo in a `.jlorc` reads the same as one on the command
    /// line.
    pub(crate) fn rejection(text: &str) -> String {
        format!(
            "unsupported version '{text}': expected a major version (8, 11, 21) \
             or a pre-release stream ('28-ea')"
        )
    }
}

/// Which stream a parsed build version belongs to.
///
/// The prerelease field is the whole test: Adoptium spells every early-access
/// build with one (`-beta`), and no released build carries one.
pub(crate) fn stream_of(version: &semver::Version) -> Stream {
    if version.pre.is_empty() {
        Stream::Ga
    } else {
        Stream::Ea
    }
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

    /// Ordering is what `installed_requests` and `prune` sort by, and the two
    /// streams of one major must not interleave with another major's.
    #[test]
    fn a_request_orders_by_major_then_stream() {
        let mut names = vec![
            Request {
                major: 28,
                stream: Stream::Ea,
            },
            Request {
                major: 21,
                stream: Stream::Ga,
            },
            Request {
                major: 28,
                stream: Stream::Ga,
            },
        ];
        names.sort();
        assert_eq!(
            names,
            vec![
                Request {
                    major: 21,
                    stream: Stream::Ga
                },
                Request {
                    major: 28,
                    stream: Stream::Ga
                },
                Request {
                    major: 28,
                    stream: Stream::Ea
                },
            ]
        );
    }
}
