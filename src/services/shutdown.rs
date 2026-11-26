//! Stopping in order (SIGTERM, Ctrl+C).
//!
//! Ryokan had no signal handling: a stop was the process dying wherever
//! it stood, and as a container's first process it did not even die on
//! SIGTERM (Linux delivers a signal to PID 1 only if it has a handler),
//! so `docker stop` waited out its grace period and killed it.
//!
//! What a stop does now, in `main.rs` (`shutdown_gently`):
//!
//! 1. The web server stops taking requests and finishes the ones in
//!    flight (`HTTP_DRAIN` at most).
//! 2. Work that must not be cut is waited for. Such work holds a
//!    [`Hold`] for as long as it runs; nothing else is waited for.
//! 3. The database is closed and the process exits 0.
//!
//! A second signal exits at once.
//!
//! **There is no time limit on step 2.** Whoever stops the process
//! already has one (Docker's stop grace period, systemd's
//! `TimeoutStopSec`, a second Ctrl+C), and a limit in here shorter than
//! theirs would cut work they were willing to wait for.
//!
//! ## What holds, and what does not
//!
//! A hold is for a unit of work that leaves the library or the database
//! wrong when cut in the middle, not merely unfinished:
//!
//! * one download's import in post-processing (all of its files: a pack
//!   cut between files in move mode has lost its sources),
//! * a manual import, which treats the stop as its own Cancel: the
//!   file being placed is finished and what landed is tagged,
//! * a custom script run, which is stopped rather than waited for.
//!
//! Two ways to take one. [`begin`] refuses once a stop has been asked
//! for, and is for loops that can simply not start their next unit.
//! [`hold`] is always granted, for work already under way that stops
//! itself by watching [`requested`].
//!
//! Everything else is dropped where it stands, as it always was. The
//! sync and sweep loops are series of single statements, a backup is
//! written under a `.partial` name, a copy into the library under
//! `.ryokan-tmp` (`post_processing::copy_staged`), and a grab's row is
//! written before the download client is asked
//! (`grabbed_torrents::AheadGrab`), so a grab cut at any point leaves
//! either nothing or a row the next import pass finds. None of these
//! needs a hold: they are safe against a kill, not only against a
//! stop.

use std::collections::BTreeMap;
use std::sync::{Arc, LazyLock, Mutex};

use tokio::sync::watch;

/// One process's stop state. Cheap to clone; clones share it.
#[derive(Clone)]
pub struct Shutdown(Arc<Inner>);

struct Inner {
    requested: watch::Sender<bool>,
    /// How many holds are alive; what [`Shutdown::idle`] waits on.
    held: watch::Sender<usize>,
    /// What each hold is for, for the "waiting for" log line.
    labels: Mutex<(u64, BTreeMap<u64, String>)>,
}

impl Default for Shutdown {
    fn default() -> Self {
        Self::new()
    }
}

impl Shutdown {
    pub fn new() -> Self {
        Self(Arc::new(Inner {
            requested: watch::Sender::new(false),
            held: watch::Sender::new(0),
            labels: Mutex::new((0, BTreeMap::new())),
        }))
    }

    /// Ask for a stop. True the first time, false when one was already
    /// asked for.
    pub fn request(&self) -> bool {
        !self.0.requested.send_replace(true)
    }

    pub fn requested(&self) -> bool {
        *self.0.requested.borrow()
    }

    /// Resolves once a stop has been asked for; at once if it already
    /// was.
    pub async fn wait(&self) {
        let mut requested = self.0.requested.subscribe();
        // The sender lives as long as `self`, so this cannot fail.
        let _ = requested.wait_for(|asked| *asked).await;
    }

    /// A hold for work that is already committed. Always granted.
    pub fn hold(&self, what: impl Into<String>) -> Hold {
        let id = {
            let mut labels = self.0.labels.lock().unwrap();
            labels.0 += 1;
            let id = labels.0;
            labels.1.insert(id, what.into());
            id
        };
        self.0.held.send_modify(|n| *n += 1);
        Hold {
            inner: self.0.clone(),
            id,
        }
    }

