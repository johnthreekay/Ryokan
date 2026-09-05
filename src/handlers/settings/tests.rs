use super::*;

#[test]
fn sanitize_label_strips_control_chars() {
    // A label with an embedded newline would survive through to
    // rtorrent's `d.custom1.set="..."` inline command and could
    // terminate the command early. sanitize_label strips any
    // control character before it can reach the wire.
    assert_eq!(sanitize_label("ryokan\nmalicious"), "ryokanmalicious");
    assert_eq!(sanitize_label("ry\tokan"), "ryokan");
    assert_eq!(sanitize_label("ryokan\0"), "ryokan");
    assert_eq!(sanitize_label("  ryokan  "), "ryokan");
}

#[test]
fn sanitize_label_defaults_to_ryokan_when_empty_or_only_control() {
    assert_eq!(sanitize_label(""), "ryokan");
    assert_eq!(sanitize_label("   "), "ryokan");
    assert_eq!(sanitize_label("\n\t\r"), "ryokan");
}

#[test]
fn sanitize_label_preserves_unicode_and_spaces() {
    // Only control characters are stripped — internal spaces and
    // non-ASCII characters (users' native-script labels) survive.
    assert_eq!(sanitize_label("anime batch"), "anime batch");
    assert_eq!(sanitize_label("アニメ"), "アニメ");
}

/// A Sonarr/Trash Guides CF that carries a `trash_description`
/// should be surfaced verbatim so the edit drawer can render it.
#[test]
fn extract_trash_description_returns_string_when_present() {
    let json = serde_json::json!({
        "name": "Example",
        "trash_description": "This CF matches high-quality BluRay releases.",
        "specifications": []
    })
    .to_string();
    assert_eq!(
        extract_trash_description(&json),
        Some("This CF matches high-quality BluRay releases.".to_string())
    );
}

/// Absent, empty, whitespace-only, wrong-typed, or unparseable
/// payloads should all return `None` so the template simply
/// doesn't render the description block.
#[test]
fn extract_trash_description_returns_none_for_missing_or_invalid() {
    let no_field = serde_json::json!({"name": "X", "specifications": []}).to_string();
    assert_eq!(extract_trash_description(&no_field), None);

    let empty = serde_json::json!({"trash_description": ""}).to_string();
    assert_eq!(extract_trash_description(&empty), None);

    let whitespace = serde_json::json!({"trash_description": "   "}).to_string();
    assert_eq!(extract_trash_description(&whitespace), None);

    let wrong_type = serde_json::json!({"trash_description": 42}).to_string();
    assert_eq!(extract_trash_description(&wrong_type), None);

    assert_eq!(extract_trash_description("not json at all"), None);
}

#[test]
fn resolve_grab_preview_mode_general_tab_accepts_form_value() {
    // On the General tab, the form value is the source of
    // truth. "never" and "batches_only" both persist.
    assert_eq!(
        resolve_grab_preview_mode(Some("never"), Some("general"), Some("batches_only")),
        "never"
    );
    assert_eq!(
        resolve_grab_preview_mode(Some("batches_only"), Some("general"), Some("never")),
        "batches_only"
    );
}

#[test]
fn resolve_grab_preview_mode_unknown_form_value_coerces_to_default() {
    // A garbage form value (hand-crafted POST, dropped `always`
    // option from the plan doc, etc.) coerces to "batches_only"
    // so the config can't end up in an unenumerated state.
    assert_eq!(
        resolve_grab_preview_mode(Some(""), Some("general"), Some("never")),
        "batches_only"
    );
    assert_eq!(
        resolve_grab_preview_mode(Some("always"), Some("general"), Some("never")),
        "batches_only"
    );
    assert_eq!(
        resolve_grab_preview_mode(None, Some("general"), Some("never")),
        "batches_only"
    );
}

#[test]
fn resolve_grab_preview_mode_other_tabs_preserve_existing() {
    // A save from the Quality tab (or anywhere else) must not
    // reset the picker. Critical — the same form shape is
    // submitted from every tab and the picker field is simply
    // omitted outside General.
    assert_eq!(
        resolve_grab_preview_mode(None, Some("quality"), Some("never")),
        "never"
    );
    assert_eq!(
        resolve_grab_preview_mode(None, Some("groups"), Some("batches_only")),
        "batches_only"
    );
    // A stray form value from a non-General tab is ignored —
    // only the existing value matters there.
    assert_eq!(
        resolve_grab_preview_mode(Some("never"), Some("library"), Some("batches_only")),
        "batches_only"
    );
}

#[test]
fn resolve_grab_preview_mode_missing_tab_uses_form_value() {
    // No tab on the form = the no-tab POST shape; treat like
    // General so the field round-trips on a "save all" flow.
    assert_eq!(
        resolve_grab_preview_mode(Some("never"), None, Some("batches_only")),
        "never"
    );
}

#[test]
fn resolve_grab_preview_mode_missing_existing_defaults_to_batches_only() {
    // Pre-PR-C DB rows never wrote the column; reads default to
    // "batches_only" in the model layer, and the settings
    // save path must do the same if the read path ever produces
    // a missing value.
    assert_eq!(
        resolve_grab_preview_mode(None, Some("quality"), None),
        "batches_only"
    );
}

// ── #62 watch-list sync interval resolver tests ───────────

#[test]
fn resolve_external_sync_interval_integrations_tab_accepts_in_range() {
    // Bounds match the plan-doc-decided range (15 min .. 7 days).
    // 15 and 10080 are inclusive endpoints.
    assert_eq!(
        resolve_external_sync_interval_minutes(Some(15), Some("integrations"), Some(30)),
        15
    );
    assert_eq!(
        resolve_external_sync_interval_minutes(Some(10080), Some("integrations"), Some(30)),
        10080
    );
    assert_eq!(
        resolve_external_sync_interval_minutes(Some(60), Some("integrations"), Some(30)),
        60
    );
}

#[test]
fn resolve_external_sync_interval_out_of_range_coerces_to_default() {
    // 14 and 10081 are just outside the allowed range; both coerce
    // to the 30-minute default so a hand-crafted POST or stale
    // form can't end up with a too-aggressive (rate-limit-
    // pressuring) or effectively-disabled cadence persisted.
    assert_eq!(
        resolve_external_sync_interval_minutes(Some(14), Some("integrations"), Some(30)),
        EXTERNAL_SYNC_INTERVAL_DEFAULT_MIN
    );
    assert_eq!(
        resolve_external_sync_interval_minutes(Some(10081), Some("integrations"), Some(30)),
        EXTERNAL_SYNC_INTERVAL_DEFAULT_MIN
    );
    assert_eq!(
        resolve_external_sync_interval_minutes(Some(0), Some("integrations"), Some(30)),
        EXTERNAL_SYNC_INTERVAL_DEFAULT_MIN
    );
    assert_eq!(
        resolve_external_sync_interval_minutes(Some(-1), Some("integrations"), Some(30)),
        EXTERNAL_SYNC_INTERVAL_DEFAULT_MIN
    );
}

#[test]
fn resolve_external_sync_interval_other_tabs_preserve_existing() {
    // Same cross-tab guarantee as grab_preview_mode: a Quality-
    // tab save shouldn't reset the picker, so a Quality save also
    // shouldn't reset the sync interval.
    assert_eq!(
        resolve_external_sync_interval_minutes(None, Some("quality"), Some(60)),
        60
    );
    // Stray form value from a non-Integrations tab is ignored —
    // only the existing value matters there.
    assert_eq!(
        resolve_external_sync_interval_minutes(Some(120), Some("quality"), Some(60)),
        60
    );
}

#[test]
fn resolve_external_sync_interval_missing_tab_uses_form_value() {
    // No-tab POST shape (full-form save) honors the form value
    // same as Integrations does.
    assert_eq!(
        resolve_external_sync_interval_minutes(Some(120), None, Some(30)),
        120
    );
}

#[test]
fn resolve_external_sync_interval_missing_form_value_preserves_existing() {
    // Field absent from a POST that should have included it
    // (template bug, scripted POST that omits it). Preserve the
    // user's persisted value rather than resetting to default —
    // losing a configured 7-day cadence to a UI bug would be a
    // user-visible regression. Out-of-range form values still
    // reset (separate test) since those signal hand-crafted
    // POSTs we don't trust.
    assert_eq!(
        resolve_external_sync_interval_minutes(None, Some("integrations"), Some(60)),
        60
    );
}

#[test]
fn resolve_external_sync_interval_missing_existing_uses_default() {
    // Pre-PR-B DB rows never wrote the column; the read path
    // defaults to 30, but if it ever returns None for any other
    // reason the resolver should still produce a valid value.
    assert_eq!(
        resolve_external_sync_interval_minutes(None, Some("quality"), None),
        EXTERNAL_SYNC_INTERVAL_DEFAULT_MIN
    );
}

#[test]
fn resolve_external_sync_interval_missing_form_and_existing_uses_default() {
    // First-time save on a fresh install with the field missing.
    // No existing value to preserve, so default is the only sane
    // landing.
    assert_eq!(
        resolve_external_sync_interval_minutes(None, Some("integrations"), None),
        EXTERNAL_SYNC_INTERVAL_DEFAULT_MIN
    );
}

