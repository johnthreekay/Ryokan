//! Sonarr shim tests, topic-split per the test-coverage-expansion
//! plan.
//!
//!   * `auth.rs` — `require_api_key` middleware: 503 when config
//!     absent / shim disabled, 401 when key missing / mismatched,
//!     200 when the key arrives via either `X-Api-Key` header or
//!     `?apikey=` query param, percent-decoding round-trip.
//!   * `system.rs` — Response-shape snapshots for the system-tier
//!     endpoints Seerr hits during a connection test
//!     (`system/status`, `qualityprofile`, `rootfolder`,
//!     `languageprofile`, `tag`, `downloadclient`). Snapshots pin
//!     the exact JSON shape so a silently-drifting field doesn't
//!     break Seerr without a test failure.
//!   * `queue.rs` — `GET /queue` (#325): the paging envelope, the
//!     record Seerr's download-sync job reads, and the rows a download
//!     is reported under.

mod auth;
mod helpers;
mod queue;
mod series;
mod system;
