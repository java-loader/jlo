use crate::adoptium::RemoteJdk;
use crate::request::{Request, Stream};
use crate::store::{InstalledJdk, JdkStore};
use console::style;
use indicatif::{ProgressBar, ProgressBarIter, ProgressStyle};
use std::cmp::Ordering;
use std::io::{IsTerminal, Read, stderr};
use std::path::{Path, PathBuf};
use std::process::exit;
use std::time::{Duration, Instant};

/// The live progress region for one JDK installation.
///
/// Download, extraction and the final move share a *single* progress line that
/// is erased once the install succeeds, leaving exactly one summary line behind.
///
/// The rule the whole module follows: output persists in proportion to how
/// durable the side effect is. A JDK on disk survives until it is removed, so
/// earns a line. Exporting `JAVA_HOME` lasts until the shell exits and is fully
/// implied by the command the user typed, so it prints nothing at all - which
/// matters because the autoload hook runs on every `cd`.
pub(crate) struct InstallUi {
    bar: ProgressBar,
    /// What is being fetched - "JDK" or "J'Lo". Only the non-tty lines and the
    /// summary name it; the live bar carries the version in its prefix.
    label: &'static str,
    version: String,
    started: Instant,
    tty: bool,
}

impl std::fmt::Debug for InstallUi {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InstallUi")
            .field("label", &self.label)
            .field("version", &self.version)
            .field("tty", &self.tty)
            .finish_non_exhaustive()
    }
}

impl InstallUi {
    pub(crate) fn new(version: &str) -> Self {
        Self::labelled("JDK", version)
    }

    /// The same live region for J'Lo's own binary, which `selfupdate`
    /// downloads over the same `ureq` stack as a JDK.
    pub(crate) fn labelled(label: &'static str, version: &str) -> Self {
        let tty = supports_live_region();
        let bar = if tty {
            // Styled via the builder, which does not draw: a bar configured
            // after construction flashes indicatif's default style on the first
            // redraw. "connecting" is literally true here - the caller has the
            // metadata but has not yet opened the download response.
            ProgressBar::new(0)
                .with_style(spinner_style(&progress_label(label)))
                .with_prefix(progress_prefix(label, version))
                .with_message("connecting")
        } else {
            // Without a terminal there is nothing to redraw over; the plain
            // phase lines below carry the progress instead.
            ProgressBar::hidden()
        };
        if tty {
            bar.enable_steady_tick(TICK);
        }
        Self::with_bar(label, version, bar, tty)
    }

    #[cfg(test)]
    pub(crate) fn hidden(version: &str) -> Self {
        Self::with_bar("JDK", version, ProgressBar::hidden(), false)
    }

    fn with_bar(label: &'static str, version: &str, bar: ProgressBar, tty: bool) -> Self {
        Self {
            bar,
            label,
            version: version.to_string(),
            started: Instant::now(),
            tty,
        }
    }

    pub(crate) fn start_download(&self, total_size: u64) {
        if self.tty {
            // Length first: with pos 0 and len 0 indicatif renders a *full*
            // bar, so switching to the bar style before the length is known
            // would flash a completed download on the first frame.
            self.bar.set_length(total_size);
            self.bar
                .set_style(download_style(&progress_label(self.label)));
        } else {
            eprintln!(
                "Downloading {} {} ({})",
                self.label,
                self.version,
                indicatif::HumanBytes(total_size)
            );
        }
    }

    pub(crate) fn set_downloaded(&self, bytes: u64) {
        self.bar.set_position(bytes);
    }

    /// Extraction deliberately gets a spinner rather than a bar: the only figure
    /// available is compressed bytes read, which runs ahead of the files
    /// actually written and so would show a bar completing before the work does.
    pub(crate) fn start_extract(&self) {
        self.phase("extracting");
    }

    pub(crate) fn start_install(&self) {
        self.phase("installing");
    }

    fn phase(&self, name: &str) {
        if self.tty {
            self.bar
                .set_style(spinner_style(&progress_label(self.label)));
            self.bar.set_message(name.to_string());
        }
    }

    /// Drive the spinner from the archive reader so it keeps moving during a
    /// long extraction without the caller having to tick it.
    pub(crate) fn wrap_read<R: Read>(&self, read: R) -> ProgressBarIter<R> {
        self.bar.wrap_read(read)
    }

    /// Erase the live region and leave the one line that records what landed.
    pub(crate) fn finish(&self, dest: &Path) {
        self.bar.finish_and_clear();
        eprintln!(
            "{}",
            format_summary(
                self.label,
                &self.version,
                &tilde(dest),
                self.started.elapsed()
            )
        );
    }

    /// Erase the live region without a summary. The caller reports the error;
    /// a half-drawn bar above it would only be in the way.
    pub(crate) fn abandon(&self) {
        self.bar.finish_and_clear();
    }
}

/// `jlo install` and `jlo update` exist to answer "is anything newer
/// available?", so the answer is their output - unlike `jlo env`, where
/// silence is the answer.
pub(crate) fn up_to_date(name: &str, version: &str) {
    eprintln!(
        "{} JDK {} is up to date {}",
        style("✓").green().for_stderr(),
        name,
        style(format!("({version})")).dim().for_stderr()
    );
}

/// That Adoptium offers no build of any of `requests` for this machine - the
/// error when nothing else is left to do. Names the platform because that is
/// the half of the fact the user may not know: Adoptium does publish JDK 8,
/// just not for macOS on Apple silicon.
pub(crate) fn not_offered(requests: &[crate::request::Request]) -> String {
    let names: Vec<String> = requests.iter().map(ToString::to_string).collect();
    format!(
        "Adoptium offers no build of {} for {}",
        crate::store::quoted_list(&names),
        crate::adoptium::platform()
    )
}

/// A name `install`/`update` passes over because Adoptium offers no build of
/// it here, while other names still go ahead. Worded like the warning for a
/// version that does not parse, which is skipped the same way.
pub(crate) fn skipping_not_offered(request: crate::request::Request) {
    warning!(
        "skipping '{request}': Adoptium offers no build of it for {}",
        crate::adoptium::platform()
    );
}

/// The advice under a name Adoptium does not offer: the listing is what it
/// does offer for this machine.
pub(crate) const NOT_OFFERED_HINT: &str = "Run 'jlo list' to see what is available.";

/// What a new build replaced, printed under its install summary.
///
/// A pre-release says so. Its stream publishes weekly and a bare `jlo update`
/// moves it, so these lines recur on every run - one that did not read as a
/// preview being swapped for the next would pass for a patch release.
pub(crate) fn replaced(request: crate::request::Request, removed: &[String], failures: &[String]) {
    if !removed.is_empty() {
        eprintln!(
            "{}",
            style(replaced_line(request, removed)).dim().for_stderr()
        );
    }
    for failure in failures {
        eprintln!("{} {}", style("!").red().for_stderr(), failure);
    }
}

fn replaced_line(request: crate::request::Request, removed: &[String]) -> String {
    format!(
        "  replaced {}{}",
        if request.is_ea() { "pre-release " } else { "" },
        removed.join(", ")
    )
}

/// The lines `install` and `update` end on: how many builds they deleted,
/// and whether the shell moved - or why the build it is on was kept.
pub(crate) fn update_report(run: &crate::store::InstallRun) {
    let count = run.removed_count();
    if count > 0 {
        eprintln!(
            "{} Removed {} superseded JDK{}",
            style("✓").green().for_stderr(),
            count,
            if count == 1 { "" } else { "s" }
        );
    }
    if let Some(java_home) = &run.repointed {
        eprintln!(
            "{}",
            style(format!("  JAVA_HOME now points at {}", tilde(java_home)))
                .dim()
                .for_stderr()
        );
    }
    if let Some(version) = &run.kept_active {
        hint!("{}", kept_active_hint(version));
    }
}

/// Said when `install` or `update` kept a superseded build because
/// `JAVA_HOME` points at it: only the `jlo` shell function can move a shell,
/// and it did not run this command.
fn kept_active_hint(version: &str) -> String {
    format!(
        "Kept {version}, which JAVA_HOME points at. Run 'jlo env' in the shell using it, \
         then 'jlo remove --superseded'."
    )
}

/// Diagnostic prefixes.
///
/// Message conventions, so diagnostics from different commands read as one
/// voice:
///
/// - Text after `Error:`/`Warning:` starts lowercase and carries no trailing
///   period, the way cargo and rustc phrase theirs. It is a clause following a
///   label, not a sentence.
/// - `anyhow` contexts follow the same rule: they compose into a chain printed
///   after that label, so a context that says "Error opening archive" renders
///   as "Error: Error opening archive".
/// - Say "could not <verb>" - not "Failed to", "Can't" or "Error <verb>ing".
/// - Do not restate the chain's first clause in the top-level message;
///   `ui::error!("{e:#}")` is right when the context already names the
///   operation.
/// - Hints are the exception: they are advice rather than a label, so they stay
///   sentence-cased with a full stop.
///
/// All four write to stderr and all four are `.for_stderr()`: `console::style`
/// decides whether to emit colour by looking at *stdout*, and the `jlo` shell
/// function runs `. <(jlo-bin env)`, so stdout is a pipe exactly when a user is
/// sitting at a terminal watching stderr.
///
/// `Error:` is bold as well as red so it still stands out in a terminal theme
/// that is already red-heavy - on a failed run it is the one line the user is
/// looking for.
pub(crate) fn print_error(args: std::fmt::Arguments) {
    eprintln!("{} {args}", style("Error:").red().bold().for_stderr());
}