// ── validate_source ───────────────────────────────────────────────
//
// Settings save uses these to coerce form values into
// canonical-lowercase strings the rest of the codebase reads
// back via `Source::from_str`. A regression that forgot to
// canonicalize would persist mixed-case values and break the
// CF / scoring matchers that case-sensitive-compare the column.

#[test]
fn validate_source_canonicalizes_known_values_to_lowercase() {
    // The user-facing dropdown emits canonical strings, but
    // hand-crafted POSTs / older DB rows can carry mixed case.
    // Every recognized variant lands in lowercase.
    assert_eq!(validate_source("BluRay", "web"), "bluray");
    assert_eq!(validate_source("BD", "web"), "bluray");
    assert_eq!(validate_source("BDRIP", "web"), "bluray");
    assert_eq!(validate_source("Web-DL", "bluray"), "web");
    assert_eq!(validate_source("WEBRIP", "bluray"), "web");
    assert_eq!(validate_source("HDTV", "web"), "hdtv");
    assert_eq!(validate_source("DVD", "web"), "dvd");
}

#[test]
fn validate_source_falls_back_to_default_on_unknown() {
    // A garbage form value resolves to the supplied default
    // rather than persisting `Unknown` — every read path
    // assumes a known variant.
    assert_eq!(validate_source("garbage", "web"), "web");
    assert_eq!(validate_source("", "bluray"), "bluray");
    // The default itself isn't canonicalized — it's a static
    // string the caller already chose.
    assert_eq!(validate_source("unknown-source", "WEB"), "WEB");
}

#[test]
fn validate_source_trims_whitespace() {
    // `Source::from_str` trims, so the validator inherits that.
    assert_eq!(validate_source("  bluray  ", "web"), "bluray");
}

// ── validate_cutoff_source ────────────────────────────────────────

#[test]
fn validate_cutoff_source_passes_through_bluray_subtiers() {
    // The cutoff dropdown surfaces three BluRay tiers: plain
    // bluray, bluray_remux, bluray_bdmv. The latter two are stored
    // as-is so `parse_cutoff_source` can branch on the exact string.
    assert_eq!(
        validate_cutoff_source("bluray_remux", "bluray"),
        "bluray_remux"
    );
    assert_eq!(
        validate_cutoff_source("bluray_bdmv", "bluray"),
        "bluray_bdmv"
    );
}

#[test]
fn validate_cutoff_source_falls_through_to_validate_source_for_other_values() {
    // Plain BluRay / WEB / etc. take the regular validate_source
    // path, including canonicalization.
    assert_eq!(validate_cutoff_source("BluRay", "web"), "bluray");
    assert_eq!(validate_cutoff_source("garbage", "bluray"), "bluray");
}

#[test]
fn validate_cutoff_source_is_case_sensitive_on_subtier_markers() {
    // `bluray_remux` / `bluray_bdmv` are exact-match in the
    // passthrough — `BLURAY_REMUX` doesn't get the special
    // treatment. It then falls through to validate_source where
    // `Source::from_str` (which underscore-matches "bdremux" /
    // "bluray" / etc., but NOT "bluray_remux") returns Unknown
    // → the default fires. Net result: a hand-crafted POST with
    // an uppercase sub-tier marker silently loses both the
    // sub-tier intent AND the BluRay source classification —
    // ends up with the supplied default. Worth pinning so a
    // refactor that adds case-folding to either path has to
    // confront this asymmetry.
    assert_eq!(validate_cutoff_source("BLURAY_REMUX", "web"), "web");
}

// ── validate_resolution ───────────────────────────────────────────

#[test]
fn validate_resolution_strips_p_suffix_for_db_storage() {
    // The DB column convention is bare-digit strings ("1080") so
    // `Resolution::from_str` reads them back uniformly. The
    // validator strips the trailing `p` Settings emits with the
    // dropdown.
    assert_eq!(validate_resolution("1080p", "1080"), "1080");
    assert_eq!(validate_resolution("720p", "1080"), "720");
    assert_eq!(validate_resolution("2160p", "1080"), "2160");
    assert_eq!(validate_resolution("480p", "1080"), "480");
}

#[test]
fn validate_resolution_accepts_bare_digit() {
    // Both shapes in the wild — bare digit and suffixed.
    assert_eq!(validate_resolution("1080", "720"), "1080");
    assert_eq!(validate_resolution("720", "1080"), "720");
}

#[test]
fn validate_resolution_accepts_4k_aliases() {
    // 4k / UHD aliases canonicalize to "2160" via Resolution::from_str.
    assert_eq!(validate_resolution("4k", "1080"), "2160");
    assert_eq!(validate_resolution("UHD", "1080"), "2160");
}

#[test]
fn validate_resolution_falls_back_to_default_on_garbage() {
    assert_eq!(validate_resolution("garbage", "1080"), "1080");
    assert_eq!(validate_resolution("", "720"), "720");
    // Sonarr's 360p / 540p don't have Ryokan tiers and fold to
    // the default rather than persisting an unrecognized value.
    assert_eq!(validate_resolution("360p", "1080"), "1080");
    assert_eq!(validate_resolution("540p", "1080"), "1080");
}

// ── normalize_settings_tab ───────────────────────────────────────

#[test]
fn normalize_settings_tab_known_tabs_pass_through() {
    for tab in ["quality", "custom_formats", "groups", "general", "indexers"] {
        assert_eq!(normalize_settings_tab(Some(tab.into())), tab);
    }
}

#[test]
fn normalize_settings_tab_unknown_or_missing_defaults_to_integrations() {
    // Integrations is the default landing — first-run users
    // most often need to wire a download client + Jellyfin
    // before doing anything else, so that's the natural first
    // tab.
    assert_eq!(normalize_settings_tab(None), "integrations");
    assert_eq!(
        normalize_settings_tab(Some("garbage".into())),
        "integrations"
    );
    assert_eq!(normalize_settings_tab(Some("".into())), "integrations");
}

// ── min_score_display ────────────────────────────────────────────

#[test]
fn min_score_display_renders_blank_for_no_floor_sentinel() {
    // i32::MIN is the "no minimum score floor" sentinel — must
    // render as an empty string so the input shows blank, not
    // "-2147483648".
    assert_eq!(min_score_display(i32::MIN), "");
}

#[test]
fn min_score_display_renders_normal_values_as_string() {
    assert_eq!(min_score_display(0), "0");
    assert_eq!(min_score_display(50), "50");
    assert_eq!(min_score_display(-5), "-5");
    // Just-above-the-sentinel renders normally — only the exact
    // i32::MIN value is special.
    assert_eq!(min_score_display(i32::MIN + 1), (i32::MIN + 1).to_string());
}

// ── humanize_relative_time ───────────────────────────────────────

#[test]
fn humanize_relative_time_none_renders_never() {
    // No row in scheduled_task_runs yet → "Never", which is
    // what the Settings dashboard shows for unrun tasks.
    assert_eq!(humanize_relative_time(None), "Never");
}

fn now_ts() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[test]
fn humanize_relative_time_under_one_minute_says_just_now() {
    assert_eq!(humanize_relative_time(Some(now_ts())), "Just now");
    assert_eq!(humanize_relative_time(Some(now_ts() - 30)), "Just now");
}

#[test]
fn humanize_relative_time_under_one_hour_uses_minutes() {
    // The pluralization arm: 1 minute is singular, 2+ is plural.
    assert_eq!(humanize_relative_time(Some(now_ts() - 60)), "1 minute ago");
    assert_eq!(
        humanize_relative_time(Some(now_ts() - 120)),
        "2 minutes ago"
    );
    assert_eq!(
        humanize_relative_time(Some(now_ts() - 30 * 60)),
        "30 minutes ago"
    );
}

#[test]
fn humanize_relative_time_under_one_day_uses_hours() {
    assert_eq!(humanize_relative_time(Some(now_ts() - 3600)), "1 hour ago");
    assert_eq!(humanize_relative_time(Some(now_ts() - 7200)), "2 hours ago");
    // 23h59m is still in hours.
    assert_eq!(
        humanize_relative_time(Some(now_ts() - (23 * 3600 + 59 * 60))),
        "23 hours ago"
    );
}

#[test]
fn humanize_relative_time_one_day_or_more_uses_days() {
    assert_eq!(humanize_relative_time(Some(now_ts() - 86400)), "1 day ago");
    assert_eq!(
        humanize_relative_time(Some(now_ts() - 86400 * 7)),
        "7 days ago"
    );
}

#[test]
fn humanize_relative_time_future_timestamp_renders_just_now() {
    // Defensive: a clock skew or pre-clock-init timestamp could
    // produce ts > now. The `.max(0)` keeps the delta non-negative
    // so we render "Just now" rather than "-N days ago".
    assert_eq!(humanize_relative_time(Some(now_ts() + 10000)), "Just now");
}

// ── extract_spec_labels ──────────────────────────────────────────

