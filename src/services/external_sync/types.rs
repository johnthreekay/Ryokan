//! Provider-agnostic value types shared across the watch-list sync
//! engine — moved out of `mod.rs` during the v1.5 refactor split so
//! the orchestrator file stays navigable. The types themselves are
//! small but they're touched by every other submodule (normalize
//! produces them, merge consumes them, removals walks the AL ids
//! they carry), so a dedicated home keeps the imports tidy.

/// Provider list status normalized across AL and MAL. AL emits SHOUTY
/// (CURRENT, PLANNING, COMPLETED, DROPPED, PAUSED, REPEATING); MAL
/// emits snake_case (watching, completed, on_hold, dropped,
/// plan_to_watch). Mapping converges both onto this enum so the
/// merge engine + monitor-mode default lookup work the same way
/// regardless of which provider produced the entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NormalizedStatus {
    /// AL `CURRENT` / MAL `watching`. The active list — anything
    /// the user is mid-watch on.
    Watching,
    /// AL `PLANNING` / MAL `plan_to_watch`. Plan-to-watch.
    Planning,
    /// AL `PAUSED` / MAL `on_hold`. The user explicitly paused this.
    Paused,
    /// AL `DROPPED` / MAL `dropped`. The user gave up.
    Dropped,
    /// AL `COMPLETED` / MAL `completed`. The user finished it.
    Completed,
    /// AL `REPEATING` (re-watch). MAL doesn't have a distinct value
    /// for this so it never appears for MAL syncs; the engine treats
    /// it the same as Watching when mapping to monitor modes.
    Repeating,
}

impl NormalizedStatus {
    /// Parse AL's status string. Unknown values fall through to
    /// `Planning` because that's the safe default — it grabs nothing
    /// from the back catalog and only acts on future episodes once
    /// the user marks the series as Watching.
    pub fn from_anilist(s: &str) -> Self {
        match s {
            "CURRENT" => Self::Watching,
            "PLANNING" => Self::Planning,
            "PAUSED" => Self::Paused,
            "DROPPED" => Self::Dropped,
            "COMPLETED" => Self::Completed,
            "REPEATING" => Self::Repeating,
            _ => Self::Planning,
        }
    }

    /// Parse MAL's status string. Same safe-default fallback as the
    /// AL path.
    pub fn from_mal(s: &str) -> Self {
        match s {
            "watching" => Self::Watching,
            "plan_to_watch" => Self::Planning,
            "on_hold" => Self::Paused,
            "dropped" => Self::Dropped,
            "completed" => Self::Completed,
            _ => Self::Planning,
        }
    }
}

/// Provider-agnostic sync entry. Both AL and MAL adapters produce
/// these so the merge engine doesn't have to dispatch on provider
/// for each row.
#[derive(Debug, Clone)]
pub struct SyncEntry {
    /// Original provider, kept for diagnostic logging and the
    /// negated-AL-id sentinel decision below.
    pub provider: String,
    /// Provider's own media id. AL ID for AniList, MAL ID for
    /// MyAnimeList. The merge engine uses this for re-link
    /// idempotency on subsequent syncs.
    pub provider_media_id: i64,
    /// AniList ID resolved to the value we'd store on
    /// `series.anilist_id`. For AL entries, identical to
    /// `provider_media_id`. For MAL entries, this is `0` at this
    /// commit — resolution to a real AL ID (or the negated-MAL-id
    /// sentinel if no mapping exists) happens in the merge commit
    /// alongside the anibridge lookup.
    pub anilist_id: i64,
    /// Normalized list status across providers.
    pub status: NormalizedStatus,
    /// Episodes the user has marked watched. **Reserved**: the
    /// current merge step doesn't write this to the `series` row
    /// (this is a "what's on the list" import, not a "watched-state
    /// mirror"). Carried through the abstraction so a follow-up PR
    /// can light up the user-progress sync without re-plumbing the
    /// fetcher.
    pub progress: i64,
    /// Score on the provider's scale; `0.0` means unrated. **Reserved**:
    /// same status as `progress` — fetched and normalized, not yet
    /// written. Render path NEVER displays "You: 0".
    pub score: f64,
    /// Unix epoch (seconds) of the entry's most-recent update on
    /// the provider. The merge engine filters by this against
    /// `external_accounts.list_last_synced_at` for delta sync.
    pub updated_at: i64,
    /// Names of provider-side custom lists this entry belongs to.
    /// AL-only — MAL has no custom-list concept (decision #5 cuts
    /// it from MAL scope). Always empty for MAL.
    pub custom_lists: Vec<String>,
}

