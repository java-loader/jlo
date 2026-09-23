//! The hidden install verb: J'Lo owns its own layout under `$JLO_HOME`.
//!
//! The shell code the user sources is not shipped any more. It lives in this
//! repo as real `.zsh`/`.bash` files, is compiled into the binary with
//! `include_str!` (the same mechanism `adoptium.rs` uses for API fixtures), and
//! is written to disk by the verb below. The binary is the shell code's
//! *transport*, not its author: there is one artifact to download and verify,
//! and the files on disk always match the binary that wrote them.
//!
//! The verb is hidden the way `sing` is - intercepted from raw argv before
//! `Cli::parse()`. `hide = true` would only drop it from `--help`:
//! `clap_complete` still emits hidden subcommands into generated completion
//! scripts, and clap's "did you mean" engine still offers it for typos.
//!
//! Publication is ordered: the generated scripts first, the receipt last, so
//! the receipt is the commit marker. `self_heal` below reads it and rewrites
//! the scripts when it names a different version than this binary, with no
//! network and no command for the user to learn. Every file goes down as a
//! temp file plus a `rename` *in its own directory*, so an interruption leaves
//! a stale script rather than a truncated one.
//!
//! What that covers is exactly one state, and the bound is worth stating: an
//! *upgrade* interrupted between the binary and the scripts. A first install
//! interrupted before its first receipt is indistinguishable from an install
//! that predates receipts, and neither is healed - re-running the installer is
//! the documented repair for both, and for anything else the receipt does not
//! speak to.

use crate::CommandError;
use crate::store::same_path;
use crate::ui;
use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use std::fs::{self, File};
use std::io::Write as _;
use std::path::{Path, PathBuf};

/// The argv token that selects this verb. Underscore-prefixed because it is
/// not part of the CLI contract: `install.sh`, `install-local.sh` and (from
/// 0.4.0) `selfupdate` call it, users do not.
pub(crate) const VERB: &str = "__install";

/// Where the install came from. Recorded in the receipt so a later
/// `selfupdate` can tell an install it owns from one a package manager placed.
const DEFAULT_METHOD: &str = "installer";

/// The published binary's file name, which the installers also give the file
/// they stage - so the staging sweep knows exactly which file was its own.
const BINARY_NAME: &str = "jlo-bin";

/// What `install.sh`, `install-local.sh` and `selfupdate` name the directory
/// they unpack into, beside the binary they are about to publish. Matched
/// rather than reconstructed: the pid in the real name belongs to whoever
/// staged it, usually a shell, not to us.
const STAGING_PREFIX: &str = ".jlo-install";

const VERSION: &str = env!("CARGO_PKG_VERSION");

// The shell sources. Verbatim - the wrappers carry no interpolation at all,
// which is what keeps them shellcheck-able files rather than templates. Only
// the three stubs below are generated with a path baked in.
//
// The jlo function is one text that parses under both shells, written out
// under each dialect's name. The cd hook differs only in how it registers
// itself, so each dialect is its own registration plus the common rest.
const INIT: &str = include_str!("../shell/jlo-init-common.sh");
const AUTOLOAD_ZSH: &str = concat!(
    include_str!("../shell/jlo-autoload.zsh"),
    include_str!("../shell/jlo-autoload-common.sh")
);
const AUTOLOAD_BASH: &str = concat!(
    include_str!("../shell/jlo-autoload.bash"),
    include_str!("../shell/jlo-autoload-common.sh")
);

