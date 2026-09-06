//! Post-processing tests, topic-split per the test-coverage-expansion
//! plan. Each file covers one behavioral area of
//! `services::post_processing`:
//!
//!   * `file_ops.rs` — `do_file_op` across hardlink / copy / move
//!     modes, happy-path same-fs tests plus parent-directory
//!     creation, hardlink-produces-shared-inode property, move-mode
//!     source removal.
//!   * `filenames.rs` — `is_video_file` extension coverage and
//!     `sanitize_filename` behavior (pure-function helpers).
//!   * `lock.rs` — `POST_PROC_LOCK` serialization: the `try_lock` in
//!     `run_once` means a second run during an in-progress first
//!     returns early without stepping on the first's state.
//!
//! Cross-filesystem test paths (EXDEV hardlink fallback, cross-fs
//! move via `.ryokan-tmp`) are intentionally out of scope — they
//! require a second mounted filesystem to produce the errno, which
//! CI runners don't guarantee. The hardlink-on-fail path is still
//! covered by integration observation when the release binary runs
//! against a real download directory.

mod batch_import_live;
mod batch_preflight;
mod client_cleanup;
mod file_ops;
mod filenames;
mod grab_claims_episode;
mod lock;
mod run_once;
mod walk_video_files;

/// Serializes the `lock.rs` test and the `run_once.rs` tests so they
/// don't race on the production `POST_PROC_LOCK`. Both touch the
/// same `tokio::Mutex` (lock.rs asserts `try_lock` semantics
/// directly; run_once tests indirectly via the production code's
/// own `try_lock`). Without this serializer a parallel test holding
/// `POST_PROC_LOCK` would make a peer test's `run_once` silently
/// no-op (try_lock → Err → early return), and the peer's
/// `list_scoped` call-count assertions would fail with no useful
/// signal.
///
/// `tokio::Mutex` rather than `std::sync::Mutex` so the test
/// `tokio::test` runtime can `.lock().await` without blocking the
/// scheduler. The mutex never holds across `.await` for production
/// reasons; here it's purely a test-harness serialization knob.
pub(super) static POST_PROC_TEST_SERIALIZER: std::sync::LazyLock<tokio::sync::Mutex<()>> =
    std::sync::LazyLock::new(|| tokio::sync::Mutex::new(()));
