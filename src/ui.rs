use crate::adoptium::{RemoteJdk, platform};
use crate::conf::{Resolved, Source};
use crate::request::Request;
use crate::resolve::Active;
use crate::store::{
    InstallRun, NameGroup, NameResult, RemoveError, RemoveReport, Status, quoted_list,
    supersedes_every_install,
};
use clap::builder::styling::Styles;
use console::style;
use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressState, ProgressStyle};
use std::io::{IsTerminal, stderr};
use std::path::Path;
use std::process::exit;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

/// One download of an install run, as the progress region names it.
#[derive(Debug, Clone)]
pub(crate) struct Download {
    pub(crate) version: String,
    /// The API's size in bytes; 0 when it gave none.
    pub(crate) size: u64,
}

/// The two columns every line of a store diff shares. Sized before the first
/// line prints: a `+` line goes out while later downloads still run, and the
/// `-` lines after them have to line up with it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Columns {
    name: usize,
    version: usize,
}

impl Columns {
    pub(crate) fn fit<'a>(
        names: impl IntoIterator<Item = Request>,
        versions: impl IntoIterator<Item = &'a str>,
    ) -> Self {
        Self {
            name: names
                .into_iter()
                .map(|name| name.to_string().len())
                .max()
                .unwrap_or(0),
            version: versions.into_iter().map(str::len).max().unwrap_or(0),
        }
    }
}

/// One download on its way into the store: what the unpacking line and the
/// `+` line name later, carried with the staged package rather than read
/// back from the region, since the next download has begun by then.
#[derive(Debug, Clone)]
pub(crate) struct Job {
    request: Request,
    version: String,
    started: Instant,
}

/// The live progress region of one install run, on a terminal:
///
/// ```text
/// + 17  17.0.16+8   19s
/// ⠸     21.0.8+9    extracting
/// ⠋ 3/3 25.0.1+8    ━━━━━─────────────   30%  39/130 MiB
/// ```
///
/// The bottom line is the download in progress, its bar this download's
/// bytes; between downloads it stays, frozen at 100%. Above it, only while a
/// package is being unpacked, a line naming it and the phase. Builds that
/// land and names that fail leave their lines above the region, which is
/// erased when the run ends. Without a terminal: one `Downloading` line per
/// download, naming the run's total when there are several.
///
/// Called from both stages of the run at once, so its state sits behind one
/// lock; the lines are indicatif's, which draws them without tearing. At most
/// one download and one unpacking are in progress at a time.
///
/// Output persists in proportion to how durable the side effect is: a JDK on
/// disk earns a line; exporting `JAVA_HOME` prints nothing, which matters
/// because the autoload hook runs on every `cd`.
#[derive(Debug)]
pub(crate) struct InstallUi {
    region: MultiProgress,
    /// The download line, created hidden and shown from the first download.
    bar: ProgressBar,
    tty: bool,
    columns: Columns,
    state: Mutex<RegionState>,
}

#[derive(Debug)]
struct RegionState {
    downloads: Vec<Download>,
    /// Index into `downloads` of the download in progress, if one is.
    current: Option<usize>,
    /// How many downloads have begun: the counter's number, which a skipped
    /// or an unplanned download must not reorder.
    begun: usize,
    /// What the whole run fetches, for the plain `Downloading` line:
    /// planned sizes, corrected by each response, less what will not come.
    total: u64,
    /// The size of the download in progress: planned, then the response's.
    length: u64,
    /// Whether the download line is in the region yet.
    shown: bool,
    /// The blank that stands in for the counter on the unpacking line, so
    /// its version lines up with the download's.
    counter_width: usize,
    /// The widest version of the run, which the prefixes pad to.
    version_width: usize,
    /// The download line's counter and version, drawn through its style:
    /// see [`InstallUi::begin`].
    label: String,
    /// The unpacking line, while a package is being unpacked.
    unpacking: Option<ProgressBar>,
}

impl InstallUi {
    /// `downloads` is what the run expects to fetch; a name it did not plan
    /// can still [`Self::begin`].
    pub(crate) fn run(downloads: Vec<Download>, columns: Columns) -> Self {
        let terminal = supports_live_region().then(ProgressDrawTarget::stderr);
        Self::drawing_to(downloads, columns, terminal)
    }

    #[cfg(test)]
    pub(crate) fn hidden() -> Self {
        Self::drawing_to(Vec::new(), Columns::default(), None)
    }

    /// `terminal` is where the live region goes; without one, plain lines
    /// carry the progress, and the hidden bars still keep the counts.
    fn drawing_to(
        downloads: Vec<Download>,
        columns: Columns,
        terminal: Option<ProgressDrawTarget>,
    ) -> Self {
        let total = downloads.iter().map(|download| download.size).sum();
        let tty = terminal.is_some();
        let target = terminal.unwrap_or_else(ProgressDrawTarget::hidden);
        // Styled via the builder, which does not draw: a bar configured
        // after construction flashes indicatif's default style first.
        let bar = ProgressBar::hidden().with_style(download_style(MAX_BAR_WIDTH, String::new()));
        Self {
            region: MultiProgress::with_draw_target(target),
            bar,
            tty,
            columns,
            state: Mutex::new(RegionState {
                downloads,
                current: None,
                begun: 0,
                total,
                length: 0,
                shown: false,
                counter_width: 0,
                version_width: 0,
                label: String::new(),
                unpacking: None,
            }),
        }
    }

    /// A panic elsewhere while the lock was held leaves nothing worse than a
    /// miscounted bar, which is no reason to panic again.
    fn state(&self) -> MutexGuard<'_, RegionState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(crate) fn begin(&self, request: Request, version: &str, size: u64) -> Job {
        let mut state = self.state();
        let index = if let Some(index) = state.downloads.iter().position(|d| d.version == version) {
            index
        } else {
            state.downloads.push(Download {
                version: version.to_string(),
                size,
            });
            state.total += size;
            state.downloads.len() - 1
        };
        state.current = Some(index);
        state.begun += 1;
        state.length = state.downloads[index].size;

        // The line switches to this download in one draw. indicatif draws on
        // every setter but `set_style`, and between two setters another
        // thread's `+` line can print the half-switched line: the previous
        // version over this download's size. So the label (counter and
        // version) lives in the style, set first without a draw, and length
        // and position go in one `update`. Nothing of the previous download
        // carries over.
        if self.tty {
            let counter = counter(state.begun, state.downloads.len());
            state.counter_width = counter.as_ref().map_or(0, |c| c.len() + 1);
            state.version_width = state
                .downloads
                .iter()
                .map(|d| d.version.len())
                .max()
                .unwrap_or(0);
            let width = state.version_width;
            let counter = counter
                .map(|counter| format!("{} ", dim(counter)))
                .unwrap_or_default();
            state.label = format!("{counter}{version:<width$}");
            self.fit_bar(&state);
        }
        let length = state.length;
        self.bar.update(|bar| {
            bar.set_len(length);
            bar.set_pos(0);
        });
        if self.tty {
            if !state.shown {
                state.shown = true;
                self.region.add(self.bar.clone());
            }
            self.bar.enable_steady_tick(TICK);
        }