#[test]
fn extract_spec_labels_parses_spec_array_into_views() {
    let json = r#"{
        "name": "BD",
        "specifications": [
            {"name": "BluRay", "implementation": "ReleaseTitleSpecification", "negate": false, "required": true},
            {"name": "WEB", "implementation": "ReleaseTitleSpecification", "negate": true, "required": false}
        ]
    }"#;
    let labels = extract_spec_labels(json);
    assert_eq!(labels.len(), 2);
    assert_eq!(labels[0].name, "BluRay");
    assert_eq!(labels[0].implementation, "ReleaseTitleSpecification");
    assert!(!labels[0].negate);
    assert!(labels[0].required);
    assert_eq!(labels[1].name, "WEB");
    assert!(labels[1].negate);
    assert!(!labels[1].required);
}

#[test]
fn extract_spec_labels_returns_empty_on_invalid_json() {
    // Defensive: a parse failure mustn't bubble up — the caller
    // already surfaces the raw parse error via `parse_error`,
    // and the spec-pill row just renders empty.
    assert!(extract_spec_labels("{not json").is_empty());
    assert!(extract_spec_labels("").is_empty());
}

#[test]
fn extract_spec_labels_returns_empty_when_specifications_missing() {
    // CF JSON without a "specifications" array (e.g. malformed
    // import or partial CF in flight) yields zero labels rather
    // than a panic.
    assert!(extract_spec_labels(r#"{"name": "BD"}"#).is_empty());
    // Wrong type for "specifications" → also empty.
    assert!(extract_spec_labels(r#"{"specifications": "oops"}"#).is_empty());
}

#[test]
fn extract_spec_labels_uses_defaults_for_missing_fields() {
    // Each spec entry that omits a field falls back to the
    // typed default — empty strings for `name`/`implementation`,
    // false for both bools. This is what unblocks rendering
    // half-imported CFs in the edit drawer.
    let json = r#"{"specifications": [{}]}"#;
    let labels = extract_spec_labels(json);
    assert_eq!(labels.len(), 1);
    assert_eq!(labels[0].name, "");
    assert_eq!(labels[0].implementation, "");
    assert!(!labels[0].negate);
    assert!(!labels[0].required);
}

/// Indexer picker / catalog rendering on the Settings → Indexers
/// tab. The URL-driven `?edit_id=N` / `?template=<slug>` inline-
/// form flow has been replaced by a click-to-modal flow whose
/// form bodies come from dedicated GET endpoints (covered by
/// `IndexerEditFormPartial` / `IndexerAddFormPartial` rendering
/// tests). What this section still needs to assert is that the
/// catalog grid is always populated from the static seed list,
/// since the page renders unconditionally without any
/// catalog-suppression branch.
mod indexer_picker {
    use super::super::*;
    use crate::test_support::{build_test_app_state, in_memory_pool};

    #[tokio::test]
    async fn catalog_grid_is_always_populated() {
        let db = in_memory_pool().await;
        let state = build_test_app_state(db, None);
        let template = build_settings_template(
            &state,
            Some("indexers".to_string()),
            None,
            None,
            None,
            None,
            None,
        )
        .await;
        assert!(
            !template.indexer_catalog.is_empty(),
            "picker grid is always populated from the static catalog"
        );
    }
}

/// Issue #129 completion — non-HTMX path coverage for the
/// three new per-tab subform handlers
/// (`settings_general_submit`, `settings_quality_submit`,
/// `settings_integrations_submit`).
///
/// The browser-e2e suite at
/// `tests/htmx_browser_e2e_settings_subforms.rs` covers the HTMX
/// path (request lands with `HX-Request: true`, handler returns
/// the small subform partial). It can't reach the no-JS fallback
/// because every request from a real browser carries the htmx
/// header once the vendored script loads. These unit tests fill
/// that gap by calling the handlers directly with
/// `HxRequest(false)`, which is the shape Axum produces when no
/// `HX-Request` header is present (regular form-POST from a JS-
/// disabled browser, or any external script hitting the
/// endpoint with `curl`).
///
/// Each test asserts:
/// 1. The DB write happened — the handler did the same persistence
///    work as the HTMX path.
/// 2. The response is the full `SettingsTemplate` HTML (carries
///    the `<h2>Settings</h2>` page header from `settings.html`,
///    which the per-tab subform partials don't include) — so
///    a regression that returns the partial regardless of the
///    HxRequest flag would visibly break a no-JS save (the
///    user would see a fragment with no nav / chrome).
mod non_htmx_path {
    use super::super::*;
    use crate::test_support::{build_test_app_state, in_memory_pool};
    use axum::body::to_bytes;
    use axum::extract::State;
    use axum::response::IntoResponse;
    use axum_htmx::HxRequest;
    use sqlx::SqlitePool;

    /// Read the response body as a UTF-8 string. axum's `Response`
    /// is `Response<Body>` where `Body` is opaque; `to_bytes` with
    /// a generous limit (2 MiB) covers the full SettingsTemplate
    /// without truncating.
    async fn body_string(resp: axum::response::Response) -> String {
        let bytes = to_bytes(resp.into_body(), 2 * 1024 * 1024)
            .await
            .expect("read body");
        String::from_utf8(bytes.to_vec()).expect("utf-8 body")
    }

    async fn seed_initial_config(db: &SqlitePool) {
        // Minimum-viable Config row — every per-tab handler reads
        // the existing row to preserve fields it doesn't own. With
        // no row, the handlers early-return with the
        // "No config row found" error path; we want to exercise
        // the success path here.
        config::save_config(db, &config::Config::default())
            .await
            .expect("seed config");
    }

    /// Seed a Config row with values **distinct from form
    /// defaults** for every field the handler under test owns.
    /// Pairs with a submit-payload built from values **distinct
    /// from both the seed and form defaults** so a mutant that
    /// deletes a single field's form-write (the most common
    /// missed-mutant shape from the cargo-mutants run) leaves
    /// that field at the seed value — which the assertion then
    /// catches by comparing against the submitted value.
    async fn seed_distinct_config(db: &SqlitePool) {
        let cfg = config::Config {
            // Integrations seeds.
            active_client: "deluge".to_string(),
            qbit_url: "http://qbit.seed:8080".to_string(),
            qbit_user: "qbit-seed-user".to_string(),
            qbit_pass: "qbit-seed-pass".to_string(),
            qbit_category: "qbit-seed-cat".to_string(),
            qbit_download_path: "/seed/qbit".to_string(),
            deluge_url: "http://deluge.seed:8112".to_string(),
            deluge_password: "deluge-seed-pass".to_string(),
            deluge_label: "deluge-seed-label".to_string(),
            deluge_download_path: "/seed/deluge".to_string(),
            transmission_url: "http://trans.seed:9091".to_string(),
            transmission_user: "trans-seed-user".to_string(),
            transmission_password: "trans-seed-pass".to_string(),
            transmission_label: "trans-seed-label".to_string(),
            transmission_download_path: "/seed/trans".to_string(),
            rtorrent_url: "http://rt.seed:8081".to_string(),
            rtorrent_user: "rt-seed-user".to_string(),
            rtorrent_password: "rt-seed-pass".to_string(),
            rtorrent_label: "rt-seed-label".to_string(),
            rtorrent_download_path: "/seed/rt".to_string(),
            jellyfin_url: "http://jelly.seed:8096".to_string(),
            jellyfin_api_key: "jelly-seed-key".to_string(),
            sonarr_enabled: false,
            sonarr_api_key: "sonarr-seed-key".to_string(),
            radarr_enabled: true,
            radarr_api_key: "radarr-seed-key".to_string(),
            grab_preview_mode: "never".to_string(),
            external_sync_interval_minutes: 60,
            // Quality seeds.
            preferred_groups: "SeedPreferred".to_string(),
            blocked_groups: "SeedBlocked".to_string(),
            preferred_source: "bluray".to_string(),
            preferred_resolution: "720".to_string(),
            cutoff_source: "dvd".to_string(),
            cutoff_resolution: "480".to_string(),
            finished_series_quality: "same".to_string(),
            prefer_subs: false,
            upgrade_search_enabled: false,
            seadex_enabled: true,
            default_custom_query_tokens: "seed-tokens".to_string(),
            default_restrict_to_uploader: "seed-uploader".to_string(),
            // General seeds.
            media_root: "/seed/media".to_string(),
            title_language: "native".to_string(),
            rss_enabled: true,
            rss_interval_minutes: 20,
            disable_nyaa_rss: false,
            post_processing_enabled: true,
            post_processing_mode: "copy".to_string(),
            search_on_monitoring_change: true,
            recycle_bin_path: "/seed/recycle".to_string(),
            recycle_bin_age_days: 7,
            ..config::Config::default()
        };
        config::save_config(db, &cfg)
            .await
            .expect("seed distinct config");
    }

    #[tokio::test]
    async fn general_submit_non_htmx_round_trips_every_field() {
        // Mutation-killer test: seeds with values distinct from
        // form defaults, submits with values distinct from BOTH
        // the seed AND form defaults, asserts every owned field
        // lands at the submitted value. A mutant that deletes
        // any single field's form-write line in
        // settings_general_submit leaves that field at the seed
        // value — caught by the per-field assertion below.
        //
        // Also checks the non-HTMX render returns the full
        // SettingsTemplate (`<h2>Settings</h2>` only appears in
        // settings.html, not the per-tab partial).
        let db = in_memory_pool().await;
        seed_distinct_config(&db).await;
        let state = build_test_app_state(db.clone(), None);
        let resp = settings_general_submit(
            State(state),
            HxRequest(false),
            axum::Form(GeneralForm {
                media_root: "/submit/media".to_string(),
                title_language: "romaji".to_string(),
                rss_enabled: None,                        // submit→false, seed=true
                rss_interval_minutes: 45,                 // seed=20
                disable_nyaa_rss: Some(String::new()),    // submit→true, seed=false
                post_processing_enabled: None,            // submit→false, seed=true
                post_processing_mode: "move".to_string(), // seed=copy
                search_on_monitoring_change: None,
                manual_search_auto_add: None, // submit→false, seed=true
                misgrab_auto_remove: None,
                grab_preview_mode: None,
                auto_grab_on_add: None,
                allow_non_english: None,
                recycle_bin_path: "/submit/recycle/".to_string(), // seed=/seed/recycle; trailing slash trimmed
                recycle_bin_age_days: 45,                         // seed=7
                series_folder_format: String::new(),
                season_folder_format: String::new(),
                episode_file_format: String::new(),
                backup_schedule: String::new(),
                backup_directory: String::new(),
                backup_retention_count: 7,
                backup_include_artwork: None,
            }),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        let body = body_string(resp).await;
        assert!(
            body.contains("<h2>Settings</h2>"),
            "non-HTMX response must be the full SettingsTemplate, not the partial"
        );
        let saved = config::get_config(&db)
            .await
            .expect("get_config")
            .expect("config row");
        // Every General-tab field round-trips at the submitted value.
        assert_eq!(saved.media_root, "/submit/media");
        assert_eq!(saved.title_language, "romaji");
        assert!(!saved.rss_enabled);
        assert_eq!(saved.rss_interval_minutes, 45);
        assert!(saved.disable_nyaa_rss);
        assert!(!saved.post_processing_enabled);
        assert_eq!(saved.post_processing_mode, "move");
        assert!(!saved.search_on_monitoring_change);
        assert_eq!(saved.recycle_bin_path, "/submit/recycle");
        assert_eq!(saved.recycle_bin_age_days, 45);
        // Cross-tab fields stay at seed values (regression guard
        // against the per-tab handler clobbering fields it
        // doesn't own).
        assert_eq!(saved.preferred_resolution, "720");
        assert_eq!(saved.jellyfin_url, "http://jelly.seed:8096");
    }

    #[tokio::test]
    async fn quality_submit_non_htmx_round_trips_every_field() {
        // Mutation-killer: seeds + submits every Quality field
        // with distinct values so a deletion of any single
        // field's form-write line surfaces as a per-field
        // assertion failure. See `general_submit_non_htmx_round_trips_every_field`
        // for the rationale.
        let db = in_memory_pool().await;
        seed_distinct_config(&db).await;
        let state = build_test_app_state(db.clone(), None);
        let resp = settings_quality_submit(
            State(state),
            HxRequest(false),
            axum::Form(QualityForm {
                preferred_groups: "SubmitPreferred".to_string(), // seed=SeedPreferred
                blocked_groups: "SubmitBlocked".to_string(),     // seed=SeedBlocked
                preferred_source: "web".to_string(),             // seed=bluray
                preferred_resolution: "2160".to_string(),        // seed=720
                cutoff_source: "bluray".to_string(),             // seed=dvd
                cutoff_resolution: "1080".to_string(),           // seed=480
                finished_series_quality: "bd_only".to_string(),  // seed=same
                prefer_subs: "1".to_string(),                    // seed=false → submit→true
                upgrade_search_enabled: Some(String::new()),     // seed=false → submit→true
                seadex_enabled: None,                            // seed=true → submit→false
                default_custom_query_tokens: Some("submit-tokens".to_string()), // seed=seed-tokens
                default_restrict_to_uploader: Some("submit-uploader".to_string()), // seed=seed-uploader
            }),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        let body = body_string(resp).await;
        assert!(
            body.contains("<h2>Settings</h2>"),
            "non-HTMX response must be the full SettingsTemplate, not the partial"
        );
        let saved = config::get_config(&db)
            .await
            .expect("get_config")
            .expect("config row");
        assert_eq!(saved.preferred_groups, "SubmitPreferred");
        assert_eq!(saved.blocked_groups, "SubmitBlocked");
        assert_eq!(saved.preferred_source, "web");
        assert_eq!(saved.preferred_resolution, "2160");
        assert_eq!(saved.cutoff_source, "bluray");
        assert_eq!(saved.cutoff_resolution, "1080");
        assert_eq!(saved.finished_series_quality, "bd_only");
        assert!(saved.prefer_subs);
        assert!(saved.upgrade_search_enabled);
        assert!(!saved.seadex_enabled);
        assert_eq!(saved.default_custom_query_tokens, "submit-tokens");
        assert_eq!(saved.default_restrict_to_uploader, "submit-uploader");
        // Cross-tab fields (General + Integrations) stay at seed.
        assert_eq!(saved.media_root, "/seed/media");
        assert_eq!(saved.jellyfin_url, "http://jelly.seed:8096");
        assert_eq!(saved.qbit_url, "http://qbit.seed:8080");
    }

    #[tokio::test]
    async fn integrations_submit_non_htmx_round_trips_every_field() {
        // Mutation-killer for the 22 Integrations field-write
        // mutants the cargo-mutants run flagged. Same shape as
        // the General + Quality versions: distinct seed, distinct
        // submit, per-field assertion. Empty Jellyfin URL so
        // the connection-test side effect is a no-op (avoids
        // hitting an unreachable host and burning the connect
        // timeout in every test run).
        let db = in_memory_pool().await;
        seed_distinct_config(&db).await;
        let state = build_test_app_state(db.clone(), None);
        let resp = settings_integrations_submit(
            State(state),
            HxRequest(false),
            axum::Form(IntegrationsForm {
                active_client: "transmission".to_string(), // seed=deluge
                qbit_url: "http://qbit.submit:9090".to_string(), // seed=...:8080
                qbit_user: "qbit-submit-user".to_string(),
                qbit_pass: "qbit-submit-pass".to_string(),
                qbit_category: "qbit-submit-cat".to_string(),
                qbit_download_path: "/submit/qbit".to_string(),
                deluge_url: "http://deluge.submit:8112".to_string(),
                deluge_password: "deluge-submit-pass".to_string(),
                deluge_label: "deluge-submit-label".to_string(),
                deluge_download_path: "/submit/deluge".to_string(),
                transmission_url: "http://trans.submit:9091".to_string(),
                transmission_user: "trans-submit-user".to_string(),
                transmission_password: "trans-submit-pass".to_string(),
                transmission_label: "trans-submit-label".to_string(),
                transmission_download_path: "/submit/trans".to_string(),
                rtorrent_url: "http://rt.submit:8081".to_string(),
                rtorrent_user: "rt-submit-user".to_string(),
                rtorrent_password: "rt-submit-pass".to_string(),
                rtorrent_label: "rt-submit-label".to_string(),
                rtorrent_download_path: "/submit/rt".to_string(),
                jellyfin_url: String::new(), // empty — skips connection-test
                jellyfin_api_key: String::new(),
                sonarr_enabled: Some(String::new()), // seed=false → submit→true
                sonarr_api_key: Some("sonarr-submit-key".to_string()),
                radarr_enabled: None, // seed=true → submit→false
                radarr_api_key: Some("radarr-submit-key".to_string()),
                external_sync_interval_minutes: Some(120), // seed=60
            }),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        let body = body_string(resp).await;
        assert!(
            body.contains("<h2>Settings</h2>"),
            "non-HTMX response must be the full SettingsTemplate, not the partial"
        );
        let saved = config::get_config(&db)
            .await
            .expect("get_config")
            .expect("config row");
        assert_eq!(saved.active_client, "transmission");
        assert_eq!(saved.qbit_url, "http://qbit.submit:9090");
        assert_eq!(saved.qbit_user, "qbit-submit-user");
        assert_eq!(saved.qbit_pass, "qbit-submit-pass");
        assert_eq!(saved.qbit_category, "qbit-submit-cat");
        assert_eq!(saved.qbit_download_path, "/submit/qbit");
        assert_eq!(saved.deluge_url, "http://deluge.submit:8112");
        assert_eq!(saved.deluge_password, "deluge-submit-pass");
        assert_eq!(saved.deluge_label, "deluge-submit-label");
        assert_eq!(saved.deluge_download_path, "/submit/deluge");
        assert_eq!(saved.transmission_url, "http://trans.submit:9091");
        assert_eq!(saved.transmission_user, "trans-submit-user");
        assert_eq!(saved.transmission_password, "trans-submit-pass");
        assert_eq!(saved.transmission_label, "trans-submit-label");
        assert_eq!(saved.transmission_download_path, "/submit/trans");
        assert_eq!(saved.rtorrent_url, "http://rt.submit:8081");
        assert_eq!(saved.rtorrent_user, "rt-submit-user");
        assert_eq!(saved.rtorrent_password, "rt-submit-pass");
        assert_eq!(saved.rtorrent_label, "rt-submit-label");
        assert_eq!(saved.rtorrent_download_path, "/submit/rt");
        assert!(saved.jellyfin_url.is_empty());
        assert!(saved.jellyfin_api_key.is_empty());
        assert!(saved.sonarr_enabled);
        assert_eq!(saved.sonarr_api_key, "sonarr-submit-key");
        assert!(!saved.radarr_enabled);
        assert_eq!(saved.radarr_api_key, "radarr-submit-key");
        // The picker lives on General now; an Integrations save preserves it.
        assert_eq!(saved.grab_preview_mode, "never");
        assert_eq!(saved.external_sync_interval_minutes, 120);
        // Cross-tab fields stay at seed values.
        assert_eq!(saved.media_root, "/seed/media");
        assert_eq!(saved.preferred_resolution, "720");
    }

    // ─── media_root accessibility-warning paths ──────────────────
    // Three tests pinning the `if !cfg.media_root.is_empty() &&
    // !std::path::Path::new(&cfg.media_root).is_dir()` branch in
    // settings_general_submit. The cargo-mutants run flagged 4
    // missed mutants on this expression (replace && with ||,
    // delete each !) because no test ever exercised the warning
    // surface. These three tests cover the three legitimate
    // states (empty, non-existent path, real dir) so any flip of
    // the boolean logic produces a wrong output for at least one
    // input.

    /// Empty media_root → no warning. Mutating `!cfg.media_root.is_empty()`
    /// to drop the `!` would emit a warning here (since
    /// !"".is_empty() is false, and `false && X` short-circuits).
    #[tokio::test]
    async fn general_tab_save_persists_misgrab_auto_remove_off_and_on() {
        let db = in_memory_pool().await;
        seed_initial_config(&db).await;
        let state = build_test_app_state(db.clone(), None);
        let form = |checked: bool| GeneralForm {
            media_root: String::new(),
            title_language: "english".to_string(),
            rss_enabled: None,
            rss_interval_minutes: 15,
            disable_nyaa_rss: None,
            post_processing_enabled: None,
            post_processing_mode: "hardlink".to_string(),
            search_on_monitoring_change: None,
            manual_search_auto_add: None,
            misgrab_auto_remove: checked.then(String::new),
            grab_preview_mode: None,
            auto_grab_on_add: None,
            allow_non_english: None,
            recycle_bin_path: String::new(),
            recycle_bin_age_days: 30,
            series_folder_format: String::new(),
            season_folder_format: String::new(),
            episode_file_format: String::new(),
            backup_schedule: String::new(),
            backup_directory: String::new(),
            backup_retention_count: 7,
            backup_include_artwork: None,
        };
        // An unchecked box is absent from the POST body: off.
        let _ = settings_general_submit(
            State(state.clone()),
            HxRequest(true),
            axum::Form(form(false)),
        )
        .await
        .into_response();
        let saved = config::get_config(&db).await.unwrap().unwrap();
        assert!(!saved.misgrab_auto_remove, "unchecked saves off");
        let _ = settings_general_submit(State(state), HxRequest(true), axum::Form(form(true)))
            .await
            .into_response();
        let saved = config::get_config(&db).await.unwrap().unwrap();
        assert!(saved.misgrab_auto_remove, "checked saves on");
    }

    #[tokio::test]
    async fn general_save_with_empty_media_root_emits_no_warning() {
        let db = in_memory_pool().await;
        seed_initial_config(&db).await;
        let state = build_test_app_state(db.clone(), None);
        let resp = settings_general_submit(
            State(state),
            HxRequest(true),
            axum::Form(GeneralForm {
                media_root: String::new(),
                title_language: "english".to_string(),
                rss_enabled: None,
                rss_interval_minutes: 15,
                disable_nyaa_rss: None,
                post_processing_enabled: None,
                post_processing_mode: "hardlink".to_string(),
                search_on_monitoring_change: None,
                manual_search_auto_add: None,
                misgrab_auto_remove: None,
                grab_preview_mode: None,
                auto_grab_on_add: None,
                allow_non_english: None,
                recycle_bin_path: String::new(),
                recycle_bin_age_days: 30,
                series_folder_format: String::new(),
                season_folder_format: String::new(),
                episode_file_format: String::new(),
                backup_schedule: String::new(),
                backup_directory: String::new(),
                backup_retention_count: 7,
                backup_include_artwork: None,
            }),
        )
        .await
        .into_response();
        let body = body_string(resp).await;
        assert!(body.contains("Settings saved."));
        assert!(
            !body.contains("not accessible"),
            "empty media_root must not surface the inaccessible-path warning"
        );
    }

    /// Non-existent media_root → warning surfaces. Mutating the
    /// `&&` to `||` would still warn here (since both branches
    /// are true), but the `if !empty` mutation that always-emits
    /// would be caught here too.
    #[tokio::test]
    async fn general_save_with_nonexistent_media_root_emits_warning() {
        let db = in_memory_pool().await;
        seed_initial_config(&db).await;
        let state = build_test_app_state(db.clone(), None);
        let resp = settings_general_submit(
            State(state),
            HxRequest(true),
            axum::Form(GeneralForm {
                media_root: "/nonexistent-test-path-9b3a2".to_string(),
                title_language: "english".to_string(),
                rss_enabled: None,
                rss_interval_minutes: 15,
                disable_nyaa_rss: None,
                post_processing_enabled: None,
                post_processing_mode: "hardlink".to_string(),
                search_on_monitoring_change: None,
                manual_search_auto_add: None,
                misgrab_auto_remove: None,
                grab_preview_mode: None,
                auto_grab_on_add: None,
                allow_non_english: None,
                recycle_bin_path: String::new(),
                recycle_bin_age_days: 30,
                series_folder_format: String::new(),
                season_folder_format: String::new(),
                episode_file_format: String::new(),
                backup_schedule: String::new(),
                backup_directory: String::new(),
                backup_retention_count: 7,
                backup_include_artwork: None,
            }),
        )
        .await
        .into_response();
        let body = body_string(resp).await;
        assert!(body.contains("Settings saved."));
        assert!(
            body.contains("not accessible"),
            "non-existent media_root must surface the inaccessible-path warning"
        );
        assert!(body.contains("/nonexistent-test-path-9b3a2"));
    }

    /// media_root pointing at a real directory → no warning.
    /// Mutating `!Path::is_dir()` to drop the `!` would emit a
    /// warning here (since is_dir() is true, and `X && true`
    /// passes both checks → the warning fires when it shouldn't).
    #[tokio::test]
    async fn general_save_with_existing_media_root_emits_no_warning() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().to_string_lossy().into_owned();
        let db = in_memory_pool().await;
        seed_initial_config(&db).await;
        let state = build_test_app_state(db.clone(), None);
        let resp = settings_general_submit(
            State(state),
            HxRequest(true),
            axum::Form(GeneralForm {
                media_root: path.clone(),
                title_language: "english".to_string(),
                rss_enabled: None,
                rss_interval_minutes: 15,
                disable_nyaa_rss: None,
                post_processing_enabled: None,
                post_processing_mode: "hardlink".to_string(),
                search_on_monitoring_change: None,
                manual_search_auto_add: None,
                misgrab_auto_remove: None,
                grab_preview_mode: None,
                auto_grab_on_add: None,
                allow_non_english: None,
                recycle_bin_path: String::new(),
                recycle_bin_age_days: 30,
                series_folder_format: String::new(),
                season_folder_format: String::new(),
                episode_file_format: String::new(),
                backup_schedule: String::new(),
                backup_directory: String::new(),
                backup_retention_count: 7,
                backup_include_artwork: None,
            }),
        )
        .await
        .into_response();
        let body = body_string(resp).await;
        assert!(body.contains("Settings saved."));
        assert!(
            !body.contains("not accessible"),
            "media_root pointing at an existing dir must not surface the warning"
        );
    }

    // ─── Validation coerce-on-bad-value paths ─────────────────────
    // The cargo-mutants run flagged delete-match-arm mutants on
    // every validation match in the per-tab handlers (e.g.,
    // `match form.post_processing_mode.as_str() { "move" |
    // "copy" | "hardlink" => form.post_processing_mode, _ =>
    // "hardlink".to_string() }`). Tests that submit only valid
    // values can't tell the difference between "valid arm
    // matched and returned form value" and "valid arm deleted
    // → fall through to default which happened to also equal
    // the valid value." The fix: submit a deliberately-invalid
    // value and assert the handler coerces to the documented
    // default. If the valid arm is deleted, the test still
    // passes (because it asserted the default); but if the
    // *default* arm is deleted, the test fails. Asymmetric but
    // useful: catches the half of the mutation surface that
    // actually changes behavior end-to-end.

    #[tokio::test]
    async fn general_save_coerces_invalid_post_processing_mode_to_hardlink() {
        let db = in_memory_pool().await;
        seed_initial_config(&db).await;
        let state = build_test_app_state(db.clone(), None);
        let _ = settings_general_submit(
            State(state),
            HxRequest(true),
            axum::Form(GeneralForm {
                media_root: String::new(),
                title_language: "english".to_string(),
                rss_enabled: None,
                rss_interval_minutes: 15,
                disable_nyaa_rss: None,
                post_processing_enabled: None,
                post_processing_mode: "garbage".to_string(),
                search_on_monitoring_change: None,
                manual_search_auto_add: None,
                misgrab_auto_remove: None,
                grab_preview_mode: None,
                auto_grab_on_add: None,
                allow_non_english: None,
                recycle_bin_path: String::new(),
                recycle_bin_age_days: 30,
                series_folder_format: String::new(),
                season_folder_format: String::new(),
                episode_file_format: String::new(),
                backup_schedule: String::new(),
                backup_directory: String::new(),
                backup_retention_count: 7,
                backup_include_artwork: None,
            }),
        )
        .await
        .into_response();
        let saved = config::get_config(&db)
            .await
            .expect("get_config")
            .expect("config row");
        assert_eq!(saved.post_processing_mode, "hardlink");
    }

    #[tokio::test]
    async fn general_save_coerces_invalid_title_language_to_english() {
        let db = in_memory_pool().await;
        seed_initial_config(&db).await;
        let state = build_test_app_state(db.clone(), None);
        let _ = settings_general_submit(
            State(state),
            HxRequest(true),
            axum::Form(GeneralForm {
                media_root: String::new(),
                title_language: "klingon".to_string(),
                rss_enabled: None,
                rss_interval_minutes: 15,
                disable_nyaa_rss: None,
                post_processing_enabled: None,
                post_processing_mode: "hardlink".to_string(),
                search_on_monitoring_change: None,
                manual_search_auto_add: None,
                misgrab_auto_remove: None,
                grab_preview_mode: None,
                auto_grab_on_add: None,
                allow_non_english: None,
                recycle_bin_path: String::new(),
                recycle_bin_age_days: 30,
                series_folder_format: String::new(),
                season_folder_format: String::new(),
                episode_file_format: String::new(),
                backup_schedule: String::new(),
                backup_directory: String::new(),
                backup_retention_count: 7,
                backup_include_artwork: None,
            }),
        )
        .await
        .into_response();
        let saved = config::get_config(&db)
            .await
            .expect("get_config")
            .expect("config row");
        assert_eq!(saved.title_language, "english");
    }

    #[tokio::test]
    async fn quality_save_coerces_invalid_finished_series_quality_to_prefer_bd() {
        let db = in_memory_pool().await;
        seed_initial_config(&db).await;
        let state = build_test_app_state(db.clone(), None);
        let _ = settings_quality_submit(
            State(state),
            HxRequest(true),
            axum::Form(QualityForm {
                preferred_groups: String::new(),
                blocked_groups: String::new(),
                preferred_source: "web".to_string(),
                preferred_resolution: "1080".to_string(),
                cutoff_source: "bluray".to_string(),
                cutoff_resolution: "1080".to_string(),
                finished_series_quality: "garbage".to_string(),
                prefer_subs: "1".to_string(),
                upgrade_search_enabled: None,
                seadex_enabled: None,
                default_custom_query_tokens: None,
                default_restrict_to_uploader: None,
            }),
        )
        .await
        .into_response();
        let saved = config::get_config(&db)
            .await
            .expect("get_config")
            .expect("config row");
        assert_eq!(saved.finished_series_quality, "prefer_bd");
    }

    #[tokio::test]
    async fn integrations_save_coerces_unknown_active_client_to_qbittorrent() {
        let db = in_memory_pool().await;
        seed_initial_config(&db).await;
        let state = build_test_app_state(db.clone(), None);
        let _ = settings_integrations_submit(
            State(state),
            HxRequest(true),
            axum::Form(IntegrationsForm {
                active_client: "garbage".to_string(),
                qbit_url: String::new(),
                qbit_user: String::new(),
                qbit_pass: String::new(),
                qbit_category: String::new(),
                qbit_download_path: String::new(),
                deluge_url: String::new(),
                deluge_password: String::new(),
                deluge_label: String::new(),
                deluge_download_path: String::new(),
                transmission_url: String::new(),
                transmission_user: String::new(),
                transmission_password: String::new(),
                transmission_label: String::new(),
                transmission_download_path: String::new(),
                rtorrent_url: String::new(),
                rtorrent_user: String::new(),
                rtorrent_password: String::new(),
                rtorrent_label: String::new(),
                rtorrent_download_path: String::new(),
                jellyfin_url: String::new(),
                jellyfin_api_key: String::new(),
                sonarr_enabled: None,
                sonarr_api_key: None,
                radarr_enabled: None,
                radarr_api_key: None,
                external_sync_interval_minutes: None,
            }),
        )
        .await
        .into_response();
        let saved = config::get_config(&db)
            .await
            .expect("get_config")
            .expect("config row");
        assert_eq!(saved.active_client, "qbittorrent");
    }

    // ─── general_response cfg-fallback path ───────────────────────

    /// `general_response`'s Ok(Some(c)) match arm reads the cfg
    /// from the DB when the caller passes `cfg=None`. cargo-
    /// mutants flagged the deletion of this arm because no test
    /// hit the path with both (a) `cfg=None` AND (b) a real config
    /// row in the DB. The existing
    /// `general_submit_with_no_config_row_renders_friendly_error`
    /// has cfg=None *and* no DB row, so it falls through to
    /// Config::default() either way.
    ///
    /// This test calls `general_response` directly with cfg=None
    /// and a seeded distinct row in the DB, then asserts the
    /// rendered response carries the seeded value. If the match
    /// arm is deleted, the response would render
    /// Config::default() values (empty media_root) and the
    /// assertion would fail.
    #[tokio::test]
    async fn general_response_with_no_cfg_falls_back_to_db_row() {
        let db = in_memory_pool().await;
        seed_distinct_config(&db).await;
        let state = build_test_app_state(db.clone(), None);
        let resp = general_response(&state, None, None, None, true).await;
        let body = body_string(resp).await;
        // The seeded media_root is "/seed/media" — should render
        // in the form's value="..." attribute. If the Ok(Some)
        // match arm were deleted, body would contain
        // Config::default()'s empty media_root instead.
        assert!(
            body.contains("/seed/media"),
            "general_response with cfg=None must read the row from the DB"
        );
    }

    /// Companion to `general_response_with_no_cfg_falls_back_to_db_row`
    /// — same Ok(Some(c)) cfg-fallback path on the Quality side.
    #[tokio::test]
    async fn quality_response_with_no_cfg_falls_back_to_db_row() {
        let db = in_memory_pool().await;
        seed_distinct_config(&db).await;
        let state = build_test_app_state(db.clone(), None);
        let resp = quality_response(&state, None, None, None, true).await;
        let body = body_string(resp).await;
        // Seeded preferred_groups = "SeedPreferred" renders as a
        // form input value. Default Config has empty
        // preferred_groups; if the Ok(Some) arm were deleted,
        // this assertion would fail.
        assert!(
            body.contains("SeedPreferred"),
            "quality_response with cfg=None must read the row from the DB"
        );
    }

    /// Companion to the General + Quality fallback tests — same
    /// Ok(Some(c)) shape on the Integrations side.
    #[tokio::test]
    async fn integrations_response_with_no_cfg_falls_back_to_db_row() {
        let db = in_memory_pool().await;
        seed_distinct_config(&db).await;
        let state = build_test_app_state(db.clone(), None);
        let resp = integrations_response(&state, None, None, None, true).await;
        let body = body_string(resp).await;
        // Seeded jellyfin_url renders as an input value attribute.
        assert!(
            body.contains("http://jelly.seed:8096"),
            "integrations_response with cfg=None must read the row from the DB"
        );
    }

    // ─── Integrations active_client coercion (per-arm coverage) ───
    // The comprehensive `integrations_submit_non_htmx_round_trips_every_field`
    // test only submits `active_client="transmission"`, so cargo-
    // mutants can delete the "deluge" or "rtorrent" arms and still
    // pass (those arms aren't exercised). Two small tests cover
    // the remaining valid arms, plus the "qbittorrent" arm gets
    // its coverage from the `integrations_save_coerces_unknown_active_client_to_qbittorrent`
    // default-fallthrough test above.

    #[tokio::test]
    async fn integrations_save_preserves_active_client_deluge() {
        let db = in_memory_pool().await;
        seed_initial_config(&db).await;
        let state = build_test_app_state(db.clone(), None);
        let _ = settings_integrations_submit(
            State(state),
            HxRequest(true),
            axum::Form(IntegrationsForm {
                active_client: "deluge".to_string(),
                qbit_url: String::new(),
                qbit_user: String::new(),
                qbit_pass: String::new(),
                qbit_category: String::new(),
                qbit_download_path: String::new(),
                deluge_url: String::new(),
                deluge_password: String::new(),
                deluge_label: String::new(),
                deluge_download_path: String::new(),
                transmission_url: String::new(),
                transmission_user: String::new(),
                transmission_password: String::new(),
                transmission_label: String::new(),
                transmission_download_path: String::new(),
                rtorrent_url: String::new(),
                rtorrent_user: String::new(),
                rtorrent_password: String::new(),
                rtorrent_label: String::new(),
                rtorrent_download_path: String::new(),
                jellyfin_url: String::new(),
                jellyfin_api_key: String::new(),
                sonarr_enabled: None,
                sonarr_api_key: None,
                radarr_enabled: None,
                radarr_api_key: None,
                external_sync_interval_minutes: None,
            }),
        )
        .await
        .into_response();
        let saved = config::get_config(&db)
            .await
            .expect("get_config")
            .expect("config row");
        assert_eq!(saved.active_client, "deluge");
    }

    #[tokio::test]
    async fn integrations_save_preserves_active_client_rtorrent() {
        let db = in_memory_pool().await;
        seed_initial_config(&db).await;
        let state = build_test_app_state(db.clone(), None);
        let _ = settings_integrations_submit(
            State(state),
            HxRequest(true),
            axum::Form(IntegrationsForm {
                active_client: "rtorrent".to_string(),
                qbit_url: String::new(),
                qbit_user: String::new(),
                qbit_pass: String::new(),
                qbit_category: String::new(),
                qbit_download_path: String::new(),
                deluge_url: String::new(),
                deluge_password: String::new(),
                deluge_label: String::new(),
                deluge_download_path: String::new(),
                transmission_url: String::new(),
                transmission_user: String::new(),
                transmission_password: String::new(),
                transmission_label: String::new(),
                transmission_download_path: String::new(),
                rtorrent_url: String::new(),
                rtorrent_user: String::new(),
                rtorrent_password: String::new(),
                rtorrent_label: String::new(),
                rtorrent_download_path: String::new(),
                jellyfin_url: String::new(),
                jellyfin_api_key: String::new(),
                sonarr_enabled: None,
                sonarr_api_key: None,
                radarr_enabled: None,
                radarr_api_key: None,
                external_sync_interval_minutes: None,
            }),
        )
        .await
        .into_response();
        let saved = config::get_config(&db)
            .await
            .expect("get_config")
            .expect("config row");
        assert_eq!(saved.active_client, "rtorrent");
    }

    // ─── Jellyfin connection-test gate ────────────────────────────
    // The gate `if !cfg.jellyfin_url.is_empty() &&
    // !cfg.jellyfin_api_key.is_empty()` decides whether to attempt
    // a Jellyfin connection on Integrations save. cargo-mutants
    // flagged 3 boolean-op mutants on this expression: the &&
    // flipping to ||, and each ! being dropped. Tests that pass
    // both fields non-empty (the existing browser-e2e test does
    // this with 127.0.0.1:1) miss the case where exactly one is
    // empty. These two tests cover (url-only, key-only) so any
    // boolean-op flip produces a wrong output for at least one
    // input — without the gate, an empty URL or empty API key
    // would still attempt a connection and surface a "connection
    // failed:" notice.

    #[tokio::test]
    async fn integrations_save_with_only_jellyfin_url_skips_connection_test() {
        let db = in_memory_pool().await;
        seed_initial_config(&db).await;
        let state = build_test_app_state(db.clone(), None);
        let resp = settings_integrations_submit(
            State(state),
            HxRequest(false),
            axum::Form(IntegrationsForm {
                active_client: "qbittorrent".to_string(),
                qbit_url: String::new(),
                qbit_user: String::new(),
                qbit_pass: String::new(),
                qbit_category: String::new(),
                qbit_download_path: String::new(),
                deluge_url: String::new(),
                deluge_password: String::new(),
                deluge_label: String::new(),
                deluge_download_path: String::new(),
                transmission_url: String::new(),
                transmission_user: String::new(),
                transmission_password: String::new(),
                transmission_label: String::new(),
                transmission_download_path: String::new(),
                rtorrent_url: String::new(),
                rtorrent_user: String::new(),
                rtorrent_password: String::new(),
                rtorrent_label: String::new(),
                rtorrent_download_path: String::new(),
                jellyfin_url: "http://127.0.0.1:1".to_string(), // would-fail address
                jellyfin_api_key: String::new(),                // empty — gate must skip
                sonarr_enabled: None,
                sonarr_api_key: None,
                radarr_enabled: None,
                radarr_api_key: None,
                external_sync_interval_minutes: None,
            }),
        )
        .await
        .into_response();
        let body = body_string(resp).await;
        assert!(
            !body.contains("Jellyfin connection failed"),
            "empty jellyfin_api_key must skip the connection test — \
             a `&&` → `||` mutation would attempt to connect against \
             the URL and surface 'connection failed:' here"
        );
        assert!(
            !body.contains("Jellyfin") || !body.contains("connected"),
            "skipped gate must not emit a 'connected' notice either"
        );
    }

    #[tokio::test]
    async fn integrations_save_with_only_jellyfin_api_key_skips_connection_test() {
        let db = in_memory_pool().await;
        seed_initial_config(&db).await;
        let state = build_test_app_state(db.clone(), None);
        let resp = settings_integrations_submit(
            State(state),
            HxRequest(false),
            axum::Form(IntegrationsForm {
                active_client: "qbittorrent".to_string(),
                qbit_url: String::new(),
                qbit_user: String::new(),
                qbit_pass: String::new(),
                qbit_category: String::new(),
                qbit_download_path: String::new(),
                deluge_url: String::new(),
                deluge_password: String::new(),
                deluge_label: String::new(),
                deluge_download_path: String::new(),
                transmission_url: String::new(),
                transmission_user: String::new(),
                transmission_password: String::new(),
                transmission_label: String::new(),
                transmission_download_path: String::new(),
                rtorrent_url: String::new(),
                rtorrent_user: String::new(),
                rtorrent_password: String::new(),
                rtorrent_label: String::new(),
                rtorrent_download_path: String::new(),
                jellyfin_url: String::new(), // empty — gate must skip
                jellyfin_api_key: "some-key".to_string(), // present
                sonarr_enabled: None,
                sonarr_api_key: None,
                radarr_enabled: None,
                radarr_api_key: None,
                external_sync_interval_minutes: None,
            }),
        )
        .await
        .into_response();
        let body = body_string(resp).await;
        assert!(
            !body.contains("Jellyfin connection failed"),
            "empty jellyfin_url must skip the connection test"
        );
    }

    /// Regression for PR 133 review item #3: read-modify-write
    /// race across concurrent saves. Without `CONFIG_WRITE_LOCK`,
    /// the General handler reading existing_cfg + the Quality
    /// handler reading existing_cfg in parallel both see the
    /// pre-mutation row, then each writes back its own merge —
    /// the second writer's write loses whatever the first
    /// writer changed (because the second writer's struct-update
    /// merge built on a stale snapshot).
    ///
    /// With the lock, the second handler waits for the first to
    /// commit, reads the post-first-save row, and merges its
    /// change on top. Both fields land.
    ///
    /// Two concurrent saves via `tokio::join!`: General sets
    /// `title_language = "romaji"`, Quality sets
    /// `preferred_resolution = "2160"`. Final config must have
    /// **both** (the loser's write would silently drop one).
    #[tokio::test]
    async fn concurrent_general_and_quality_saves_dont_lose_updates() {
        let db = in_memory_pool().await;
        seed_initial_config(&db).await;
        let state = build_test_app_state(db.clone(), None);

        let general_state = state.clone();
        let quality_state = state.clone();
        let general = settings_general_submit(
            State(general_state),
            HxRequest(false),
            axum::Form(GeneralForm {
                media_root: String::new(),
                title_language: "romaji".to_string(),
                rss_enabled: None,
                rss_interval_minutes: 15,
                disable_nyaa_rss: None,
                post_processing_enabled: None,
                post_processing_mode: "hardlink".to_string(),
                search_on_monitoring_change: None,
                manual_search_auto_add: None,
                misgrab_auto_remove: None,
                grab_preview_mode: None,
                auto_grab_on_add: None,
                allow_non_english: None,
                recycle_bin_path: String::new(),
                recycle_bin_age_days: 30,
                series_folder_format: String::new(),
                season_folder_format: String::new(),
                episode_file_format: String::new(),
                backup_schedule: String::new(),
                backup_directory: String::new(),
                backup_retention_count: 7,
                backup_include_artwork: None,
            }),
        );
        let quality = settings_quality_submit(
            State(quality_state),
            HxRequest(false),
            axum::Form(QualityForm {
                preferred_groups: String::new(),
                blocked_groups: String::new(),
                preferred_source: "web".to_string(),
                preferred_resolution: "2160".to_string(),
                cutoff_source: "bluray".to_string(),
                cutoff_resolution: "1080".to_string(),
                finished_series_quality: "prefer_bd".to_string(),
                prefer_subs: "1".to_string(),
                upgrade_search_enabled: None,
                seadex_enabled: None,
                default_custom_query_tokens: None,
                default_restrict_to_uploader: None,
            }),
        );
        let (_a, _b) = tokio::join!(general, quality);

        let saved = config::get_config(&db)
            .await
            .expect("get_config")
            .expect("config row");
        // Both handlers' field changes must land — interleaving
        // would have dropped one of them.
        assert_eq!(saved.title_language, "romaji");
        assert_eq!(saved.preferred_resolution, "2160");
    }

    /// Companion regression: the early-return path when the
    /// config row is missing. Surfaces the "No config row found —
    /// run /setup first." error string in the response, which
    /// the operator sees when they hit the endpoint before
    /// completing first-run setup.
    #[tokio::test]
    async fn general_submit_with_no_config_row_renders_friendly_error() {
        // Note: NO seed_initial_config call here — pool is empty.
        let db = in_memory_pool().await;
        let state = build_test_app_state(db, None);
        let resp = settings_general_submit(
            State(state),
            HxRequest(false),
            axum::Form(GeneralForm {
                media_root: String::new(),
                title_language: "english".to_string(),
                rss_enabled: None,
                rss_interval_minutes: 15,
                disable_nyaa_rss: None,
                post_processing_enabled: None,
                post_processing_mode: "hardlink".to_string(),
                search_on_monitoring_change: None,
                manual_search_auto_add: None,
                misgrab_auto_remove: None,
                grab_preview_mode: None,
                auto_grab_on_add: None,
                allow_non_english: None,
                recycle_bin_path: String::new(),
                recycle_bin_age_days: 30,
                series_folder_format: String::new(),
                season_folder_format: String::new(),
                episode_file_format: String::new(),
                backup_schedule: String::new(),
                backup_directory: String::new(),
                backup_retention_count: 7,
                backup_include_artwork: None,
            }),
        )
        .await
        .into_response();
        let body = body_string(resp).await;
        assert!(
            body.contains("No config row found"),
            "expected friendly first-run error in response body"
        );
    }
}

/// Issue #124: naming templates on the General tab.
mod naming_templates {
    use super::super::*;
    use crate::services::naming::{
        DEFAULT_EPISODE_FILE_FORMAT, DEFAULT_SEASON_FOLDER_FORMAT, DEFAULT_SERIES_FOLDER_FORMAT,
    };
    use crate::test_support::{build_test_app_state, in_memory_pool};
    use axum::body::to_bytes;
    use axum::extract::State;
    use axum::response::IntoResponse;
    use axum_htmx::HxRequest;

    async fn body_string(resp: axum::response::Response) -> String {
        let bytes = to_bytes(resp.into_body(), usize::MAX).await.expect("body");
        String::from_utf8(bytes.to_vec()).expect("utf8 body")
    }

    async fn seed_config(db: &sqlx::SqlitePool) {
        config::save_config(db, &config::Config::default())
            .await
            .expect("seed config");
    }

    fn form(series: &str, season: &str, episode: &str) -> GeneralForm {
        GeneralForm {
            media_root: "/media".to_string(),
            title_language: "english".to_string(),
            rss_enabled: None,
            rss_interval_minutes: 15,
            disable_nyaa_rss: None,
            post_processing_enabled: Some(String::new()),
            post_processing_mode: "hardlink".to_string(),
            search_on_monitoring_change: None,
            manual_search_auto_add: None,
            misgrab_auto_remove: None,
            grab_preview_mode: None,
            auto_grab_on_add: None,
            allow_non_english: None,
            recycle_bin_path: String::new(),
            recycle_bin_age_days: 14,
            series_folder_format: series.to_string(),
            season_folder_format: season.to_string(),
            episode_file_format: episode.to_string(),
            backup_schedule: String::new(),
            backup_directory: String::new(),
            backup_retention_count: 7,
            backup_include_artwork: None,
        }
    }

    #[tokio::test]
    async fn custom_templates_round_trip_and_render_previews() {
        let db = in_memory_pool().await;
        seed_config(&db).await;
        let state = build_test_app_state(db.clone(), None);
        let resp = settings_general_submit(
            State(state),
            HxRequest(true),
            axum::Form(form(
                "{series.title} ({series.year})",
                "S{season.number:00}",
                "[{group}] {series.title} - {episode.number:00} [{quality.full}]{ext}",
            )),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        let body = body_string(resp).await;
        assert!(body.contains("Settings saved."), "{body}");
        assert!(
            body.contains("[SubsPlease] Sousou no Frieren - 07 [1080p WEB-DL].mkv"),
            "the saved form re-renders the sample under the episode field: {body}"
        );
        assert!(
            body.contains("Sousou no Frieren (2023)/S01/[SubsPlease]"),
            "{body}"
        );
        let saved = config::get_config(&db).await.unwrap().unwrap();
        assert_eq!(saved.series_folder_format, "{series.title} ({series.year})");
        assert_eq!(saved.season_folder_format, "S{season.number:00}");
        assert_eq!(
            saved.episode_file_format,
            "[{group}] {series.title} - {episode.number:00} [{quality.full}]{ext}"
        );
    }

    #[tokio::test]
    async fn empty_template_fields_store_the_defaults() {
        let db = in_memory_pool().await;
        seed_config(&db).await;
        let state = build_test_app_state(db.clone(), None);
        let _ = settings_general_submit(
            State(state),
            HxRequest(true),
            axum::Form(form("", "  ", "")),
        )
        .await
        .into_response();
        let saved = config::get_config(&db).await.unwrap().unwrap();
        assert_eq!(saved.series_folder_format, DEFAULT_SERIES_FOLDER_FORMAT);
        assert_eq!(saved.season_folder_format, DEFAULT_SEASON_FOLDER_FORMAT);
        assert_eq!(saved.episode_file_format, DEFAULT_EPISODE_FILE_FORMAT);
    }

    #[tokio::test]
    async fn invalid_template_is_rejected_without_saving_anything() {
        let db = in_memory_pool().await;
        seed_config(&db).await;
        let state = build_test_app_state(db.clone(), None);
        let mut f = form(
            "{series.title}",
            "Season {season.number:00}",
            "{series.title} - {episode.number:00}",
        );
        f.media_root = "/changed".to_string();
        let resp = settings_general_submit(State(state), HxRequest(true), axum::Form(f))
            .await
            .into_response();
        let body = body_string(resp).await;
        assert!(body.contains("must end with {ext}"), "{body}");
        assert!(
            body.contains("value=\"{series.title} - {episode.number:00}\""),
            "the rejected input stays in the field for editing: {body}"
        );
        let saved = config::get_config(&db).await.unwrap().unwrap();
        assert_eq!(
            saved.media_root, "",
            "nothing on the tab is saved when a template is rejected"
        );
        assert_eq!(saved.episode_file_format, DEFAULT_EPISODE_FILE_FORMAT);
    }

    #[tokio::test]
    async fn preview_endpoint_reports_each_field_and_the_sample_path() {
        let db = in_memory_pool().await;
        seed_config(&db).await;
        sqlx::query("UPDATE config SET media_root = '/srv/media/anime' WHERE id = 1")
            .execute(&db)
            .await
            .unwrap();
        let state = build_test_app_state(db.clone(), None);
        let out = naming::naming_preview(
            State(state.clone()),
            axum::Json(naming::NamingPreviewRequest {
                series_folder_format: "{series.title} ({series.year})".to_string(),
                season_folder_format: String::new(),
                episode_file_format: "{series.title} {episode.number}{ext}".to_string(),
            }),
        )
        .await
        .0;
        assert_eq!(out["ok"], true);
        assert_eq!(out["fields"]["series_folder_format"]["ok"], true);
        assert_eq!(
            out["fields"]["series_folder_format"]["preview"],
            "Sousou no Frieren (2023)"
        );
        assert_eq!(
            out["fields"]["season_folder_format"]["preview"],
            "Season 01"
        );
        assert_eq!(out["fields"]["episode_file_format"]["ok"], false);
        assert!(
            out["fields"]["episode_file_format"]["error"]
                .as_str()
                .unwrap()
                .contains("cannot read the episode number back")
        );
        assert_eq!(out["path"], "", "no combined path while a field is invalid");

        let out = naming::naming_preview(
            State(state),
            axum::Json(naming::NamingPreviewRequest {
                series_folder_format: String::new(),
                season_folder_format: String::new(),
                episode_file_format: String::new(),
            }),
        )
        .await
        .0;
        assert_eq!(
            out["path"],
            "Sousou no Frieren/Season 01/Sousou no Frieren - S01E07 - Like a Fairy Tale.mkv"
        );
        assert!(out["warning"].is_null());
    }

    #[tokio::test]
    async fn preview_warns_past_the_windows_path_limit() {
        let db = in_memory_pool().await;
        seed_config(&db).await;
        let long_root = format!("/{}", "m".repeat(230));
        sqlx::query("UPDATE config SET media_root = ? WHERE id = 1")
            .bind(&long_root)
            .execute(&db)
            .await
            .unwrap();
        let state = build_test_app_state(db.clone(), None);
        let out = naming::naming_preview(
            State(state),
            axum::Json(naming::NamingPreviewRequest {
                series_folder_format: String::new(),
                season_folder_format: String::new(),
                episode_file_format: String::new(),
            }),
        )
        .await
        .0;
        assert!(
            out["warning"]
                .as_str()
                .unwrap()
                .contains("Windows limits paths to 260"),
            "{out}"
        );
    }
}