/// Picks `bin/jlo-init.$_jlo_d` at source time.
///
/// Only the shell knows which shell it is: `ZSH_VERSION` and `BASH_VERSION`
/// are shell variables, not exported, so the binary that wrote the stub cannot
/// read them. The builtin test is not decoration - nothing stops a user from
/// exporting `ZSH_VERSION`, and a bash child then inherits it *and* sets
/// `BASH_VERSION` itself, so `${ZSH_VERSION:+zsh}${BASH_VERSION:+bash}` yields
/// `zshbash`, names a file that does not exist, and jlo silently fails to
/// initialise. `setopt` and `shopt` are builtins and cannot arrive through the
/// environment.
const DIALECT_DISPATCH: &str = "\
_jlo_d=
if [ -n \"${ZSH_VERSION-}\" ] && command -v setopt >/dev/null 2>&1; then
  _jlo_d=zsh
elif [ -n \"${BASH_VERSION-}\" ] && command -v shopt >/dev/null 2>&1; then
  _jlo_d=bash
fi
";

/// The last line of every stub. A sourced file's status is its last command's,
/// so the cleanup above it would otherwise report success for a load that
/// failed - and the `&&`-joined reload `selfupdate` prints has no other way to
/// tell. `eval` expands the status before `unset` runs: the variable goes, its
/// value does not.
///
/// A missing or unreadable target is tested for rather than handed to `.`,
/// which a POSIX-mode shell treats as fatal - but it still fails the stub.
/// Only "not applicable here" (another shell; autoload without the wrapper)
/// returns 0 having loaded nothing.
///
/// Except under `set -e` when a profile sources the stub: there a non-zero
/// return ends the login shell, which is worse than a J'Lo that did not load.
/// The reload says it is one with [`RELOAD_ARG`] and keeps its failure - zsh
/// still shows `e` in `$-` inside the wrapper's guarded `eval`, so errexit
/// alone cannot tell the two apart.
const RETURN_STATUS: &str = "\
case $- in *e*) [ \"${1-}\" = __jlo_reload ] || _jlo_rc=0 ;; esac
eval \"unset _jlo_rc; return $_jlo_rc\"
";

/// Passed by [`print_reload`] as the stub's `$1`. An argument to `.` rather
/// than a variable: bash and zsh scope it to the sourced file and restore the
/// caller's afterwards, so nothing is left behind to clean up. Spelled so no
/// user argument collides with it: a `.` without arguments inherits the
/// enclosing function's `$1`. Must match the literal in [`RETURN_STATUS`].
const RELOAD_ARG: &str = "__jlo_reload";

const GENERATED_HEADER: &str = "\
# Generated by J'Lo - edits are lost on the next install or 'jlo selfupdate'.
";

/// Set by the optional stubs once they have actually loaded, and read by
/// [`print_reload`] to decide what the post-update `eval` re-sources.
///
/// Plain shell variables, deliberately not exported: the question they answer
/// is "did *this* shell opt in?", and an exported one would leak the answer
/// into every child and back out of a subshell that set it.
const AUTOLOAD_MARKER: &str = "_JLO_AUTOLOAD";
const COMPLETIONS_MARKER: &str = "_JLO_COMPLETIONS";

/// The directories and files J'Lo owns under `$JLO_HOME`.
#[derive(Debug)]
pub(crate) struct Layout {
    home: PathBuf,
    bin: PathBuf,
    completions: PathBuf,
}

impl Layout {
    pub(crate) fn new(home: PathBuf) -> Self {
        Self {
            bin: home.join("bin"),
            completions: home.join("completions"),
            home,
        }
    }

    fn receipt(&self) -> PathBuf {
        self.home.join("install-receipt.json")
    }

    pub(crate) fn home(&self) -> &Path {
        &self.home
    }

    fn bin_dir(&self) -> &Path {
        &self.bin
    }

    /// Where `selfupdate` stages the download, under the name [`Staging`]
    /// sweeps once the staged binary has taken over. A *sibling* of the
    /// binary, because `rename` is only atomic within one filesystem and
    /// `bin/` can itself be a mount point or a symlink.
    ///
    /// The name is a contract across versions: the binary that stages is the
    /// old one, the binary that sweeps is the release it downloaded.
    pub(crate) fn staging_dir(&self, pid: u32) -> PathBuf {
        self.bin.join(format!("{STAGING_PREFIX}-{pid}"))
    }

    /// The binary's path, which is also the symlink target.
    ///
    /// The name `jlo-bin` is load-bearing: `~/.local/bin/jlo` is only ever
    /// refreshed when it already points at exactly this path, so renaming the
    /// file would leave every existing user's symlink dangling.
    pub(crate) fn binary(&self) -> PathBuf {
        self.bin.join(BINARY_NAME)
    }
}

/// `$JLO_HOME/install-receipt.json` - the uv/cargo-dist model.
///
/// Written last, so it is the commit marker for the whole publication. A
/// receipt whose `version` disagrees with the running binary is the
/// known-incomplete state; see [`self_heal`].
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct Receipt {
    pub(crate) version: String,
    pub(crate) method: String,
    pub(crate) jlo_home: String,
    pub(crate) binary: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    symlink: Option<String>,
}

// ---------------------------------------------------------------------------
// The publication lock
// ---------------------------------------------------------------------------

/// Where the lock file sits. Under `$JLO_HOME`, beside the layout it guards.
const LOCK_FILE: &str = ".selfupdate.lock";

/// An exclusive `flock` under `$JLO_HOME`, held for as long as somebody is
/// writing the layout.
///
/// It lives here rather than in `selfupdate` because *this* is the only code
/// that publishes: `install.sh`, `install-local.sh`, `selfupdate` and the
/// self-heal all end up in [`write_layout`] + [`write_receipt`], and a lock
/// only one of them takes guards nothing. The binary, the generated scripts
/// and the receipt are separate filesystem writes, and no `rename` makes them
/// one transaction.
#[derive(Debug)]
pub(crate) struct Lock {
    /// Never read: the lock lives on the open file description, so holding
    /// this alive *is* the whole behaviour. Underscore-prefixed so that stays
    /// legible rather than looking like an oversight.
    ///
    /// `None` for the lock inherited across `selfupdate`'s `exec`: the fd is
    /// open in this process, but no `File` here owns it.
    _file: Option<fs::File>,
    /// Unlinked on drop. The lock file is the one thing under `$JLO_HOME`
    /// with no purpose once the run that took it is over.
    path: PathBuf,
}

impl Lock {
    /// `None` when somebody else holds it. Fails only when the lock file
    /// itself cannot be opened or locked.
    pub(crate) fn try_acquire(home: &Path) -> Result<Option<Self>> {
        use std::os::unix::fs::MetadataExt as _;

        fs::create_dir_all(home).with_context(|| format!("could not create {home:?}"))?;
        let path = home.join(LOCK_FILE);

        // Bounded retry rather than a single attempt, because [`Drop`] below
        // unlinks the file: a miss is a holder that finished between our
        // `open` and our `flock`, and the next pass opens whatever is at the
        // path now - or creates it. Three is a count, not a timeout; nothing
        // here waits on anything.
        for _ in 0..3 {
            let file = fs::OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(false)
                .open(&path)
                .with_context(|| format!("could not open the update lock {path:?}"))?;

            // The lock lives on the open file description, not on the fd
            // number, so it survives `exec` - but only if the fd does. std
            // opens every file `O_CLOEXEC`, which would drop the lock at
            // exactly the moment `selfupdate` hands over: the staged binary
            // would then publish itself, its scripts and its receipt with
            // nothing holding anyone else off. Silently, with no error to
            // report.
            rustix::io::fcntl_setfd(&file, rustix::io::FdFlags::empty())
                .with_context(|| format!("could not keep {path:?} open across exec"))?;

            match rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockExclusive) {
                Ok(()) => {}
                // Non-blocking on purpose: an unbounded wait on a lock nobody
                // can see is worse than a message, and the self-heal below has
                // somewhere better to go than waiting.
                Err(rustix::io::Errno::WOULDBLOCK) => return Ok(None),
                Err(e) => return Err(anyhow!("could not lock {path:?}: {e}")),
            }

            // We hold a lock on an inode the previous holder already unlinked.
            // It guards nothing - the next process creates a fresh file at the
            // path and locks that instead, and the two publish side by side.
            // `nlink == 0` is precisely that state, and the answer is to open
            // the path again.
            if file.metadata().is_ok_and(|m| m.nlink() == 0) {
                continue;
            }

            return Ok(Some(Self {
                _file: Some(file),
                path,
            }));
        }
        Ok(None)
    }

    /// The lock this process inherited across `selfupdate`'s `exec`.
    ///
    /// The fd, and the `flock` on it, came across with the process image, so
    /// there is nothing to take here - and taking it again from a second open
    /// file description would deadlock us against ourselves. Dropping this
    /// still removes the file, which is what makes the `exec`ed half of an
    /// update tidy up after the half that could not.
    pub(crate) fn inherited(home: &Path) -> Self {
        Self {
            _file: None,
            path: home.join(LOCK_FILE),
        }
    }

    /// The same lock, where contention is an error rather than a fork in the
    /// road: two processes must not publish at once.
    pub(crate) fn acquire(home: &Path) -> Result<Self> {
        Self::try_acquire(home)?.ok_or_else(|| {
            anyhow!(
                "another J'Lo install or update is already running (lock file {:?}).",
                home.join(LOCK_FILE)
            )
        })
    }
}

/// Unlink the lock file at the end of the run.
///
/// Safe only because it happens *while the lock is still held*: a struct's
/// fields are dropped after its `Drop::drop` body, so `_file` - and the
/// `flock` on it - outlives this line. A process that opened the file before
/// the unlink and locked it after sees `nlink == 0` in [`Lock::try_acquire`]
/// and opens the path again, so it can never end up guarding an inode that
/// nobody else will ever reach.
impl Drop for Lock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

// ---------------------------------------------------------------------------
// The verb
// ---------------------------------------------------------------------------

/// Run the install verb. `args` is the raw argv *after* the verb token;
/// `wrapped` is whether the `jlo` shell function evaluates the reload lines.
pub(crate) fn cmd_install(args: &[String], wrapped: bool) -> Result<(), CommandError> {
    let mut method = DEFAULT_METHOD.to_string();
    let mut reload = false;
    let mut locked = false;
    let mut publish_self = false;
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "--method" => {
                method.clone_from(
                    rest.next()
                        .ok_or_else(|| anyhow!("--method needs a value"))?,
                );
            }
            "--reload" => reload = true,
            "--locked" => locked = true,
            "--publish-self" => publish_self = true,
            other => return Err(anyhow!("unknown option {other:?} for {VERB}").into()),
        }
    }

    let layout = Layout::new(crate::jlo_home_dir()?);
    // Armed before the lock, because a lock we do not get is the likeliest
    // reason this run ends early - and the installer that `exec`d us has no
    // line left to run, so nothing else can clear the directory we came out
    // of. Dropping it removes the directory on every path, including the
    // successful one, where the rename has already emptied it.
    let _staging = Staging::around(&layout, publish_self);
    // `selfupdate` `exec`s this verb while already holding the lock, and the
    // fd came across the exec with it. Taking it again from a second open file
    // description would deadlock against ourselves, so the caller says so.
    let _lock = if locked {
        Lock::inherited(layout.home())
    } else {
        Lock::acquire(layout.home())?
    };
    // Read before writing: the three re-install cases below turn on whether a
    // receipt was already there, and the new one is about to replace it.
    let had_receipt = read_receipt(&layout).is_some();

    if publish_self {
        // Nothing under `$JLO_HOME` has been written yet, for `install.sh`
        // and `selfupdate` alike: whatever was installed before is intact.
        publish_binary(&layout).map_err(|e| {
            CommandError::with_hint(
                e,
                "J'Lo was not changed; any existing install is still in place.",
            )
        })?;
    }
    write_layout(&layout)?;
    let symlink = ensure_symlink(&layout);
    write_receipt(&layout, &method, symlink.as_deref())?;

    report(&layout, had_receipt, symlink.as_deref());
    if reload {
        print_reload(&layout, wrapped)?;
    }
    Ok(())
}