pub(crate) fn print_warning(args: std::fmt::Arguments) {
    eprintln!("{} {args}", style("Warning:").yellow().bold().for_stderr());
}

/// Advice printed *alongside* an error (a usage line, a suggested flag), so it
/// is dimmed rather than labelled - it must not read as a second failure.
pub(crate) fn print_hint(args: std::fmt::Arguments) {
    eprintln!("{}", style(args.to_string()).dim().for_stderr());
}

/// The one thing a pre-bundle macOS install is missing, and how to fix it.
///
/// Two lines rather than one because they are different kinds of statement:
/// the warning is what is wrong, the hint is what to type. `request` drives
/// the reinstall command because `jlo install` takes a version name - the
/// exact build may no longer be offered, which is also why this asks rather
/// than migrating anything by itself. The whole name, so a flat early-access
/// install is not told to reinstall the released stream.
pub(crate) fn legacy_layout(version: &str, request: crate::request::Request) {
    warning!(
        "{version} predates J'Lo's macOS bundle layout, so '/usr/libexec/java_home' cannot see it."
    );
    hint!("Reinstall it to fix that: 'jlo remove {version}' then 'jlo install {request}'.");
}

pub(crate) fn print_created(args: std::fmt::Arguments) {
    eprintln!("{} {args}", style("✓").green().for_stderr());
}

/// The installer's voice. These return styled strings rather than printing,
/// because the caller owns the blank lines between them - but the *choice* of
/// colour stays here, which is the whole point: a `style()` call outside this
/// module is how the vocabulary gets lost.
///
/// A heading names the block of commands under it. Dim, and never coloured:
/// weight carries structure, colour carries meaning, and a heading is
/// scaffolding for the lines below it - dimming it is what lets the commands
/// carry less colour and still win the eye.
pub(crate) fn heading(text: &str) -> String {
    style(text).dim().for_stderr().to_string()
}

/// A command offered for copying.
///
/// Bright green, and yes: green also marks the `✓` that says the run
/// succeeded. The two readings are told apart by shape rather than by hue -
/// the marker is a single character at the head of a line of prose, this is a
/// whole line of shell standing alone under a heading. Blue was tried first
/// and is the correct choice on paper; ANSI 34 is illegible on a dark
/// background and the bright slot was not much better in practice, which is
/// worth more than the tidier rule.
///
/// The colour is only half of it. The caller must print this at column 0 -
/// double-click and shift-select take a leading indent with them, so an
/// indented command is one a user cannot copy cleanly, which is exactly what
/// these blocks exist for. A command merely *named* in a sentence is not this;
/// it keeps that line's weight and stays out of blue.
pub(crate) fn command(line: &str) -> String {
    style(line).green().bright().for_stderr().to_string()
}

/// The label `selfupdate` builds its progress region with. Named, because
/// three places have to agree on it to keep the identity colour in one piece.
pub(crate) const JLO_LABEL: &str = "J'Lo";

/// The `J'Lo <version>` mark: the one place the program names itself rather
/// than a JDK. Magenta belongs to it and to nothing else, and never appears in
/// an `Error:`/`Warning:` line - a failure is what the reader needs first.
pub(crate) fn jlo_mark(version: &str) -> String {
    style(format!("J'Lo {version}"))
        .magenta()
        .for_stderr()
        .to_string()
}

/// The target half of `J'Lo 0.4.0 → 0.5.0`. One mark spans the arrow there,
/// so the second version is magenta without repeating the name.
pub(crate) fn jlo_mark_bare(version: &str) -> String {
    style(version).magenta().for_stderr().to_string()
}

/// The `→` that is punctuation rather than the active marker.
///
/// Dim is what tells the two apart, and position backs it up when colour is
/// off: the marker is a line's first character, this one always sits between
/// two operands.
pub(crate) fn punctuation_arrow() -> String {
    style("→").dim().for_stderr().to_string()
}

/// `<label> <version>` as the install lines say it: the magenta mark when the
/// subject is J'Lo itself, plain when it is a JDK. Name and version together -
/// half a mark in colour would read as an accident.
fn install_mark(label: &str, version: &str) -> String {
    if label == JLO_LABEL {
        jlo_mark(version)
    } else {
        format!("{label} {version}")
    }
}

/// The label inside a progress template, and the prefix beside it. Both are
/// styled so the mark stays whole across the two template slots it occupies.
fn progress_label(label: &str) -> String {
    if label == JLO_LABEL {
        style(label).magenta().for_stderr().to_string()
    } else {
        label.to_string()
    }
}

fn progress_prefix(label: &str, version: &str) -> String {
    if label == JLO_LABEL {
        style(version).magenta().for_stderr().to_string()
    } else {
        version.to_string()
    }
}

/// A closing note under a block of commands: dim, because it is secondary to
/// the commands it follows.
pub(crate) fn footnote(text: &str) -> String {
    style(text).dim().for_stderr().to_string()
}

macro_rules! error {
    ($($arg:tt)*) => { $crate::ui::print_error(format_args!($($arg)*)) };
}

macro_rules! warning {
    ($($arg:tt)*) => { $crate::ui::print_warning(format_args!($($arg)*)) };
}

macro_rules! hint {
    ($($arg:tt)*) => { $crate::ui::print_hint(format_args!($($arg)*)) };
}

macro_rules! created {
    ($($arg:tt)*) => { $crate::ui::print_created(format_args!($($arg)*)) };
}

pub(crate) use {created, error, hint, warning};

/// The line that replaces the green tick when some deletions went and others
/// failed: what did go, so the `!` lines above are read as the remainder
/// rather than as the whole run.
fn partial_line(removed: usize, failures: usize) -> String {
    format!(
        "  removed {removed} JDK{} before the failure{}",
        if removed == 1 { "" } else { "s" },
        if failures == 1 { "" } else { "s" }
    )
}

/// Report a `jlo remove --superseded` run.
///
/// Deletion is the one thing jlo does that cannot be undone, so unlike `jlo
/// env` it always says what it did - including when the answer is "nothing".
pub(crate) fn prune_report(report: &crate::store::PruneReport) {
    // Styling adds invisible escape bytes, so pad the plain number first and
    // style the padded string - the same rule the `jlo list` columns follow.
    // `Display for Request` writes straight to the formatter and so ignores a
    // width; render first, pad second.
    let width = report
        .removed
        .iter()
        .map(|(request, _)| request.to_string().len())
        .max()
        .unwrap_or(0);

    for (request, versions) in &report.removed {
        eprintln!(
            "{}  {} {}",
            style(format!("{:<width$}", request.to_string()))
                .dim()
                .for_stderr(),
            style("removed").dim().for_stderr(),
            versions.join(", ")
        );
    }

    for failure in &report.failures {
        eprintln!("{} {}", style("!").red().for_stderr(), failure);
    }

    // The green tick is reserved for a run that did what it was asked, so a
    // run whose deletions all failed gets neither it nor "Nothing to remove" -
    // that line would be a false claim about a store still holding every one
    // of them. The caller turns the same condition into a non-zero exit.
    let count = report.removed_count();
    if !report.failures.is_empty() {
        if count > 0 {
            eprintln!(
                "{}",
                style(partial_line(count, report.failures.len()))
                    .dim()
                    .for_stderr()
            );
        }
    } else if count == 0 {
        // The parenthetical is the *reason* nothing went, so it cannot be
        // printed when the reason was the skip below - the store is then
        // holding a superseded build, and saying otherwise would contradict
        // the warning two lines later.
        eprintln!(
            "{} Nothing to remove{}",
            style("✓").green().for_stderr(),
            if report.skipped_in_use.is_some() {
                String::new()
            } else {
                format!(
                    " {}",
                    style("(only the newest build of each name is installed)")
                        .dim()
                        .for_stderr()
                )
            }
        );
    } else {
        eprintln!(
            "{} Removed {} JDK{}",
            style("✓").green().for_stderr(),
            count,
            if count == 1 { "" } else { "s" }
        );
    }

    // Warned rather than noted, unlike `skipped_unmanaged`: this is the one
    // skip the user can act on, and the rule behind it is the same one
    // `jlo remove` applies to a target it was given by name.
    if let Some(version) = &report.skipped_in_use {
        warning!("left {version} alone: JAVA_HOME points at it");
        hint!("Switch the shell to another JDK first, e.g. 'jlo env 21', then run it again.");
    }

    if report.skipped_unmanaged > 0 {
        eprintln!(
            "{}",
            style(format!(
                "  left {} install{} alone (not managed by jlo)",
                report.skipped_unmanaged,
                if report.skipped_unmanaged == 1 {
                    ""
                } else {
                    "s"
                }
            ))
            .dim()
            .for_stderr()
        );
    }
}