        Job {
            request,
            version: version.to_string(),
            started: Instant::now(),
        }
    }

    /// The bar's width for this download: what is left of 80 columns once
    /// the counter, the version and its size are placed. Does not draw.
    fn fit_bar(&self, state: &RegionState) {
        let width = bar_width(
            state.counter_width + state.version_width,
            mib(state.length).to_string().len(),
        );
        self.bar
            .set_style(download_style(width, state.label.clone()));
    }

    /// A planned download the run no longer needs: the name turned out to be
    /// current when its turn came, or could not be checked.
    pub(crate) fn skip(&self, version: &str) {
        let mut state = self.state();
        if let Some(index) = state.downloads.iter().position(|d| d.version == version) {
            let skipped = state.downloads.remove(index);
            state.total = state.total.saturating_sub(skipped.size);
        }
    }

    /// `content_length` is what is actually read; a planned size that
    /// differs only moves the totals, since they are all it ever sized.
    pub(crate) fn start_download(&self, content_length: u64) {
        let mut state = self.state();
        let planned = std::mem::replace(&mut state.length, content_length);
        state.total = corrected_total(state.total, planned, content_length);
        if self.tty {
            // The width first, which does not draw; the length then draws once.
            self.fit_bar(&state);
        }
        self.bar.set_length(content_length);
        if !self.tty
            && let Some(download) = state.current.and_then(|i| state.downloads.get(i))
        {
            // Absent only for a download nobody began, which no caller does.
            eprintln!(
                "{}",
                downloading_line(
                    counter(state.begun, state.downloads.len()),
                    &download.version,
                    content_length,
                    state.total
                )
            );
        }
    }

    pub(crate) fn set_downloaded(&self, bytes: u64) {
        self.bar.set_position(bytes);
    }

    /// The download in progress arrived whole and matched its checksum: its
    /// line stays, full and still, until the next download begins.
    pub(crate) fn downloaded(&self) {
        let mut state = self.state();
        self.bar.set_position(state.length);
        self.end_download(&mut state);
    }

    /// The download in progress failed or was cancelled, wherever it was: its
    /// bytes will not arrive, so they leave the run's total, and its line
    /// goes back to empty - the next download starts from nothing either way.
    pub(crate) fn download_failed(&self) {
        let mut state = self.state();
        state.total = state.total.saturating_sub(state.length);
        self.bar.set_position(0);
        self.end_download(&mut state);
    }

    fn end_download(&self, state: &mut RegionState) {
        state.current = None;
        state.length = 0;
        self.bar.disable_steady_tick();
        // Draws are rate-limited, and with the tick gone nothing else would
        // draw the line's final state: it would stand at 99%.
        self.bar.force_draw();
    }

    /// The unpacking stage takes `job`'s package: its line appears above the
    /// download's.
    pub(crate) fn start_extract(&self, job: &Job) {
        let mut state = self.state();
        if !self.tty {
            return;
        }
        let blank = " ".repeat(state.counter_width);
        let width = state.version_width.max(job.version.len());
        let line = ProgressBar::hidden().with_style(unpack_style());
        line.set_prefix(format!("{blank}{:<width$}", job.version));
        line.set_message("extracting");
        let line = if state.shown {
            self.region.insert_before(&self.bar, line)
        } else {
            self.region.add(line)
        };
        line.enable_steady_tick(TICK);
        if let Some(previous) = state.unpacking.replace(line) {
            self.region.remove(&previous);
        }
    }

    pub(crate) fn start_install(&self) {
        if let Some(line) = &self.state().unpacking {
            line.set_message("installing");
        }
    }

    /// The unpacking stage is done with its package, however it went: its
    /// line goes.
    pub(crate) fn unpacked(&self) {
        if let Some(line) = self.state().unpacking.take() {
            line.finish_and_clear();
            self.region.remove(&line);
        }
    }

    /// The build is in the store: its `+` line goes above the region.
    pub(crate) fn landed(&self, job: &Job) {
        self.print(added_line(
            self.columns,
            job.request,
            &job.version,
            job.started.elapsed(),
        ));
    }

    /// `name` could not be installed: said the moment it is known, above the
    /// region like a `+` line, since the run goes on with the other names.
    pub(crate) fn failed(&self, name: Request, error: &str) {
        self.print(failed_line(name, error));
    }

    fn print(&self, line: String) {
        if self.tty {
            // Through the region, which clears its lines, prints, and draws
            // them again below; a plain eprintln would tear them.
            let _ = self.region.println(line);
        } else {
            eprintln!("{line}");
        }
    }

    #[cfg(test)]
    pub(crate) fn position(&self) -> u64 {
        self.bar.position()
    }

    #[cfg(test)]
    fn length(&self) -> u64 {
        self.bar.length().unwrap_or(0)
    }

    #[cfg(test)]
    fn total(&self) -> u64 {
        self.state().total
    }

    #[cfg(test)]
    fn counter_now(&self) -> Option<String> {
        let state = self.state();
        counter(state.begun, state.downloads.len())
    }

    /// Erase the live region: at the end of the run, or before its error.
    pub(crate) fn finish(&self) {
        if let Some(line) = self.state().unpacking.take() {
            line.finish_and_clear();
        }
        self.bar.finish_and_clear();
        let _ = self.region.clear();
    }
}

/// `2/3` once the second of three downloads began; `None` for a run of one.
fn counter(begun: usize, count: usize) -> Option<String> {
    (count > 1).then(|| format!("{begun}/{count}"))
}

/// `Error: 21: could not install JDK: checksum mismatch: ...` - the label
/// every error carries, then the name, since the run's other names go on.
fn failed_line(name: Request, error: &str) -> String {
    format!(
        "{} {name}: {error}",
        style("Error:").red().bold().for_stderr()
    )
}

/// The run's total once a download's real size is known: the planned size
/// came from the API, the response's is what is read.
fn corrected_total(total: u64, planned: u64, actual: u64) -> u64 {
    total.saturating_sub(planned) + actual
}

/// Without a terminal: one line per download, naming what the whole run
/// fetches when it is more than this one.
fn downloading_line(counter: Option<String>, version: &str, size: u64, total: u64) -> String {
    match counter {
        Some(counter) => format!(
            "Downloading {counter} {version} ({} MiB of {} MiB)",
            mib(size),
            mib(total)
        ),
        None => format!("Downloading {version} ({} MiB)", mib(size)),
    }
}

/// `+ 25  25.0.4+101.0.LTS  12s`: a build that landed.
fn added_line(columns: Columns, request: Request, version: &str, elapsed: Duration) -> String {
    let Columns {
        name: name_width,
        version: version_width,
    } = columns;
    format!(
        "{} {}  {version:<version_width$}  {}",
        style("+").green().for_stderr(),
        style(format!("{:<name_width$}", request.to_string()))
            .bold()
            .for_stderr(),
        dim(format_elapsed(elapsed))
    )
}

/// `- 25  25.0.1+8.0.LTS`: a build that went, named the way `jlo list` names
/// it - which is also what tells a replaced pre-release (`28-ea`) apart.
fn removed_line(columns: Columns, version: &str) -> String {
    let name_width = columns.name;
    let name = build_name(version)
        .map(|r| r.to_string())
        .unwrap_or_default();
    format!(
        "{}{}",
        dim(format!("- {name:<name_width$}  ")),
        style(version).dim().strikethrough().for_stderr()
    )
}

/// `  17  17.0.20+101  up to date`: a name the run left alone, with its
/// newest build.
fn current_line(columns: Columns, request: Request, version: &str) -> String {
    let Columns {
        name: name_width,
        version: version_width,
    } = columns;
    format!(
        "  {:<name_width$}  {}",
        request.to_string(),
        dim(format!("{version:<version_width$}  up to date"))
    )
}

/// The name a store build answers to; `None` for a version that does not
/// parse, which no build in the store has.
fn build_name(version: &str) -> Option<Request> {
    Request::of_build(&crate::version::parse(version).ok()?)
}

/// Names the platform, the half of the fact the user may not know: Adoptium
/// publishes JDK 8, just not for macOS on Apple silicon.
pub(crate) fn not_offered(requests: &[Request]) -> String {
    let names: Vec<String> = requests.iter().map(ToString::to_string).collect();
    format!(
        "Adoptium offers no build of {} for {}",
        quoted_list(&names),
        platform()
    )
}

/// Worded like the warning for a version that does not parse, which is
/// skipped the same way.
fn skipping_not_offered(request: Request) {
    warning!(
        "skipping '{request}': Adoptium offers no build of it for {}",
        platform()
    );
}

pub(crate) const NOT_OFFERED_HINT: &str = "Run 'jlo list' to see what is available.";

/// Everything `install` and `update` say after the live region: the `+`
/// lines are already out, so the `-` lines follow directly under them, then
/// the names left alone, then what needs the reader.
pub(crate) fn update_report(run: &InstallRun) {
    let replacements = || {
        run.names.iter().filter_map(|(_, result)| match result {
            NameResult::Installed { replaced, failures } => Some((replaced, failures)),
            _ => None,
        })
    };
    for (replaced, _) in replacements() {
        for version in replaced {
            eprintln!("{}", removed_line(run.columns, version));
        }
    }
    for (_, failures) in replacements() {
        print_failures(failures);
    }
    for (request, result) in &run.names {
        if let NameResult::UpToDate(newest) = result {
            eprintln!("{}", current_line(run.columns, *request, newest));
        }
    }
    for (request, result) in &run.names {
        if *result == NameResult::NotOffered {
            skipping_not_offered(*request);
        }
    }
    let mut requests: Vec<Request> = run.names.iter().map(|(request, _)| *request).collect();
    requests.sort_unstable_by_key(|request| request.listing_order());
    announce_released_ea(&requests, &run.released);

    if let Some(java_home) = &run.repointed {
        let target = crate::store::version_in(java_home)
            .unwrap_or_else(|| tilde(java_home, std::env::home_dir().as_deref()));
        note(format!("JAVA_HOME now points at {target}"));
    }
    if let Some(version) = &run.kept_active {
        hint!("{}", kept_active_hint(version));
    }
}

fn kept_active_hint(version: &str) -> String {
    format!(
        "Kept {version}, which JAVA_HOME points at. {}",
        switch_shell_hint("run 'jlo remove --superseded'")
    )
}

/// One wording for every JDK left alone because `JAVA_HOME` points at it.
/// `then` is what to do once the shell has moved.
fn switch_shell_hint(then: &str) -> String {
    format!("Switch the shell to another JDK first, e.g. 'jlo env 21', then {then}.")
}

pub(crate) fn remove_refusal_hint(refusal: &RemoveError) -> Option<String> {
    let advice = match refusal {
        RemoveError::NotInstalled(_) => {
            "Run 'jlo list --offline' to see what is installed.".to_string()
        }
        RemoveError::InUse(_) => switch_shell_hint("remove it"),
        RemoveError::Store(_) => return None,
    };
    Some(format!("Nothing was removed. {advice}"))
}

/// Message conventions, so diagnostics read as one voice:
///
/// - Text after `Error:`/`Warning:` starts lowercase with no trailing period,
///   as cargo and rustc phrase theirs; `anyhow` contexts too, since they
///   compose into a chain after that label.
/// - Say "could not `<verb>`" - not "Failed to", "Can't" or "Error `<verb>`ing".
/// - Do not restate the chain's first clause in the top-level message.
/// - Hints are advice, not a label: sentence-cased with a full stop.
///
/// Every style here is `.for_stderr()`: `console::style` checks *stdout* for
/// colour support, and the `jlo` wrapper captures stdout, so it is a pipe
/// exactly when a user is watching stderr.
pub(crate) fn print_error(args: std::fmt::Arguments) {
    eprintln!("{} {args}", style("Error:").red().bold().for_stderr());
}

pub(crate) fn print_warning(args: std::fmt::Arguments) {
    eprintln!("{} {args}", style("Warning:").yellow().bold().for_stderr());
}

/// Dimmed rather than labelled: advice must not read as a second failure.
/// The commands it quotes are bold, so the eye lands on what to run.
pub(crate) fn print_hint(args: std::fmt::Arguments) {
    eprintln!("{}", hint_text(&args.to_string()));
}