/// Move the running executable to its published path.
///
/// The installers and `selfupdate` unpack into a staging directory *beside* the
/// destination and run the staged binary from there, so that this - the write that replaces
/// `bin/jlo-bin` - happens under the lock with every other part of the
/// publication, rather than before the lock exists, and only once the new
/// binary has proved it can start. An installer that wrote the binary itself
/// would be a second publisher standing outside the gate: it
/// could swap the executable out from under a `selfupdate` that holds the
/// lock, and on Linux it would hit `ETXTBSY` trying to overwrite a binary that
/// is still running.
///
/// Beside the destination, and not under `$TMPDIR`, because `rename` is atomic
/// only within one filesystem and fails outright across two.
///
/// On Unix the process keeps its inode across the rename, so from here on this
/// process *is* the published binary - which is what makes it safe for the
/// same run to go on and write the layout from its own `include_str!`
/// templates. There is no second `exec`.
///
/// A binary that is already at the published path - `install-local.sh`, a
/// re-run, the self-heal - is left alone rather than renamed onto itself.
fn publish_binary(layout: &Layout) -> Result<()> {
    let exe = std::env::current_exe().context("could not find the running J'Lo binary")?;
    let target = layout.binary();
    if same_path(&exe, &target) {
        return Ok(());
    }

    fs::create_dir_all(layout.bin_dir())
        .with_context(|| format!("could not create {:?}", layout.bin_dir()))?;
    // The staged file's *contents* first. The installers wrote it with `tar`
    // or `cp`, `selfupdate` with its own unpack, and none of them flushed it, so a crash just after the rename could
    // leave a durable directory entry - and a receipt vouching for it - in
    // front of a binary that is still only in the page cache. `sync_dir`
    // below flushes the entry, not the data behind it.
    File::open(&exe)
        .and_then(|handle| handle.sync_all())
        .with_context(|| format!("could not flush {exe:?} to disk"))?;
    fs::rename(&exe, &target)
        .with_context(|| format!("could not publish {exe:?} to {target:?}"))?;
    // The rename is only durable once the directory entry is, and the receipt
    // written at the end of this run vouches for the binary published here.
    sync_dir(layout.bin_dir());

    Ok(())
}

/// The installer's staging directory, swept when this value is dropped.
///
/// `install.sh` and `selfupdate` unpack into `bin/.jlo-install-<pid>/` and
/// `exec` the binary from there, so from that moment the staged process is the
/// only one that can still tidy up: a refused publish would otherwise leave a whole copy of J'Lo
/// in `bin/` for every failed install.
///
/// What it removes is the file it knows was staged, and then the directory
/// *only if that emptied it*. Never a recursive delete: this runs on a path
/// derived from `current_exe()`, and a wrong answer there must cost nothing
/// that belongs to somebody else.
#[derive(Debug)]
struct Staging(Option<PathBuf>);

impl Staging {
    /// `None` unless this executable is demonstrably a staged one.
    ///
    /// Three things have to hold, and the name is the weakest of them: the
    /// caller must have asked to publish, the executable must not already be
    /// the published binary - `install-local.sh`, a re-run, the self-heal -
    /// and the directory it sits in must be a child of *this* install's
    /// `bin/`. Without that last test a binary parked in any directory whose
    /// name happens to start with `.jlo-install` would be swept while the verb
    /// was pointed at an unrelated `$JLO_HOME`.
    fn around(layout: &Layout, publish_self: bool) -> Self {
        if !publish_self {
            return Self(None);
        }
        let Ok(exe) = std::env::current_exe() else {
            return Self(None);
        };
        if same_path(&exe, &layout.binary()) {
            return Self(None);
        }
        let dir = exe.parent().filter(|dir| {
            dir.file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with(STAGING_PREFIX))
                && dir
                    .parent()
                    .is_some_and(|up| same_path(up, layout.bin_dir()))
        });
        Self(dir.map(Path::to_path_buf))
    }
}

impl Drop for Staging {
    fn drop(&mut self) {
        let Some(dir) = self.0.take() else {
            return;
        };
        // Gone already on the happy path: publishing renamed it out.
        let _ = fs::remove_file(dir.join(BINARY_NAME));
        // Not `remove_dir_all`. Anything still in here was not put there by
        // this run, and keeping it costs an empty directory at worst.
        let _ = fs::remove_dir(&dir);
    }
}

/// The one thing this verb writes to **stdout** (ADR-0001): shell code for the
/// caller to `eval`, which is how the shell that ran `jlo selfupdate` gets the
/// freshly generated wrapper in place of its resident one.
///
/// Only `selfupdate` passes `--reload`. `install.sh` must not: its stdout is
/// not evaluated by anything, and the line would be noise there - the
/// activation block `report` prints is what a bootstrap needs.
///
/// The two optional lines re-source only what this shell had *already*
/// enabled, never more. The signal is the marker each stub sets after it
/// successfully loads: a plain (unexported) shell variable, which is exactly
/// the state the wrapper left behind in this shell and nothing a child process
/// or another session can fake into existence here.
///
/// Every optional line is a full `if`, not `[ ... ] && . ...`: a
/// `[ -n ... ]` that is simply false would fail the `&&`-joined payload and
/// turn a successful update into a non-zero `jlo selfupdate`.
fn print_reload(layout: &Layout, wrapped: bool) -> Result<()> {
    let jlo_sh = sq(&display(&layout.home.join("jlo.sh")));
    let autoload = sq(&display(&layout.home.join("autoload.sh")));
    let completions = sq(&display(&layout.home.join("completions.sh")));
    crate::shellenv::emit(
        &[
            format!(". {jlo_sh} {RELOAD_ARG}"),
            format!("if [ -n \"${{{AUTOLOAD_MARKER}-}}\" ]; then . {autoload} {RELOAD_ARG}; fi"),
            format!(
                "if [ -n \"${{{COMPLETIONS_MARKER}-}}\" ]; then . {completions} {RELOAD_ARG}; fi"
            ),
        ],
        wrapped,
    )
}

/// Everything under `$JLO_HOME` except the binary, the symlink and the receipt.
///
/// Shared with [`self_heal`], which is the whole reason there is no `--repair`
/// verb: the recovery path and the install path are the same code.
///
/// The two optional stubs and the completion scripts warn rather than fail,
/// and the receipt is still written afterwards. That is deliberate, and it is
/// the narrow thing the receipt does *not* promise: it marks which binary
/// published this layout, not that every optional convenience beside it
/// landed. Withholding it instead would overload the one state that already
/// has a meaning - a missing receipt is an install that predates receipts, not
/// a broken one - and would put every later invocation into a rewrite for as
/// long as the underlying write keeps failing. The user gets a warning naming
/// the file, and re-running the installer is the documented repair.
fn write_layout(layout: &Layout) -> Result<()> {
    fs::create_dir_all(&layout.bin)
        .with_context(|| format!("could not create {:?}", layout.bin))?;

    // The wrappers first: the stubs below are what point at them.
    for (name, body) in [
        ("jlo-init.zsh", INIT),
        ("jlo-init.bash", INIT),
        ("jlo-autoload.zsh", AUTOLOAD_ZSH),
        ("jlo-autoload.bash", AUTOLOAD_BASH),
    ] {
        write_atomic(&layout.bin.join(name), body.as_bytes())?;
    }

    // The two compatibility shims, at the exact paths every *released*
    // install.sh (0.2.0 and 0.3.0) pasted into the user's profile. Fatal for
    // the same reason the dialect files above are: for anyone still carrying
    // that block these are the only load points there are.
    //
    // Generated rather than left at their 0.3.0 contents: the entry files
    // landed after 0.3.0 was tagged, so *no* released version knows `jlo.sh`
    // exists, and a stale shim would leave every existing user loading the
    // old wrapper permanently - including its `curl | bash` selfupdate, which
    // never reaches the Rust command - with nothing about it looking broken.
    // `jlo-init.sh` gets the same body as `jlo.sh`; its baked `JLO_HOME` is
    // the truth rather than the `export` the old block puts above it, which
    // says `$HOME/.jlo` whatever directory the files actually went to. The
    // old block sources `jlo-autoload.sh` unconditionally, so re-pointing it
    // preserves exactly the hook those users already had.
    write_atomic(
        &layout.bin.join("jlo-init.sh"),
        init_stub(layout, &compat_note("jlo.sh", layout)).as_bytes(),
    )?;
    write_atomic(
        &layout.bin.join("jlo-autoload.sh"),
        autoload_stub(layout, &compat_note("autoload.sh", layout)).as_bytes(),
    )?;

    let mut partial = write_completions(layout);

    // The one file a user cannot skip: it defines the wrapper function and
    // exports the JLO_HOME the rest of the layout hangs off. A failure here
    // means the printed instructions would point at nothing, so it is fatal
    // where the two optional stubs are warnings.
    let jlo_sh = init_stub(
        layout,
        "\
# Source this from your shell profile. The line never changes: an upgrade
# regenerates the files it points at.
",
    );
    write_atomic(&layout.home.join("jlo.sh"), jlo_sh.as_bytes())
        .context("could not write jlo.sh; J'Lo cannot be loaded from your shell profile.")?;

    let autoload_sh = autoload_stub(
        layout,
        "\
# Optional: switches JDK on cd when a .jlorc is in scope. Source after jlo.sh.
",
    );
    if let Err(e) = write_atomic(&layout.home.join("autoload.sh"), autoload_sh.as_bytes()) {
        ui::warning!("{e:#} cd autoloading is unavailable.");
        partial = true;
    }
    if let Err(e) = write_atomic(
        &layout.home.join("completions.sh"),
        completions_sh(layout).as_bytes(),
    ) {
        ui::warning!("{e:#} Tab completion is unavailable.");
        partial = true;
    }
    // One recovery line for every optional file that did not land, however
    // many of them failed.
    if partial {
        ui::hint!("Re-run the installer once that path is writable to restore it.");
    }
    Ok(())
}