/// Report a `jlo remove` run.
///
/// Shaped like [`prune_report`] - it destroys things, so it always says what
/// it did - but it never has a "nothing to do" line: `JdkStore::remove`
/// returns an error rather than an empty report, so reaching here means at
/// least one JDK was deleted or failed to delete. The trailing notes cover
/// the targets that did not contribute one.
pub(crate) fn remove_report(report: &crate::store::RemoveReport) {
    for version in &report.removed {
        eprintln!("{}  {}", style("removed").dim().for_stderr(), version);
    }

    for failure in &report.failures {
        eprintln!("{} {}", style("!").red().for_stderr(), failure);
    }

    // As in `prune_report`: the tick means the run did what it was asked, so
    // a failed deletion does not get one, and the caller exits non-zero on
    // the same condition.
    let count = report.removed.len();
    if report.failures.is_empty() {
        eprintln!(
            "{} Removed {} JDK{}",
            style("✓").green().for_stderr(),
            count,
            if count == 1 { "" } else { "s" }
        );
    } else if count > 0 {
        eprintln!(
            "{}",
            style(partial_line(count, report.failures.len()))
                .dim()
                .for_stderr()
        );
    }

    // Listed, not counted: the user named these, so anything they expected
    // to go and which did not is worth a line of its own.
    for version in &report.skipped_unmanaged {
        eprintln!(
            "{}",
            style(format!("  left {version} alone (not managed by jlo)"))
                .dim()
                .for_stderr()
        );
    }

    // Not a failure - the JDK is already absent, which is what was asked for
    // - so this is a dim note under a successful run rather than a warning.
    // It is still said, because it is usually a typo.
    if !report.not_installed.is_empty() {
        eprintln!(
            "{}",
            style(format!(
                "  nothing installed matched {}",
                crate::store::quoted_list(&report.not_installed)
            ))
            .dim()
            .for_stderr()
        );
    }

    // Last, and a warning rather than a dim note: of the three skips this is
    // the only one the user can act on, so it is the line the run should end
    // on - not something buried among the notes above it.
    if let Some(version) = &report.skipped_in_use {
        warning!("left {version} alone: JAVA_HOME points at it");
        hint!("Switch the shell to another JDK first, e.g. 'jlo env 21', then remove it.");
    }
}

/// The `jlo list --offline` listing: the JDKs already installed.
///
/// The same rows and the same status vocabulary as the networked listing,
/// minus the catalogue - so a build reads the same way whichever listing you
/// found it in. Colours switch themselves off when stdout is not a terminal,
/// so a pipe sees plain text.
pub(crate) fn offline_list(
    installed: &[InstalledJdk],
    active_version: Option<&str>,
    store: &JdkStore,
) {
    if installed.is_empty() {
        eprintln!("No JDKs installed in {}.", store.base().display());
        return;
    }

    print_listing(&build_rows(&[], installed, active_version));
}

/// The `jlo list` listing: what Adoptium offers for this machine, merged with
/// what is installed locally.
///
/// One row per major, led by the name `install`, `update` and `env` take.
/// Since every verb keeps one build per name, a row per *build* was mostly
/// the same name twice; the builds that do still sit beside the newest - a
/// leftover, an unmanaged install - get an indented line of their own, so
/// `jlo remove <build>` still finds its argument here.
pub(crate) fn remote_list(
    available: &[RemoteJdk],
    installed: &[InstalledJdk],
    active_version: Option<&str>,
) {
    if available.is_empty() {
        eprintln!("Adoptium offers no JDKs for this OS and architecture.");
        // Not a return: installs still present are still removable, and
        // hiding them here is the bug this listing exists to fix.
        if installed.is_empty() {
            return;
        }
    }

    print_listing(&build_rows(available, installed, active_version));
}

/// The header, the rows, then at most one line of advice.
fn print_listing(rows: &[Row]) {
    let listing = render_rows(rows);

    // The header and the tip go to stderr, so a pipe sees only the rows -
    // `jlo list | grep` should not have to step over a line of column names.
    eprintln!("{}", listing.header);
    print_lines(listing.lines);

    if let Some(tip) = tip_line(rows) {
        eprintln!("\n{tip}");
    }
}

/// What is active in this shell, and why.
///
/// Built by `main` and handed here formatted-but-undecided: `ui` never
/// consults the store, the config or the environment - it turns this into a
/// line. The shape is deliberately wider than any one caller needs, so the
/// machine-readable output still to come reports the same four facts under the
/// same names rather than inventing a second schema.
#[derive(Debug)]
pub(crate) struct Active {
    /// The directory `$JAVA_HOME` points at.
    pub path: PathBuf,
    /// The install's version, e.g. `25.0.4+101`. `None` when the JDK is not
    /// one of jlo's, which is the one case that reports a path instead.
    pub version: Option<String>,
    /// The version name of `version` - its major and its stream, so a GA
    /// build and a pre-release of one major are told apart here too.
    pub request: Option<crate::request::Request>,
    /// Where the active JDK came from. `None` when it is one of jlo's but
    /// nothing accounts for it - either nothing is pinned, or what is pinned
    /// is a different name, which `pinned_elsewhere` distinguishes.
    pub source: Option<crate::conf::Source>,
    /// A config that pins a *different* name than the one active. Set only
    /// when the two disagree; that disagreement is the whole reason this
    /// command answers "and why" rather than just "what".
    pub pinned_elsewhere: Option<crate::conf::Resolved>,
}

/// The one line that answers "which JDK, and why".
///
/// `jlo current`'s whole stdout line. A formatter rather than a `println!` at
/// the call site so it can be unit-tested against every state it
/// distinguishes: pure - no filesystem, no environment.
pub(crate) fn provenance_line(active: &Active) -> String {
    // A JDK jlo did not install has no version to name, so the path is the
    // answer: it says "not mine" completely, and the version is usually in it
    // anyway.
    let subject = match &active.version {
        Some(version) => version.clone(),
        None => active.path.display().to_string(),
    };

    let note = match &active.source {
        Some(crate::conf::Source::Foreign) => "$JAVA_HOME, set outside jlo".to_string(),
        Some(source) => format!("from {}", source.label()),
        // Active, and a config pins something else. The stdout line still
        // answers the question asked; the disagreement is the warning below.
        None if active.pinned_elsewhere.is_some() => "active".to_string(),
        None => "active, nothing pinned".to_string(),
    };

    format!("{subject}  ({note})")
}

/// The active JDK is not the one the config pins.
///
/// Unconditional - deliberately not gated on `is_terminal()` the way the
/// unsourced-`env` hint is. That gate exists because the autoload hook sources
/// the `env` path; nothing hooks this.
pub(crate) fn pin_mismatch(pinned: &crate::conf::Resolved) {
    warning!(
        "{} pins Java {}; run 'jlo env' to switch.",
        pinned.source.label(),
        pinned.request
    );
}

/// `$JAVA_HOME` is set, but to something jlo did not install.
///
/// Said rather than passed over: the listing has just drawn a column whose
/// whole job is to show which row you are on, and with no row marked the
/// honest reading is "jlo does not know", not "nothing is active".
pub(crate) fn foreign_java_home(path: &Path) {
    hint!("JAVA_HOME points outside jlo's store ({}).", path.display());
}

/// The advice line under both of the states in which `jlo current` has no
/// answer to print. Naming `jlo env` is the whole of it: that is the command
/// that puts a JDK back in this shell.
pub(crate) const NO_ACTIVE_JDK_HINT: &str = "Run 'jlo env' to activate a JDK in this shell.";

/// The line `jlo env` ends on when its exports went nowhere.
///
/// Keyed on stdout being a terminal, which is a reliable enough negative: the
/// `jlo` shell function captures the exports in a command substitution and
/// evals them (`out="$(jlo-bin env ...)"; eval "$out"` - deliberately not a
/// process substitution, which is a silent no-op under bash 3.2 and discards
/// the exit status everywhere), and the autoload hook calls that same
/// function, so on the sourced path stdout is a pipe and this never fires -
/// not even on the `cd` hook that runs on every directory change.
/// `jlo-bin env 21 > file` stays silent too - an accepted gap, since the case
/// that actually misleads is the interactive/agent one.
pub(crate) fn unsourced_env_hint(java_version: &str) -> String {
    format!(
        "jlo env prints exports for a shell to source; it did not change anything. \
         Use 'jlo exec {java_version} -- <command>' or \
         'export JAVA_HOME=\"$(jlo home {java_version})\"'."
    )
}

/// The line `jlo install` and `jlo update` end on when a superseded build is
/// still on disk after an install. Both delete what their own downloads
/// supersede, so this is only ever a leftover - a name this run did not move,
/// or a deletion that failed.
///
/// `None` when there is nothing to say: no install happened (nagging on every
/// no-op run trains the user to ignore the line), or nothing is superseded.
pub(crate) fn superseded_hint(installed_any: bool, superseded: usize) -> Option<String> {
    if !installed_any || superseded == 0 {
        return None;
    }

    let plural = if superseded == 1 { "" } else { "s" };
    Some(format!(
        "{superseded} superseded JDK{plural} still installed - run 'jlo remove --superseded' to remove {}.",
        if superseded == 1 { "it" } else { "them" }
    ))
}