/// Dim prose, its `'…'`-quoted commands bold. Built from segments, each with
/// its own reset: bold and dim share SGR 22, so a bold span styled inside a
/// dim one would end the dim early.
fn hint_text(text: &str) -> String {
    quoted_spans(text)
        .into_iter()
        .map(|(quoted, span)| {
            if quoted {
                style(span).bold().for_stderr().to_string()
            } else {
                dim(span)
            }
        })
        .collect()
}

/// `text` cut into prose and the content of its `'…'`-quoted spans, in
/// order; `true` marks a span's content, and the quotes stay with the prose.
/// A quote opens a span only at the start of a word and closes it only at
/// the end of one, so the apostrophe in `J'Lo` opens nothing.
fn quoted_spans(text: &str) -> Vec<(bool, &str)> {
    // Chars, not bytes, around a quote: whitespace may be a tab or wider
    // than one byte. The quote itself is ASCII, so its byte index is always a
    // char boundary to slice at.
    let opens = |i: usize| {
        text[..i]
            .chars()
            .next_back()
            .is_none_or(|c| c.is_whitespace() || c == '(')
    };
    let closes = |i: usize| {
        text[i + 1..]
            .chars()
            .next()
            .is_none_or(|c| c.is_whitespace() || ".,;:)?!".contains(c))
    };
    let quotes: Vec<usize> = text.match_indices('\'').map(|(i, _)| i).collect();
    let mut spans = Vec::new();
    let mut prose = 0;
    let mut k = 0;
    while k < quotes.len() {
        let open = quotes[k];
        let close = opens(open)
            .then(|| quotes[k + 1..].iter().position(|&q| closes(q)))
            .flatten();
        if let Some(offset) = close {
            let close = quotes[k + 1 + offset];
            spans.push((false, &text[prose..=open]));
            spans.push((true, &text[open + 1..close]));
            prose = close;
            k += offset + 2;
        } else {
            k += 1;
        }
    }
    spans.push((false, &text[prose..]));
    spans.retain(|(_, span)| !span.is_empty());
    spans
}

fn note(text: impl std::fmt::Display) {
    eprintln!("{}", dim(text));
}

/// The one weight every secondary piece of output shares.
pub(crate) fn dim(text: impl std::fmt::Display) -> String {
    style(text).dim().for_stderr().to_string()
}

pub(crate) fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

fn print_failures(failures: &[String]) {
    for failure in failures {
        eprintln!("{} {}", style("!").red().for_stderr(), failure);
    }
}

pub(crate) fn print_created(args: std::fmt::Arguments) {
    eprintln!("{} {args}", style("✓").green().for_stderr());
}

/// A command offered for copying; the caller prints it at column 0, since
/// terminal selection takes a leading indent with it.
///
/// Green, like the `✓`, told apart by shape: blue (ANSI 34) is illegible on a
/// dark background.
pub(crate) fn command(line: &str) -> String {
    style(line).green().bright().for_stderr().to_string()
}

// The help helpers render escape codes unconditionally: clap decides on
// colour itself and strips them when it decides against, where a `console`
// style would decide a second time, against stderr.

/// In clap's heading style, so the sections jlo adds cannot drift from clap's.
pub(crate) fn help_heading(text: &str) -> String {
    let style = *Styles::styled().get_header();
    format!("{style}{text}{style:#}")
}

/// A command mentioned in help text, in clap's literal style.
pub(crate) fn help_literal(text: &str) -> String {
    let style = *Styles::styled().get_literal();
    format!("{style}{text}{style:#}")
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

/// Deletion cannot be undone, so it always says what it did - one `-` line
/// per build that went - including "nothing", which only `--superseded`
/// reaches.
pub(crate) fn remove_report(report: &RemoveReport) {
    let columns = Columns::fit(
        report
            .removed
            .iter()
            .filter_map(|version| build_name(version)),
        report.removed.iter().map(String::as_str),
    );
    for version in &report.removed {
        eprintln!("{}", removed_line(columns, version));
    }

    print_failures(&report.failures);

    // No "Nothing to remove" after a failure: it would be a false claim about
    // a store still holding those builds.
    if report.removed.is_empty() && report.failures.is_empty() {
        // The parenthetical is the *reason* nothing went, which is false when
        // the in-use skip below kept a superseded build.
        eprintln!(
            "{} Nothing to remove{}",
            style("✓").green().for_stderr(),
            if report.skipped_in_use.is_some() {
                String::new()
            } else {
                format!(
                    " {}",
                    dim("(only the newest build of each name is installed)")
                )
            }
        );
    }

    // Not a failure - already absent is what was asked for - but usually a
    // typo, so still said.
    if !report.not_installed.is_empty() {
        note(format!(
            "nothing installed matched {}",
            quoted_list(&report.not_installed)
        ));
    }

    // Last, and a warning: the only skip the user can act on.
    if let Some(version) = &report.skipped_in_use {
        warning!("left {version} alone: JAVA_HOME points at it");
        hint!("{}", switch_shell_hint("run it again"));
    }
}

/// The `jlo list --offline` listing: the networked listing's rows, minus the
/// catalogue.
pub(crate) fn offline_list(groups: &[NameGroup], active_version: Option<&str>, foreign: &Foreign) {
    if groups.is_empty() {
        eprintln!("No JDKs installed in {}.", foreign.base.display());
        print_footer(Vec::new(), foreign, false);
        return;
    }

    print_listing(&build_rows(&[], groups, active_version), foreign);
}

/// The `jlo list` listing: what Adoptium offers, merged with what is
/// installed. One row per name; any other build of it gets an indented line
/// of its own, so `jlo remove <build>` still finds its argument here.
pub(crate) fn remote_list(
    available: &[RemoteJdk],
    groups: &[NameGroup],
    active_version: Option<&str>,
    foreign: &Foreign,
) {
    if available.is_empty() {
        eprintln!("Adoptium offers no JDKs for this OS and architecture.");
        // Not a return: installs still present are still removable.
        if groups.is_empty() {
            print_footer(Vec::new(), foreign, false);
            return;
        }
    }

    print_listing(&build_rows(available, groups, active_version), foreign);
}

/// The directories in the store that are not jlo's, and where they are.
pub(crate) struct Foreign<'a> {
    pub(crate) names: &'a [String],
    pub(crate) base: &'a Path,
}

/// The footer labels share one width, so their values line up.
const AVAILABLE_LABEL: &str = "available:";
const NOT_MANAGED_LABEL: &str = "not managed:";
const FOOTER_LABEL_WIDTH: usize = NOT_MANAGED_LABEL.len();

/// A footer row: a dim label, then its value. `label` is `None` on a
/// continuation row, which keeps the label's width blank.
fn footer_row(label: Option<&str>, value: &str) -> String {
    let label = format!("{:<FOOTER_LABEL_WIDTH$}", label.unwrap_or(""));
    format!("  {}  {value}", style(label).dim())
}

/// The directories jlo will never delete - an earlier jlo's bare-version
/// installs, an IDE's downloads - one per row, so a name with spaces in it
/// stays readable and the user can delete them by hand.
fn foreign_rows(names: &[String]) -> Vec<String> {
    names
        .iter()
        .enumerate()
        .map(|(i, name)| {
            let label = (i == 0).then_some(NOT_MANAGED_LABEL);
            footer_row(label, &style(name).dim().to_string())
        })
        .collect()
}

/// The footer rows to stdout, like every other listed name; the blank line
/// before them, only when rows precede, and the reason after them to stderr.
fn print_footer(mut rows: Vec<String>, foreign: &Foreign, after_rows: bool) {
    rows.extend(foreign_rows(foreign.names));
    if rows.is_empty() {
        return;
    }
    if after_rows {
        eprintln!();
    }
    print_lines(rows);
    if !foreign.names.is_empty() {
        // Without it, a JDK that works but is never used, updated or deleted
        // reads as a bug.
        note("  (jlo manages only jlo-temurin-* directories)");
    }
}

fn print_listing(rows: &[NameRow], foreign: &Foreign) {
    let listing = render_rows(rows);

    // Blank lines and the tip go to stderr, so a pipe sees only the rows.
    let installed_any = !listing.installed.is_empty();
    print_lines(listing.installed);
    print_footer(
        listing.available.into_iter().collect(),
        foreign,
        installed_any,
    );

    if let Some(tip) = tip_line(rows) {
        eprintln!("\n  {tip}");
    }
}

/// `jlo current`'s stdout line: which JDK, and why.
pub(crate) fn provenance_line(active: &Active) -> String {
    // A JDK that is not jlo's has no version to name; the path says "not
    // mine", and usually holds the version anyway.
    let subject = match &active.version {
        Some(version) => version.clone(),
        None => active.path.display().to_string(),
    };

    let note = match &active.source {
        Some(Source::Foreign) => "$JAVA_HOME, set outside jlo".to_string(),
        Some(source) => format!("from {}", source.label()),
        // A config pins something else: that is the warning, not this line.
        None if active.pinned_elsewhere.is_some() => "active".to_string(),
        None => "active, nothing pinned".to_string(),
    };

    format!("{subject}  ({note})")
}

/// Not gated on `is_terminal()` like the unsourced-`env` hint: that gate exists
/// because the autoload hook runs `env`, and nothing hooks this.
pub(crate) fn pin_mismatch(pinned: &Resolved) {
    warning!(
        "{} pins Java {}; run 'jlo env' to switch.",
        pinned.source.label(),
        pinned.request
    );
}

pub(crate) const NO_ACTIVE_JDK_HINT: &str = "Run 'jlo env' to activate a JDK in this shell.";

