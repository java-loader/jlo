//! SIGINT and SIGTERM during an install: recorded instead of fatal, so the
//! run stops at its next read and its staging directories are deleted by
//! their owners rather than left for the stale-staging sweep.

use std::io::{self, Read};
use std::sync::atomic::{AtomicI32, Ordering};

/// The signal that asked the process to stop; 0 until one did. The first one
/// wins: it decides the exit code.
static RECEIVED: AtomicI32 = AtomicI32::new(0);

const SIGNALS: [libc::c_int; 2] = [libc::SIGINT, libc::SIGTERM];

/// Where a run reads whether it was asked to stop: the process's signals in
/// production, a token of its own in tests.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Cancel<'a> {
    signal: &'a AtomicI32,
    /// Whether this is the process's own, which [`Self::arm`] installs the
    /// handlers for. A test token must not touch the test process's.
    process: bool,
}

impl Cancel<'static> {
    pub(crate) fn process() -> Self {
        Self {
            signal: &RECEIVED,
            process: true,
        }
    }
}

impl<'a> Cancel<'a> {
    #[cfg(test)]
    pub(crate) fn token(signal: &'a AtomicI32) -> Self {
        Self {
            signal,
            process: false,
        }
    }

    pub(crate) fn signal(self) -> Option<i32> {
        match self.signal.load(Ordering::SeqCst) {
            0 => None,
            signal => Some(signal),
        }
    }

    pub(crate) fn check(self) -> Result<(), Cancelled> {
        self.signal()
            .map_or(Ok(()), |signal| Err(Cancelled(signal)))
    }

    /// `error`, unless a signal arrived meanwhile: then the failure is most
    /// likely the signal's doing (a read refused by [`Checked`]), and either
    /// way it is the signal the caller must act on.
    pub(crate) fn explain(self, error: anyhow::Error) -> anyhow::Error {
        match self.signal() {
            Some(signal) => Cancelled(signal).into(),
            None => error,
        }
    }

    /// `inner`, refusing every read once a signal has arrived.
    pub(crate) fn reader<R: Read>(self, inner: R) -> Checked<'a, R> {
        Checked {
            inner,
            cancel: self,
        }
    }

    /// Record SIGINT and SIGTERM instead of dying of them, until the returned
    /// guard is dropped. A no-op for a test token.
    pub(crate) fn arm(self) -> anyhow::Result<Armed> {
        let mut armed = Armed {
            previous: Vec::new(),
        };
        if self.process {
            for signal in SIGNALS {
                let previous = set_action(signal, &recording_action()).map_err(|e| {
                    anyhow::anyhow!("could not install a handler for signal {signal}: {e}")
                })?;
                // Pushed one by one, so a failure on the second restores the
                // first when `armed` drops.
                armed.previous.push((signal, previous));
            }
        }
        Ok(armed)
    }
}

/// A run stopped by `SIGINT` or `SIGTERM`. `main` turns it into exit code
/// `128 + signal`, the code a shell reports for a process the signal killed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Cancelled(pub(crate) i32);

impl Cancelled {
    pub(crate) fn exit_code(self) -> i32 {
        128 + self.0
    }
}

impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("interrupted")
    }
}

impl std::error::Error for Cancelled {}

/// A reader that fails once a signal has arrived. Not with
/// `ErrorKind::Interrupted`: `io::copy`, `read_exact` and the tar reader
/// retry that, and would read on.
#[derive(Debug)]
pub(crate) struct Checked<'a, R> {
    inner: R,
    cancel: Cancel<'a>,
}

impl<R: Read> Read for Checked<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if let Some(signal) = self.cancel.signal() {
            return Err(io::Error::other(Cancelled(signal)));
        }
        self.inner.read(buf)
    }
}

/// The dispositions [`Cancel::arm`] replaced, put back on drop.
pub(crate) struct Armed {
    previous: Vec<(libc::c_int, libc::sigaction)>,
}

impl std::fmt::Debug for Armed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let signals: Vec<libc::c_int> = self.previous.iter().map(|(signal, _)| *signal).collect();
        f.debug_struct("Armed").field("signals", &signals).finish()
    }
}

impl Drop for Armed {
    fn drop(&mut self) {
        for (signal, previous) in self.previous.drain(..).rev() {
            // Nothing to do about a failure here: the worst outcome is a
            // signal recorded rather than fatal for the rest of a short-lived
            // process.
            let _ = set_action(signal, &previous);
        }
    }
}

/// Runs inside a signal handler, on whichever thread the signal landed on:
/// an atomic compare-exchange, `signal(2)` and `raise(3)`, all
/// async-signal-safe, and nothing else. The default disposition goes back for
/// both signals, so a second Ctrl-C (or a `kill` after one) terminates at
/// once - the way out of a read that never returns.
///
/// A second signal can also arrive *before* that reset: SIGTERM interrupting
/// the SIGINT handler on the same thread, or either signal landing on another
/// thread while the first handler runs. It finds a signal already recorded,
/// and must not be swallowed, so it puts the default back for itself and
/// raises itself again: blocked while its own handler runs, it stays pending
/// and is delivered - fatally - the moment the handler returns.
#[allow(unsafe_code)]
extern "C" fn record(signal: libc::c_int) {
    let first = RECEIVED
        .compare_exchange(0, signal, Ordering::SeqCst, Ordering::SeqCst)
        .is_ok();
    for each in SIGNALS {
        // SAFETY: `signal(2)` is on POSIX's list of async-signal-safe
        // functions, and `SIG_DFL` installs no code of ours.
        unsafe {
            libc::signal(each, libc::SIG_DFL);
        }
    }
    if !first {
        // SAFETY: `raise(3)` is async-signal-safe; the disposition is
        // `SIG_DFL` by now, so the raised signal runs no code of ours.
        unsafe {
            libc::raise(signal);
        }
    }
}

#[allow(unsafe_code)]
fn recording_action() -> libc::sigaction {
    // SAFETY: `sigaction` is a plain C struct of integers, a mask and (on
    // Linux) an optional function pointer; all-zero is a valid value of each,
    // and the fields that matter are set below.
    let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
    action.sa_sigaction = record as extern "C" fn(libc::c_int) as libc::sighandler_t;
    // SA_RESTART: a read or a channel wait the signal lands in carries on
    // instead of failing with EINTR, and the run notices the signal at its
    // next check rather than as a spurious download error.
    action.sa_flags = libc::SA_RESTART;
    // SAFETY: the mask is a field of a live, exclusively borrowed struct.
    unsafe {
        libc::sigemptyset(&raw mut action.sa_mask);
    }
    action
}

#[allow(unsafe_code)]
fn set_action(signal: libc::c_int, action: &libc::sigaction) -> io::Result<libc::sigaction> {
    // SAFETY: as in `recording_action`; overwritten by the call on success.
    let mut previous: libc::sigaction = unsafe { std::mem::zeroed() };
    // SAFETY: both pointers come from live references for the duration of the
    // call, and the handler `action` may name only touches an atomic.
    let status = unsafe { libc::sigaction(signal, action, &raw mut previous) };
    if status == 0 {
        Ok(previous)
    } else {
        Err(io::Error::last_os_error())
    }
}