/// Said after a pre-release install whose major has since shipped.
///
/// `-ea` is literal: it asks for the unreleased stream, and Adoptium keeps one
/// running after a major goes GA - so the pin keeps delivering previews of the
/// *next patch*. That is deliberately not changed under the user's feet; it is
/// announced instead.
///
/// Also said on every bare `jlo update` while such a stream is installed,
/// since it moves pre-release names too - so it names the way to stop that as
/// well as the way to switch.
pub(crate) fn ea_is_now_released(request: crate::request::Request) -> String {
    format!(
        "{request} still tracks pre-release builds; Java {major} has since been released. \
         Pin '{major}' to follow the released builds instead, or 'jlo remove {request}' \
         to stop updating the pre-release.",
        major = request.major
    )
}

/// The pre-release names among `requests` whose major has since shipped,
/// deduplicated and in the order given.
///
/// Split out from the printing so the rule is testable without a server: it is
/// the *selection* that is easy to get wrong, not the sentence.
fn released_ea_names(
    requests: &[crate::request::Request],
    released: &[i64],
) -> Vec<crate::request::Request> {
    let mut names: Vec<crate::request::Request> = Vec::new();
    for request in requests.iter().filter(|r| r.is_ea()) {
        if released.contains(&request.major) && !names.contains(request) {
            names.push(*request);
        }
    }
    names
}

/// Say, once per name, that a pre-release pin is still a pre-release pin.
///
/// Pure: the caller supplies the released majors from whatever it already
/// holds, so neither emit site owes this an extra request.
pub(crate) fn announce_released_ea(requests: &[crate::request::Request], released: &[i64]) {
    for request in released_ea_names(requests, released) {
        hint!("{}", ea_is_now_released(request));
    }
}

/// `println!` panics when the reader goes away, and this output is meant to be
/// piped (`jlo list | head`), so treat a closed pipe as a normal end of output.
pub(crate) fn print_lines(lines: impl IntoIterator<Item = String>) {
    use std::io::Write;

    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    for line in lines {
        match writeln!(out, "{line}") {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => return,
            Err(e) => {
                error!("could not write to stdout: {e}");
                exit(1);
            }
        }
    }
}

/// What an indented line of `jlo list` says about a build beside the one its
/// name reports.
///
/// Exactly one token per line, ordered by how much it constrains what the
/// user can do with the install: a build that is both unmanaged and older
/// reports `unmanaged` - the fact that decides whether `jlo remove
/// --superseded` will touch it at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Status {
    /// As new as the build its name reports: two spellings of one version
    /// (`v21.0.11+9` beside `21.0.11+9`). Neither will be deleted, so neither
    /// is called superseded.
    Installed,
    /// Managed, and older than the newest managed build of its name.
    Superseded,
    /// Installed without a `.jlo-managed` marker: jlo will not delete it.
    Unmanaged,
}

/// An installed build that is not the one its name reports, on a line of its
/// own so its exact version is there to pass to `jlo remove`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Build {
    version: String,
    status: Status,
    /// `$JAVA_HOME` points at this install.
    active: bool,
}

/// One name - `27` or `28-ea` - and what jlo knows about it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct NameRow {
    name: Request,
    /// The newest managed build of the name: the one `jlo env <name>` picks
    /// and the one `jlo update` measures against.
    installed: Option<String>,
    /// The build Adoptium offers, only when it is not already here: nothing
    /// of the name is installed, or it is newer than everything that is.
    latest: Option<String>,
    /// `latest` is newer than an install of this name, so `jlo update` would
    /// download it.
    update: bool,
    lts: bool,
    /// `$JAVA_HOME` points at `installed`.
    active: bool,
    /// Every other install of the name, newest first.
    others: Vec<Build>,
}

/// One major of the listing.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Row {
    head: NameRow,
    /// The pre-release name of the major, when its released name heads the
    /// row - typically a `27-ea` left over from before 27 shipped.
    pre_release: Option<NameRow>,
}

impl Row {
    fn names(&self) -> impl Iterator<Item = &NameRow> {
        std::iter::once(&self.head).chain(self.pre_release.as_ref())
    }
}

/// Merge the remote catalogue and the local installs into one row per major,
/// newest first.
fn build_rows(
    available: &[RemoteJdk],
    installed: &[InstalledJdk],
    active_version: Option<&str>,
) -> Vec<Row> {
    let mut majors: Vec<i64> = available
        .iter()
        .map(|jdk| jdk.major)
        .chain(installed.iter().map(|jdk| jdk.major))
        .collect();
    majors.sort_unstable_by(|a, b| b.cmp(a));
    majors.dedup();

    majors
        .into_iter()
        .filter_map(|major| {
            let row = |stream| {
                name_row(
                    Request { major, stream },
                    available,
                    installed,
                    active_version,
                )
            };
            // The released name leads whenever the major has one: once a
            // major ships, `27` is the name anyone reaches for, and a `27-ea`
            // beside it is the exception worth an indented line.
            match (row(Stream::Ga), row(Stream::Ea)) {
                (Some(head), pre_release) => Some(Row { head, pre_release }),
                (None, head) => head.map(|head| Row {
                    head,
                    pre_release: None,
                }),
            }
        })
        .collect()
}

/// The line for one name, or `None` when Adoptium offers nothing of it and
/// nothing of it is installed.
///
/// Only builds of this name are compared, never the other stream of the same
/// major: a pre-release sorts above the release it previews, so across
/// streams an offered beta would read as an update to an installed release,
/// and following it would change streams - which `jlo update` never does.
fn name_row(
    name: Request,
    available: &[RemoteJdk],
    installed: &[InstalledJdk],
    active_version: Option<&str>,
) -> Option<NameRow> {
    let offered = available.iter().find(|jdk| jdk.request() == name);
    let mut builds: Vec<&InstalledJdk> = installed
        .iter()
        .filter(|jdk| jdk.request() == name)
        .collect();
    if offered.is_none() && builds.is_empty() {
        return None;
    }
    // Stable, so two spellings of one version keep the store's order.
    builds
        .sort_by(|a, b| crate::version::compare(&b.version, &a.version).unwrap_or(Ordering::Equal));

    let latest = offered
        .filter(|jdk| supersedes_every_install(jdk, &builds))
        .map(|jdk| jdk.version.clone());
    let update = latest.is_some() && !builds.is_empty();

    // Only a managed build can be the one the name reports, because only
    // managed builds are what `jlo remove --superseded` sorts: it filters on
    // the marker before it picks the newest of a name. An unmanaged 21.0.3
    // beside a managed 21.0.1 would otherwise make the managed one read as
    // superseded, recommending a command that removes nothing.
    let head = builds
        .iter()
        .position(|jdk| jdk.managed)
        .map(|index| builds.remove(index));
    let is_active = |version: &str| active_version == Some(version);

    let others = builds
        .iter()
        .map(|jdk| Build {
            version: jdk.version.clone(),
            status: other_status(jdk, head),
            active: is_active(&jdk.version),
        })
        .collect();

    Some(NameRow {
        name,
        installed: head.map(|jdk| jdk.version.clone()),
        latest,
        update,
        lts: offered.is_some_and(|jdk| jdk.lts),
        active: head.is_some_and(|jdk| is_active(&jdk.version)),
        others,
    })
}

/// Whether an offered build is newer than every install of its name - true
/// when nothing of the name is installed.
///
/// The catalogue can sit *behind* the store - an install that came from
/// somewhere else, or a major Adoptium has since rolled back - and showing it
/// there would be offering a downgrade. Unmanaged installs count too: `jlo
/// update` finds the exact build already on disk and does nothing, so
/// offering it would name an action that does not happen.
fn supersedes_every_install(offered: &RemoteJdk, builds: &[&InstalledJdk]) -> bool {
    builds.iter().all(|jdk| {
        crate::version::compare(&offered.version, &jdk.version)
            .is_ok_and(|ord| ord == Ordering::Greater)
    })
}

/// The status of an install that is not the newest managed build of its name.
///
/// `Unmanaged` comes first because it decides whether jlo will act on the
/// install at all: both of `remove`'s selectors leave a marker-less directory
/// alone, so `superseded` there would name an action that cannot happen.
fn other_status(jdk: &InstalledJdk, head: Option<&InstalledJdk>) -> Status {
    if !jdk.managed {
        return Status::Unmanaged;
    }
    let older = head.is_some_and(|head| {
        crate::version::compare(&head.version, &jdk.version)
            .is_ok_and(|ord| ord == Ordering::Greater)
    });
    if older {
        Status::Superseded
    } else {
        Status::Installed
    }
}

/// A listing rendered: the column header, and one line per name or build.
struct Listing {
    header: String,
    lines: Vec<String>,
}

/// One printed line before padding: the cells of a name, or of a build under
/// it.
struct Cells<'a> {
    active: bool,
    name: String,
    installed: &'a str,
    latest: &'a str,
    lts: bool,
    status: String,
}