/// The files `bash -l` looks for, in the order it looks. It reads only the
/// first one that exists, so creating `~/.bash_profile` beside a `~/.profile`
/// switches that one off.
pub(crate) const BASH_LOGIN_FILES: &str = "~/.bash_profile, ~/.bash_login, ~/.profile";

/// `jlo env` with stdout on a terminal: nothing is there to evaluate its
/// exports. Keyed on the terminal, not on the wrapped prefix alone: the wrapper
/// and the autoload hook capture stdout, and `jlo-bin env 21 > file` is a
/// deliberate use, so neither is refused.
pub(crate) const UNSOURCED_ENV_ERROR: &str =
    "this shell has no 'jlo' function, so jlo env cannot change it.";

/// Under [`UNSOURCED_ENV_ERROR`]: the commands that work without the function,
/// and - when this install has a profile line to name - where it goes. The
/// line itself follows on its own, as a command.
pub(crate) fn unsourced_env_hint(version: Option<&str>, has_profile_line: bool) -> String {
    let v = version.map(|v| format!(" {v}")).unwrap_or_default();
    let alternatives = format!(
        "For one command instead: 'jlo exec{v} -- <command>', or \
         'export JAVA_HOME=\"$(jlo home{v})\"'."
    );
    if has_profile_line {
        format!(
            "{alternatives}\nThe function comes from a line in your shell profile. Add it to \
             ~/.zshrc; for bash, to the first of {BASH_LOGIN_FILES} that exists, else \
             ~/.bash_profile. Then open a new shell:"
        )
    } else {
        alternatives
    }
}

pub(crate) fn newer_jlo_hint(latest: &str) -> String {
    format!("J'Lo {latest} is available. Run 'jlo selfupdate' to install it.")
}

/// Said after a pre-release install whose major has since shipped. `-ea` is
/// literal: Adoptium keeps that stream running after GA, so the pin keeps
/// delivering previews of the *next patch* - announced, not changed under the
/// user's feet. Repeats on every bare `jlo update`, so it also names the way
/// to stop.
pub(crate) fn ea_is_now_released(request: Request) -> String {
    format!(
        "{request} still tracks pre-release builds; Java {major} has since been released. \
         Pin '{major}' to follow the released builds instead, or 'jlo remove {request}' \
         to stop updating the pre-release.",
        major = request.major
    )
}

/// The pre-release names among `requests` whose major has since shipped,
/// deduplicated, in the order given.
fn released_ea_names(requests: &[Request], released: &[i64]) -> Vec<Request> {
    let mut names: Vec<Request> = Vec::new();
    for request in requests.iter().filter(|r| r.is_ea()) {
        if released.contains(&request.major) && !names.contains(request) {
            names.push(*request);
        }
    }
    names
}

fn announce_released_ea(requests: &[Request], released: &[i64]) {
    for request in released_ea_names(requests, released) {
        hint!("{}", ea_is_now_released(request));
    }
}

/// `println!` panics when the reader goes away, and this output is meant to be
/// piped (`jlo list | head`), so a closed pipe is a normal end of output.
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

/// An installed build that is not the one its name reports.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Build {
    version: String,
    status: Status,
    /// `$JAVA_HOME` points at this install.
    active: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct NameRow {
    name: Request,
    /// The newest build of the name, as shown in the column.
    installed: Option<String>,
    /// The build Adoptium offers, when that exact build is not installed.
    /// Older than what is when the catalogue sits behind the store.
    latest: Option<String>,
    /// `latest` is newer than every install of this name.
    update: bool,
    /// `$JAVA_HOME` points at `installed`.
    active: bool,
    /// Every other install of the name, newest first.
    others: Vec<Build>,
}

fn build_rows(
    available: &[RemoteJdk],
    groups: &[NameGroup],
    active_version: Option<&str>,
) -> Vec<NameRow> {
    let mut names: Vec<Request> = available
        .iter()
        .map(|jdk| jdk.request)
        .chain(groups.iter().map(|group| group.request))
        .collect();
    names.sort_unstable_by_key(|name| name.listing_order());
    names.dedup();

    names
        .into_iter()
        .map(|name| {
            let offered = available.iter().find(|jdk| jdk.request == name);
            let group = groups.iter().find(|group| group.request == name);
            name_row(name, offered, group, active_version)
        })
        .collect()
}

/// Only builds of this name are compared, never the other stream: a
/// pre-release of a later patch sorts above the current release, so an offered
/// beta would read as an update to an installed release.
fn name_row(
    name: Request,
    offered: Option<&RemoteJdk>,
    group: Option<&NameGroup>,
    active_version: Option<&str>,
) -> NameRow {
    let builds = group.map(NameGroup::builds).unwrap_or_default();

    // `latest` is any offer not already on disk, so a catalogue behind the
    // store stays visible; only `update` claims an improvement, by the rule
    // `jlo update` downloads by.
    let latest = offered
        .filter(|jdk| !builds.iter().any(|build| build.version == jdk.version))
        .map(|jdk| jdk.version.clone());
    let update = !builds.is_empty()
        && offered.is_some_and(|jdk| supersedes_every_install(&jdk.version, &builds));

    let head = group.map(|group| group.head);
    let is_active = |version: &str| active_version == Some(version);
    let others = group
        .map(|group| {
            group
                .others
                .iter()
                .map(|(jdk, status)| Build {
                    version: jdk.version.clone(),
                    status: *status,
                    active: is_active(&jdk.version),
                })
                .collect()
        })
        .unwrap_or_default();

    NameRow {
        name,
        installed: head.map(|jdk| jdk.version.clone()),
        latest,
        update,
        active: head.is_some_and(|jdk| is_active(&jdk.version)),
        others,
    }
}

struct Listing {
    installed: Vec<String>,
    available: Option<String>,
}

/// One printed line before padding.
struct Cells<'a> {
    /// Empty on a build line: the blank is what ties it to the name above.
    name: String,
    version: &'a str,
    active: bool,
    /// A superseded build's line is dim throughout, its `active` aside.
    superseded: bool,
    /// The state words, already styled. Words rather than glyphs, so
    /// `jlo list | grep update` works and nothing depends on colour.
    states: String,
}

/// Anything of the name on disk.
fn is_installed(row: &NameRow) -> bool {
    row.installed.is_some() || !row.others.is_empty()
}

/// `active` first: it is the one word a glance looks for.
fn states(active: bool, rest: Option<String>) -> String {
    let active = active.then(|| style("active").cyan().to_string());
    active
        .into_iter()
        .chain(rest)
        .collect::<Vec<_>>()
        .join("  ")
}

fn push_name<'a>(out: &mut Vec<Cells<'a>>, row: &'a NameRow) {
    let offer = match (&row.latest, row.update) {
        (Some(latest), true) => Some(format!(
            "{}{}{latest}",
            style("update").yellow(),
            style(" \u{2192} ").dim()
        )),
        (Some(latest), false) => Some(style(format!("{latest} offered")).dim().to_string()),
        (None, _) => None,
    };
    out.push(Cells {
        name: row.name.to_string(),
        version: row.installed.as_deref().unwrap_or(""),
        active: row.active,
        superseded: false,
        states: states(row.active, offer),
    });
    for build in &row.others {
        let superseded = build.status == Status::Superseded;
        out.push(Cells {
            name: String::new(),
            version: &build.version,
            active: build.active,
            superseded,
            states: states(
                build.active,
                superseded.then(|| style("superseded").dim().to_string()),
            ),
        });
    }
}

fn render_rows(rows: &[NameRow]) -> Listing {
    let (on_disk, not_installed): (Vec<&NameRow>, Vec<&NameRow>) =
        rows.iter().partition(|row| is_installed(row));

    let mut cells = Vec::new();
    for row in on_disk {
        push_name(&mut cells, row);
    }

    let name_width = cells.iter().map(|c| c.name.len()).max().unwrap_or(0);
    let version_width = cells.iter().map(|c| c.version.len()).max().unwrap_or(0);

    let installed = cells
        .iter()
        .map(|c| render_line(c, name_width, version_width))
        .collect();

    let not_installed: Vec<String> = not_installed
        .iter()
        .map(|row| row.name.to_string())
        .collect();
    let available = (!not_installed.is_empty())
        .then(|| footer_row(Some(AVAILABLE_LABEL), &not_installed.join("  ")));

    Listing {
        installed,
        available,
    }
}

/// Each field is padded only when something follows it, so no line ends in
/// spaces, and before it is styled: the formatter counts escape bytes.
fn render_line(c: &Cells, name_width: usize, version_width: usize) -> String {
    let pad = |text: &str, width: usize, last: bool| {
        if last {
            text.to_string()
        } else {
            format!("{text:<width$}")
        }
    };
    let version_last = c.states.is_empty();
    let name_last = version_last && c.version.is_empty();

    let name = pad(&c.name, name_width, name_last);
    let version = pad(c.version, version_width, version_last);
    let name = if c.active {
        style(name).cyan().bold()
    } else {
        style(name).bold()
    };
    let version = if c.superseded {
        style(version).dim()
    } else {
        style(version)
    };

    let mut fields = vec![name.to_string()];
    if !name_last {
        fields.push(version.to_string());
    }
    if !version_last {
        fields.push(c.states.clone());
    }
    format!("  {}", fields.join("  "))
}

/// One line whatever applies: this prints on every `jlo list`, and a stack of
/// suggestions reads as nagging.
fn tip_line(rows: &[NameRow]) -> Option<String> {
    let outdated = rows.iter().filter(|name| name.update).count();
    // Not counted where an update is on offer: `jlo update` deletes it too.
    let superseded = rows
        .iter()
        .filter(|name| !name.update)
        .flat_map(|name| &name.others)
        .filter(|build| build.status == Status::Superseded)
        .count();

    let mut offers = Vec::new();
    if outdated > 0 {
        offers.push(format!("'jlo update' ({outdated} outdated)"));
    }
    if superseded > 0 {
        offers.push(format!(
            "'jlo remove --superseded' ({superseded} superseded)"
        ));
    }
    if offers.is_empty() {
        return None;
    }

    Some(hint_text(&format!("run {}", offers.join(" \u{b7} "))))
}

