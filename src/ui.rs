use console::style;
use indicatif::{ProgressBar, ProgressBarIter, ProgressStyle};
use std::io::{IsTerminal, Read, stderr};
use std::path::Path;
use std::time::{Duration, Instant};

/// The live progress region for one JDK installation.
///
/// Download, extraction and the final move share a *single* progress line that
/// is erased once the install succeeds, leaving exactly one summary line behind.
///
/// The rule the whole module follows: output persists in proportion to how
/// durable the side effect is. A JDK on disk survives until `jlo clean`, so it
/// earns a line. Exporting `JAVA_HOME` lasts until the shell exits and is fully
/// implied by the command the user typed, so it prints nothing at all - which
/// matters because the autoload hook runs on every `cd`.
pub(crate) struct InstallUi {
    bar: ProgressBar,
    version: String,
    started: Instant,
    tty: bool,
}

impl std::fmt::Debug for InstallUi {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InstallUi")
            .field("version", &self.version)
            .field("tty", &self.tty)
            .finish_non_exhaustive()
    }
}

impl InstallUi {
    pub(crate) fn new(version: &str) -> Self {
        let tty = supports_live_region();
        let bar = if tty {
            // Styled via the builder, which does not draw: a bar configured
            // after construction flashes indicatif's default style on the first
            // redraw. "connecting" is literally true here - the caller has the
            // metadata but has not yet opened the download response.
            ProgressBar::new(0)
                .with_style(spinner_style())
                .with_prefix(version.to_string())
                .with_message("connecting")
        } else {
            // Without a terminal there is nothing to redraw over; the plain
            // phase lines below carry the progress instead.
            ProgressBar::hidden()
        };
        if tty {
            bar.enable_steady_tick(TICK);
        }
        Self::with_bar(version, bar, tty)
    }

    #[cfg(test)]
    pub(crate) fn hidden(version: &str) -> Self {
        Self::with_bar(version, ProgressBar::hidden(), false)
    }

    fn with_bar(version: &str, bar: ProgressBar, tty: bool) -> Self {
        Self {
            bar,
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
            self.bar.set_style(download_style());
        } else {
            eprintln!(
                "Downloading JDK {} ({})",
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
            self.bar.set_style(spinner_style());
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
            format_summary(&self.version, &tilde(dest), self.started.elapsed())
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

/// Report a `jlo clean` run.
///
/// `clean` is the one command that destroys things, so unlike `jlo env` it
/// always says what it did - including when the answer is "nothing".
pub(crate) fn clean_report(report: &crate::store::CleanReport) {
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
            "{} Nothing to clean {}",
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
fn download_style() -> ProgressStyle {
    ProgressStyle::default_bar()
        .template(
            "{spinner:.cyan} JDK {prefix}  [{bar:20.cyan/blue}]  {bytes}/{total_bytes}  {eta}",
        )
        .expect("progress bar template is a valid literal")
        .progress_chars("#>-")
}

fn spinner_style() -> ProgressStyle {
    ProgressStyle::default_spinner()
        .template("{spinner:.cyan} JDK {prefix}  {msg}")
        .expect("spinner template is a valid literal")
}

/// One line per durable side effect, e.g.
/// `✓ JDK 21.0.8+9 → ~/Library/Java/JavaVirtualMachines/21.0.8  (18s)`.
///
/// Every styled piece is `.for_stderr()`. `console::style` decides whether to
/// emit colour by looking at *stdout*, and the `jlo` shell function runs
/// `. <(jlo-bin env)` - so stdout is a pipe on the one path that matters and
/// the default targeting silently strips every colour from this line.
fn format_summary(version: &str, dest: &str, elapsed: Duration) -> String {
    format!(
        "{} JDK {} {} {}  {}",
        style("✓").green().for_stderr(),
        version,
        style("→").dim().for_stderr(),
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
        let line = format_summary("21.0.8+9", "~/.jdks/21.0.8", Duration::from_secs(18));
        assert!(line.contains("JDK 21.0.8+9"), "got: {line}");
        assert!(line.contains("~/.jdks/21.0.8"), "got: {line}");
        assert!(line.contains("(18s)"), "got: {line}");
        assert_eq!(line.lines().count(), 1, "got: {line}");
    }
}