fn cells(rows: &[Row]) -> Vec<Cells<'_>> {
    let mut out = Vec::new();
    for row in rows {
        push_name(&mut out, &row.head, "");
        if let Some(pre_release) = &row.pre_release {
            push_name(&mut out, pre_release, "  ");
        }
    }
    out
}

/// A name's line, then its other builds. A build's name cell is left empty:
/// the build sits in the INSTALLED column, where the exact version `jlo
/// remove` takes is read from, and the blank is what marks it as belonging to
/// the name above.
fn push_name<'a>(out: &mut Vec<Cells<'a>>, row: &'a NameRow, indent: &str) {
    out.push(Cells {
        active: row.active,
        name: format!("{indent}{}", row.name),
        installed: row.installed.as_deref().unwrap_or(""),
        latest: row.latest.as_deref().unwrap_or(""),
        lts: row.lts,
        status: if row.update {
            style("update").yellow().to_string()
        } else {
            String::new()
        },
    });
    for build in &row.others {
        out.push(Cells {
            active: build.active,
            name: String::new(),
            installed: &build.version,
            latest: "",
            lts: false,
            status: render_status(build.status),
        });
    }
}

const NAME_HEADER: &str = "NAME";
const INSTALLED_HEADER: &str = "INSTALLED";
const LATEST_HEADER: &str = "LATEST";

/// Render the rows as aligned columns: active gutter, name, installed build,
/// latest build, LTS tag, status.
///
/// The gutter is emitted on every line whether or not anything is active, so
/// the columns sit in the same place from one run to the next - a listing that
/// shifted sideways the moment `$JAVA_HOME` was set would be worse than one
/// that never marked anything.
fn render_rows(rows: &[Row]) -> Listing {
    let cells = cells(rows);
    let width = |header: &str, cell: fn(&Cells) -> usize| {
        cells
            .iter()
            .map(cell)
            .chain([header.len()])
            .max()
            .unwrap_or(0)
    };
    let name_width = width(NAME_HEADER, |c| c.name.len());
    let installed_width = width(INSTALLED_HEADER, |c| c.installed.len());
    let latest_width = width(LATEST_HEADER, |c| c.latest.len());
    // `jlo list --offline` has no catalogue, so it has neither a latest build
    // nor an LTS tag to show, and either column would be blank on every line.
    let any_latest = cells.iter().any(|c| !c.latest.is_empty());
    let any_lts = cells.iter().any(|c| c.lts);

    // Every styled field is padded as a plain string first: the escape bytes
    // `console::style` adds are invisible but still counted by the formatter,
    // so styling before padding shifts the columns.
    let line = |gutter: &str, name: &str, installed: &str, latest: &str, rest: &str| {
        let latest = if any_latest {
            format!("{latest:<latest_width$}  ")
        } else {
            String::new()
        };
        format!(" {gutter}  {name}  {installed:<installed_width$}  {latest}{rest}")
            .trim_end()
            .to_string()
    };

    let header = heading(&line(
        " ",
        &format!("{NAME_HEADER:<name_width$}"),
        INSTALLED_HEADER,
        LATEST_HEADER,
        "",
    ));

    let lines = cells
        .iter()
        .map(|c| {
            let gutter = if c.active {
                style("\u{2192}").cyan().bold().to_string()
            } else {
                " ".to_string()
            };
            let lts = match (any_lts, c.lts) {
                (false, _) => String::new(),
                (true, true) => format!("{}  ", style("LTS").bold()),
                (true, false) => "     ".to_string(),
            };
            line(
                &gutter,
                &style(format!("{:<name_width$}", c.name)).dim().to_string(),
                c.installed,
                c.latest,
                &format!("{lts}{}", c.status),
            )
        })
        .collect();

    Listing { header, lines }
}

/// The one word an indented build line ends on. Each is a single token - no
/// spaces, no parentheses - so `jlo list | grep superseded` stays a usable
/// way to ask which installs `jlo remove --superseded` would take.
fn render_status(status: Status) -> String {
    match status {
        Status::Installed => String::new(),
        Status::Superseded => style("superseded").dim().to_string(),
        Status::Unmanaged => style("unmanaged").dim().to_string(),
    }
}

/// The single advice line under a listing, or `None` when there is nothing to
/// advise.
///
/// One line whatever applies: this prints on every `jlo list`, and a stack of
/// suggestions under every listing reads as nagging rather than as help.
fn tip_line(rows: &[Row]) -> Option<String> {
    let names = || rows.iter().flat_map(Row::names);
    let outdated = names().filter(|name| name.update).count();
    // A superseded build of a name with an update on offer is not advertised:
    // `jlo update` deletes it along with the build it replaces, so offering
    // `jlo remove --superseded` as well would be two commands for one job.
    let superseded = names()
        .filter(|name| !name.update)
        .flat_map(|name| &name.others)
        .filter(|build| build.status == Status::Superseded)
        .count();

    let mut offers = Vec::new();
    if outdated > 0 {
        offers.push(format!(
            "{} ({outdated} outdated)",
            style("`jlo update`").bold().for_stderr()
        ));
    }
    if superseded > 0 {
        offers.push(format!(
            "{} ({superseded} superseded)",
            style("`jlo remove --superseded`").bold().for_stderr()
        ));
    }
    if offers.is_empty() {
        return None;
    }

    Some(format!(
        "{} {}",
        style("TIP:").bold().for_stderr(),
        offers.join(" \u{b7} ")
    ))
}

/// Whether stderr can carry a bar that redraws over itself.
///
/// Being a terminal is not enough. Under `TERM=dumb` - Emacs `shell-mode`, some
/// CI runners, several IDE consoles - there is no cursor addressing, and
/// indicatif silently suppresses the bar. Trusting `is_terminal` alone there
/// would leave a 20-second install printing nothing whatsoever, so treat a dumb
/// or unset `TERM` as "no live region" and fall back to plain phase lines.
fn supports_live_region() -> bool {
    if !stderr().is_terminal() {
        return false;
    }
    match std::env::var("TERM") {
        Ok(term) => !term.is_empty() && term != "dumb",
        Err(_) => false,
    }
}

const TICK: Duration = Duration::from_millis(100);

/// Sized to fit an 80-column terminal even for Adoptium's longest version
/// strings (`21.0.12+101.0.LTS`). A line that wraps turns the live region into
/// two rows, and the redraw then clears and repaints both - the stacked,
/// flickering output this module exists to avoid.
///
/// No phase word here: the bar and the byte counts already say "downloading".
/// The transfer rate is dropped for the same reason - the spinner answers "is
/// it stuck?", and the ETA answers the question the rate was standing in for.
/// The live region carries no colour that means anything elsewhere. It is
/// erased when the install succeeds, and a colour that disappears teaches the
/// reader that it never meant much - a cyan spinner beside the cyan "this one
/// is active" gutter costs that gutter its meaning. Dim is what is left: the
/// bar is secondary to the line it leaves behind.
///
/// `label` is interpolated rather than hard-coded. Both templates said `JDK`
/// whatever they were downloading, including the one `selfupdate` builds by
/// calling `InstallUi::labelled("J'Lo", ..)` - the label existed and the live
/// region ignored it.
fn download_style(label: &str) -> ProgressStyle {
    ProgressStyle::default_bar()
        .template(&format!(
            "{{spinner:.dim}} {label} {{prefix}}  [{{bar:20.dim}}]  {{bytes}}/{{total_bytes}}  {{eta}}"
        ))
        .expect("progress bar template is a valid literal")
        .progress_chars("#>-")
}

fn spinner_style(label: &str) -> ProgressStyle {
    ProgressStyle::default_spinner()
        .template(&format!("{{spinner:.dim}} {label} {{prefix}}  {{msg}}"))
        .expect("spinner template is a valid literal")
}

/// One line per durable side effect, e.g.
/// `✓ JDK 21.0.8+9 → ~/Library/Java/JavaVirtualMachines/21.0.8  (18s)`.
///
/// Every styled piece is `.for_stderr()`. `console::style` decides whether to
/// emit colour by looking at *stdout*, and the `jlo` shell function runs
/// `. <(jlo-bin env)` - so stdout is a pipe on the one path that matters and
/// the default targeting silently strips every colour from this line.
fn format_summary(label: &str, version: &str, dest: &str, elapsed: Duration) -> String {
    format!(
        "{} {} {} {}  {}",
        style("✓").green().for_stderr(),
        install_mark(label, version),
        punctuation_arrow(),
        dest,
        style(format!("({})", format_elapsed(elapsed)))
            .dim()
            .for_stderr()
    )
}

/// Compact enough to sit at the end of a line without drawing the eye.
fn format_elapsed(elapsed: Duration) -> String {
    let secs = elapsed.as_secs();
    if secs >= 60 {
        format!("{}m{:02}s", secs / 60, secs % 60)
    } else {
        format!("{secs}s")
    }
}