/// Whether stderr can carry a bar that redraws over itself. A terminal is not
/// enough: under a dumb or unset `TERM` indicatif silently suppresses the bar,
/// and an install would print nothing at all.
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

const MAX_BAR_WIDTH: usize = 22;
const MIN_BAR_WIDTH: usize = 8;
/// A live line has to fit 80 columns: a wrapped one makes the redraw
/// repaint two rows and flicker.
const LINE_WIDTH: usize = 80;
const MIB: u64 = 1024 * 1024;

/// Whole MiB, rounded: a download is a few hundred of them, and decimals
/// would only make the line flicker.
fn mib(bytes: u64) -> u64 {
    (bytes + MIB / 2) / MIB
}

/// The download line. Fits 80 columns for the longest versions
/// (`21.0.12+101.0.LTS`) with a counter: a wrapped line makes the redraw
/// repaint two rows and flicker.
///
/// No palette colour: the region is erased when the run ends, and a colour
/// that disappears would cost the same colour its meaning elsewhere. Weight
/// carries the bar instead - a heavy line done, a light one to go - which
/// also survives `NO_COLOR`.
fn download_style(bar_width: usize, label: String) -> ProgressStyle {
    run_style("{spinner:.dim} {label}  {fill}  {pct}  {mib}", bar_width).with_key(
        "label",
        move |_: &ProgressState, w: &mut dyn std::fmt::Write| {
            let _ = w.write_str(&label);
        },
    )
}

/// The unpacking line: the package and what is being done to it.
fn unpack_style() -> ProgressStyle {
    ProgressStyle::with_template("{spinner:.dim} {prefix}  {msg:.dim}")
        .expect("progress template is a valid literal")
}

/// What is left of the download line for the bar, given the prefix (counter
/// and version) and the digits of the download's size in MiB: spinner, the
/// three gaps, `100%` and `pos/size MiB` take the rest.
fn bar_width(prefix: usize, size_digits: usize) -> usize {
    let mib = 2 * size_digits + "/ MiB".len();
    let fixed = 2 + prefix + 2 + 2 + "100%".len() + 2 + mib;
    LINE_WIDTH
        .saturating_sub(fixed)
        .clamp(MIN_BAR_WIDTH, MAX_BAR_WIDTH)
}

fn run_style(template: &str, bar_width: usize) -> ProgressStyle {
    ProgressStyle::with_template(template)
        .expect("progress template is a valid literal")
        .with_key(
            "fill",
            move |state: &ProgressState, w: &mut dyn std::fmt::Write| {
                let _ = w.write_str(&fill(state.pos(), state.len().unwrap_or(0), bar_width));
            },
        )
        .with_key(
            "pct",
            |state: &ProgressState, w: &mut dyn std::fmt::Write| {
                let _ = write!(w, "{:>3}%", percent(state.pos(), state.len().unwrap_or(0)));
            },
        )
        .with_key(
            "mib",
            |state: &ProgressState, w: &mut dyn std::fmt::Write| {
                let total = mib(state.len().unwrap_or(0)).to_string();
                let digits = total.len();
                let _ = write!(
                    w,
                    "{:>digits$}{}",
                    mib(state.pos()),
                    dim(format!("/{total} MiB"))
                );
            },
        )
}

fn percent(pos: u64, len: u64) -> u64 {
    (pos.min(len) * 100).checked_div(len).unwrap_or(0)
}

/// `━━━━╾───`: the done part and the rest styled apart. indicatif's own
/// `{bar:N.bold/dim}` nests the rest's dim inside the bold, and the two share
/// one SGR reset.
fn fill(pos: u64, len: u64, width: usize) -> String {
    let done = usize::try_from(percent(pos, len)).unwrap_or(100) * width / 100;
    if done >= width {
        return style("\u{2501}".repeat(width))
            .bold()
            .for_stderr()
            .to_string();
    }
    format!(
        "{}{}",
        style(format!("{}\u{257e}", "\u{2501}".repeat(done)))
            .bold()
            .for_stderr(),
        dim("\u{2500}".repeat(width - done - 1))
    )
}

fn format_elapsed(elapsed: Duration) -> String {
    let secs = elapsed.as_secs();
    if secs >= 60 {
        format!("{}m{:02}s", secs / 60, secs % 60)
    } else {
        format!("{secs}s")
    }
}

