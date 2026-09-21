use crate::adoptium::RemoteJdk;
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
/// durable the side effect is. A JDK on disk survives until `jlo prune`, so it
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

/// `jlo update` exists to answer "is anything newer available?", so the answer
/// is its output - unlike `jlo env`, where silence is the answer.
pub(crate) fn up_to_date(major: &str, version: &str) {
    eprintln!(
        "{} JDK {} is up to date {}",
        style("✓").green().for_stderr(),
        major,
        style(format!("({version})")).dim().for_stderr()
    );
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

/// Report a `jlo prune` run.
///
/// `prune` is the one command that destroys things, so unlike `jlo env` it
/// always says what it did - including when the answer is "nothing".
pub(crate) fn prune_report(report: &crate::store::PruneReport) {
    // Styling adds invisible escape bytes, so pad the plain number first and
    // style the padded string - the same rule the `jlo list` columns follow.
    let width = report
        .removed
        .iter()
        .map(|(major, _)| major.to_string().len())
        .max()
        .unwrap_or(0);

    for (major, versions) in &report.removed {
        eprintln!(
            "{}  {} {}",
            style(format!("{major:<width$}")).dim().for_stderr(),
            style("removed").dim().for_stderr(),
            versions.join(", ")
        );
    }

    for failure in &report.failures {
        eprintln!("{} {}", style("!").red().for_stderr(), failure);
    }

    let count = report.removed_count();
    if count == 0 {
        eprintln!(
            "{} Nothing to prune {}",
            style("✓").green().for_stderr(),
            style("(only the newest of each major is installed)")
                .dim()
                .for_stderr()
        );
    } else {
        eprintln!(
            "{} Removed {} JDK{}",
            style("✓").green().for_stderr(),
            count,
            if count == 1 { "" } else { "s" }
        );
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

    let count = report.removed.len();
    eprintln!(
        "{} Removed {} JDK{}",
        style("✓").green().for_stderr(),
        count,
        if count == 1 { "" } else { "s" }
    );

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
        hint!("  Switch the shell to another JDK first, e.g. 'jlo env 21', then remove it.");
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

/// Width of the leading major-version column. Styling adds invisible escape
/// bytes, so the width has to come from the plain numbers.
fn major_column_width(majors: impl IntoIterator<Item = i64>) -> usize {
    majors
        .into_iter()
        .map(|major| major.to_string().len())
        .max()
        .unwrap_or(0)
}

/// The `jlo list` listing: what Adoptium offers for this machine, merged with
/// what is installed locally.
///
/// One row per version, not per major: an install that is not the newest of
/// its major used to collapse into a parenthetical on the row above, which
/// left `jlo remove 17.0.11+10` with nowhere to read its argument from.
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

/// The rows, then at most one line of advice.
fn print_listing(rows: &[Row]) {
    print_lines(render_rows(rows));

    // stderr, so neither the tip nor the blank line above it lands in a pipe
    // alongside the listing.
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
    /// The major version of `version`.
    pub major: Option<i64>,
    /// Where the active JDK came from. `None` when it is one of jlo's but
    /// nothing accounts for it - either nothing is pinned, or what is pinned
    /// is a different major, which `pinned_elsewhere` distinguishes.
    pub source: Option<crate::conf::Source>,
    /// A config that pins a *different* major than the one active. Set only
    /// when the two disagree; that disagreement is the whole reason this
    /// command answers "and why" rather than just "what".
    pub pinned_elsewhere: Option<crate::conf::Resolved>,
    /// The caller found the environment already correct and changed nothing.
    /// `jlo env --verbose` sets it - saying so is the difference between
    /// "already right" and "did nothing", which were indistinguishable
    /// before. A command that only ever reports leaves it false: there it
    /// would be true of every run and so say nothing.
    pub unchanged: bool,
}

/// The one line that answers "which JDK, and why".
///
/// One formatter, every caller: `jlo current` and `jlo env --verbose` are two
/// askings of the same question, and two spellings of the answer would drift.
/// Pure - no filesystem, no environment.
pub(crate) fn provenance_line(active: &Active) -> String {
    // A JDK jlo did not install has no version to name, so the path is the
    // answer: it says "not mine" completely, and the version is usually in it
    // anyway.
    let subject = match &active.version {
        Some(version) => version.clone(),
        None => active.path.display().to_string(),
    };

    let mut note = match &active.source {
        Some(crate::conf::Source::Foreign) => "$JAVA_HOME, set outside jlo".to_string(),
        Some(source) => format!("from {}", source.label()),
        // Active, and a config pins something else. The stdout line still
        // answers the question asked; the disagreement is the warning below.
        None if active.pinned_elsewhere.is_some() => "active".to_string(),
        None => "active, nothing pinned".to_string(),
    };

    if active.unchanged {
        note.push_str(", already active");
    }

    format!("{subject}  ({note})")
}

/// `jlo env --verbose`: one line saying which JDK is now active and where the
/// version came from.
///
/// stderr, because stdout on this path is the environment channel - the `jlo`
/// shell function sources it, and a status line arriving there would be
/// executed rather than read.
pub(crate) fn env_report(active: &Active) {
    eprintln!("{}", provenance_line(active));
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
        pinned.version
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

/// What one row of `jlo list` says about a single JDK version.
///
/// Exactly one token per row: the statuses are ordered by how much they
/// constrain what the user can do with the install, so a build that is both
/// unmanaged and superseded reports `unmanaged` - the fact that decides
/// whether `jlo prune` will touch it at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Status {
    /// Adoptium offers it and it is not installed - either nothing of this
    /// major is, or what is installed is newer than this build.
    Available,
    /// Adoptium offers it, it is not installed, and it is newer than every
    /// build of this major that is.
    Update,
    /// Installed, and the newest build of its major that is installed.
    Installed,
    /// Installed, but a newer build of the same major is installed too.
    Superseded,
    /// Installed without a `.jlo-managed` marker: jlo will not delete it.
    Unmanaged,
}

/// One line of the listing: a version, plus what jlo knows about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Row {
    pub(crate) major: i64,
    pub(crate) version: String,
    pub(crate) lts: bool,
    pub(crate) status: Status,
    /// `$JAVA_HOME` points at this install.
    pub(crate) active: bool,
}

/// Merge the remote catalogue and the local installs into one row per version.
fn build_rows(
    available: &[RemoteJdk],
    installed: &[InstalledJdk],
    active_version: Option<&str>,
) -> Vec<Row> {
    let mut rows: Vec<Row> = available
        .iter()
        .map(|jdk| Row {
            major: jdk.major,
            version: jdk.version.clone(),
            lts: jdk.lts,
            status: match installed.iter().find(|i| i.version == jdk.version) {
                Some(local) => local_status(local, installed),
                None if supersedes_every_install(jdk, installed) => Status::Update,
                None => Status::Available,
            },
            active: false,
        })
        .collect();

    // Installs Adoptium does not offer under that exact version still get a
    // row: they are removable, and a listing that hides them is the reason
    // `jlo remove <build>` had nowhere to read its argument from.
    rows.extend(
        installed
            .iter()
            .filter(|i| !available.iter().any(|a| a.version == i.version))
            .map(|i| Row {
                major: i.major,
                version: i.version.clone(),
                // LTS is a property of the major, so an older build keeps the
                // tag even once Adoptium stops offering that exact version.
                lts: available.iter().any(|a| a.major == i.major && a.lts),
                status: local_status(i, installed),
                active: false,
            }),
    );

    // A version, not a path: the caller has already resolved `$JAVA_HOME`
    // against the store, so a row only has to match the name.
    if let Some(active) = active_version {
        for row in &mut rows {
            row.active = row.version == active && row.status != Status::Available;
        }
    }

    rows.sort_by(|a, b| {
        b.major.cmp(&a.major).then_with(|| {
            semver_rs::compare(&b.version, &a.version, None).unwrap_or(Ordering::Equal)
        })
    });
    rows
}

/// Whether an offered build is newer than every install of its major.
///
/// The catalogue can sit *behind* the store - an install that came from
/// somewhere else, or a major Adoptium has since rolled back - and `update`
/// there would be offering a downgrade.
fn supersedes_every_install(jdk: &RemoteJdk, installed: &[InstalledJdk]) -> bool {
    let mut majors = installed.iter().filter(|i| i.major == jdk.major).peekable();
    if majors.peek().is_none() {
        return false;
    }
    majors.all(|i| {
        semver_rs::compare(&jdk.version, &i.version, None).is_ok_and(|ord| ord == Ordering::Greater)
    })
}

/// The status of a row backed by an install.
///
/// `Superseded` is a property of the *build* - a newer build of the same major
/// is installed alongside it - which is what `jlo prune` acts on. Being older
/// than something Adoptium offers is a different fact, and it lands on the
/// remote row as `Update`, where `jlo update <major>` is the command that
/// answers it.
///
/// `Unmanaged` comes first because it decides whether jlo will act on the
/// install at all: `prune` and `remove` both leave a marker-less directory
/// alone, so `superseded` there would name an action that cannot happen.
fn local_status(jdk: &InstalledJdk, installed: &[InstalledJdk]) -> Status {
    if !jdk.managed {
        return Status::Unmanaged;
    }
    let superseded = installed.iter().any(|other| {
        other.major == jdk.major
            && semver_rs::compare(&other.version, &jdk.version, None)
                .is_ok_and(|ord| ord == Ordering::Greater)
    });
    if superseded {
        Status::Superseded
    } else {
        Status::Installed
    }
}

/// Render the rows as aligned columns: active gutter, major, version, LTS tag,
/// status.
///
/// The gutter is emitted on every line whether or not anything is active, so
/// the columns sit in the same place from one run to the next - a listing that
/// shifted sideways the moment `$JAVA_HOME` was set would be worse than one
/// that never marked anything.
fn render_rows(rows: &[Row]) -> Vec<String> {
    let major_width = major_column_width(rows.iter().map(|row| row.major));
    let version_width = rows.iter().map(|row| row.version.len()).max().unwrap_or(0);
    // `jlo list --offline` has no catalogue to read LTS out of, so the column
    // would be three blank characters on every line of it.
    let any_lts = rows.iter().any(|row| row.lts);

    rows.iter()
        .map(|row| {
            // Every styled field is padded as a plain string first: the escape
            // bytes `console::style` adds are invisible but still counted by
            // the formatter, so styling before padding shifts the columns.
            let gutter = if row.active {
                style("\u{2192}").cyan().bold().to_string()
            } else {
                " ".to_string()
            };
            let lts = match (any_lts, row.lts) {
                (false, _) => String::new(),
                (true, true) => format!("{}  ", style("LTS").bold()),
                (true, false) => "     ".to_string(),
            };
            format!(
                " {gutter}  {:>major_width$}  {:<version_width$}  {lts}{}",
                style(row.major).dim(),
                row.version,
                render_status(row.status),
            )
            .trim_end()
            .to_string()
        })
        .collect()
}

/// The one word a row ends on. Each is a single token - no spaces, no
/// parentheses - so `jlo list | grep superseded` stays a usable way to ask
/// which installs `jlo prune` would take.
fn render_status(status: Status) -> String {
    match status {
        Status::Available => String::new(),
        Status::Update => style("update").yellow().to_string(),
        Status::Installed => style("installed").green().to_string(),
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
    let outdated = rows.iter().filter(|r| r.status == Status::Update).count();
    let superseded = rows
        .iter()
        .filter(|r| r.status == Status::Superseded)
        .count();

    let mut offers = Vec::new();
    if outdated > 0 {
        offers.push(format!(
            "{} ({outdated} outdated)",
            style("`jlo update --all`").bold().for_stderr()
        ));
    }
    if superseded > 0 {
        offers.push(format!(
            "{} ({superseded} superseded)",
            style("`jlo prune`").bold().for_stderr()
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

    // -- provenance_line --
    //
    // The six states `jlo current` distinguishes, plus the annotation
    // `jlo env --verbose` adds. Pure formatting: no store, no config, no
    // environment. Cases 1 and 6 never reach a formatter - they have no
    // answer to print - so they are covered by the integration suite's exit
    // codes instead.

    fn active(version: &str, major: i64) -> Active {
        Active {
            path: PathBuf::from("/jdks").join(version),
            version: Some(version.to_string()),
            major: Some(major),
            source: None,
            pinned_elsewhere: None,
            unchanged: false,
        }
    }

    fn pinned(version: &str, file: &str) -> crate::conf::Resolved {
        crate::conf::Resolved {
            version: version.to_string(),
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
            major: None,
            source: Some(crate::conf::Source::Foreign),
            pinned_elsewhere: None,
            unchanged: false,
        };
        assert_eq!(
            provenance_line(&a),
            "/opt/jdk-21  ($JAVA_HOME, set outside jlo)"
        );
    }

    /// `jlo env --verbose` on a run that changed nothing. Without this the
    /// line is identical to the one a real switch prints, which is the
    /// complaint the flag exists to answer: "already correct" and "did
    /// nothing" were indistinguishable.
    #[test]
    fn provenance_line_marks_a_run_that_changed_nothing() {
        let mut a = active("25.0.4+101", 25);
        a.source = Some(pinned("25", "./.jlorc").source);
        a.unchanged = true;
        assert_eq!(
            provenance_line(&a),
            "25.0.4+101  (from ./.jlorc, already active)"
        );
    }

    /// A version given on the command line is a provenance too, and the one
    /// `jlo env 21 --verbose` reports.
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
            lts: false,
        }
    }

    fn local(version: &str, major: i64) -> InstalledJdk {
        InstalledJdk {
            version: version.to_string(),
            major,
            managed: true,
        }
    }

    fn row(major: i64, version: &str, status: Status) -> Row {
        Row {
            major,
            version: version.to_string(),
            lts: false,
            status,
            active: false,
        }
    }

    #[test]
    fn build_rows_merges_a_version_that_is_both_offered_and_installed() {
        let rows = build_rows(
            &[remote("21.0.12+101.0.LTS", 21)],
            &[local("21.0.12+101.0.LTS", 21)],
            None,
        );
        assert_eq!(rows, vec![row(21, "21.0.12+101.0.LTS", Status::Installed)]);
    }

    #[test]
    fn build_rows_marks_an_offered_build_update_when_an_older_one_is_installed() {
        let rows = build_rows(
            &[remote("21.0.12+101.0.LTS", 21)],
            &[local("21.0.11+10.0.LTS", 21)],
            None,
        );
        let offered = rows
            .iter()
            .find(|r| r.version == "21.0.12+101.0.LTS")
            .expect("the offered build has a row");
        assert_eq!(offered.status, Status::Update);
    }

    #[test]
    fn build_rows_gives_an_installed_build_its_own_row_under_the_offered_one() {
        let rows = build_rows(
            &[remote("21.0.12+101.0.LTS", 21)],
            &[local("21.0.11+10.0.LTS", 21)],
            None,
        );
        assert_eq!(
            rows,
            vec![
                row(21, "21.0.12+101.0.LTS", Status::Update),
                row(21, "21.0.11+10.0.LTS", Status::Installed),
            ]
        );
    }

    #[test]
    fn build_rows_marks_every_installed_build_but_the_newest_superseded() {
        let rows = build_rows(
            &[remote("21.0.12+101.0.LTS", 21)],
            &[local("21.0.11+10.0.LTS", 21), local("21.0.9+10.0.LTS", 21)],
            None,
        );
        assert_eq!(
            rows,
            vec![
                row(21, "21.0.12+101.0.LTS", Status::Update),
                row(21, "21.0.11+10.0.LTS", Status::Installed),
                row(21, "21.0.9+10.0.LTS", Status::Superseded),
            ]
        );
    }

    fn unmanaged(version: &str, major: i64) -> InstalledJdk {
        InstalledJdk {
            version: version.to_string(),
            major,
            managed: false,
        }
    }

    #[test]
    fn build_rows_reports_unmanaged_ahead_of_superseded() {
        // `jlo prune` will not touch it whatever else is true of it, so
        // `superseded` would name an action that cannot happen.
        let rows = build_rows(
            &[],
            &[local("17.0.20+101", 17), unmanaged("17.0.11+10", 17)],
            None,
        );
        assert_eq!(
            rows,
            vec![
                row(17, "17.0.20+101", Status::Installed),
                row(17, "17.0.11+10", Status::Unmanaged),
            ]
        );
    }

    #[test]
    fn build_rows_flags_the_build_java_home_points_at() {
        let rows = build_rows(
            &[remote("21.0.12+101.0.LTS", 21)],
            &[local("21.0.11+10.0.LTS", 21)],
            Some("21.0.11+10.0.LTS"),
        );
        let active: Vec<&str> = rows
            .iter()
            .filter(|r| r.active)
            .map(|r| r.version.as_str())
            .collect();
        assert_eq!(active, vec!["21.0.11+10.0.LTS"]);
    }

    fn remote_lts(version: &str, major: i64) -> RemoteJdk {
        RemoteJdk {
            version: version.to_string(),
            major,
            lts: true,
        }
    }

    #[test]
    fn build_rows_carries_the_lts_tag_down_to_older_builds_of_the_major() {
        // LTS is a property of the major, not of one build, so a row that
        // Adoptium no longer offers must not silently lose the tag.
        let rows = build_rows(
            &[remote_lts("21.0.12+101.0.LTS", 21)],
            &[local("21.0.11+10.0.LTS", 21)],
            None,
        );
        let older = rows
            .iter()
            .find(|r| r.version == "21.0.11+10.0.LTS")
            .expect("the installed build has a row");
        assert!(older.lts, "got: {older:?}");
    }

    #[test]
    fn render_rows_aligns_the_columns_and_marks_the_active_build() {
        let rows = vec![
            Row {
                major: 21,
                version: "21.0.12+101.0.LTS".into(),
                lts: true,
                status: Status::Update,
                active: false,
            },
            Row {
                major: 21,
                version: "21.0.11+10.0.LTS".into(),
                lts: true,
                status: Status::Installed,
                active: true,
            },
            Row {
                major: 8,
                version: "8.0.412+8".into(),
                lts: false,
                status: Status::Available,
                active: false,
            },
        ];
        assert_eq!(
            render_rows(&rows),
            vec![
                "    21  21.0.12+101.0.LTS  LTS  update",
                " \u{2192}  21  21.0.11+10.0.LTS   LTS  installed",
                "     8  8.0.412+8",
            ]
        );
    }

    #[test]
    fn tip_line_is_silent_when_everything_is_current() {
        let rows = vec![row(21, "21.0.12+101.0.LTS", Status::Installed)];
        assert_eq!(tip_line(&rows), None);
    }

    #[test]
    fn tip_line_joins_both_offers_on_one_line() {
        // One line whatever applies: a listing that ends in a stack of
        // advice reads as nagging, and this one prints on every `jlo list`.
        let rows = vec![
            row(21, "21.0.12+101.0.LTS", Status::Update),
            row(21, "21.0.9+10.0.LTS", Status::Superseded),
            row(17, "17.0.20+101", Status::Update),
            row(17, "17.0.11+10", Status::Superseded),
        ];
        assert_eq!(
            tip_line(&rows).as_deref(),
            Some("TIP: `jlo update --all` (2 outdated) \u{b7} `jlo prune` (2 superseded)")
        );
    }

    #[test]
    fn tip_line_offers_only_what_applies() {
        let rows = vec![row(21, "21.0.9+10.0.LTS", Status::Superseded)];
        assert_eq!(
            tip_line(&rows).as_deref(),
            Some("TIP: `jlo prune` (1 superseded)")
        );
    }

    #[test]
    fn tip_line_does_not_offer_to_prune_an_unmanaged_install() {
        // `jlo prune` leaves it alone, so counting it would promise a
        // removal that will not happen.
        let rows = vec![row(21, "21.0.9+10.0.LTS", Status::Unmanaged)];
        assert_eq!(tip_line(&rows), None);
    }

    #[test]
    fn build_rows_does_not_offer_an_update_to_an_older_build_than_is_installed() {
        // Adoptium's catalogue can sit behind an install that came from
        // somewhere else. `update` there would offer a downgrade.
        let rows = build_rows(
            &[remote("21.0.11+10.0.LTS", 21)],
            &[local("21.0.12+101.0.LTS", 21)],
            None,
        );
        let offered = rows
            .iter()
            .find(|r| r.version == "21.0.11+10.0.LTS")
            .expect("the offered build has a row");
        assert_eq!(offered.status, Status::Available);
    }

    #[test]
    fn render_rows_drops_the_lts_column_when_nothing_carries_the_tag() {
        // `jlo list --offline` never knows which majors are LTS, so the
        // column would be three blank characters on every line.
        let rows = vec![
            row(26, "26.0.2+101", Status::Installed),
            row(21, "21.0.9+10.0.LTS", Status::Superseded),
        ];
        assert_eq!(
            render_rows(&rows),
            vec![
                "    26  26.0.2+101       installed",
                "    21  21.0.9+10.0.LTS  superseded",
            ]
        );
    }
}