/// Collapse the home directory so install paths stay readable. Unlike the
/// Debug-quoted paths in error messages, this line is only ever printed on
/// success, where there is no odd whitespace to expose.
fn tilde(path: &Path) -> String {
    let Some(home) = std::env::home_dir() else {
        return path.display().to_string();
    };
    match path.strip_prefix(&home) {
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => path.display().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- ea_is_now_released --

    /// One notice per EA name whose major has shipped, and none for the rest.
    /// `21-ea` on a released 21 is the case the notice exists for; `28-ea` on
    /// an unreleased 28 is the ordinary case and must stay silent.
    #[test]
    fn only_a_released_major_earns_the_notice() {
        use crate::request::{Request, Stream};

        let ea = |major| Request {
            major,
            stream: Stream::Ea,
        };
        assert_eq!(released_ea_names(&[ea(21), ea(28)], &[21]), vec![ea(21)]);
        assert_eq!(
            released_ea_names(
                &[Request {
                    major: 21,
                    stream: Stream::Ga
                }],
                &[21]
            ),
            vec![],
            "a GA name is not a pin on the unreleased stream"
        );
    }

    /// The same name twice - two installs of one pin, or a request set that
    /// repeats it - is still one thing to say.
    #[test]
    fn a_repeated_name_is_announced_once() {
        use crate::request::{Request, Stream};

        let ea = Request {
            major: 21,
            stream: Stream::Ea,
        };
        assert_eq!(released_ea_names(&[ea, ea], &[21]), vec![ea]);
    }

    /// -ea means "unreleased", not "newest", so the pin keeps delivering
    /// betas after the major ships. That is the intended behaviour and
    /// therefore a note, not a warning - but it must name the way out.
    #[test]
    fn the_notice_names_the_major_and_the_plain_name() {
        use crate::request::{Request, Stream};

        let notice = ea_is_now_released(Request {
            major: 28,
            stream: Stream::Ea,
        });
        assert!(
            notice.contains("28-ea"),
            "names what was asked for: {notice}"
        );
        assert!(
            notice.contains("Pin '28'"),
            "names the plain major: {notice}"
        );
    }

    // -- replaced_line --

    /// A pre-release stream publishes weekly and a bare `jlo update` moves
    /// it, so its replacement line has to read as a preview swapped for the
    /// next, not as a patch release.
    #[test]
    fn a_replaced_pre_release_says_so() {
        use crate::request::{Request, Stream};

        let removed = vec!["28.0.0-beta+14.0.ea".to_string()];
        assert_eq!(
            replaced_line(
                Request {
                    major: 28,
                    stream: Stream::Ea
                },
                &removed
            ),
            "  replaced pre-release 28.0.0-beta+14.0.ea"
        );
        assert_eq!(
            replaced_line(
                Request {
                    major: 21,
                    stream: Stream::Ga
                },
                &["21.0.8+9".to_string()]
            ),
            "  replaced 21.0.8+9"
        );
    }

    // -- unsourced_env_hint --

    /// The hint exists to hand the caller a command that does work without a
    /// sourcing shell, so it has to name both alternatives and carry the
    /// version the user actually asked for.
    #[test]
    fn unsourced_env_hint_names_both_alternatives() {
        let hint = unsourced_env_hint("21");
        assert!(hint.contains("did not change anything"), "{hint}");
        assert!(hint.contains("jlo exec 21 -- <command>"), "{hint}");
        assert!(
            hint.contains("export JAVA_HOME=\"$(jlo home 21)\""),
            "{hint}"
        );
    }

    // -- superseded_hint --

    #[test]
    fn superseded_hint_names_the_command_and_the_count() {
        let hint = superseded_hint(true, 2).expect("an install plus leftovers earns a hint");
        assert!(hint.contains('2'), "hint should say how many: {hint}");
        assert!(
            hint.contains("jlo remove --superseded"),
            "hint should name the command: {hint}"
        );
    }

    #[test]
    fn superseded_hint_singular_for_one() {
        let hint = superseded_hint(true, 1).unwrap();
        assert!(hint.contains("1 superseded JDK "), "{hint}");
    }

    /// Nothing was superseded, so pointing at the command would send the user
    /// to a command that removes nothing.
    #[test]
    fn superseded_hint_silent_when_nothing_is_superseded() {
        assert!(superseded_hint(true, 0).is_none());
    }

    /// Every JDK was already current: the leftovers are pre-existing clutter,
    /// not something this run caused, and `jlo update` would nag on every run.
    #[test]
    fn superseded_hint_silent_when_nothing_was_installed() {
        assert!(superseded_hint(false, 3).is_none());
    }

    // -- provenance_line --
    //
    // The six states `jlo current` distinguishes. Pure formatting: no store,
    // no config, no environment. Cases 1 and 6 never reach a formatter - they
    // have no answer to print - so they are covered by the integration
    // suite's exit codes instead.

    fn request(name: &str) -> crate::request::Request {
        crate::request::Request::parse(name).expect("the fixture names a valid version")
    }

    fn active(version: &str, major: i64) -> Active {
        Active {
            path: PathBuf::from("/jdks").join(version),
            version: Some(version.to_string()),
            request: Some(request(&major.to_string())),
            source: None,
            pinned_elsewhere: None,
        }
    }

    fn pinned(version: &str, file: &str) -> crate::conf::Resolved {
        crate::conf::Resolved {
            request: request(version),
            source: crate::conf::Source::ProjectConfig(PathBuf::from(file)),
        }
    }

    /// Case 2: active, and the config that pins it is named.
    #[test]
    fn provenance_line_names_the_config_the_active_jdk_came_from() {
        let mut a = active("25.0.4+101", 25);
        a.source = Some(pinned("25", "./.jlorc").source);
        assert_eq!(provenance_line(&a), "25.0.4+101  (from ./.jlorc)");
    }

    /// Case 3: the config pins a different major. The stdout line still
    /// answers "what is active" - the disagreement is `pin_mismatch`'s job,
    /// on stderr - so it must not turn into an apology here.
    #[test]
    fn provenance_line_stays_an_answer_when_the_config_disagrees() {
        let mut a = active("25.0.4+101", 25);
        a.pinned_elsewhere = Some(pinned("21", "./.jlorc"));
        assert_eq!(provenance_line(&a), "25.0.4+101  (active)");
    }

    /// Case 4: nothing pins anything, which is a different statement from
    /// case 3 and has to read as one.
    #[test]
    fn provenance_line_says_so_when_nothing_is_pinned() {
        assert_eq!(
            provenance_line(&active("25.0.4+101", 25)),
            "25.0.4+101  (active, nothing pinned)"
        );
    }

    /// Case 5: a JDK jlo does not manage. The path is the answer - it says
    /// "not mine" completely, and naming a version would claim knowledge jlo
    /// does not have.
    #[test]
    fn provenance_line_prints_the_path_for_a_foreign_java_home() {
        let a = Active {
            path: PathBuf::from("/opt/jdk-21"),
            version: None,
            request: None,
            source: Some(crate::conf::Source::Foreign),
            pinned_elsewhere: None,
        };
        assert_eq!(
            provenance_line(&a),
            "/opt/jdk-21  ($JAVA_HOME, set outside jlo)"
        );
    }

    /// A version given on the command line is a provenance too.
    #[test]
    fn provenance_line_names_the_command_line_as_a_source() {
        let mut a = active("21.0.5+11", 21);
        a.source = Some(crate::conf::Source::Argument);
        assert_eq!(provenance_line(&a), "21.0.5+11  (from the command line)");
    }

    #[test]
    fn elapsed_under_a_minute_is_bare_seconds() {
        assert_eq!(format_elapsed(Duration::from_secs(18)), "18s");
        assert_eq!(format_elapsed(Duration::from_millis(1_900)), "1s");
        assert_eq!(format_elapsed(Duration::from_secs(59)), "59s");
    }

    #[test]
    fn elapsed_past_a_minute_pads_the_seconds() {
        assert_eq!(format_elapsed(Duration::from_mins(1)), "1m00s");
        assert_eq!(format_elapsed(Duration::from_secs(95)), "1m35s");
        assert_eq!(format_elapsed(Duration::from_hours(1)), "60m00s");
    }

    #[test]
    fn tilde_collapses_the_home_directory() {
        let Some(home) = std::env::home_dir() else {
            return;
        };
        let path = home.join("Library/Java/JavaVirtualMachines/21.0.8");
        assert_eq!(tilde(&path), "~/Library/Java/JavaVirtualMachines/21.0.8");
    }

    #[test]
    fn tilde_leaves_paths_outside_home_alone() {
        let path = Path::new("/opt/jdks/21.0.8");
        assert_eq!(tilde(path), "/opt/jdks/21.0.8");
    }

    #[test]
    fn summary_names_the_exact_version_and_destination() {
        // The major version is what the user typed; the exact version is the
        // one fact the summary exists to record.
        let line = format_summary("JDK", "21.0.8+9", "~/.jdks/21.0.8", Duration::from_secs(18));
        assert!(line.contains("JDK 21.0.8+9"), "got: {line}");
        assert!(line.contains("~/.jdks/21.0.8"), "got: {line}");
        assert!(line.contains("(18s)"), "got: {line}");
        assert_eq!(line.lines().count(), 1, "got: {line}");
    }

    fn remote(version: &str, major: i64) -> RemoteJdk {
        RemoteJdk {
            version: version.to_string(),
            major,
            stream: Stream::Ga,
            lts: false,
        }
    }

    fn remote_ea(version: &str, major: i64) -> RemoteJdk {
        RemoteJdk {
            version: version.to_string(),
            major,
            stream: Stream::Ea,
            lts: false,
        }
    }

    fn remote_lts(version: &str, major: i64) -> RemoteJdk {
        RemoteJdk {
            version: version.to_string(),
            major,
            stream: Stream::Ga,
            lts: true,
        }
    }

    fn local(version: &str, major: i64) -> InstalledJdk {
        InstalledJdk {
            version: version.to_string(),
            major,
            stream: Stream::Ga,
            managed: true,
        }
    }

    fn local_ea(version: &str, major: i64) -> InstalledJdk {
        InstalledJdk {
            version: version.to_string(),
            major,
            stream: Stream::Ea,
            managed: true,
        }
    }

    fn unmanaged(version: &str, major: i64) -> InstalledJdk {
        InstalledJdk {
            version: version.to_string(),
            major,
            stream: Stream::Ga,
            managed: false,
        }
    }

    /// A name with nothing known about it yet; the builders below fill it in.
    fn name(text: &str) -> NameRow {
        NameRow {
            name: request(text),
            installed: None,
            latest: None,
            update: false,
            lts: false,
            active: false,
            others: Vec::new(),
        }
    }

    impl NameRow {
        fn installed(mut self, version: &str) -> Self {
            self.installed = Some(version.to_string());
            self
        }

        fn latest(mut self, version: &str) -> Self {
            self.latest = Some(version.to_string());
            self
        }

        fn update(mut self) -> Self {
            self.update = true;
            self
        }

        fn lts(mut self) -> Self {
            self.lts = true;
            self
        }

        fn active(mut self) -> Self {
            self.active = true;
            self
        }

        fn other(mut self, version: &str, status: Status) -> Self {
            self.others.push(Build {
                version: version.to_string(),
                status,
                active: false,
            });
            self
        }
    }

    fn row(head: NameRow) -> Row {
        Row {
            head,
            pre_release: None,
        }
    }

    fn row_with(head: NameRow, pre_release: NameRow) -> Row {
        Row {
            head,
            pre_release: Some(pre_release),
        }
    }

    // -- build_rows --

    #[test]
    fn a_name_not_installed_shows_only_the_latest_build() {
        let rows = build_rows(&[remote("26.0.2+101", 26)], &[], None);
        assert_eq!(rows, vec![row(name("26").latest("26.0.2+101"))]);
    }

    /// Current means nothing to add: LATEST would only repeat INSTALLED.
    #[test]
    fn a_current_install_shows_no_latest_build() {
        let rows = build_rows(
            &[remote("21.0.12+101.0.LTS", 21)],
            &[local("21.0.12+101.0.LTS", 21)],
            None,
        );
        assert_eq!(rows, vec![row(name("21").installed("21.0.12+101.0.LTS"))]);
    }

    /// The installed build and the offered one are one row now, not two: both
    /// answer to `21`, and one build per name is what every verb keeps.
    #[test]
    fn an_outdated_install_shares_its_row_with_the_update() {
        let rows = build_rows(
            &[remote("21.0.12+101.0.LTS", 21)],
            &[local("21.0.11+10.0.LTS", 21)],
            None,
        );
        assert_eq!(
            rows,
            vec![row(name("21")
                .installed("21.0.11+10.0.LTS")
                .latest("21.0.12+101.0.LTS")
                .update())]
        );
    }

    /// The weekly case: an older build of the stream is installed, and the
    /// row is named the way `jlo update` takes it.
    #[test]
    fn an_outdated_pre_release_stream_is_one_row_named_as_typed() {
        let rows = build_rows(
            &[remote_ea("28.0.0-beta+16.0.ea", 28)],
            &[local_ea("28.0.0-beta+14.0.ea", 28)],
            None,
        );
        assert_eq!(
            rows,
            vec![row(name("28-ea")
                .installed("28.0.0-beta+14.0.ea")
                .latest("28.0.0-beta+16.0.ea")
                .update())]
        );
    }

    #[test]
    fn an_offered_pre_release_that_is_installed_is_one_row() {
        let rows = build_rows(
            &[remote_ea("28.0.0-beta+16.0.ea", 28)],
            &[local_ea("28.0.0-beta+16.0.ea", 28)],
            None,
        );
        assert_eq!(
            rows,
            vec![row(name("28-ea").installed("28.0.0-beta+16.0.ea"))]
        );
    }

    /// A `27-ea` left over from before 27 shipped: one row for the major, led
    /// by the name anyone reaches for now, the pre-release under it.
    #[test]
    fn a_pre_release_left_beside_its_release_is_a_line_under_it() {
        let rows = build_rows(
            &[remote("27.0.0+35", 27)],
            &[local_ea("27.0.0-beta+30.0.ea", 27), local("27.0.0+35", 27)],
            None,
        );
        assert_eq!(
            rows,
            vec![row_with(
                name("27").installed("27.0.0+35"),
                name("27-ea").installed("27.0.0-beta+30.0.ea"),
            )]
        );
    }

    /// The exceptions keep their exact builds, which is what `jlo remove
    /// <build>` takes. Unmanaged wins over superseded: `jlo remove
    /// --superseded` will not touch it whatever else is true.
    #[test]
    fn superseded_and_unmanaged_builds_are_lines_under_their_name() {
        let rows = build_rows(
            &[remote("21.0.12+101.0.LTS", 21)],
            &[
                local("21.0.11+10.0.LTS", 21),
                local("21.0.9+10.0.LTS", 21),
                unmanaged("21.0.8+9.0.LTS", 21),
            ],
            None,
        );
        assert_eq!(
            rows,
            vec![row(name("21")
                .installed("21.0.11+10.0.LTS")
                .latest("21.0.12+101.0.LTS")
                .update()
                .other("21.0.9+10.0.LTS", Status::Superseded)
                .other("21.0.8+9.0.LTS", Status::Unmanaged))]
        );
    }

    /// Two builds of one patch differ only in the build number, and the
    /// higher one wins. The deletion rule reads the same ordering, so the line
    /// that says `superseded` has to be the build `remove --superseded` deletes.
    #[test]
    fn the_lower_build_of_one_patch_is_superseded() {
        let rows = build_rows(
            &[remote("21.0.11+10.0.LTS", 21)],
            &[local("21.0.11+9.0.LTS", 21), local("21.0.11+10.0.LTS", 21)],
            None,
        );
        assert_eq!(
            rows,
            vec![row(name("21")
                .installed("21.0.11+10.0.LTS")
                .other("21.0.11+9.0.LTS", Status::Superseded))]
        );
    }

    /// Two names for one version are genuinely equal, so neither can be
    /// `superseded`: neither will be deleted, and a line promising the
    /// deletion would name an action that never happens.
    #[test]
    fn two_names_for_one_version_are_both_listed_and_neither_superseded() {
        let rows = build_rows(
            &[],
            &[local("v21.0.11+9", 21), local("21.0.11+9", 21)],
            None,
        );
        assert_eq!(
            rows,
            vec![row(name("21")
                .installed("v21.0.11+9")
                .other("21.0.11+9", Status::Installed))]
        );
    }

    /// An unmanaged *newer* build must not make a managed one read as
    /// superseded. `jlo remove --superseded` filters on the marker before it
    /// picks the newest of a name, so the managed build is still the one it
    /// keeps - and it is the one INSTALLED reports.
    #[test]
    fn an_unmanaged_newer_build_does_not_displace_the_managed_one() {
        let rows = build_rows(
            &[],
            &[unmanaged("21.0.3+9", 21), local("21.0.1+12", 21)],
            None,
        );
        assert_eq!(
            rows,
            vec![row(name("21")
                .installed("21.0.1+12")
                .other("21.0.3+9", Status::Unmanaged))]
        );
    }

    /// `jlo update` counts an unmanaged install as the name being present:
    /// an older one is moved past, an exact one reports "up to date". The row
    /// has to say the same in both cases.
    #[test]
    fn an_unmanaged_install_is_measured_against_like_any_other() {
        let rows = build_rows(
            // Out of order on purpose: the rows are sorted, not inherited.
            &[remote("17.0.20+101", 17), remote("21.0.12+7", 21)],
            &[unmanaged("21.0.11+9", 21), unmanaged("17.0.20+101", 17)],
            None,
        );
        assert_eq!(
            rows,
            vec![
                row(name("21")
                    .latest("21.0.12+7")
                    .update()
                    .other("21.0.11+9", Status::Unmanaged)),
                row(name("17").other("17.0.20+101", Status::Unmanaged)),
            ]
        );
    }

    #[test]
    fn the_catalogue_behind_the_store_is_not_offered_as_a_downgrade() {
        let rows = build_rows(
            &[remote("21.0.11+10.0.LTS", 21)],
            &[local("21.0.12+101.0.LTS", 21)],
            None,
        );
        assert_eq!(rows, vec![row(name("21").installed("21.0.12+101.0.LTS"))]);
    }

    /// The gutter follows the build, wherever it is printed: on the name's
    /// line, or on the indented line of a build under it.
    #[test]
    fn the_active_mark_lands_on_the_line_of_the_build_java_home_points_at() {
        let installed = [
            local("21.0.11+10.0.LTS", 21),
            local("21.0.9+10.0.LTS", 21),
            local("17.0.20+101", 17),
        ];

        let rows = build_rows(&[], &installed, Some("17.0.20+101"));
        assert!(rows[1].head.active, "{rows:?}");
        assert!(!rows[0].head.active, "{rows:?}");

        let rows = build_rows(&[], &installed, Some("21.0.9+10.0.LTS"));
        assert!(!rows[0].head.active, "{rows:?}");
        assert!(rows[0].head.others[0].active, "{rows:?}");
    }

    /// LTS is a property of the major, so a row whose installed build is not
    /// the offered one keeps the tag.
    #[test]
    fn the_lts_tag_is_the_majors_whichever_build_is_installed() {
        let rows = build_rows(
            &[remote_lts("21.0.12+101.0.LTS", 21)],
            &[local("21.0.11+10.0.LTS", 21)],
            None,
        );
        assert!(rows[0].head.lts, "{rows:?}");
    }

    /// The two streams of one major are two names, so neither supersedes the
    /// other. Without the per-name rule the beta - which sorts above the
    /// release it previews - would make the GA build read as superseded and
    /// send the reader to 'jlo remove --superseded', which would not touch it.
    #[test]
    fn neither_stream_supersedes_the_other() {
        let rows = build_rows(
            &[],
            &[local("26.0.1+9", 26), local_ea("26.0.2-beta+101.0.ea", 26)],
            None,
        );
        assert_eq!(
            rows,
            vec![row_with(
                name("26").installed("26.0.1+9"),
                name("26-ea").installed("26.0.2-beta+101.0.ea"),
            )]
        );
    }

    /// An offered release must not be called an update to a pre-release
    /// install: following it would change streams. The release still leads
    /// the row - it is the name the major is now known by.
    #[test]
    fn a_ga_release_is_not_an_update_to_an_installed_pre_release() {
        let rows = build_rows(
            &[remote("28.0.1+9", 28)],
            &[local_ea("28.0.0-beta+16.0.ea", 28)],
            None,
        );
        assert_eq!(
            rows,
            vec![row_with(
                name("28").latest("28.0.1+9"),
                name("28-ea").installed("28.0.0-beta+16.0.ea"),
            )]
        );
    }

    /// The mirror image: a pre-release sorts above the release it previews,
    /// so without the per-name rule the offered beta would read as an update
    /// to the installed release.
    #[test]
    fn an_offered_pre_release_is_not_an_update_to_an_installed_release() {
        let rows = build_rows(
            &[remote_ea("26.0.2-beta+101.0.ea", 26)],
            &[local("26.0.1+9", 26)],
            None,
        );
        assert_eq!(
            rows,
            vec![row_with(
                name("26").installed("26.0.1+9"),
                name("26-ea").latest("26.0.2-beta+101.0.ea"),
            )]
        );
    }

    /// `--offline` has no catalogue: the same rows, with nothing to put under
    /// LATEST and nothing to call an update.
    #[test]
    fn offline_rows_are_the_same_rows_without_a_latest_build() {
        let rows = build_rows(
            &[],
            &[
                local_ea("28.0.0-beta+14.0.ea", 28),
                local("21.0.11+10.0.LTS", 21),
                local("21.0.9+10.0.LTS", 21),
            ],
            None,
        );
        assert_eq!(
            rows,
            vec![
                row(name("28-ea").installed("28.0.0-beta+14.0.ea")),
                row(name("21")
                    .installed("21.0.11+10.0.LTS")
                    .other("21.0.9+10.0.LTS", Status::Superseded)),
            ]
        );
    }

    // -- render_rows --

    #[test]
    fn render_rows_aligns_the_columns_and_marks_the_active_build() {
        let mut outdated = name("21")
            .installed("21.0.11+10.0.LTS")
            .latest("21.0.12+101.0.LTS")
            .update()
            .lts()
            .other("21.0.9+10.0.LTS", Status::Superseded)
            .other("21.0.8+9.0.LTS", Status::Unmanaged);
        outdated.others[0].active = true;

        let listing = render_rows(&[
            row(name("28-ea")
                .installed("28.0.0-beta+14.0.ea")
                .latest("28.0.0-beta+16.0.ea")
                .update()),
            row_with(
                name("27").installed("27.0.0+35").active(),
                name("27-ea").installed("27.0.0-beta+30.0.ea"),
            ),
            row(name("26").latest("26.0.2+101")),
            row(outdated),
        ]);

        assert_eq!(listing.header, "    NAME     INSTALLED            LATEST");
        assert_eq!(
            listing.lines,
            vec![
                "    28-ea    28.0.0-beta+14.0.ea  28.0.0-beta+16.0.ea       update",
                " \u{2192}  27       27.0.0+35",
                "      27-ea  27.0.0-beta+30.0.ea",
                "    26                            26.0.2+101",
                "    21       21.0.11+10.0.LTS     21.0.12+101.0.LTS    LTS  update",
                " \u{2192}           21.0.9+10.0.LTS                                superseded",
                "             21.0.8+9.0.LTS                                 unmanaged",
            ]
        );
    }

    #[test]
    fn render_rows_drops_the_latest_and_lts_columns_when_nothing_fills_them() {
        // `jlo list --offline` knows neither, so each column would be blank
        // on every line.
        let listing = render_rows(&[
            row(name("26").installed("26.0.2+101")),
            row(name("21")
                .installed("21.0.11+10.0.LTS")
                .other("21.0.9+10.0.LTS", Status::Superseded)),
        ]);
        assert_eq!(listing.header, "    NAME  INSTALLED");
        assert_eq!(
            listing.lines,
            vec![
                "    26    26.0.2+101",
                "    21    21.0.11+10.0.LTS",
                "          21.0.9+10.0.LTS   superseded",
            ]
        );
    }

    // -- tip_line --

    #[test]
    fn tip_line_is_silent_when_everything_is_current() {
        let rows = vec![row(name("21").installed("21.0.12+101.0.LTS"))];
        assert_eq!(tip_line(&rows), None);
    }

    #[test]
    fn tip_line_joins_both_offers_on_one_line() {
        // One line whatever applies: a listing that ends in a stack of
        // advice reads as nagging, and this one prints on every `jlo list`.
        let rows = vec![
            row(name("21")
                .installed("21.0.11+10.0.LTS")
                .latest("21.0.12+101.0.LTS")
                .update()),
            row(name("17")
                .installed("17.0.19+7")
                .latest("17.0.20+101")
                .update()),
            row(name("11")
                .installed("11.0.25+9")
                .other("11.0.24+8", Status::Superseded)),
        ];
        assert_eq!(
            tip_line(&rows).as_deref(),
            Some("TIP: `jlo update` (2 outdated) \u{b7} `jlo remove --superseded` (1 superseded)")
        );
    }

    /// `jlo update` deletes the builds its install supersedes, so a
    /// superseded build of a name with an update on offer is already covered
    /// by the first offer - advertising the second would be two commands for
    /// one job. An update of the other stream of that major deletes nothing of
    /// this one, so the leftover under `28-ea` keeps its offer and the one
    /// under `29-ea` does not.
    #[test]
    fn tip_line_leaves_to_update_what_update_removes() {
        let rows = vec![
            row(name("29-ea")
                .installed("29.0.0-beta+1.0.ea")
                .latest("29.0.0-beta+3.0.ea")
                .update()
                .other("29.0.0-beta+0.0.ea", Status::Superseded)),
            row_with(
                name("28").latest("28.0.1+3").update(),
                name("28-ea")
                    .installed("28.0.0-beta+16.0.ea")
                    .other("28.0.0-beta+14.0.ea", Status::Superseded),
            ),
            row(name("21")
                .installed("21.0.9+10.0.LTS")
                .latest("21.0.12+101.0.LTS")
                .update()
                .other("21.0.8+9.0.LTS", Status::Superseded)),
        ];
        assert_eq!(
            tip_line(&rows).as_deref(),
            Some("TIP: `jlo update` (3 outdated) \u{b7} `jlo remove --superseded` (1 superseded)")
        );
    }

    #[test]
    fn tip_line_offers_only_what_applies() {
        let rows = vec![row(name("21")
            .installed("21.0.11+10.0.LTS")
            .other("21.0.9+10.0.LTS", Status::Superseded))];
        assert_eq!(
            tip_line(&rows).as_deref(),
            Some("TIP: `jlo remove --superseded` (1 superseded)")
        );
    }

    #[test]
    fn tip_line_does_not_offer_to_remove_an_unmanaged_install() {
        // `jlo remove --superseded` leaves it alone, so counting it would
        // promise a removal that will not happen.
        let rows = vec![row(name("21")
            .installed("21.0.11+10.0.LTS")
            .other("21.0.9+10.0.LTS", Status::Unmanaged))];
        assert_eq!(tip_line(&rows), None);
    }
}