/// Collapse the home directory. Prose only: never where a shell reads the
/// line back.
pub(crate) fn tilde(path: &Path, home: Option<&Path>) -> String {
    match home.and_then(|h| path.strip_prefix(h).ok()) {
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::request::{Stream, request};
    use crate::store::{InstalledJdk, group_by_name};
    use std::path::PathBuf;
    use std::sync::Arc;

    // -- ea_is_now_released --

    /// One notice per EA name whose major has shipped, and none for the rest.
    /// `21-ea` on a released 21 is the case the notice exists for; `28-ea` on
    /// an unreleased 28 is the ordinary case and must stay silent.
    #[test]
    fn only_a_released_major_earns_the_notice() {
        let ea = |major| Request {
            major,
            stream: Stream::Ea,
        };
        assert_eq!(released_ea_names(&[ea(21), ea(28)], &[21]), vec![ea(21)]);
        assert_eq!(
            released_ea_names(&[request("21")], &[21]),
            vec![],
            "a GA name is not a pin on the unreleased stream"
        );
    }

    /// The same name twice - two installs of one pin, or a request set that
    /// repeats it - is still one thing to say.
    #[test]
    fn a_repeated_name_is_announced_once() {
        let ea = request("21-ea");
        assert_eq!(released_ea_names(&[ea, ea], &[21]), vec![ea]);
    }

    /// -ea means "unreleased", not "newest", so the pin keeps delivering
    /// betas after the major ships. That is the intended behaviour and
    /// therefore a note, not a warning - but it must name the way out.
    #[test]
    fn the_notice_names_the_major_and_the_plain_name() {
        let notice = ea_is_now_released(request("28-ea"));
        assert!(
            notice.contains("28-ea"),
            "names what was asked for: {notice}"
        );
        assert!(
            notice.contains("Pin '28'"),
            "names the plain major: {notice}"
        );
    }

    // -- the store diff --

    /// A pre-release stream publishes weekly and a bare `jlo update` moves
    /// it, so a replaced build has to read as a preview swapped for the next,
    /// not as a patch release: its line is named `28-ea`, as `jlo list` names
    /// the stream.
    #[test]
    fn a_removed_pre_release_is_named_as_its_stream() {
        plain();
        let columns = Columns::fit([request("28-ea")], ["28.0.0-beta+14.0.ea"]);
        assert_eq!(
            removed_line(columns, "28.0.0-beta+14.0.ea"),
            "- 28-ea  28.0.0-beta+14.0.ea"
        );
        assert_eq!(removed_line(columns, "21.0.8+9"), "- 21     21.0.8+9");
    }

    /// The `+`, `-` and up-to-date lines of one run share their columns,
    /// whichever line is the widest, so the report reads as one table.
    #[test]
    fn the_diff_lines_of_a_run_share_their_columns() {
        plain();
        let columns = Columns::fit(
            [request("25"), request("21"), request("28-ea")],
            [
                "25.0.4+101.0.LTS",
                "21.0.12+101.0.LTS",
                "28.0.0-beta+17.0.ea",
            ],
        );
        assert_eq!(
            added_line(
                columns,
                request("25"),
                "25.0.4+101.0.LTS",
                Duration::from_secs(12)
            ),
            "+ 25     25.0.4+101.0.LTS     12s"
        );
        assert_eq!(
            removed_line(columns, "21.0.8+9.0.LTS"),
            "- 21     21.0.8+9.0.LTS"
        );
        assert_eq!(
            current_line(columns, request("28-ea"), "28.0.0-beta+17.0.ea"),
            "  28-ea  28.0.0-beta+17.0.ea  up to date"
        );
    }

    // -- the progress region --

    /// The API's size only sized the totals: once the response says
    /// otherwise, the run's total follows the response, in either direction.
    #[test]
    fn the_total_follows_the_size_actually_read() {
        assert_eq!(corrected_total(300, 100, 120), 320);
        assert_eq!(corrected_total(300, 100, 90), 290);
        assert_eq!(corrected_total(300, 0, 120), 420, "no size from the API");
    }

    /// Without a terminal the total is only said when the run fetches more
    /// than this one download.
    #[test]
    fn a_downloading_line_names_the_run_total_only_for_several() {
        assert_eq!(
            downloading_line(Some("1/2".into()), "25.0.4+101.0.LTS", 130 * MIB, 315 * MIB),
            "Downloading 1/2 25.0.4+101.0.LTS (130 MiB of 315 MiB)"
        );
        assert_eq!(
            downloading_line(None, "25.0.4+101.0.LTS", 130 * MIB, 130 * MIB),
            "Downloading 25.0.4+101.0.LTS (130 MiB)"
        );
    }

    fn planned(sizes: &[(&str, u64)]) -> InstallUi {
        let downloads = sizes
            .iter()
            .map(|&(version, size)| Download {
                version: version.to_string(),
                size,
            })
            .collect();
        InstallUi::drawing_to(downloads, Columns::default(), None)
    }

    /// The bar is the download in progress, not the run: each download starts
    /// from nothing at its own size, the response's size wins over the API's,
    /// and a finished download stands full until the next begins. The run's
    /// total - all the plain `Downloading` line still needs - follows the
    /// same corrections and leaves out a skipped download.
    #[test]
    fn the_bar_counts_the_download_in_progress() {
        let ui = planned(&[("21.0.9+10", 100), ("21.0.10+7", 200), ("21.0.11+9", 50)]);
        assert_eq!(ui.total(), 350);

        ui.begin(request("21"), "21.0.9+10", 100);
        assert_eq!((ui.position(), ui.length()), (0, 100));
        ui.start_download(120);
        assert_eq!(ui.length(), 120, "the response's size wins");
        assert_eq!(ui.total(), 370);
        ui.set_downloaded(60);
        assert_eq!(ui.position(), 60);
        ui.downloaded();
        assert_eq!((ui.position(), ui.length()), (120, 120), "stands full");

        ui.skip("21.0.10+7");
        assert_eq!(ui.total(), 170, "a skipped download leaves the total");

        ui.begin(request("21"), "21.0.11+9", 50);
        assert_eq!(
            (ui.position(), ui.length()),
            (0, 50),
            "nothing carried over"
        );
        assert_eq!(ui.counter_now().as_deref(), Some("2/2"));
        ui.start_download(50);
        ui.set_downloaded(10);
        assert_eq!(ui.position(), 10);
    }

    /// A failed download empties its bar and takes its bytes out of the run's
    /// total - the planned size before the response said otherwise, the
    /// response's after - wherever it failed; the next download starts clean.
    #[test]
    fn a_failed_download_leaves_nothing_behind_for_the_next() {
        // Before headers: the planned size goes.
        let ui = planned(&[("21.0.9+10", 100), ("17.0.20+101", 200)]);
        ui.begin(request("21"), "21.0.9+10", 100);
        ui.download_failed();
        assert_eq!((ui.position(), ui.total()), (0, 200));

        // Mid-body, after a corrected size: the response's size goes.
        let ui = planned(&[("21.0.9+10", 100), ("17.0.20+101", 200)]);
        ui.begin(request("21"), "21.0.9+10", 100);
        ui.start_download(250);
        ui.set_downloaded(180);
        ui.download_failed();
        assert_eq!((ui.position(), ui.total()), (0, 200));
        ui.begin(request("17"), "17.0.20+101", 200);
        assert_eq!((ui.position(), ui.length()), (0, 200));

        // Whole but failing its checksum: the same.
        let ui = planned(&[("21.0.9+10", 100)]);
        ui.begin(request("21"), "21.0.9+10", 100);
        ui.start_download(100);
        ui.set_downloaded(100);
        ui.download_failed();
        assert_eq!((ui.position(), ui.total()), (0, 0));
    }

    /// A package that fails to unpack was still downloaded: the run's total
    /// keeps its bytes.
    #[test]
    fn a_failed_unpacking_keeps_the_downloaded_bytes() {
        let ui = planned(&[("21.0.9+10", 100), ("17.0.20+101", 200)]);
        let job = ui.begin(request("21"), "21.0.9+10", 100);
        ui.start_download(100);
        ui.downloaded();
        ui.start_extract(&job);
        ui.unpacked();
        assert_eq!(ui.total(), 300);
    }

    /// A download the up-front plan did not have joins the total and the
    /// count after the ones already begun, so the counter never runs back.
    #[test]
    fn an_unplanned_download_counts_on_from_where_the_run_is() {
        let ui = planned(&[("21.0.9+10", 100), ("17.0.20+101", 100)]);
        ui.begin(request("21"), "21.0.9+10", 100);
        assert_eq!(ui.counter_now().as_deref(), Some("1/2"));
        ui.downloaded();
        ui.begin(request("25"), "25.0.4+101.0.LTS", 130);
        assert_eq!(ui.total(), 330);
        assert_eq!(ui.counter_now().as_deref(), Some("2/3"));
    }

    /// A terminal for the live region to draw on, kept as plain text: enough
    /// of a screen to follow indicatif's cursor movements, plus every frame
    /// it flushed. Tests what the region shows, not how indicatif renders it.
    #[derive(Debug, Clone, Default)]
    struct Screen(Arc<Mutex<ScreenState>>);

    const SCREEN_WIDTH: usize = 80;

    #[derive(Debug, Default)]
    struct ScreenState {
        lines: Vec<String>,
        row: usize,
        col: usize,
        frames: Vec<Vec<String>>,
    }

    impl ScreenState {
        fn put(&mut self, text: &str) {
            for c in console::strip_ansi_codes(text).chars() {
                match c {
                    '\r' => self.col = 0,
                    '\n' => {
                        self.row += 1;
                        self.col = 0;
                    }
                    c => {
                        // A full line wraps at the next character, as a
                        // terminal's does: indicatif pads the last line to
                        // the width and relies on it.
                        if self.col >= SCREEN_WIDTH {
                            self.row += 1;
                            self.col = 0;
                        }
                        if self.lines.len() <= self.row {
                            self.lines.resize(self.row + 1, String::new());
                        }
                        let mut line: Vec<char> = self.lines[self.row].chars().collect();
                        if line.len() <= self.col {
                            line.resize(self.col + 1, ' ');
                        }
                        line[self.col] = c;
                        self.lines[self.row] = line.into_iter().collect();
                        self.col += 1;
                    }
                }
            }
        }

        fn visible(&self) -> Vec<String> {
            let mut lines: Vec<String> = self
                .lines
                .iter()
                .map(|line| line.trim_end().to_string())
                .collect();
            while lines.last().is_some_and(String::is_empty) {
                lines.pop();
            }
            lines
        }
    }

    impl Screen {
        fn state(&self) -> std::sync::MutexGuard<'_, ScreenState> {
            self.0.lock().unwrap()
        }

        fn visible(&self) -> Vec<String> {
            self.state().visible()
        }

        fn take_frames(&self) -> Vec<Vec<String>> {
            std::mem::take(&mut self.state().frames)
        }
    }

    impl indicatif::TermLike for Screen {
        fn width(&self) -> u16 {
            u16::try_from(SCREEN_WIDTH).unwrap()
        }

        fn move_cursor_up(&self, n: usize) -> std::io::Result<()> {
            let mut state = self.state();
            state.row = state.row.saturating_sub(n);
            Ok(())
        }

        fn move_cursor_down(&self, n: usize) -> std::io::Result<()> {
            self.state().row += n;
            Ok(())
        }

        fn move_cursor_right(&self, n: usize) -> std::io::Result<()> {
            self.state().col += n;
            Ok(())
        }

        fn move_cursor_left(&self, n: usize) -> std::io::Result<()> {
            let mut state = self.state();
            state.col = state.col.saturating_sub(n);
            Ok(())
        }

        fn write_line(&self, line: &str) -> std::io::Result<()> {
            let mut state = self.state();
            state.put(line);
            state.put("\n");
            Ok(())
        }

        fn write_str(&self, text: &str) -> std::io::Result<()> {
            self.state().put(text);
            Ok(())
        }

        fn clear_line(&self) -> std::io::Result<()> {
            let mut state = self.state();
            let row = state.row;
            if let Some(line) = state.lines.get_mut(row) {
                line.clear();
            }
            state.col = 0;
            Ok(())
        }

        fn flush(&self) -> std::io::Result<()> {
            let mut state = self.state();
            let frame = state.visible();
            state.frames.push(frame);
            Ok(())
        }
    }

    /// A run planned with `sizes`, drawing its live region on a [`Screen`].
    fn on_screen(sizes: &[(&str, u64)]) -> (InstallUi, Screen) {
        let screen = Screen::default();
        let downloads = sizes
            .iter()
            .map(|&(version, size)| Download {
                version: version.to_string(),
                size,
            })
            .collect();
        let target = ProgressDrawTarget::term_like(Box::new(screen.clone()));
        let ui = InstallUi::drawing_to(downloads, Columns::default(), Some(target));
        (ui, screen)
    }

    /// Long enough for the spinners' steady ticks to draw.
    fn settle() {
        std::thread::sleep(TICK * 3);
    }

    /// While a package unpacks, its line stands above the download in
    /// progress, under the same version column; when it is done the line
    /// goes, and at the end of the run the region with it.
    #[test]
    fn the_unpacking_line_stands_above_the_download_only_while_it_unpacks() {
        let (ui, screen) = on_screen(&[("21.0.9+10", 200 * MIB), ("17.0.20+101", 100 * MIB)]);
        let job = ui.begin(request("21"), "21.0.9+10", 200 * MIB);
        ui.start_download(200 * MIB);
        ui.downloaded();
        ui.begin(request("17"), "17.0.20+101", 100 * MIB);
        ui.start_download(100 * MIB);
        ui.start_extract(&job);
        settle();

        let lines = screen.visible();
        assert_eq!(lines.len(), 2, "{lines:#?}");
        assert!(
            lines[0].ends_with("    21.0.9+10    extracting"),
            "{lines:#?}"
        );
        assert!(lines[1].contains(" 2/2 17.0.20+101 "), "{lines:#?}");
        assert_eq!(
            lines[0].find("21.0.9+10"),
            lines[1].find("17.0.20+101"),
            "the versions line up: {lines:#?}"
        );

        ui.start_install();
        settle();
        assert!(screen.visible()[0].ends_with("installing"));

        ui.unpacked();
        settle();
        let lines = screen.visible();
        assert_eq!(lines.len(), 1, "{lines:#?}");
        assert!(lines[0].contains("17.0.20+101"), "{lines:#?}");

        ui.finish();
        assert_eq!(screen.visible(), Vec::<String>::new());
    }

    /// A finished download's line stays, full and still, until the next one
    /// begins - however fast its last bytes came.
    #[test]
    fn a_finished_download_stands_full_with_its_spinner_stopped() {
        let (ui, screen) = on_screen(&[("21.0.9+10", 100 * MIB), ("17.0.20+101", 100 * MIB)]);
        ui.begin(request("21"), "21.0.9+10", 100 * MIB);
        ui.start_download(100 * MIB);
        settle();
        // Faster than indicatif draws, as a download's reads are.
        for chunk in 1..1000 {
            ui.set_downloaded(chunk * 100 * MIB / 1000);
        }
        ui.downloaded();

        let full = screen.visible();
        assert!(full[0].ends_with("100%  100/100 MiB"), "{full:#?}");
        settle();
        assert_eq!(screen.visible(), full, "the line still moves");
    }

    /// A failed download leaves nothing of the region behind: its error
    /// line is all that stays.
    #[test]
    fn a_failed_run_leaves_only_its_error_line() {
        let (ui, screen) = on_screen(&[("21.0.9+10", 100 * MIB)]);
        ui.begin(request("21"), "21.0.9+10", 100 * MIB);
        ui.start_download(100 * MIB);
        ui.set_downloaded(50 * MIB);
        settle();
        ui.download_failed();
        ui.failed(request("21"), "could not install JDK: checksum mismatch");
        ui.finish();

        assert_eq!(
            screen.visible(),
            ["Error: 21: could not install JDK: checksum mismatch"]
        );
    }

    /// The line switches from one download to the next in one frame: no
    /// frame shows the previous version over the next one's size, which a
    /// `+` line printed from the other stage would otherwise make visible.
    #[test]
    fn the_download_line_switches_in_one_frame() {
        let (ui, screen) = on_screen(&[("21.0.9+10", 200 * MIB), ("17.0.20+101", 100 * MIB)]);
        ui.begin(request("21"), "21.0.9+10", 200 * MIB);
        ui.start_download(200 * MIB);
        ui.set_downloaded(200 * MIB);
        ui.downloaded();
        settle();
        screen.take_frames();

        ui.begin(request("17"), "17.0.20+101", 100 * MIB);
        settle();

        let frames = screen.take_frames();
        assert!(!frames.is_empty());
        for line in frames.iter().flatten().filter(|line| line.contains("MiB")) {
            assert_eq!(
                line.contains("21.0.9+10"),
                line.contains("/200 MiB"),
                "a half-switched line: {line}"
            );
        }
        assert!(screen.visible()[0].contains("2/2 17.0.20+101"));
    }

    /// The download line fits 80 columns: the bar gives way to a long
    /// counter, version and size, and never vanishes.
    #[test]
    fn the_bar_gives_way_to_fit_80_columns() {
        // `10/12 21.0.12+101.0.LTS`, 230 MiB: the common case keeps the full bar.
        assert_eq!(bar_width(23, 3), MAX_BAR_WIDTH);
        let width = bar_width(40, 4);
        assert!(width < MAX_BAR_WIDTH);
        assert_eq!(2 + 40 + 2 + width + 2 + 4 + 2 + 13, LINE_WIDTH);
        assert_eq!(bar_width(70, 5), MIN_BAR_WIDTH);
    }

    /// Heavy for done, light for the rest, always the same width - and an
    /// unknown total (no size, no response yet) is an empty bar, not a
    /// division by zero.
    #[test]
    fn the_bar_keeps_its_width_from_empty_to_full() {
        plain();
        for (pos, len) in [
            (0, 0),
            (0, 100),
            (50, 100),
            (99, 100),
            (100, 100),
            (120, 100),
        ] {
            assert_eq!(fill(pos, len, 22).chars().count(), 22, "{pos}/{len}");
        }
        assert_eq!(fill(100, 100, 22), "\u{2501}".repeat(22));
        assert!(fill(50, 100, 22).starts_with(&"\u{2501}".repeat(11)));
        assert_eq!(percent(1, 0), 0);
    }

    // -- unsourced_env_hint --

    /// Both commands the hint hands over carry the version the user asked
    /// for, and without one they resolve it the way env would have.
    #[test]
    fn unsourced_env_hint_names_both_alternatives() {
        let hint = unsourced_env_hint(Some("21"), false);
        assert!(hint.contains("'jlo exec 21 -- <command>'"), "{hint}");
        assert!(
            hint.contains("'export JAVA_HOME=\"$(jlo home 21)\"'"),
            "{hint}"
        );
        let bare = unsourced_env_hint(None, false);
        assert!(bare.contains("'jlo exec -- <command>'"), "{bare}");
        assert!(bare.contains("\"$(jlo home)\""), "{bare}");
    }

    // -- provenance_line --
    //
    // The six states `jlo current` distinguishes. Pure formatting: no store,
    // no config, no environment. Cases 1 and 6 never reach a formatter - they
    // have no answer to print - so they are covered by the integration
    // suite's exit codes instead.

    fn active(version: &str, major: i64) -> Active {
        Active {
            path: PathBuf::from("/jdks").join(version),
            version: Some(version.to_string()),
            request: Some(request(&major.to_string())),
            source: None,
            pinned_elsewhere: None,
        }
    }

    fn pinned(version: &str, file: &str) -> Resolved {
        Resolved {
            request: request(version),
            source: Source::ProjectConfig(PathBuf::from(file)),
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
            source: Some(Source::Foreign),
            pinned_elsewhere: None,
        };
        assert_eq!(
            provenance_line(&a),
            "/opt/jdk-21  ($JAVA_HOME, set outside jlo)"
        );
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
        let home = Path::new("/Users/jane");
        let path = home.join("Library/Java/JavaVirtualMachines/21.0.8");
        assert_eq!(
            tilde(&path, Some(home)),
            "~/Library/Java/JavaVirtualMachines/21.0.8"
        );
    }

    #[test]
    fn tilde_leaves_paths_outside_home_alone() {
        let path = Path::new("/opt/jdks/21.0.8");
        assert_eq!(
            tilde(path, Some(Path::new("/Users/jane"))),
            "/opt/jdks/21.0.8"
        );
        assert_eq!(tilde(path, None), "/opt/jdks/21.0.8");
    }

    /// The exact version is the one fact a landed line exists to record; the
    /// major is what the user typed.
    #[test]
    fn a_landed_line_names_the_exact_version() {
        plain();
        let columns = Columns::fit([request("21")], ["21.0.8+9"]);
        assert_eq!(
            added_line(columns, request("21"), "21.0.8+9", Duration::from_secs(18)),
            "+ 21  21.0.8+9  18s"
        );
    }

    fn remote(version: &str, major: i64) -> RemoteJdk {
        RemoteJdk {
            version: version.to_string(),
            request: Request {
                major,
                stream: Stream::Ga,
            },
        }
    }

    fn remote_ea(version: &str, major: i64) -> RemoteJdk {
        RemoteJdk {
            version: version.to_string(),
            request: Request {
                major,
                stream: Stream::Ea,
            },
        }
    }

    fn local(version: &str, major: i64) -> InstalledJdk {
        InstalledJdk {
            version: version.to_string(),
            request: Request {
                major,
                stream: Stream::Ga,
            },
        }
    }

    fn local_ea(version: &str, major: i64) -> InstalledJdk {
        InstalledJdk {
            version: version.to_string(),
            request: Request {
                major,
                stream: Stream::Ea,
            },
        }
    }

    /// A name with nothing known about it yet; the builders below fill it in.
    fn name(text: &str) -> NameRow {
        NameRow {
            name: request(text),
            installed: None,
            latest: None,
            update: false,
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

    // -- build_rows --

    /// The rows `jlo list` prints for `installed`, grouped as `cmd_list`
    /// groups them.
    fn listed(
        available: &[RemoteJdk],
        installed: &[InstalledJdk],
        active_version: Option<&str>,
    ) -> Vec<NameRow> {
        build_rows(available, &group_by_name(installed), active_version)
    }

    #[test]
    fn a_name_not_installed_shows_only_the_latest_build() {
        let rows = listed(&[remote("26.0.2+101", 26)], &[], None);
        assert_eq!(rows, vec![name("26").latest("26.0.2+101")]);
    }

    /// Current means nothing to add: LATEST would only repeat INSTALLED.
    #[test]
    fn a_current_install_shows_no_latest_build() {
        let rows = listed(
            &[remote("21.0.12+101.0.LTS", 21)],
            &[local("21.0.12+101.0.LTS", 21)],
            None,
        );
        assert_eq!(rows, vec![name("21").installed("21.0.12+101.0.LTS")]);
    }

    /// The installed build and the offered one are one row now, not two: both
    /// answer to `21`, and one build per name is what every verb keeps.
    #[test]
    fn an_outdated_install_shares_its_row_with_the_update() {
        let rows = listed(
            &[remote("21.0.12+101.0.LTS", 21)],
            &[local("21.0.11+10.0.LTS", 21)],
            None,
        );
        assert_eq!(
            rows,
            vec![
                name("21")
                    .installed("21.0.11+10.0.LTS")
                    .latest("21.0.12+101.0.LTS")
                    .update()
            ]
        );
    }

    /// The weekly case: an older build of the stream is installed, and the
    /// row is named the way `jlo update` takes it.
    #[test]
    fn an_outdated_pre_release_stream_is_one_row_named_as_typed() {
        let rows = listed(
            &[remote_ea("28.0.0-beta+16.0.ea", 28)],
            &[local_ea("28.0.0-beta+14.0.ea", 28)],
            None,
        );
        assert_eq!(
            rows,
            vec![
                name("28-ea")
                    .installed("28.0.0-beta+14.0.ea")
                    .latest("28.0.0-beta+16.0.ea")
                    .update()
            ]
        );
    }

    #[test]
    fn an_offered_pre_release_that_is_installed_is_one_row() {
        let rows = listed(
            &[remote_ea("28.0.0-beta+16.0.ea", 28)],
            &[local_ea("28.0.0-beta+16.0.ea", 28)],
            None,
        );
        assert_eq!(rows, vec![name("28-ea").installed("28.0.0-beta+16.0.ea")]);
    }

    /// A `27-ea` left over from before 27 shipped: two rows, the name anyone
    /// reaches for now first, the pre-release after it.
    #[test]
    fn a_pre_release_left_beside_its_release_follows_it() {
        let rows = listed(
            &[remote("27.0.0+35", 27)],
            &[local_ea("27.0.0-beta+30.0.ea", 27), local("27.0.0+35", 27)],
            None,
        );
        assert_eq!(
            rows,
            vec![
                name("27").installed("27.0.0+35"),
                name("27-ea").installed("27.0.0-beta+30.0.ea"),
            ]
        );
    }

    /// A catalogue behind the store - Adoptium rolled back, or the install
    /// came from elsewhere - still shows what it offers, since that differs
    /// from what is installed; it is just not called an update.
    #[test]
    fn the_catalogue_behind_the_store_is_shown_but_not_as_an_update() {
        let rows = listed(
            &[remote("21.0.11+10.0.LTS", 21)],
            &[local("21.0.12+101.0.LTS", 21)],
            None,
        );
        assert_eq!(
            rows,
            vec![
                name("21")
                    .installed("21.0.12+101.0.LTS")
                    .latest("21.0.11+10.0.LTS")
            ]
        );
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

        let rows = listed(&[], &installed, Some("17.0.20+101"));
        assert!(rows[1].active, "{rows:?}");
        assert!(!rows[0].active, "{rows:?}");

        let rows = listed(&[], &installed, Some("21.0.9+10.0.LTS"));
        assert!(!rows[0].active, "{rows:?}");
        assert!(rows[0].others[0].active, "{rows:?}");
    }

    /// An offered release must not be called an update to a pre-release
    /// install: following it would change streams. The release still comes
    /// first - it is the name the major is now known by.
    #[test]
    fn a_ga_release_is_not_an_update_to_an_installed_pre_release() {
        let rows = listed(
            &[remote("28.0.1+9", 28)],
            &[local_ea("28.0.0-beta+16.0.ea", 28)],
            None,
        );
        assert_eq!(
            rows,
            vec![
                name("28").latest("28.0.1+9"),
                name("28-ea").installed("28.0.0-beta+16.0.ea"),
            ]
        );
    }

    /// The mirror image: a pre-release of a later patch sorts above the current
    /// release, so without the per-name rule the offered beta would read as an
    /// update to the installed release.
    #[test]
    fn an_offered_pre_release_is_not_an_update_to_an_installed_release() {
        let rows = listed(
            &[remote_ea("26.0.2-beta+101.0.ea", 26)],
            &[local("26.0.1+9", 26)],
            None,
        );
        assert_eq!(
            rows,
            vec![
                name("26").installed("26.0.1+9"),
                name("26-ea").latest("26.0.2-beta+101.0.ea"),
            ]
        );
    }

    /// `--offline` has no catalogue: the same rows, with nothing to put under
    /// LATEST and nothing to call an update.
    #[test]
    fn offline_rows_are_the_same_rows_without_a_latest_build() {
        let rows = listed(
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
                name("28-ea").installed("28.0.0-beta+14.0.ea"),
                name("21")
                    .installed("21.0.11+10.0.LTS")
                    .other("21.0.9+10.0.LTS", Status::Superseded),
            ]
        );
    }

    // -- render_rows --

    /// Pins `console`'s colour off for the rows under test, which would
    /// otherwise follow `CLICOLOR_FORCE`, `NO_COLOR` and `TERM` in the
    /// environment of whoever runs the suite. The switch is process-wide;
    /// that is safe only because no test here wants colour on.
    fn plain() {
        console::set_colors_enabled(false);
        console::set_colors_enabled_stderr(false);
    }

    #[test]
    fn render_rows_puts_installed_names_first_and_names_each_state() {
        plain();
        let mut outdated = name("21")
            .installed("21.0.11+10.0.LTS")
            .latest("21.0.12+101.0.LTS")
            .update()
            .other("21.0.9+10.0.LTS", Status::Superseded);
        outdated.others[0].active = true;

        let listing = render_rows(&[
            name("28-ea")
                .installed("28.0.0-beta+14.0.ea")
                .latest("28.0.0-beta+16.0.ea")
                .update(),
            name("27").installed("27.0.0+35").active(),
            name("27-ea").installed("27.0.0-beta+30.0.ea"),
            name("26").latest("26.0.2+101"),
            outdated,
            name("20").latest("20.0.2+9"),
        ]);

        assert_eq!(
            listing.installed,
            vec![
                "  28-ea  28.0.0-beta+14.0.ea  update \u{2192} 28.0.0-beta+16.0.ea",
                "  27     27.0.0+35            active",
                "  27-ea  27.0.0-beta+30.0.ea",
                "  21     21.0.11+10.0.LTS     update \u{2192} 21.0.12+101.0.LTS",
                "         21.0.9+10.0.LTS      active  superseded",
            ]
        );
        assert_eq!(listing.available.as_deref(), Some("  available:    26  20"));
    }

    /// A catalogue sitting behind the store is shown, but as an offer rather
    /// than an update, and a superseded build says so in words.
    #[test]
    fn render_rows_says_offered_for_an_older_build_on_offer() {
        plain();
        let listing = render_rows(&[name("21")
            .installed("21.0.12+101.0.LTS")
            .latest("21.0.11+10.0.LTS")
            .other("21.0.9+10.0.LTS", Status::Superseded)]);
        assert_eq!(
            listing.installed,
            vec![
                "  21  21.0.12+101.0.LTS  21.0.11+10.0.LTS offered",
                "      21.0.9+10.0.LTS    superseded",
            ]
        );
        assert_eq!(listing.available, None);
    }

    // -- hints --

    /// The command a hint quotes is what the eye should land on; prose
    /// around it, the quotes and a brand apostrophe stay prose.
    #[test]
    fn a_hint_marks_only_its_quoted_commands() {
        assert_eq!(
            quoted_spans("J'Lo 0.9.0 is available. Run 'jlo selfupdate' to install it."),
            vec![
                (false, "J'Lo 0.9.0 is available. Run '"),
                (true, "jlo selfupdate"),
                (false, "' to install it."),
            ]
        );
        assert_eq!(
            quoted_spans("e.g. 'jlo env 21', then run it again."),
            vec![
                (false, "e.g. '"),
                (true, "jlo env 21"),
                (false, "', then run it again."),
            ]
        );
        assert_eq!(
            quoted_spans("run 'jlo update' (2 outdated)"),
            vec![
                (false, "run '"),
                (true, "jlo update"),
                (false, "' (2 outdated)")
            ]
        );
        assert_eq!(
            quoted_spans("no quotes here"),
            vec![(false, "no quotes here")]
        );
        // Any whitespace ends a span, a tab or a non-breaking space too.
        assert_eq!(
            quoted_spans("Run 'jlo env 21'\tthen\u{a0}'jlo update'."),
            vec![
                (false, "Run '"),
                (true, "jlo env 21"),
                (false, "'\tthen\u{a0}'"),
                (true, "jlo update"),
                (false, "'."),
            ]
        );
    }

    // -- tip_line --

    #[test]
    fn tip_line_is_silent_when_everything_is_current() {
        plain();
        let rows = vec![name("21").installed("21.0.12+101.0.LTS")];
        assert_eq!(tip_line(&rows), None);
    }

    #[test]
    fn tip_line_joins_both_offers_on_one_line() {
        plain();
        // One line whatever applies: a listing that ends in a stack of
        // advice reads as nagging, and this one prints on every `jlo list`.
        let rows = vec![
            name("21")
                .installed("21.0.11+10.0.LTS")
                .latest("21.0.12+101.0.LTS")
                .update(),
            name("17")
                .installed("17.0.19+7")
                .latest("17.0.20+101")
                .update(),
            name("11")
                .installed("11.0.25+9")
                .other("11.0.24+8", Status::Superseded),
        ];
        assert_eq!(
            tip_line(&rows).as_deref(),
            Some("run 'jlo update' (2 outdated) \u{b7} 'jlo remove --superseded' (1 superseded)")
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
        plain();
        let rows = vec![
            name("29-ea")
                .installed("29.0.0-beta+1.0.ea")
                .latest("29.0.0-beta+3.0.ea")
                .update()
                .other("29.0.0-beta+0.0.ea", Status::Superseded),
            name("28").latest("28.0.1+3").update(),
            name("28-ea")
                .installed("28.0.0-beta+16.0.ea")
                .other("28.0.0-beta+14.0.ea", Status::Superseded),
            name("21")
                .installed("21.0.9+10.0.LTS")
                .latest("21.0.12+101.0.LTS")
                .update()
                .other("21.0.8+9.0.LTS", Status::Superseded),
        ];
        assert_eq!(
            tip_line(&rows).as_deref(),
            Some("run 'jlo update' (3 outdated) \u{b7} 'jlo remove --superseded' (1 superseded)")
        );
    }

    #[test]
    fn tip_line_offers_only_what_applies() {
        plain();
        let rows = vec![
            name("21")
                .installed("21.0.11+10.0.LTS")
                .other("21.0.9+10.0.LTS", Status::Superseded),
        ];
        assert_eq!(
            tip_line(&rows).as_deref(),
            Some("run 'jlo remove --superseded' (1 superseded)")
        );
    }
}