/// Completion scripts, generated from this binary's own clap tree.
///
/// Written at install time rather than loaded via `source <(jlo completions
/// bash)` so that shell startup costs no subprocess. A failure is a
/// convenience lost, not a broken install: it is warned about here, and
/// reported as `true` so the caller prints the one repair hint for every
/// optional file that did not land.
fn write_completions(layout: &Layout) -> bool {
    if let Err(e) = fs::create_dir_all(&layout.completions) {
        ui::warning!(
            "could not create {:?}: {e}. Shell completions are unavailable.",
            layout.completions
        );
        return true;
    }
    let mut partial = false;
    for (shell, name) in [
        (clap_complete::Shell::Bash, "jlo.bash"),
        (clap_complete::Shell::Zsh, "_jlo"),
    ] {
        let script = crate::cli::completion_script(shell);
        if let Err(e) = write_atomic(&layout.completions.join(name), &script) {
            ui::warning!("{e:#} {shell} completions are unavailable.");
            partial = true;
        }
    }
    partial
}

// ---------------------------------------------------------------------------
// Generated stubs
// ---------------------------------------------------------------------------

fn init_stub(layout: &Layout, note: &str) -> String {
    format!(
        "{GENERATED_HEADER}{note}export JLO_HOME={home}
{DIALECT_DISPATCH}\
_jlo_rc=0
if [ -n \"$_jlo_d\" ]; then
{load}
fi
unset _jlo_d
{RETURN_STATUS}",
        home = sq(&display(&layout.home)),
        load = guarded_source("\"$JLO_HOME/bin/jlo-init.$_jlo_d\"", None),
    )
}

/// Source `path` - a shell word, quoted by the caller - with its failure, or a
/// missing target, left in `_jlo_rc` for [`RETURN_STATUS`], and `marker` set
/// only once it has actually loaded. Indented for a block one level deep.
fn guarded_source(path: &str, marker: Option<&str>) -> String {
    let load = match marker {
        None => format!("    . {path} || _jlo_rc=$?\n"),
        Some(marker) => {
            format!("    if . {path}; then\n      {marker}=1\n    else\n      _jlo_rc=$?\n    fi\n")
        }
    };
    format!("  if [ -s {path} ] && [ -r {path} ]; then\n{load}  else\n    _jlo_rc=1\n  fi")
}

/// The comment the two shims carry: what they are, why they exist, and when
/// they go away. `replaced_by` is the entry file the modern one-line form
/// loads instead, spelled out so the file answers "and what do I do about it"
/// without the user leaving it.
fn compat_note(replaced_by: &str, layout: &Layout) -> String {
    let line = commented(
        &source_line(&sq(&display(&layout.home.join(replaced_by)))),
        "    ",
    );
    format!(
        "\
# Compatibility shim - not part of the layout, and removed in v1.0.0.
#
# J'Lo 0.2.0 and 0.3.0 (every release there has been) print a profile block
# that sources this path directly. Nothing in those profiles knows about the
# entry files, so without this shim an upgrade leaves them loading the old
# wrapper forever, including its 'curl | bash' selfupdate - and nothing about
# it looks broken.
#
# The one line that replaces the old block, and outlives this file:
#
{line}#
"
    )
}

/// Comment out `body`, one `#` per **physical** line.
///
/// The paths in these files come from the user's `JLO_HOME`, and a directory
/// name may contain a newline. A single leading `#` would then end at the
/// first one and leave the rest of the path standing as shell code with an
/// unmatched quote - a file that does not parse, sourced from a profile. The
/// rest of the layout is safe by construction because a path only ever appears
/// inside single quotes there; a comment is the one place that is not true.
fn commented(body: &str, indent: &str) -> String {
    let mut out = String::new();
    for line in body.lines() {
        out.push('#');
        out.push_str(indent);
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// The wrapper path is baked in rather than read from `$JLO_HOME`: this file
/// is opt-in and may be sourced on its own, by a profile that never ran
/// `jlo.sh` - and under `set -u` an unset `$JLO_HOME` would abort it.
///
/// Guarded on the 'jlo' *function* rather than on the directory, because that
/// is what `jlo_after_cd` calls: sourcing this without `jlo.sh` has to be inert,
/// not a stream of errors on every cd. `typeset -f` needs no subshell, and
/// under a shell that lacks it the guard fails closed - which is the right
/// answer there anyway.
fn autoload_stub(layout: &Layout, note: &str) -> String {
    format!(
        "{GENERATED_HEADER}{note}\
{DIALECT_DISPATCH}\
_jlo_rc=0
if [ -n \"$_jlo_d\" ] && typeset -f jlo >/dev/null 2>&1; then
  _jlo_f={prefix}\"$_jlo_d\"
{load}
  unset _jlo_f
fi
unset _jlo_d
{RETURN_STATUS}",
        prefix = sq(&display(&layout.bin.join("jlo-autoload."))),
        load = guarded_source("\"$_jlo_f\"", Some(AUTOLOAD_MARKER)),
    )
}

fn completions_sh(layout: &Layout) -> String {
    // The zsh half is wrapped in `eval` so this file still parses under a
    // POSIX sh: `(( ... ))` and an array assignment are syntax errors there,
    // and a profile that sources this unconditionally would die on the parse
    // before the dialect test ever ran.
    //
    // zsh autoloads from $fpath, so the directory goes there and `_jlo` is
    // read on the first Tab press rather than in every shell. bash has no
    // equivalent - `complete -F` needs the function to exist - so its half
    // stays eager.
    format!(
        "{GENERATED_HEADER}\
# Optional: tab completion for the 'jlo' command. Source after jlo.sh.
{DIALECT_DISPATCH}\
_jlo_rc=0
if [ \"$_jlo_d\" = bash ]; then
{load_bash}
fi
if [ \"$_jlo_d\" = zsh ]; then
  if [ -s {comp_zsh} ] && [ -r {comp_zsh} ]; then
    _jlo_comp_dir={comp_dir}
    if eval '
      # Must come before compinit: compinit scans $fpath once, so a user whose
      # framework (oh-my-zsh and friends) already ran it gets nothing from this
      # line alone - hence the elif below.
      fpath=(\"$_jlo_comp_dir\" $fpath)
      if (( ! $+functions[compdef] )); then
        autoload -Uz compinit && compinit -i
      elif (( ! $+functions[_jlo] )); then
        # compinit already ran, so register after the fact. The autoload is not
        # optional: \"compdef _jlo jlo\" on its own records the mapping without
        # making _jlo loadable, and completion comes up silently empty.
        autoload -Uz _jlo && compdef _jlo jlo
      fi
    '; then
      {COMPLETIONS_MARKER}=1
    else
      _jlo_rc=$?
    fi
    unset _jlo_comp_dir
  else
    _jlo_rc=1
  fi
fi
unset _jlo_d
{RETURN_STATUS}",
        load_bash = guarded_source(
            &sq(&display(&layout.completions.join("jlo.bash"))),
            Some(COMPLETIONS_MARKER)
        ),
        comp_zsh = sq(&display(&layout.completions.join("_jlo"))),
        comp_dir = sq(&display(&layout.completions)),
    )
}

// ---------------------------------------------------------------------------
// Symlink
// ---------------------------------------------------------------------------

/// Expose a real `jlo` on PATH for non-interactive shells (CI, scripts,
/// agents). The interactive shell function still shadows it and keeps handling
/// `env`/`use`, which must mutate the current shell.
///
/// An optional convenience, so any failure is a warning. We only ever create or
/// refresh a symlink that already points at our own binary; an unrelated file,
/// directory or symlink at that path is left untouched.
fn ensure_symlink(layout: &Layout) -> Option<PathBuf> {
    let local_bin = std::env::home_dir()?.join(".local").join("bin");
    let link = local_bin.join("jlo");
    let target = layout.binary();

    if link.symlink_metadata().is_ok() {
        // Already ours and already right: leave it alone. Not merely an
        // optimisation - removing and recreating a correct link opens a window
        // in which 'jlo' is missing from PATH, and one in which something else
        // can take the name between the two syscalls.
        if fs::read_link(&link).is_ok_and(|dest| dest == target) {
            return Some(link);
        }
        // The hint is a command the user pastes, so its paths need shell
        // quoting rather than the Debug quoting the rest of our diagnostics
        // use: a '$' or a backtick in the path would otherwise be expanded.
        ui::warning!("{link:?} already exists and is not managed by J'Lo; leaving it untouched.");
        ui::hint!(
            "To put 'jlo' on PATH yourself: ln -s {} {}",
            sq(&display(&target)),
            sq(&display(&link))
        );
        return None;
    }

    if let Err(e) =
        fs::create_dir_all(&local_bin).and_then(|()| std::os::unix::fs::symlink(&target, &link))
    {
        ui::warning!(
            "could not create {link:?}: {e}. 'jlo' may not be available in non-interactive shells."
        );
        return None;
    }
    Some(link)
}

// ---------------------------------------------------------------------------
// Receipt
// ---------------------------------------------------------------------------

/// The reading `selfupdate` needs, where the two failures are *not* the same
/// thing: a missing receipt is an install that predates them and may be
/// updated, a malformed one is an error that stops the update and names the
/// file, because guessing is how a package-manager install gets clobbered.
pub(crate) fn load_receipt(layout: &Layout) -> Result<Option<Receipt>> {
    let path = layout.receipt();
    let raw = match fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(anyhow!("could not read {path:?}: {e}.")),
    };
    serde_json::from_str(&raw)
        .map(Some)
        .map_err(|e| anyhow!("{path:?} is not a valid install receipt: {e}."))
}

/// `None` for both a missing and a malformed receipt.
///
/// The two are distinguished where it matters - `selfupdate` stops on a
/// malformed one rather than guessing - but neither is a reason for an
/// ordinary command to refuse to run.
fn read_receipt(layout: &Layout) -> Option<Receipt> {
    load_receipt(layout).ok().flatten()
}

fn write_receipt(layout: &Layout, method: &str, symlink: Option<&Path>) -> Result<()> {
    Receipt {
        version: VERSION.to_string(),
        method: method.to_string(),
        jlo_home: display(&layout.home),
        binary: display(&layout.binary()),
        symlink: symlink.map(display),
    }
    .write(layout)
}

impl Receipt {
    fn write(&self, layout: &Layout) -> Result<()> {
        let mut json = serde_json::to_string_pretty(self)
            .context("could not serialise the install receipt")?;
        json.push('\n');
        write_atomic(&layout.receipt(), json.as_bytes())
    }
}

// ---------------------------------------------------------------------------
// Self-healing
// ---------------------------------------------------------------------------

/// Rewrite the generated files when the receipt disagrees with this binary.
///
/// That disagreement is the known-incomplete state: the binary landed, the
/// scripts or the receipt did not. Rather than report it and hand the user a
/// `--repair` command to type, any invocation clears it - a few idempotent
/// writes, no network.
///
/// Three guards keep this from running where it would be wrong. A missing
/// receipt is an install that predates them, not a broken one. A receipt
/// naming a *different* binary means this executable is not the install it
/// describes - a build in `target/release`, say. And a receipt describing a
/// *different* `$JLO_HOME` than the one we resolved means the two do not
/// belong together however the binary path lines up, so a copied install
/// cannot rewrite the original's scripts.
pub(crate) fn self_heal() {
    let Ok(home) = crate::jlo_home_dir() else {
        return;
    };
    let layout = Layout::new(home);
    // Cheap first, so the common case - a receipt that already agrees - costs
    // a single read and never touches the lock.
    if stale_receipt(&layout).is_none() {
        return;
    }

    // Somebody else is publishing. They will leave a consistent layout behind,
    // so there is nothing here worth waiting - or racing - for.
    let Ok(Some(_lock)) = Lock::try_acquire(&layout.home) else {
        return;
    };

    // Asked again, now that nobody else can be writing. Between the check
    // above and this line a publisher can have finished a *newer* version in
    // full and released the lock; the receipt then names a version this binary
    // does not have, which reads exactly like the incomplete upgrade this
    // function repairs. Healing it would write this binary's older scripts and
    // stamp its own version over the newer one - the heal causing the
    // disagreement it exists to clear.
    let Some(receipt) = stale_receipt(&layout) else {
        return;
    };

    if let Err(e) = write_layout(&layout).and_then(|()| {
        write_receipt(
            &layout,
            &receipt.method,
            receipt.symlink.as_deref().map(Path::new),
        )
    }) {
        ui::warning!("could not refresh the generated shell files: {e:#}");
        ui::hint!("Re-run the installer to repair this install.");
    }
}

/// The receipt this binary would heal, or `None` when there is nothing to do.
///
/// Separated out because the answer has to be obtained twice - see the call
/// sites in [`self_heal`] - and two spellings of it would be two chances to
/// diverge.
fn stale_receipt(layout: &Layout) -> Option<Receipt> {
    let receipt = read_receipt(layout)?;
    // Older than this binary, not merely different. The receipt is written
    // last, so an interrupted upgrade leaves one describing the version that
    // came *before* the binary now running - that is the state worth
    // repairing. A receipt that is newer means the opposite: a publication
    // finished while this executable was already running, and this process is
    // the previous binary rather than the published one. Rewriting the layout
    // then downgrades it, which is what the two path-based guards below cannot
    // detect, because a new binary is published at the very path the old one
    // was running from.
    //
    // A version neither side can parse is not a guess worth making.
    //
    // The bound this draws, stated because it is a real one: a publication
    // that moved *downwards* - an older build put in place over a newer
    // receipt, then cut off before the receipt was rewritten - is not healed
    // either. From the version alone it looks exactly like the case above,
    // because both leave a receipt newer than the running binary.
    //
    // They could be told apart, and the means is not exotic: under the lock,
    // run `layout.binary() --version` and see whether the published file is
    // this build or another one - the same probe `selfupdate` already performs
    // on a staged binary. It is left out on purpose. That puts a subprocess
    // spawn in a path every single command passes through, and the child's own
    // self-heal is kept from recursing only by the lock this process is
    // holding, which is a coupling that would have to be maintained as
    // carefully as the lock itself. The states below are not worth it.
    //
    // How they are reached: not by anything that moves on its own.
    // `selfupdate` refuses to put a name on an older build, and `install.sh`
    // defaults to the latest release - though it compares no versions, so an
    // older one it is *pointed* at (a pinned JLO_INSTALL_BASE_URL, a cached
    // installer, a local build) publishes without complaint. Re-running the
    // installer repairs it, as it already does for every other state the
    // receipt does not speak to.
    if std::cmp::Ordering::Less != crate::version::compare(&receipt.version, VERSION).ok()? {
        return None;
    }
    if !same_path(Path::new(&receipt.jlo_home), &layout.home) {
        return None;
    }
    if !is_current_exe(Path::new(&receipt.binary)) {
        return None;
    }
    Some(receipt)
}

pub(crate) fn is_current_exe(path: &Path) -> bool {
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    same_path(&exe, path)
}

// ---------------------------------------------------------------------------
// Reporting
// ---------------------------------------------------------------------------

/// What the installer prints. Runnable commands, not prose to act on: one line
/// makes jlo permanent, the next makes it effective in the shell the user is
/// sitting in.
///
/// The installer cannot do that second part itself - it runs in a subshell
/// under `curl | bash` and cannot mutate its parent (ADR-0001, the same reason
/// `jlo env` exists at all). That the *user* runs it is what keeps this
/// non-invasive rather than merely convenient: nothing writes to their profile
/// but them.
///
/// Everything here goes to stderr. stdout is the environment channel, and from
/// 0.4.0 `selfupdate` prints a `. jlo.sh` line there for the wrapper to eval.
fn report(layout: &Layout, had_receipt: bool, symlink: Option<&Path>) {
    let home_dir = std::env::home_dir();
    let profile = home_dir.as_deref().map(login_shell_profile);
    // Read once: the profile answers two questions now, and reading it twice
    // could answer them from two different versions of the file.
    let body = profile
        .as_deref()
        .and_then(|p| fs::read_to_string(p).ok())
        .unwrap_or_default();
    let activated = sources_actively(&body, "jlo.sh");
    let legacy = home_dir
        .as_deref()
        .zip(profile.as_deref())
        .and_then(|(home, login)| find_legacy_block(home, login));

    ui::created!(
        "{} installed to {}.",
        ui::jlo_mark(VERSION),
        tilde(&layout.home, home_dir.as_deref())
    );

    // The old block is worth naming wherever it is - it is the one thing a
    // v1.0.0 upgrade will break - so the notice is printed on its own terms,
    // before the activation cases below and independent of the receipt.
    if let Some((found_in, block)) = &legacy {
        legacy_notice(layout, found_in, home_dir.as_deref(), *block);
    }

    // What the notice must *not* do is take the place of the activation
    // instructions. It only does that when the block is in the very file the
    // login shell reads, because only then is J'Lo actually loaded. A stale
    // block in a file this shell never opens would otherwise send someone
    // away with no working install and an edit to make in the wrong file -
    // the one failure mode this whole check exists to avoid.
    let loaded_by_the_login_shell = legacy
        .as_ref()
        .is_some_and(|(found_in, _)| profile.as_deref() == Some(found_in.as_path()));

    if loaded_by_the_login_shell || (had_receipt && activated) {
        path_nudge(symlink);
        return;
    }

    let Some(profile) = profile else {
        // No home directory: there is no profile to name and no portable line
        // to print. The absolute paths in the layout still work.
        ui::hint!(
            "Source {:?} from your shell profile.",
            layout.home.join("jlo.sh")
        );
        return;
    };
    let target = profile_target(&profile, home_dir.as_deref());

    if had_receipt {
        eprintln!();
        ui::warning!(
            "{} does not source jlo.sh - it looks like the line below was never added.",
            tilde(&profile, home_dir.as_deref())
        );
    }

    // Column 0, not indented. These lines exist to be copied, and every
    // terminal's double-click and shift-select take a leading indent with
    // them - the block a user meets on first contact was the least
    // copy-pasteable output the program produced. Structure is carried by the
    // blank line and the heading above each group instead.
    let main = snippet(layout, home_dir.as_deref(), "jlo.sh");
    eprintln!(
        "\n{}\n",
        ui::heading("To activate — run this, then the line below it:")
    );
    for line in heredoc(layout, home_dir.as_deref(), &target) {
        eprintln!("{}", ui::command(&line));
    }
    eprintln!(
        "\n{}",
        ui::footnote(
            "The last two lines are optional: switch JDK on cd, and tab completion.\n\
             Delete them before pasting if you do not want them."
        )
    );
    eprintln!("\n{}\n", ui::heading("Then load it into this shell:"));
    eprintln!("{}", ui::command(&format!(". {main}")));
    eprintln!(
        "\n{}",
        ui::footnote("These lines never change: upgrades regenerate the files they point at.")
    );
    path_nudge(symlink);
}

/// Only nudge about PATH when the symlink was actually created and
/// `~/.local/bin` is not already on PATH - usually the case on macOS, rarely
/// on Linux.
fn path_nudge(symlink: Option<&Path>) {
    let Some(link) = symlink else { return };
    let Some(dir) = link.parent() else { return };
    let on_path = std::env::var_os("PATH")
        .is_some_and(|path| std::env::split_paths(&path).any(|entry| entry == dir));
    if on_path {
        return;
    }
    // The prose is dim like the heading under it: in this block everything
    // except the command is scaffolding, and a paragraph louder than its own
    // heading reads as the point when it is not.
    eprintln!(
        "\n{}\n",
        ui::footnote(&format!(
            "'jlo' also wants {} on PATH - for non-interactive shells\n\
             (CI, scripts, AI agents) and for 'jlo home'.",
            tilde(dir, std::env::home_dir().as_deref())
        ))
    );
    eprintln!("{}\n", ui::heading("Add it to your PATH:"));
    eprintln!("{}", ui::command("export PATH=\"$HOME/.local/bin:$PATH\""));
}

/// The profile the *login* shell reads.
///
/// `$SHELL` is the right signal here - unlike in the dialect dispatch, where
/// the *running* shell is what matters and `$SHELL` would be wrong. The trap
/// is bash: macOS's Terminal.app starts login shells, which read
/// `.bash_profile` and never `.bashrc`.
///
/// A wrong guess is harmless by construction: the path stays visible in the
/// printed command, so correcting it is a one-word edit rather than a line
/// silently appended to the wrong file.
fn login_shell_profile(home: &Path) -> PathBuf {
    let shell = std::env::var("SHELL").unwrap_or_default();
    let name = Path::new(&shell)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    match name.as_str() {
        "zsh" => home.join(".zshrc"),
        "bash" if cfg!(target_os = "macos") => home.join(".bash_profile"),
        "bash" => home.join(".bashrc"),
        // Neither, or no $SHELL at all: ~/.profile is what a POSIX login shell
        // reads.
        _ => home.join(".profile"),
    }
}

/// Fail open: an unreadable or missing profile counts as "not activated", so
/// the worst case is a duplicated line rather than a silent non-install.
///
/// Commented-out mentions do not count, whether the `#` opens the line or
/// trails a command on it. That is not pedantry: the line the installer prints
/// is exactly the one a user comments out to turn J'Lo off, and counting it is
/// the one way this check can fail *closed* - withholding the instructions
/// from someone whose shell is not in fact set up.
///
/// The comment stripping is deliberately crude - a `#` at the start of a line
/// or after whitespace - rather than a shell tokeniser. It errs toward
/// stripping too much, which is the harmless direction: over-stripping shows
/// the instructions to someone who did not need them, and the same is true of
/// the other way this can be wrong, a line that lives in `~/.zprofile` or in a
/// file sourced from the candidate. Both cost one duplicated line.
fn sources_actively(body: &str, needle: &str) -> bool {
    body.lines().any(|line| {
        let code = match line
            .char_indices()
            .find(|&(i, c)| c == '#' && (i == 0 || line[..i].ends_with(char::is_whitespace)))
        {
            Some((i, _)) => &line[..i],
            None => line,
        };
        code.contains(needle)
    })
}

/// Which parts of the pre-0.4.0 profile block this profile still carries.
///
/// The shims make that block keep working, so this is not a failure to
/// report. It is the one moment J'Lo gets to name the form that replaces it,
/// while the shims are still there to make either one work.
///
/// Each line is tracked separately because the old block was three opt-ins,
/// and the replacement has to offer back exactly what the user had: the
/// autoload line only to someone who sources the hook, the completions line
/// only to someone on 0.3.0, whose block had one (0.2.0's did not).
#[derive(Debug, Clone, Copy)]
struct LegacyBlock {
    init: bool,
    autoload: bool,
    completions: bool,
}

impl LegacyBlock {
    fn found_in(body: &str) -> Self {
        Self {
            init: sources_actively(body, "bin/jlo-init.sh"),
            autoload: sources_actively(body, "bin/jlo-autoload.sh"),
            // 0.3.0's block sources the generated completion scripts straight
            // out of completions/, under its own $BASH_VERSION/$ZSH_VERSION
            // test. Those files are still generated, so that half keeps
            // working on its own - but completions.sh is what replaces it.
            completions: sources_actively(body, "completions/jlo.bash")
                || sources_actively(body, "completions/_jlo"),
        }
    }

    /// The init line is what makes it the old block; the other two never
    /// appear without it.
    fn present(self) -> bool {
        self.init
    }
}

/// Where the old block is, if it is anywhere we can see.
///
/// Wider than the single candidate `activated` uses, and deliberately so. The
/// released installers said "e.g., ~/.bashrc, ~/.zshrc", and the case that
/// needs it is bash on macOS: the login shell reads `~/.bash_profile`, so a
/// block pasted into `~/.bashrc` and sourced from there is active and
/// invisible to a one-file check.
///
/// Only this scan is widened. `activated` stays on the one candidate, where a
/// miss costs a duplicated line and a false positive would cost a silent
/// non-install - the asymmetry that put it there in the first place.
fn find_legacy_block(home: &Path, login: &Path) -> Option<(PathBuf, LegacyBlock)> {
    let mut seen: Vec<PathBuf> = Vec::new();
    let candidates = std::iter::once(login.to_path_buf())
        .chain([".zshrc", ".bashrc", ".bash_profile", ".profile"].map(|name| home.join(name)));
    for candidate in candidates {
        if seen.contains(&candidate) {
            continue;
        }
        seen.push(candidate.clone());
        let Ok(body) = fs::read_to_string(&candidate) else {
            continue;
        };
        let legacy = LegacyBlock::found_in(&body);
        if legacy.present() {
            return Some((candidate, legacy));
        }
    }
    None
}

/// Name the old block, and print the lines that replace it. Nothing is
/// written: the installer only ever reads the profile, and a block a user
/// pasted is theirs to remove.
fn legacy_notice(layout: &Layout, profile: &Path, home: Option<&Path>, legacy: LegacyBlock) {
    eprintln!();
    // "contains", not "loads": the scan reaches past the login profile, and a
    // block in a file this shell never reads is still worth replacing.
    ui::warning!(
        "{} contains the pre-0.4.0 J'Lo block.",
        tilde(profile, home)
    );
    eprintln!(
        "\nIt keeps working - this install writes a compatibility shim for it - but the\n\
         shim is removed in v1.0.0.\n"
    );
    eprintln!("{}\n", ui::heading("Replace that block with:"));
    let mut lines = vec!["jlo.sh"];
    if legacy.autoload {
        lines.push("autoload.sh");
    }
    if legacy.completions {
        lines.push("completions.sh");
    }
    for name in lines {
        eprintln!(
            "{}",
            ui::command(&source_line(&snippet(layout, home, name)))
        );
    }
    eprintln!(
        "\n{}",
        ui::footnote("These lines never change: upgrades regenerate the files they point at.")
    );
}

// ---------------------------------------------------------------------------
// Rendering helpers
// ---------------------------------------------------------------------------

/// A default install prints the unexpanded `"$HOME/.jlo/..."` so the same line
/// works on another machine; the generated files it points at hold the real
/// paths. A custom `JLO_HOME` is quoted like any other baked path, so a `$` in
/// it stays a `$`.
fn snippet(layout: &Layout, home: Option<&Path>, name: &str) -> String {
    if home.is_some_and(|h| layout.home == h.join(crate::JLO_HOME_DIR_NAME)) {
        format!("\"$HOME/{}/{name}\"", crate::JLO_HOME_DIR_NAME)
    } else {
        sq(&display(&layout.home.join(name)))
    }
}

fn source_line(path: &str) -> String {
    format!("[ -s {path} ] && . {path}")
}

/// The one command that puts J'Lo in the user's profile, as a heredoc.
///
/// Three `printf '%s\n' '...' >> ~/.zshrc` lines came before this, and the
/// quoting was most of what the reader saw. A heredoc shows the *content*
/// instead: what the user copies is what ends up in their file, and one paste
/// replaces three.
///
/// **The delimiter is quoted** - `<<'EOF'`, not `<<EOF`. An unquoted delimiter
/// expands `$HOME` while the heredoc is being written, which would bake this
/// machine's absolute path into a profile that is meant to stay portable. It
/// also ends the `echo`-versus-`printf` problem the old form existed to dodge:
/// a quoted heredoc is literal, so a backslash in a custom `JLO_HOME` arrives
/// as a backslash under every shell.
///
/// The **blank first line** is not decoration either. `>>` appends at the
/// exact end of the file, and a profile whose last line has no trailing
/// newline would otherwise have jlo's first line welded onto it.
///
/// The **terminator is chosen against the body**, not hard-coded. A directory
/// name may contain a newline (the shims already have to survive that), so a
/// `JLO_HOME` of `/tmp/jlo\nEOF\nx` would put a bare `EOF` on a line of its
/// own inside the block and end the heredoc in the middle of a path. Quoting
/// cannot help: inside a heredoc body the quotes are data. The user would be
/// left with half a statement appended to their profile and the rest handed to
/// the shell as input.
fn heredoc(layout: &Layout, home: Option<&Path>, target: &str) -> Vec<String> {
    let body: Vec<String> = ["jlo.sh", "autoload.sh", "completions.sh"]
        .into_iter()
        .map(|name| source_line(&snippet(layout, home, name)))
        .collect();
    let delimiter = terminator(&body);

    let mut lines = vec![format!("cat >> {target} <<'{delimiter}'"), String::new()];
    lines.extend(body);
    lines.push(delimiter);
    lines
}

/// The shortest `EOF`-ish word that appears on no line of the body.
///
/// `<<` (rather than `<<-`) ignores no leading whitespace, so only a line that
/// is *exactly* the delimiter ends the block - but a body line can contain
/// newlines of its own, so the comparison is against physical lines, not
/// against the strings this function was handed.
fn terminator(body: &[String]) -> String {
    let mut delimiter = String::from("EOF");
    while body
        .iter()
        .flat_map(|entry| entry.lines())
        .any(|line| line == delimiter)
    {
        delimiter.push('_');
    }
    delimiter
}

/// The `>>` target. `~` is left unquoted so the shell expands it; a `$HOME`
/// with anything unusual in it falls back to a quoted absolute path, which is
/// less readable but correct.
fn profile_target(profile: &Path, home: Option<&Path>) -> String {
    let simple = |rest: &str| {
        !rest.is_empty()
            && rest
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '/'))
    };
    if let Some(rest) = home.and_then(|h| profile.strip_prefix(h).ok()) {
        let rest = display(rest);
        if simple(&rest) {
            return format!("~/{rest}");
        }
    }
    sq(&display(profile))
}

/// `~/x` for human-readable prose. Never used where a shell will read it back.
fn tilde(path: &Path, home: Option<&Path>) -> String {
    match home.and_then(|h| path.strip_prefix(h).ok()) {
        Some(rest) => format!("~/{}", display(rest)),
        None => display(path),
    }
}

/// POSIX single-quoting, as the one implementation of it.
///
/// Every value this module writes into generated shell code goes through
/// here, for the same reason `jlo env`'s exports do: the line is executed, so
/// a `$`, a backtick or a quote in a `JLO_HOME` path is code rather than
/// data. A second copy of the rule is a second thing to get wrong, so this is
/// a rename of `shellenv::shell_quote` and nothing more.
use crate::shellenv::shell_quote as sq;

fn display(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// Write `contents` to `path` via a temp file **in `path`'s own directory**.
///
/// `rename` is only atomic within one filesystem, so the staging file cannot
/// live in `std::env::temp_dir()`; a sibling is the only placement that holds
/// when `bin/` is itself a mount point or a symlink. Without this an
/// interruption leaves a *truncated* script, which is worse than a stale one.
fn write_atomic(path: &Path, contents: &[u8]) -> Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| anyhow!("{path:?} has no parent directory."))?;
    let name = path
        .file_name()
        .ok_or_else(|| anyhow!("{path:?} has no file name."))?
        .to_string_lossy()
        .into_owned();
    let tmp = dir.join(format!(".{name}.tmp{}", std::process::id()));

    let staged = fs::File::create(&tmp).and_then(|mut f| {
        f.write_all(contents)?;
        f.sync_all()
    });
    if let Err(e) = staged {
        let _ = fs::remove_file(&tmp);
        return Err(anyhow!("could not write {path:?}: {e}."));
    }
    if let Err(e) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(anyhow!("could not write {path:?}: {e}."));
    }
    sync_dir(dir);
    Ok(())
}

/// Make a `rename` durable by flushing the *directory entry*, not just the
/// file's contents.
///
/// Without it the ordering this module relies on is not a crash guarantee at
/// all: the receipt is the commit marker, and a crash could leave it on disk
/// while the script it vouches for is still only in the page cache. Best
/// effort, because some filesystems refuse an `fsync` on a directory and that
/// is not a reason to fail an install that otherwise succeeded.
fn sync_dir(dir: &Path) {
    if let Ok(handle) = fs::File::open(dir) {
        let _ = handle.sync_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout_at(home: &Path) -> Layout {
        Layout::new(home.to_path_buf())
    }

    /// Writes a receipt naming this executable and `home`, at `version`.
    fn receipt_at(layout: &Layout, version: &str) -> Receipt {
        let receipt = Receipt {
            version: version.to_string(),
            method: DEFAULT_METHOD.to_string(),
            jlo_home: display(layout.home()),
            binary: display(&std::env::current_exe().unwrap()),
            symlink: None,
        };
        fs::create_dir_all(layout.home()).unwrap();
        receipt.write(layout).unwrap();
        receipt
    }

    #[test]
    fn a_receipt_that_matches_this_binary_is_not_stale() {
        let dir = tempfile::tempdir().unwrap();
        let layout = layout_at(dir.path());
        receipt_at(&layout, VERSION);
        assert!(
            stale_receipt(&layout).is_none(),
            "a receipt naming this very version asked to be healed"
        );
    }

    /// The interleaving the self-heal must not join in with: this binary is
    /// the *old* one, still running, while a newer one has already been
    /// published in full at the same path. The receipt disagrees with
    /// `VERSION` exactly as an interrupted upgrade would, and the binary path
    /// it names is this one - so neither of those tests can tell the two
    /// apart. Only the direction can: an interrupted upgrade leaves a receipt
    /// *older* than the running binary, because the receipt is written last.
    ///
    /// Healing here would write this binary's older scripts beside the newer
    /// binary and stamp its own version over the newer receipt - a downgrade
    /// performed by the function that exists to repair downgrades.
    #[test]
    fn a_receipt_naming_a_newer_version_is_not_stale() {
        let dir = tempfile::tempdir().unwrap();
        let layout = layout_at(dir.path());
        receipt_at(&layout, "999.0.0");
        assert!(
            stale_receipt(&layout).is_none(),
            "a receipt from a newer publication was treated as an interrupted upgrade"
        );
    }

    #[test]
    fn a_receipt_whose_version_cannot_be_compared_is_not_stale() {
        let dir = tempfile::tempdir().unwrap();
        let layout = layout_at(dir.path());
        receipt_at(&layout, "not-a-version");
        assert!(
            stale_receipt(&layout).is_none(),
            "an unreadable receipt version was healed on a guess"
        );
    }

    #[test]
    fn a_receipt_naming_an_older_version_is_stale() {
        let dir = tempfile::tempdir().unwrap();
        let layout = layout_at(dir.path());
        receipt_at(&layout, "0.0.1-not-this-build");
        assert!(
            stale_receipt(&layout).is_some(),
            "the incomplete-upgrade state went unnoticed"
        );
    }

    #[test]
    fn a_receipt_describing_another_jlo_home_is_not_stale() {
        let dir = tempfile::tempdir().unwrap();
        let layout = layout_at(dir.path());
        receipt_at(&layout, "0.0.1-not-this-build");

        let elsewhere = layout_at(&dir.path().join("copied"));
        fs::create_dir_all(elsewhere.home()).unwrap();
        fs::copy(layout.receipt(), elsewhere.receipt()).unwrap();
        assert!(
            stale_receipt(&elsewhere).is_none(),
            "a receipt carried over from another install was treated as ours"
        );
    }

    #[test]
    fn a_receipt_naming_another_binary_is_not_stale() {
        let dir = tempfile::tempdir().unwrap();
        let layout = layout_at(dir.path());
        let mut receipt = receipt_at(&layout, "0.0.1-not-this-build");
        receipt.binary = display(&dir.path().join("somewhere-else").join("jlo-bin"));
        receipt.write(&layout).unwrap();
        assert!(
            stale_receipt(&layout).is_none(),
            "a receipt describing a different executable was treated as ours"
        );
    }

    /// ADR-0006: the lock lives on the open file description, so it survives
    /// `selfupdate`'s `exec` - but only if the fd does. std opens every file
    /// `O_CLOEXEC`, and nothing in the type system undoes that, so a refactor
    /// that drops the `fcntl_setfd` call compiles, passes every other test,
    /// and silently releases the lock at exactly the moment `selfupdate` hands
    /// over to the staged binary, which then publishes unguarded.
    ///
    /// `tests/selfupdate.rs` proves an *externally* held lock blocks a second
    /// update. This is the other half: that ours is still held after the
    /// `exec`, which is a property of the descriptor rather than of anything
    /// observable from outside.
    #[test]
    // The underscore says "nothing reads this in production", which is still
    // true; this test reads it precisely because the field's whole purpose is
    // the fd underneath it.
    #[allow(clippy::used_underscore_binding)]
    fn the_lock_fd_survives_an_exec() {
        let home = tempfile::tempdir().unwrap();
        let lock = Lock::acquire(home.path()).expect("nothing else holds it");
        let file = lock
            ._file
            .as_ref()
            .expect("acquire opens the lock file itself");

        let flags = rustix::io::fcntl_getfd(file).unwrap();
        assert!(
            !flags.contains(rustix::io::FdFlags::CLOEXEC),
            "the lock fd carries FD_CLOEXEC and would be closed by selfupdate's exec"
        );
    }

    #[test]
    fn a_default_home_keeps_the_snippet_portable() {
        let home = Path::new("/home/u");
        let layout = layout_at(&home.join(".jlo"));
        assert_eq!(
            snippet(&layout, Some(home), "jlo.sh"),
            "\"$HOME/.jlo/jlo.sh\""
        );
    }

    #[test]
    fn a_custom_home_is_spelled_out_and_quoted() {
        let home = Path::new("/home/u");
        let layout = layout_at(Path::new("/opt/jlo"));
        assert_eq!(snippet(&layout, Some(home), "jlo.sh"), "'/opt/jlo/jlo.sh'");
    }

    #[test]
    fn profile_target_uses_tilde_for_a_plain_path() {
        let home = Path::new("/home/u");
        assert_eq!(profile_target(&home.join(".zshrc"), Some(home)), "~/.zshrc");
    }

    /// `~` is the shell's own expansion of `$HOME`, so an apostrophe in the
    /// home directory is not the printed line's problem - it stays readable,
    /// and correct, exactly where a baked absolute path would need escaping.
    #[test]
    fn profile_target_keeps_the_tilde_over_an_unusual_home() {
        let home = Path::new("/home/o'brien");
        assert_eq!(profile_target(&home.join(".zshrc"), Some(home)), "~/.zshrc");
    }

    /// What the quoting is actually for: a name `~/` cannot stand in for.
    #[test]
    fn profile_target_quotes_a_name_the_tilde_cannot_carry() {
        let home = Path::new("/home/u");
        let odd = home.join("my profile").join(".zshrc");
        assert_eq!(
            profile_target(&odd, Some(home)),
            "'/home/u/my profile/.zshrc'"
        );
        assert_eq!(
            profile_target(Path::new("/etc/zshrc"), Some(home)),
            "'/etc/zshrc'"
        );
    }

    #[test]
    fn write_atomic_leaves_no_temp_file_behind() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("jlo.sh");
        write_atomic(&path, b"hello\n").expect("write");
        assert_eq!(fs::read_to_string(&path).expect("read"), "hello\n");
        let leftovers: Vec<_> = fs::read_dir(dir.path())
            .expect("read_dir")
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n != "jlo.sh")
            .collect();
        assert!(
            leftovers.is_empty(),
            "temp files left behind: {leftovers:?}"
        );
    }

    #[test]
    fn a_receipt_round_trips() {
        let dir = tempfile::tempdir().expect("tempdir");
        let layout = layout_at(dir.path());
        write_receipt(
            &layout,
            "installer",
            Some(Path::new("/home/u/.local/bin/jlo")),
        )
        .expect("write receipt");
        let back = read_receipt(&layout).expect("receipt");
        assert_eq!(back.version, VERSION);
        assert_eq!(back.method, "installer");
        assert_eq!(back.symlink.as_deref(), Some("/home/u/.local/bin/jlo"));
    }

    #[test]
    fn a_malformed_receipt_reads_as_absent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let layout = layout_at(dir.path());
        fs::write(layout.receipt(), "{ not json").expect("write");
        assert!(read_receipt(&layout).is_none());
    }
}