/// Aggregate result from a `merge_into_library` call. Each counter
/// tracks one outcome category so the supervised-loop summary line +
/// future "Sync now" UI both have a single number to render per
/// bucket. `failed` holds the per-entry errors so the operator can
/// see specifically which AL ids didn't merge (most often: the
/// detail-fetch returned no payload for that id, e.g. an AL deletion
/// the user's list still references).
#[derive(Debug, Default, Clone)]
pub struct MergeOutcome {
    /// Series rows freshly inserted by this merge run.
    pub created: i32,
    /// Series rows that already existed and whose stored monitor_mode
    /// differed from the target — bumped to the new mode.
    pub monitor_mode_updated: i32,
    /// Series rows that already existed and whose monitor_mode already
    /// matched the target — left untouched.
    pub unchanged: i32,
    /// MAL-sourced entries whose anibridge lookup missed; merging them
    /// requires the Jikan-fallback path (negated-id sentinel + Jikan
    /// metadata fetch). Counted here for visibility; the actual Jikan
    /// merge lands in a follow-up commit.
    pub deferred_jikan: i32,
    /// Per-entry failures: `(anilist_id, error message)`. The merge
    /// keeps going on a single-row failure rather than aborting; one
    /// AL id deleted upstream shouldn't block the other 199 entries
    /// from importing.
    pub failed: Vec<(i64, String)>,
    /// Entries that would have been created but the user's import
    /// preferences are off for the entry's status (e.g. new Dropped
    /// entry while `import_dropped = false`). Counted separately from
    /// `unchanged` because the surface visible to the user is "we
    /// skipped these on purpose" vs. "these were already in sync."
    /// Existing-series monitor_mode updates run regardless of import
    /// preferences — flipping a Watching series to Dropped on AL
    /// always downgrades local monitor_mode, even with
    /// `import_dropped = false`.
    pub skipped_by_preference: i32,
    /// Existing series with `monitor_mode_manual_override = 1` that
    /// the merge step left alone. The user explicitly pinned the
    /// monitor_mode through the UI; sync honors the pin until the
    /// user clears it via "Sync from AL/MAL" in the dropdown.
    pub pinned_manually: i32,
    /// Newly-created series rows that need artwork caching, collected
    /// for the deferred bulk-mode pass that runs after merge. Carrying
    /// just the IDs + image URLs (not the full AnimeDetail) keeps
    /// memory bounded on a 500-series first sync.
    /// Entries on the sync exclusion list.
    pub excluded: i32,
    pub new_artwork: Vec<NewArtworkSpec>,
}

impl MergeOutcome {
    /// Combine two outcomes from sequential merge passes (typically
    /// the AL-detail pass followed by the Jikan-fallback pass for the
    /// same `entries` slice). Counts add; `failed` lists concatenate.
    /// `deferred_jikan` is taken from `self` and reduced by the
    /// number of entries the second pass actually handled — so a
    /// successful Jikan pass on every deferred entry zeroes out the
    /// counter, matching what the operator sees in the library.
    pub fn merge_pass(mut self, other: MergeOutcome) -> MergeOutcome {
        let handled_by_other = other.created
            + other.monitor_mode_updated
            + other.unchanged
            + other.skipped_by_preference
            + other.pinned_manually
            + other.excluded
            + other.failed.len() as i32;
        self.created += other.created;
        self.monitor_mode_updated += other.monitor_mode_updated;
        self.unchanged += other.unchanged;
        self.skipped_by_preference += other.skipped_by_preference;
        self.pinned_manually += other.pinned_manually;
        self.excluded += other.excluded;
        self.deferred_jikan = (self.deferred_jikan - handled_by_other).max(0);
        self.failed.extend(other.failed);
        self.new_artwork.extend(other.new_artwork);
        self
    }
}

/// Pointer payload for the deferred artwork-cache pass. The merge
/// step writes one of these per newly-created series; the post-merge
/// background task in sync_anilist / sync_mal walks the list and
/// calls `artwork::cache_image` once per non-empty URL.
///
/// Both URLs may be empty when the upstream provider doesn't supply
/// banner artwork for a series — Jikan in particular often returns
/// only a cover image. The post-merge task skips empty URLs rather
/// than logging a per-series failure.
#[derive(Debug, Clone)]
pub struct NewArtworkSpec {
    pub series_id: i64,
    pub cover_url: String,
    pub banner_url: String,
}

/// Internal per-entry merge result, mapped 1-1 to `MergeOutcome`'s
/// counters by the calling loop. Lives here (vs. inside `merge.rs`)
/// because both the AL-detail and Jikan-fallback merge paths return
/// the same shape and the `merge_into_library` / `merge_jikan_fallback_entries`
/// dispatch loops sit in the orchestrator side of merge.rs.
#[derive(Debug, Clone)]
pub(crate) enum MergeAction {
    /// New series row created. Carries the artwork spec so the bulk-
    /// mode post-merge task can cache cover + banner without re-
    /// fetching the AnimeDetail.
    Created(NewArtworkSpec),
    MonitorUpdated,
    Unchanged,
    /// Entry would have been a new create, but the user's import
    /// preferences are off for this status. Existing series with the
    /// same status DO still get monitor_mode updated — only the
    /// create branch checks preferences.
    SkippedByPreference,
    /// Existing series whose `monitor_mode_manual_override = 1` —
    /// user explicitly pinned this monitor_mode through the UI.
    /// Sync stamps `synced_from` so removal-detection still tracks
    /// it but leaves the monitor_mode alone.
    PinnedManually,
}

/// Report from a removal-detection pass. `removed` is the list of
/// `series.id` whose monitor_mode got downgraded to `None` because
/// they were no longer in the user's AL/MAL list. Surfaces in the
/// supervised-loop summary so an unexpected removal is visible
/// (e.g. user accidentally cleared their list — the count tells
/// them how much got downgraded).
#[derive(Debug, Default, Clone)]
pub struct RemovalReport {
    pub removed: Vec<i64>,
}