    /// A hold for a unit of work about to start, or `None` once a stop
    /// has been asked for: the caller starts nothing.
    pub fn begin(&self, what: impl Into<String>) -> Option<Hold> {
        // Taken first, checked second: a stop that lands in between
        // sees the hold and waits, where the other order would let the
        // work start unseen.
        let hold = self.hold(what);
        (!self.requested()).then_some(hold)
    }

    /// What is being waited for, oldest first.
    pub fn in_progress(&self) -> Vec<String> {
        self.0.labels.lock().unwrap().1.values().cloned().collect()
    }

    /// Resolves when no hold is alive.
    pub async fn idle(&self) {
        let mut held = self.0.held.subscribe();
        let _ = held.wait_for(|n| *n == 0).await;
    }
}

/// Keeps the process from exiting while it is alive. See the module
/// docs for what deserves one.
pub struct Hold {
    inner: Arc<Inner>,
    id: u64,
}

impl Drop for Hold {
    fn drop(&mut self) {
        self.inner.labels.lock().unwrap().1.remove(&self.id);
        self.inner.held.send_modify(|n| *n = n.saturating_sub(1));
    }
}

static GLOBAL: LazyLock<Shutdown> = LazyLock::new(Shutdown::new);

/// The process's own stop state. Code that is tested against a stop
/// takes a `&Shutdown` and is handed this one in production.
pub fn global() -> &'static Shutdown {
    &GLOBAL
}

/// [`Shutdown::requested`] on the process's own state.
pub fn requested() -> bool {
    GLOBAL.requested()
}

/// [`Shutdown::wait`] on the process's own state.
pub async fn wait() {
    GLOBAL.wait().await
}

/// [`Shutdown::hold`] on the process's own state.
pub fn hold(what: impl Into<String>) -> Hold {
    GLOBAL.hold(what)
}

/// [`Shutdown::begin`] on the process's own state.
pub fn begin(what: impl Into<String>) -> Option<Hold> {
    GLOBAL.begin(what)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn a_stop_is_asked_for_once_and_wakes_whoever_waits() {
        let shutdown = Shutdown::new();
        assert!(!shutdown.requested());
        let waiter = {
            let shutdown = shutdown.clone();
            tokio::spawn(async move { shutdown.wait().await })
        };
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!waiter.is_finished());

        assert!(shutdown.request(), "the first request");
        assert!(!shutdown.request(), "already asked for");
        assert!(shutdown.requested());
        tokio::time::timeout(Duration::from_secs(2), waiter)
            .await
            .expect("the waiter wakes")
            .unwrap();
        // Asked for before the wait began: resolves at once.
        tokio::time::timeout(Duration::from_secs(2), shutdown.wait())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn the_exit_waits_for_every_hold() {
        let shutdown = Shutdown::new();
        tokio::time::timeout(Duration::from_secs(2), shutdown.idle())
            .await
            .expect("nothing held: idle at once");

        let import = shutdown.begin("importing Show - 01").expect("no stop yet");
        let grab = shutdown.hold("grab: Show - 02");
        assert_eq!(
            shutdown.in_progress(),
            vec!["importing Show - 01", "grab: Show - 02"]
        );
        let idle = {
            let shutdown = shutdown.clone();
            tokio::spawn(async move { shutdown.idle().await })
        };
        drop(import);
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!idle.is_finished(), "one hold is still alive");
        assert_eq!(shutdown.in_progress(), vec!["grab: Show - 02"]);
        drop(grab);
        tokio::time::timeout(Duration::from_secs(2), idle)
            .await
            .expect("idle once the last hold is gone")
            .unwrap();
        assert!(shutdown.in_progress().is_empty());
    }

    #[tokio::test]
    async fn after_a_stop_nothing_new_begins_but_committed_work_still_holds() {
        let shutdown = Shutdown::new();
        shutdown.request();
        assert!(shutdown.begin("importing Show - 03").is_none());
        // The refused hold is not left behind to wait on.
        tokio::time::timeout(Duration::from_secs(2), shutdown.idle())
            .await
            .expect("a refused begin holds nothing");

        let grab = shutdown.hold("grab: Show - 04");
        assert_eq!(shutdown.in_progress(), vec!["grab: Show - 04"]);
        drop(grab);
    }
}
