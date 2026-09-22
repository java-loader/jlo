//! Guards over the source tree itself, for two rules the compiler cannot see.
//!
//! Both are ADR decisions whose failure mode is silent: a `println!` on an
//! untested path is sourced by the user's shell, and a `style()` call outside
//! `ui.rs` gives one command a palette nobody agreed to. Individual commands
//! assert `stdout("")` and individual lines assert their colours, but neither
//! enumerates the call sites, so neither notices a new one.

#![allow(clippy::unwrap_used)]

use std::path::Path;

/// Every `.rs` file under `src/`, as (path, source).
fn sources() -> Vec<(String, String)> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(Path::new(env!("CARGO_MANIFEST_DIR")).join("src")).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) == Some("rs") {
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            out.push((name, std::fs::read_to_string(&path).unwrap()));
        }
    }
    out.sort();
    assert!(out.len() > 5, "src/ should hold the whole crate");
    out
}

/// Lines with the `//` stripped off, so a rule cannot be tripped by prose
/// *about* it - both files below discuss what they must not do.
fn code_lines(source: &str) -> impl Iterator<Item = (usize, &str)> {
    source.lines().enumerate().filter_map(|(i, line)| {
        let code = line.split("//").next().unwrap_or("");
        (!code.trim().is_empty()).then_some((i + 1, code))
    })
}

/// ADR-0001: stdout is the environment channel. The `jlo` shell function
/// evaluates what lands there, so a status message written with `println!`
/// is not a cosmetic bug - the shell tries to execute it.
///
/// One writer is allowed, and it is the one machine-output path that does not
/// go through `ui::print_lines`: `jlo home`'s bare path. (`print_lines` itself
/// writes with `writeln!`, because it has to treat a closed pipe as an ending
/// rather than a panic.) Anything else is a bug, whether or not a test happens
/// to cover the path it sits on.
#[test]
fn stdout_is_written_only_by_the_one_machine_output_path() {
    // The call site is identified by what it is, not by where it sits: a
    // line number in the expected list makes an unrelated comment two
    // functions above it fail this test, which teaches the next person to
    // edit the expectation rather than to read it. The line is still
    // reported, because a violation is much easier to find with one.
    const ALLOWED: &str = "println!(\"{}\", path_str(&java_home)?);";

    let mut found = Vec::new();
    for (name, source) in sources() {
        for (line, code) in code_lines(&source) {
            if writes_to_stdout(code) {
                found.push((format!("src/{name}:{line}"), code.trim().to_string()));
            }
        }
    }

    let unexpected: Vec<&(String, String)> =
        found.iter().filter(|(_, code)| code != ALLOWED).collect();

    assert!(
        unexpected.is_empty(),
        "unexpected write to stdout - every user-facing message is an eprintln!, \
         and listing rows go through ui::print_lines: {unexpected:?}"
    );
    assert_eq!(
        found.len(),
        1,
        "the one allowed stdout writer is `jlo home`'s bare path; found {found:?}"
    );
}

/// `println!`/`print!` as a macro call, not as the tail of `eprintln!`.
fn writes_to_stdout(code: &str) -> bool {
    ["println!", "print!"].iter().any(|macro_name| {
        code.match_indices(macro_name).any(|(at, _)| {
            code[..at]
                .chars()
                .next_back()
                .is_none_or(|c| !c.is_alphanumeric() && c != '_')
        })
    })
}

/// ADR-0007, rule 6: styling is chosen in `ui.rs` and nowhere else. Commands
/// say what a line *is* by calling a named helper; they never pick a colour.
///
/// Indicatif templates count, and are named here explicitly because that is
/// where the fourth palette hid last time: a template string is styling by
/// another name, and `{spinner:.cyan}` reads as configuration rather than as
/// a colour decision.
#[test]
fn styling_is_chosen_only_in_ui_rs() {
    const MARKERS: [&str; 6] = [
        "style(",
        "ProgressStyle",
        ".template(",
        "\\x1b[",
        "ansi_term",
        "colored::",
    ];

    let mut found = Vec::new();
    for (name, source) in sources() {
        if name == "ui.rs" {
            continue;
        }
        for (line, code) in code_lines(&source) {
            if let Some(marker) = MARKERS.iter().find(|m| code.contains(**m)) {
                found.push(format!("src/{name}:{line}: {marker} in {}", code.trim()));
            }
        }
    }

    assert!(
        found.is_empty(),
        "styling belongs in ui.rs behind a named helper (ADR-0007 rule 6):\n{found:#?}"
    );
}
