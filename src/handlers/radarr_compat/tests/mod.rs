//! Radarr shim tests, parallel structure to the Sonarr side.
//! Auth + system-tier response shapes only here; the movie
//! endpoints follow in a later PR.

mod auth;
mod helpers;
mod movie;
mod queue;
mod system;
