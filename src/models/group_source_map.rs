//! Release group → source classification map.
//!
//! Backs Layer 3 of the classification pipeline: when Layer 1 (filename
//! parsing) can't determine a source because the release group's naming
//! convention doesn't carry source keywords (SubsPlease, HorribleSubs,
//! VCB-Studio, …), look up the group in this table and emit its known
//! source as evidence.
//!
//! The table is seeded on migration with well-known groups and is also
//! user-editable via the settings UI. User edits set `is_user_edit = 1` so
//! re-running `seed_defaults` doesn't clobber them.

use sqlx::{Row, SqlitePool};

use crate::services::source::{Source, WebKind};

/// A single group → source mapping.
#[derive(Debug, Clone)]
pub struct GroupSourceEntry {
    pub group_name: String,
    pub source: Source,
    pub confidence: f32,
    pub is_user_edit: bool,
    pub notes: String,
    /// Optional Web sub-tier hint. Set on groups that ship **exclusively**
    /// re-encoded WEB releases (WEB-Rip). Ignored unless
    /// `source == Source::Web`.
    ///
    /// Since issue #48 the aggregator's label layer no longer
    /// distinguishes `WEBDL` from bare `WEB` — both render as
    /// `WEB-1080p`. The only remaining role of `web_kind = WebDl` on
    /// a group is CF `SourceSpecification` value-3 matching, and
    /// since the post-#48 predicate widened value 3 to match any
    /// `Source::Web && !WebRip`, even that hint isn't load-bearing
    /// for CFs — bare-Web releases already match. The column is kept
    /// for the `WebRip` side (groups that consistently ship re-
    /// encoded WEB) so the value-4 CF path can still gate on group
    /// identity without a filename token, and for future re-
    /// introduction of sub-tier-specific behavior.
    pub web_kind: WebKind,
}

/// Known-WEB-Rip groups, layered on top of `SEED_DEFAULTS` by the seed
/// pass. A group only appears here when its output is consistently one
/// sub-tier.
///
/// WebDl seeding was dropped (issue #48): the classifier's output
/// didn't change behavior enough to justify the UI asymmetry between
/// "WEBDL-1080p" (for seeded groups) and plain "WEB-1080p" (for
/// everyone else), and the group-map seeding was the only path that
/// produced WebDl without an explicit title token. Explicit-token
/// detection still fires in `source_filename.rs`; this slice is just
/// for groups whose filenames omit the token but whose sub-tier is
/// known. Currently empty — add WebRip-only groups here if needed.
pub const SEED_WEB_KIND: &[(&str, WebKind)] = &[
    // EMBER almost exclusively ships re-encoded WebRips, so when the group
    // prior fires we should resolve to the WebRip sub-tier rather than
    // leaving it as plain WEB. Confirmed from EMBER's Wajutsushi batch
    // (released 2024-12-20, four days after the show finished airing on
    // 2024-12-16) where ffprobe observed E-AC-3 audio (a WEB fingerprint)
    // on every episode.
    ("EMBER", WebKind::WebRip),
];

// ─────────────────────────────────────────────────────────────────────────
// Seed data
// ─────────────────────────────────────────────────────────────────────────

/// Built-in seed mappings applied on every migration.
///
/// Sourced from TRaSH Guides' Sonarr anime custom formats
/// (<https://github.com/TRaSH-Guides/Guides>), specifically the
/// `anime-bd-tier-*`, `anime-web-tier-*`, and `anime-raws` CF lists. A group
/// is only included here if it appears **exclusively** in the BD tiers or
/// **exclusively** in the WEB tiers — groups that show up in both (sam, FLE,
/// LYS1TH3A, LostYears, Arg0, Arid, Vodes, MTBB, Okay-Subs, Foxtrot, Pizza,
/// Reza, SCY, Baws, McBalls, Asakura, Commie, GJM, Chihiro, Dae, Judas, …)
/// are source-ambiguous and are left out so Layer 3 falls through to other
/// evidence instead of guessing. Groups from TRaSH's `anime-lq-groups`
/// blocklist (ASW, bonkai77, Trix, …) are intentionally omitted. That list
/// governs scoring, not source classification.
///
/// Confidence is 0.95 for single-source groups: a group's tier ranking
/// reflects encoding quality, not how reliably the source can be inferred,
/// but when a group works exclusively in one source the name alone is a
/// strong signal.
///
/// Entries are inserted with `INSERT OR IGNORE`, so user edits (which set
/// `is_user_edit = 1`) are never overwritten by the seed pass. One-shot
/// corrections to non-user-edited rows (e.g. "Judas was seeded at 0.95
/// before we realised it was mixed-source") live in `reconcile_seed_drift`
/// and run once per migration.
pub const SEED_DEFAULTS: &[(&str, Source, f32, &str)] = &[
    // ── Legacy BD encoders ────────────────────────────────────────────────
    // Well-known BD-only encoders that predate or sit outside the current
    // TRaSH custom formats but are still widely seeded.
    (
        "VCB-Studio",
        Source::BluRay,
        0.95,
        "legacy BD encode specialist",
    ),
    ("Coalgirls", Source::BluRay, 0.95, "legacy BD encoder"),
    // ── TRaSH BD Tier 01 ──────────────────────────────────────────────────
    ("DemiHuman", Source::BluRay, 0.95, "TRaSH BD tier 01"),
    ("Flugel", Source::BluRay, 0.95, "TRaSH BD tier 01"),
    ("Moxie", Source::BluRay, 0.95, "TRaSH BD tier 01"),
    ("NAN0", Source::BluRay, 0.95, "TRaSH BD tier 03"),
    // ── TRaSH BD Tier 02 ──────────────────────────────────────────────────
    ("Aergia", Source::BluRay, 0.95, "TRaSH BD tier 02"),
    ("FateSucks", Source::BluRay, 0.95, "TRaSH BD tier 02"),
    ("hchcsen", Source::BluRay, 0.95, "TRaSH BD tier 02"),
    ("hydes", Source::BluRay, 0.95, "TRaSH BD tier 02"),
    ("JOHNTiTOR", Source::BluRay, 0.95, "TRaSH BD tier 02"),
    ("JySzE", Source::BluRay, 0.95, "TRaSH BD tier 02"),
    ("koala", Source::BluRay, 0.95, "TRaSH BD tier 02"),
    ("Kulot", Source::BluRay, 0.95, "TRaSH BD tier 02"),
    ("Lulu", Source::BluRay, 0.95, "TRaSH BD tier 02"),
    ("Meakes", Source::BluRay, 0.95, "TRaSH BD tier 02"),
    ("Orphan", Source::BluRay, 0.95, "TRaSH BD tier 02"),
    ("PMR", Source::BluRay, 0.95, "TRaSH BD tier 03"),
    ("WAP", Source::BluRay, 0.95, "TRaSH BD tier 02"),
    ("YURI", Source::BluRay, 0.95, "TRaSH BD tier 02"),
    // ── TRaSH BD Tier 03 ──────────────────────────────────────────────────
    ("ARC", Source::BluRay, 0.95, "TRaSH BD tier 03"),
    ("BBT-RMX", Source::BluRay, 0.95, "TRaSH BD tier 03"),
    ("cappybara", Source::BluRay, 0.95, "TRaSH BD tier 03"),
    ("ChucksMux", Source::BluRay, 0.95, "TRaSH BD tier 03"),
    ("CRUCiBLE", Source::BluRay, 0.95, "TRaSH BD tier 03"),
    ("CUNNY", Source::BluRay, 0.95, "TRaSH BD tier 03"),
    ("Cunnysseur", Source::BluRay, 0.95, "TRaSH BD tier 03"),
    ("Doc", Source::BluRay, 0.95, "TRaSH BD tier 03"),
    ("fig", Source::BluRay, 0.95, "TRaSH BD tier 03"),
    ("Headpatter", Source::BluRay, 0.95, "TRaSH BD tier 03"),
    ("Inka-Subs", Source::BluRay, 0.95, "TRaSH BD tier 03"),
    ("LaCroiX", Source::BluRay, 0.95, "TRaSH BD tier 03"),
    ("Legion", Source::BluRay, 0.95, "TRaSH BD tier 03"),
    ("Mehul", Source::BluRay, 0.95, "TRaSH BD tier 03"),
    ("Mysteria", Source::BluRay, 0.95, "TRaSH BD tier 03"),
    ("Netaro", Source::BluRay, 0.95, "TRaSH BD tier 03"),
    ("Noiy", Source::BluRay, 0.95, "TRaSH BD tier 03"),
    ("npz", Source::BluRay, 0.95, "TRaSH BD tier 03"),
    ("NTRX", Source::BluRay, 0.95, "TRaSH BD tier 03"),
    ("P9", Source::BluRay, 0.95, "TRaSH BD tier 03"),
    ("RaiN", Source::BluRay, 0.95, "TRaSH BD tier 03"),
    ("RMX", Source::BluRay, 0.95, "TRaSH BD tier 03"),
    ("RUDY", Source::BluRay, 0.95, "TRaSH BD tier 03"),
    ("Sekkon", Source::BluRay, 0.95, "TRaSH BD tier 03"),
    ("Serendipity", Source::BluRay, 0.95, "TRaSH BD tier 03"),
    // sgt is listed in TRaSH BD tier 03 but has also released WEB on Nyaa,
    // so treat it as source-ambiguous and let other evidence decide.
    ("SubsMix", Source::BluRay, 0.95, "TRaSH BD tier 03"),
    ("uba", Source::BluRay, 0.95, "TRaSH BD tier 03"),
    // ── TRaSH BD Tier 04 ──────────────────────────────────────────────────
    ("ABdex", Source::BluRay, 0.95, "TRaSH BD tier 04"),
    ("Afro", Source::BluRay, 0.95, "TRaSH BD tier 04"),
    ("aRMX", Source::BluRay, 0.95, "TRaSH BD tier 04"),
    ("BiRJU", Source::BluRay, 0.95, "TRaSH BD tier 04"),
    ("BKC", Source::BluRay, 0.95, "TRaSH BD tier 04"),
    ("CBT", Source::BluRay, 0.95, "TRaSH BD tier 04"),
    ("Chimera", Source::BluRay, 0.95, "TRaSH BD tier 04"),
    ("derp", Source::BluRay, 0.95, "TRaSH BD tier 04"),
    ("DIY", Source::BluRay, 0.95, "TRaSH BD tier 04"),
    ("EXP", Source::BluRay, 0.95, "TRaSH BD tier 04"),
    ("grimf", Source::BluRay, 0.95, "TRaSH BD tier 04"),
    ("IK", Source::BluRay, 0.95, "TRaSH BD tier 04"),
    ("Iznjie_Biznjie", Source::BluRay, 0.95, "TRaSH BD tier 04"),
    ("Kaleido-subs", Source::BluRay, 0.95, "TRaSH BD tier 04"),
    ("Kametsu", Source::BluRay, 0.95, "TRaSH BD tier 04"),
    ("Kawatare", Source::BluRay, 0.95, "TRaSH BD tier 04"),
    ("KH", Source::BluRay, 0.95, "TRaSH BD tier 04"),
    ("LazyRemux", Source::BluRay, 0.95, "TRaSH BD tier 04"),
    ("Metal", Source::BluRay, 0.95, "TRaSH BD tier 04"),
    ("MK", Source::BluRay, 0.95, "TRaSH BD tier 04"),
    ("neko-kBaraka", Source::BluRay, 0.95, "TRaSH BD tier 04"),
    ("OZR", Source::BluRay, 0.95, "TRaSH BD tier 04"),
    ("pog42", Source::BluRay, 0.95, "TRaSH BD tier 04"),
    ("Quetzal", Source::BluRay, 0.95, "TRaSH BD tier 04"),
    ("Shimatta", Source::BluRay, 0.95, "TRaSH BD tier 04"),
    ("Smoke", Source::BluRay, 0.95, "TRaSH BD tier 04"),
    ("Spirale", Source::BluRay, 0.95, "TRaSH BD tier 04"),
    ("UDF", Source::BluRay, 0.95, "TRaSH BD tier 04"),
    ("UQW", Source::BluRay, 0.95, "TRaSH BD tier 04"),
    ("Vanilla", Source::BluRay, 0.95, "TRaSH BD tier 04"),
    ("Virtuality", Source::BluRay, 0.95, "TRaSH BD tier 04"),
    ("VULCAN", Source::BluRay, 0.95, "TRaSH BD tier 04"),
    // ── TRaSH BD Tier 05 ──────────────────────────────────────────────────
    ("Animorphs", Source::BluRay, 0.95, "TRaSH BD tier 05"),
    ("AOmundson", Source::BluRay, 0.95, "TRaSH BD tier 05"),
    ("ASC", Source::BluRay, 0.95, "TRaSH BD tier 05"),
    ("B00BA", Source::BluRay, 0.95, "TRaSH BD tier 05"),
    ("Beatrice-Raws", Source::BluRay, 0.95, "TRaSH BD tier 05"),
    ("Cait-Sidhe", Source::BluRay, 0.95, "TRaSH BD tier 05"),
    ("CsS", Source::BluRay, 0.95, "TRaSH BD tier 05"),
    ("CTR", Source::BluRay, 0.95, "TRaSH BD tier 05"),
    ("D4C", Source::BluRay, 0.95, "TRaSH BD tier 05"),
    ("deanzel", Source::BluRay, 0.95, "TRaSH BD tier 05"),
    ("Drag", Source::BluRay, 0.95, "TRaSH BD tier 05"),
    ("eldon", Source::BluRay, 0.95, "TRaSH BD tier 05"),
    ("Freehold", Source::BluRay, 0.95, "TRaSH BD tier 05"),
    ("GHS", Source::BluRay, 0.95, "TRaSH BD tier 05"),
    ("Hark0N", Source::BluRay, 0.95, "TRaSH BD tier 05"),
    ("Holomux", Source::BluRay, 0.95, "TRaSH BD tier 05"),
    ("Judgment", Source::BluRay, 0.95, "TRaSH BD tier 05"),
    ("MC", Source::BluRay, 0.95, "TRaSH BD tier 05"),
    ("mottoj", Source::BluRay, 0.95, "TRaSH BD tier 05"),
    ("NH", Source::BluRay, 0.95, "TRaSH BD tier 05"),
    ("NTRM", Source::BluRay, 0.95, "TRaSH BD tier 05"),
    ("o7", Source::BluRay, 0.95, "TRaSH BD tier 05"),
    ("QM", Source::BluRay, 0.95, "TRaSH BD tier 05"),
    ("Thighs", Source::BluRay, 0.95, "TRaSH BD tier 05"),
    ("TTGA", Source::BluRay, 0.95, "TRaSH BD tier 05"),
    ("UltraRemux", Source::BluRay, 0.95, "TRaSH BD tier 05"),
    ("WBDP", Source::BluRay, 0.95, "TRaSH BD tier 05"),
    ("WSE", Source::BluRay, 0.95, "TRaSH BD tier 05"),
    ("Yuki", Source::BluRay, 0.95, "TRaSH BD tier 05"),
    // ── TRaSH BD Tier 06 ──────────────────────────────────────────────────
    ("ANE", Source::BluRay, 0.95, "TRaSH BD tier 06"),
    ("Bunny-Apocalypse", Source::BluRay, 0.95, "TRaSH BD tier 06"),
    ("CyC", Source::BluRay, 0.95, "TRaSH BD tier 06"),
    ("Datte13", Source::BluRay, 0.95, "TRaSH BD tier 06"),
    ("EJF", Source::BluRay, 0.95, "TRaSH BD tier 06"),
    ("GetItTwisted", Source::BluRay, 0.95, "TRaSH BD tier 06"),
    ("iKaos", Source::BluRay, 0.95, "TRaSH BD tier 06"),
    ("karios", Source::BluRay, 0.95, "TRaSH BD tier 06"),
    ("Pookie", Source::BluRay, 0.95, "TRaSH BD tier 06"),
    ("RASETSU", Source::BluRay, 0.95, "TRaSH BD tier 06"),
    ("Starbez", Source::BluRay, 0.95, "TRaSH BD tier 06"),
    ("Yoghurt", Source::BluRay, 0.95, "TRaSH BD tier 06"),
    ("YURASUKA", Source::BluRay, 0.95, "TRaSH BD tier 06"),
    // ── TRaSH BD Tier 07 ──────────────────────────────────────────────────
    ("AC", Source::BluRay, 0.95, "TRaSH BD tier 07"),
    ("Almighty", Source::BluRay, 0.95, "TRaSH BD tier 07"),
    ("BlurayDesuYo", Source::BluRay, 0.95, "TRaSH BD tier 07"),
    ("Bolshevik", Source::BluRay, 0.95, "TRaSH BD tier 07"),
    ("Brrrrrrr", Source::BluRay, 0.95, "TRaSH BD tier 07"),
    ("Crow", Source::BluRay, 0.95, "TRaSH BD tier 07"),
    ("Dekinai", Source::BluRay, 0.95, "TRaSH BD tier 07"),
    ("Dragon-Releases", Source::BluRay, 0.95, "TRaSH BD tier 07"),
    ("DragsterPS", Source::BluRay, 0.95, "TRaSH BD tier 07"),
    ("E-D", Source::BluRay, 0.95, "TRaSH BD tier 07"),
    ("Exiled-Destiny", Source::BluRay, 0.95, "TRaSH BD tier 07"),
    ("FFF", Source::BluRay, 0.95, "TRaSH BD tier 07"),
    ("Final8", Source::BluRay, 0.95, "TRaSH BD tier 07"),
    ("Geonope", Source::BluRay, 0.95, "TRaSH BD tier 07"),
    ("iAHD", Source::BluRay, 0.95, "TRaSH BD tier 07"),
    ("inid4c", Source::BluRay, 0.95, "TRaSH BD tier 07"),
    ("Koten_Gars", Source::BluRay, 0.95, "TRaSH BD tier 07"),
    ("kuchikirukia", Source::BluRay, 0.95, "TRaSH BD tier 07"),
    ("LCE", Source::BluRay, 0.95, "TRaSH BD tier 07"),
    ("NTW", Source::BluRay, 0.95, "TRaSH BD tier 07"),
    ("orz", Source::BluRay, 0.95, "TRaSH BD tier 07"),
    ("RAI", Source::BluRay, 0.95, "TRaSH BD tier 07"),
    ("REVO", Source::BluRay, 0.95, "TRaSH BD tier 07"),
    ("SCP-2223", Source::BluRay, 0.95, "TRaSH BD tier 07"),
    ("Senjou", Source::BluRay, 0.95, "TRaSH BD tier 07"),
    ("SEV", Source::BluRay, 0.95, "TRaSH BD tier 07"),
    ("THORA", Source::BluRay, 0.95, "TRaSH BD tier 07"),
    ("Vivid", Source::BluRay, 0.95, "TRaSH BD tier 07"),
    // ── TRaSH BD Tier 08 ──────────────────────────────────────────────────
    ("AkihitoSubs", Source::BluRay, 0.95, "TRaSH BD tier 08"),
    ("Arukoru", Source::BluRay, 0.95, "TRaSH BD tier 08"),
    ("EDGE", Source::BluRay, 0.95, "TRaSH BD tier 08"),
    // EMBER was previously seeded as TRaSH BD tier 08, but empirically they
    // almost exclusively ship re-encoded WebRips. EMBER's Wajutsushi batch
    // (released 2024-12-20, after the show finished airing on 2024-12-16)
    // was the trigger — every episode came back from ffprobe as Web
    // (E-AC-3 audio). Reclassified to WEB and matched into SEED_WEB_KIND
    // with WebRip above so the prior produces `Web (WebRip)` instead of
    // `BluRay`. `reconcile_episode_seed_drift` resets existing user rows
    // that had been stamped under the old BD prior.
    //
    // Confidence sits at 0.85 rather than the 0.95 we use for
    // single-source groups: EMBER has shipped occasional BD rips in the
    // past (most recent 2025-11-14), so a high-confidence BD fingerprint
    // from ffprobe — FLAC / PGS subs / native DVD dimensions, all 0.90
    // in source_ffprobe.rs — should out-rank the WEB prior. 0.85 still
    // comfortably beats the 0.80 WEB ffprobe fingerprint (same verdict
    // anyway) and a filename-layer Web hint, which is what we want for
    // the common case.
    (
        "EMBER",
        Source::Web,
        0.85,
        "WebRip re-encoder (occasional BD)",
    ),
    ("GHOST", Source::BluRay, 0.95, "TRaSH BD tier 08"),
    // Judas is TRaSH BD tier 08 but also ships weekly WEB rips during airing,
    // so per the mixed-source rule in the module docs above we leave them out
    // entirely and let filename/ffprobe/temporal decide per release.
    ("naiyas", Source::BluRay, 0.95, "TRaSH BD tier 08"),
    ("Nep_Blanc", Source::BluRay, 0.95, "TRaSH BD tier 08"),
    ("Prof", Source::BluRay, 0.95, "TRaSH BD tier 08"),
    ("Shiro", Source::BluRay, 0.95, "TRaSH BD tier 08"),
    // ── TRaSH anime-raws (Japanese BD raw encoders) ───────────────────────
    // BD-only raw encoding specialists from the `anime-raws` CF. Confidence
    // is 0.95 because these groups exclusively work from Blu-ray sources.
    ("Asuka-Raws", Source::BluRay, 0.95, "TRaSH anime raws"),
    ("Daddy-Raws", Source::BluRay, 0.95, "TRaSH anime raws"),
    ("Fumi-Raws", Source::BluRay, 0.95, "TRaSH anime raws"),
    ("Iriza-Raws", Source::BluRay, 0.95, "TRaSH anime raws"),
    ("Kawaiika-Raws", Source::BluRay, 0.95, "TRaSH anime raws"),
    ("Koi-Raws", Source::BluRay, 0.95, "TRaSH anime raws"),
    ("Lilith-Raws", Source::BluRay, 0.95, "TRaSH anime raws"),
    ("LowPower-Raws", Source::BluRay, 0.95, "TRaSH anime raws"),
    ("Moozzi2", Source::BluRay, 0.95, "TRaSH anime raws"),
    ("Nanako-Raws", Source::BluRay, 0.95, "TRaSH anime raws"),
    ("NC-Raws", Source::BluRay, 0.95, "TRaSH anime raws"),
    ("neko-raws", Source::BluRay, 0.95, "TRaSH anime raws"),
    ("New-Raws", Source::BluRay, 0.95, "TRaSH anime raws"),
    ("Ohys-Raws", Source::BluRay, 0.95, "TRaSH anime raws"),
    ("Pandoratv-Raws", Source::BluRay, 0.95, "TRaSH anime raws"),
    ("Raws-Maji", Source::BluRay, 0.95, "TRaSH anime raws"),
    ("ReinForce", Source::BluRay, 0.95, "TRaSH anime raws"),
    ("Scryous-Raws", Source::BluRay, 0.95, "TRaSH anime raws"),
    ("Seicher-Raws", Source::BluRay, 0.95, "TRaSH anime raws"),
    ("Shiniori-Raws", Source::BluRay, 0.95, "TRaSH anime raws"),
    // ── TRaSH WEB Tier 01 ─────────────────────────────────────────────────
    ("Setsugen", Source::Web, 0.95, "TRaSH WEB tier 01"),
    ("Z4ST1N", Source::Web, 0.95, "TRaSH WEB tier 01"),
    // ── TRaSH WEB Tier 02 ─────────────────────────────────────────────────
    ("0x539", Source::Web, 0.95, "TRaSH WEB tier 02"),
    ("Cyan", Source::Web, 0.95, "TRaSH WEB tier 02"),
    ("Cytox", Source::Web, 0.95, "TRaSH WEB tier 02"),
    ("Gao", Source::Web, 0.95, "TRaSH WEB tier 02"),
    ("Half-Baked", Source::Web, 0.95, "TRaSH WEB tier 02"),
    ("HatSubs", Source::Web, 0.95, "TRaSH WEB tier 02"),
    ("MALD", Source::Web, 0.95, "TRaSH WEB tier 02"),
    ("Not-Vodes", Source::Web, 0.95, "TRaSH WEB tier 02"),
    ("Slyfox", Source::Web, 0.95, "TRaSH WEB tier 02"),
    ("SoLCE", Source::Web, 0.95, "TRaSH WEB tier 02"),
    ("tenshi", Source::Web, 0.95, "TRaSH WEB tier 02"),
    // ── TRaSH WEB Tier 03 ─────────────────────────────────────────────────
    ("AnoZu", Source::Web, 0.95, "TRaSH WEB tier 03"),
    ("Dooky", Source::Web, 0.95, "TRaSH WEB tier 03"),
    ("Kitsune", Source::Web, 0.95, "TRaSH WEB tier 03"),
    ("SubsPlus+", Source::Web, 0.95, "TRaSH WEB tier 03"),
    // ── TRaSH WEB Tier 04 ─────────────────────────────────────────────────
    ("Erai-raws", Source::Web, 0.95, "TRaSH WEB tier 04"),
    ("ToonsHub", Source::Web, 0.95, "TRaSH WEB tier 04"),
    ("VARYG", Source::Web, 0.95, "TRaSH WEB tier 04"),
    // ── TRaSH WEB Tier 05 ─────────────────────────────────────────────────
    ("BlueLobster", Source::Web, 0.95, "TRaSH WEB tier 05"),
    ("GST", Source::Web, 0.95, "TRaSH WEB tier 05"),
    ("HorribleRips", Source::Web, 0.95, "TRaSH WEB tier 05"),
    ("HorribleSubs", Source::Web, 0.95, "TRaSH WEB tier 05"),
    ("KAN3D2M", Source::Web, 0.95, "TRaSH WEB tier 05"),
    ("KiyoshiStar", Source::Web, 0.95, "TRaSH WEB tier 05"),
    ("Lia", Source::Web, 0.95, "TRaSH WEB tier 05"),
    ("NanDesuKa", Source::Web, 0.95, "TRaSH WEB tier 05"),
    ("PlayWeb", Source::Web, 0.95, "TRaSH WEB tier 05"),
    ("SobsPlease", Source::Web, 0.95, "TRaSH WEB tier 05"),
    ("Some-Stuffs", Source::Web, 0.95, "TRaSH WEB tier 05"),
    ("SubsPlease", Source::Web, 0.95, "TRaSH WEB tier 05"),
    ("URANIME", Source::Web, 0.95, "TRaSH WEB tier 05"),
    ("ZigZag", Source::Web, 0.95, "TRaSH WEB tier 05"),
    // ── TRaSH WEB Tier 06 ─────────────────────────────────────────────────
    ("DameDesuYo", Source::Web, 0.95, "TRaSH WEB tier 06"),
    ("Doki", Source::Web, 0.95, "TRaSH WEB tier 06"),
    ("Kaleido", Source::Web, 0.95, "TRaSH WEB tier 06"),
    ("Kantai", Source::Web, 0.95, "TRaSH WEB tier 06"),
    ("KawaSubs", Source::Web, 0.95, "TRaSH WEB tier 06"),
];

// ─────────────────────────────────────────────────────────────────────────
// Migration + seeding
// ─────────────────────────────────────────────────────────────────────────

/// Create the `group_source_map` table if it doesn't exist, then refresh the
/// seed defaults. Idempotent — safe to call on every startup.
///
/// `COLLATE NOCASE` on the primary key lets lookups be case-insensitive
/// without us having to normalize strings at every call site.
///
/// Non-user rows (`is_user_edit = 0`) are cleared before re-seeding so
/// [`SEED_DEFAULTS`] stays the single source of truth — groups we remove or
/// re-classify in code automatically propagate on the next startup. User
/// edits made via the settings UI are untouched.
pub async fn migrate(db: &SqlitePool) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS group_source_map (
            group_name    TEXT PRIMARY KEY COLLATE NOCASE,
            source        TEXT NOT NULL,
            confidence    REAL NOT NULL,
            is_user_edit  INTEGER NOT NULL DEFAULT 0,
            notes         TEXT NOT NULL DEFAULT '',
            updated_at    DATETIME DEFAULT CURRENT_TIMESTAMP
        )
        "#,
    )
    .execute(db)
    .await?;

    // ADD COLUMN on existing DBs; CREATE TABLE above already has the
    // column for fresh installs. `.ok()` silently absorbs "duplicate
    // column" on upgrade paths where the column is already present.
    sqlx::query("ALTER TABLE group_source_map ADD COLUMN web_kind TEXT NOT NULL DEFAULT ''")
        .execute(db)
        .await
        .ok();

    sqlx::query("DELETE FROM group_source_map WHERE is_user_edit = 0")
        .execute(db)
        .await?;

    seed_defaults(db).await?;
    reconcile_seed_drift(db).await?;

    Ok(())
}

/// Insert any missing built-in seed rows. Existing rows (including
/// user-edited ones) are left untouched thanks to `INSERT OR IGNORE`.
///
/// Wrapped in a single transaction so the 256 INSERTs commit as one
/// fsync at boot — used to be one fsync per row, blocking the very
/// first incoming request behind ~256 sequential disk syncs.
pub async fn seed_defaults(db: &SqlitePool) -> Result<(), sqlx::Error> {
    let mut tx = db.begin().await?;
    for (name, source, confidence, notes) in SEED_DEFAULTS {
        sqlx::query(
            "INSERT OR IGNORE INTO group_source_map (group_name, source, confidence, is_user_edit, notes)
             VALUES (?, ?, ?, 0, ?)",
        )
        .bind(name)
        .bind(source.as_str())
        .bind(*confidence)
        .bind(notes)
        .execute(&mut *tx)
        .await?;
    }
    // Layer the Web sub-tier hints on top. Separate pass (instead of
    // widening the SEED_DEFAULTS tuple) because only a handful of
    // groups carry a stable WEB-DL vs WEB-Rip signal, and threading a
    // fifth column through every one of the ~100 source-only rows
    // would be noise. Update-only so user-edited rows are left alone.
    for (name, web_kind) in SEED_WEB_KIND {
        sqlx::query(
            "UPDATE group_source_map
             SET web_kind = ?
             WHERE group_name = ? AND is_user_edit = 0",
        )
        .bind(web_kind.as_str())
        .bind(name)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

/// Episode rows to wipe back to "unknown" so the next library_classify
/// sweep re-runs the full pipeline against them. Used when `SEED_DEFAULTS`
/// flips a group's source classification (the group_source_map row gets
/// realigned by [`reconcile_seed_drift`], but `episode_quality_tags` rows
/// already stamped under the old prior would otherwise stay wrong forever
/// — `manual_override = 0` rows aren't user-pinned but our #53 sweep skip
/// rule never re-attempts a non-empty source).
///
/// Format: `(group_name, prior_source_to_match)`. A row matches when its
/// `release_group` equals the group (case-insensitive), its `source`
/// equals the listed prior, and `manual_override = 0`. Manual overrides
/// are always preserved.
const SEED_DRIFT_EPISODE_RESETS: &[(&str, &str)] = &[
    // EMBER was seeded as BD tier 08 in earlier releases; reclassified
    // 2026-04-20 to WEB / WebRip after empirical confirmation that they
    // ship WebRip re-encodes (their Wajutsushi batch released 2024-12-20,
    // four days after the show finished airing on 2024-12-16). Reset
    // BluRay-stamped EMBER rows so the next sweep re-runs them under the
    // corrected prior.
    ("EMBER", "BluRay"),
];

/// One-shot corrections for seed rows whose built-in value has changed
/// since an earlier release. `seed_defaults` uses `INSERT OR IGNORE`, so
/// an existing row keeps whatever value it was first seeded with — which
/// means a bad default (e.g. a group seeded with the wrong source or
/// confidence before we corrected it) never self-corrects on upgrade.
///
/// This pass realigns non-user-edited rows (`is_user_edit = 0`) to the
/// current `SEED_DEFAULTS` and `SEED_WEB_KIND` values. User edits are
/// preserved: anyone who deliberately set their own confidence for a
/// group keeps it. The update is idempotent — rows that already match
/// the seed are a no-op.
///
/// `episode_quality_tags` rows affected by a seed flip are reset later
/// in `models::mod::migrate` via [`reconcile_episode_seed_drift`], which
/// runs *after* the Phase 1b `ALTER TABLE` passes that add the `source`
/// / `resolution` / etc. columns this crate references. Keeping the
/// episode reset out of this function means [`reconcile_seed_drift`]
/// stays callable from inside `group_source_map::migrate` (where those
/// columns may not exist yet on a fresh database) without tripping a
/// "no such column: source" error on boot.
pub async fn reconcile_seed_drift(db: &SqlitePool) -> Result<(), sqlx::Error> {
    // See seed_defaults — same fsync-coalesce reason for the tx wrap.
    let mut tx = db.begin().await?;
    for (name, source, confidence, notes) in SEED_DEFAULTS {
        sqlx::query(
            "UPDATE group_source_map
             SET source = ?, confidence = ?, notes = ?
             WHERE group_name = ? AND is_user_edit = 0",
        )
        .bind(source.as_str())
        .bind(*confidence)
        .bind(notes)
        .bind(name)
        .execute(&mut *tx)
        .await?;
    }
    // web_kind isn't part of SEED_DEFAULTS — sync it from SEED_WEB_KIND
    // so a group that flipped from `BluRay` to `Web` (e.g. EMBER) lands
    // with the right WebRip / WebDl sub-tier instead of an empty string.
    for (name, web_kind) in SEED_WEB_KIND {
        sqlx::query(
            "UPDATE group_source_map
             SET web_kind = ?
             WHERE group_name = ? AND is_user_edit = 0",
        )
        .bind(web_kind.as_str())
        .bind(name)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

/// Schema-migration ledger: one row per one-shot migration that we
/// need to run exactly once across the installed base. `id` is a
/// stable string key (e.g. `"seed_drift_episode_v1"`); `applied_at`
/// records the first successful run. Idempotent by virtue of
/// `INSERT OR IGNORE` on the primary key — existing rows aren't
/// touched, and the lookup gate returns `true` as soon as the row
/// exists.
///
/// Kept deliberately minimal (no tooling around it, no CLI) because
/// one-shot migrations are rare in Ryokan — the usual pattern is
/// idempotent `ALTER TABLE ADD COLUMN ... .ok()`. This exists solely
/// so a repeat-on-every-boot migration can guard itself without each
/// one inventing its own config flag.
pub(crate) async fn ensure_schema_migrations_table(db: &SqlitePool) -> Result<(), sqlx::Error> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS schema_migrations (
             id TEXT PRIMARY KEY,
             applied_at DATETIME DEFAULT CURRENT_TIMESTAMP
         )",
    )
    .execute(db)
    .await?;
    Ok(())
}

pub(crate) async fn migration_already_applied(
    db: &SqlitePool,
    id: &str,
) -> Result<bool, sqlx::Error> {
    let row = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM schema_migrations WHERE id = ?")
        .bind(id)
        .fetch_one(db)
        .await?;
    Ok(row > 0)
}

pub(crate) async fn mark_migration_applied(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    id: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query("INSERT OR IGNORE INTO schema_migrations (id) VALUES (?)")
        .bind(id)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// Stable ID for the EMBER BluRay→Web seed-drift episode reset. If a
/// future EMBER recalibration wants to reset rows again, bump the
/// version suffix (`_v2`) so the new reset runs once.
const SEED_DRIFT_EPISODE_V1_MIGRATION_ID: &str = "seed_drift_episode_v1";

/// Companion to [`reconcile_seed_drift`]: wipe `episode_quality_tags`
/// rows whose group was reclassified by a seed flip so the next library
/// sweep re-runs the full pipeline against them under the corrected
/// prior. The #53 sweep skip rule normally leaves a row alone once
/// `classification_attempted_at` is set, so this reset must clear that
/// timestamp as well.
///
/// Split from `reconcile_seed_drift` because it touches columns added
/// later in `models::mod::migrate` (`source`, `resolution`,
/// `classification_attempted_at`, etc.). Call this once, after the
/// Phase 1b `ALTER TABLE episode_quality_tags ADD COLUMN ...` block
/// has run. User-edited group_source_map entries and `manual_override`
/// episode rows are both preserved.
///
/// **Gated on a one-shot migration ID.** This matters because the
/// reset targets `release_group = X AND source = Y AND
/// manual_override = 0` — which is *also* the shape of legitimate
/// ffprobe-stamped rows for groups that ship occasional releases in
/// their non-default source (e.g. EMBER's rare BD rips, classified
/// correctly at 0.90 ffprobe confidence out-ranking the 0.85 group
/// prior). Without a migration gate, every boot would re-wipe those
/// legit rows and only the next library sweep (≤6h later) would
/// restore them; a one-shot guard makes the reset fire exactly once
/// per (install, seed change) pair.
pub async fn reconcile_episode_seed_drift(db: &SqlitePool) -> Result<(), sqlx::Error> {
    ensure_schema_migrations_table(db).await?;
    if migration_already_applied(db, SEED_DRIFT_EPISODE_V1_MIGRATION_ID).await? {
        return Ok(());
    }

    let mut tx = db.begin().await?;
    for (group, prior_source) in SEED_DRIFT_EPISODE_RESETS {
        sqlx::query(
            // Clear `quality_tag` too — it's the legacy label string
            // (e.g. "BluRay-1080p") that the UI renders alongside the
            // structured columns. Without this, a reset row briefly
            // shows the old BD tag against an empty internal source
            // until the next library_classify sweep re-stamps it, which
            // can be up to 6h after boot.
            "UPDATE episode_quality_tags
             SET source = '',
                 resolution = '',
                 is_remux = 0,
                 is_bdmv = 0,
                 web_kind = '',
                 quality_tag = '',
                 classification_confidence = 0,
                 needs_review = 0,
                 classification_evidence = '',
                 classification_attempted_at = NULL
             WHERE release_group = ? COLLATE NOCASE
               AND source = ?
               AND COALESCE(manual_override, 0) = 0",
        )
        .bind(group)
        .bind(prior_source)
        .execute(&mut *tx)
        .await?;
    }
    mark_migration_applied(&mut tx, SEED_DRIFT_EPISODE_V1_MIGRATION_ID).await?;
    tx.commit().await?;
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────
// CRUD
// ─────────────────────────────────────────────────────────────────────────

/// Look up a single group by name. Case-insensitive thanks to the table's
/// `COLLATE NOCASE` primary key.
pub async fn get(
    db: &SqlitePool,
    group_name: &str,
) -> Result<Option<GroupSourceEntry>, sqlx::Error> {
    let row = sqlx::query(
        "SELECT group_name, source, confidence, is_user_edit, notes,
                COALESCE(web_kind, '') AS web_kind
         FROM group_source_map WHERE group_name = ?",
    )
    .bind(group_name)
    .fetch_optional(db)
    .await?;

    Ok(row.map(row_to_entry))
}

/// List all entries, alphabetically sorted.
pub async fn list_all(db: &SqlitePool) -> Result<Vec<GroupSourceEntry>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT group_name, source, confidence, is_user_edit, notes,
                COALESCE(web_kind, '') AS web_kind
         FROM group_source_map ORDER BY group_name COLLATE NOCASE ASC",
    )
    .fetch_all(db)
    .await?;

    Ok(rows.into_iter().map(row_to_entry).collect())
}

/// Insert or update a user-edited entry. Always marks the row as
/// `is_user_edit = 1` so subsequent `seed_defaults` calls won't revert it.
pub async fn upsert_user_edit(
    db: &SqlitePool,
    group_name: &str,
    source: Source,
    confidence: f32,
    notes: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO group_source_map (group_name, source, confidence, is_user_edit, notes, updated_at)
         VALUES (?, ?, ?, 1, ?, CURRENT_TIMESTAMP)
         ON CONFLICT(group_name) DO UPDATE SET
             source = excluded.source,
             confidence = excluded.confidence,
             is_user_edit = 1,
             notes = excluded.notes,
             updated_at = CURRENT_TIMESTAMP",
    )
    .bind(group_name)
    .bind(source.as_str())
    .bind(confidence.clamp(0.0, 1.0))
    .bind(notes)
    .execute(db)
    .await?;
    Ok(())
}

/// Delete an entry by group name.
pub async fn delete(db: &SqlitePool, group_name: &str) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM group_source_map WHERE group_name = ?")
        .bind(group_name)
        .execute(db)
        .await?;
    Ok(())
}

fn row_to_entry(row: sqlx::sqlite::SqliteRow) -> GroupSourceEntry {
    let source_str: String = row.get("source");
    let is_user_edit_int: i64 = row.get("is_user_edit");
    let web_kind_str: String = row.try_get("web_kind").unwrap_or_default();
    GroupSourceEntry {
        group_name: row.get("group_name"),
        source: Source::from_str(&source_str),
        confidence: row.get::<f64, _>("confidence") as f32,
        is_user_edit: is_user_edit_int != 0,
        notes: row.get("notes"),
        web_kind: WebKind::from_str(&web_kind_str),
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Manual-override feedback loop
// ─────────────────────────────────────────────────────────────────────────

/// A suggested new group → source mapping inferred from the user's manual
/// overrides. Surfaced on the settings "Release Groups" tab with an
/// "Accept" button that calls the regular upsert endpoint.
#[derive(Debug, Clone)]
pub struct GroupSuggestion {
    pub group_name: String,
    pub source: Source,
    /// How many distinct episodes the user has manually pinned to this
    /// (group, source) pair. Used to sort suggestions and justify them in
    /// the UI ("3 manual overrides say …").
    pub episode_count: i64,
    /// The existing mapping's source if this group is already in
    /// `group_source_map` but with a *different* source than the user's
    /// overrides agree on. `None` means the group is brand new and we're
    /// suggesting a fresh entry.
    pub existing_source: Option<Source>,
}

/// Walk `episode_quality_tags` for manually-overridden rows, group by
/// (release_group, source), and surface combinations with ≥ `min_count`
/// matching overrides that either (a) aren't in `group_source_map` at all
/// or (b) are mapped to a *different* source than the user's overrides
/// agree on.
///
/// Requires the override row to carry a non-empty `release_group`, which
/// is only populated when the file was actually grabbed via Ryokan (so we
/// know which group produced it). Pure-import overrides — where the user
/// pinned a classification on a file that the grab pipeline never saw —
/// have empty `release_group` and are skipped: there's nothing to attribute
/// the signal to.
///
/// Suggestions are returned sorted by descending episode_count so the
/// most-supported inferences render first. `min_count` of 2 is the
/// smallest defensible threshold — a single override is noise, two
/// matching overrides on the same group is a pattern worth surfacing.
pub async fn compute_suggestions(
    db: &SqlitePool,
    min_count: i64,
) -> Result<Vec<GroupSuggestion>, sqlx::Error> {
    // Tally manual overrides per (release_group, source) and LEFT JOIN the
    // existing `group_source_map` so we can distinguish "brand-new group"
    // from "existing mapping disagrees" in one round trip instead of N+1
    // `get()` calls. `COLLATE NOCASE` on the GROUP BY and the join predicate
    // so "subsplease" and "SubsPlease" coalesce the way the existing lookup
    // table does.
    let rows = sqlx::query(
        "SELECT t.release_group AS release_group,
                t.source         AS source,
                COUNT(*)         AS cnt,
                g.source         AS existing_source
         FROM episode_quality_tags t
         LEFT JOIN group_source_map g
                ON g.group_name = t.release_group COLLATE NOCASE
         WHERE COALESCE(t.manual_override, 0) = 1
           AND t.release_group != ''
           AND t.source != ''
         GROUP BY t.release_group COLLATE NOCASE, t.source
         HAVING cnt >= ?
         ORDER BY cnt DESC, t.release_group COLLATE NOCASE ASC",
    )
    .bind(min_count)
    .fetch_all(db)
    .await?;

    let mut suggestions = Vec::new();
    for row in rows {
        let group_name: String = row.get("release_group");
        let source_str: String = row.get("source");
        let episode_count: i64 = row.get("cnt");
        let existing_source_str: Option<String> = row.get("existing_source");

        let source = Source::from_str(&source_str);
        if source == Source::Unknown {
            continue;
        }

        // Map the joined column onto the same three-way fate the old
        // per-row `get()` call produced:
        //   - no row in group_source_map → brand-new suggestion
        //   - existing row agrees with the override → nothing to suggest, skip
        //   - existing row disagrees → suggestion flagged with existing_source
        let existing_source = match existing_source_str {
            Some(s) => {
                let parsed = Source::from_str(&s);
                if parsed == source {
                    continue;
                }
                Some(parsed)
            }
            None => None,
        };

        suggestions.push(GroupSuggestion {
            group_name,
            source,
            episode_count,
            existing_source,
        });
    }

    Ok(suggestions)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::SqlitePool;

    async fn fresh_db() -> SqlitePool {
        let db = SqlitePool::connect("sqlite::memory:")
            .await
            .expect("in-memory sqlite");
        // `group_source_map` owns its own CREATE TABLE, but the
        // `schema_migrations` table is created on demand by the
        // helpers under test — no pre-seeding needed.
        migrate(&db).await.expect("group_source_map::migrate");
        db
    }

    // ─── schema_migrations ledger round-trip ────────────────

    #[tokio::test]
    async fn ensure_schema_migrations_table_is_idempotent() {
        // Running the setup twice must not error — the `CREATE TABLE
        // IF NOT EXISTS` is the documented idempotency knob and a
        // refactor that swapped to the non-`IF NOT EXISTS` variant
        // would break every second startup.
        let db = fresh_db().await;
        ensure_schema_migrations_table(&db)
            .await
            .expect("first create");
        ensure_schema_migrations_table(&db)
            .await
            .expect("second create must also succeed");
    }

    #[tokio::test]
    async fn migration_already_applied_returns_false_for_unknown_id() {
        let db = fresh_db().await;
        ensure_schema_migrations_table(&db).await.unwrap();
        assert!(
            !migration_already_applied(&db, "some_random_migration_id")
                .await
                .unwrap(),
            "unknown migration id should report as not-applied"
        );
    }

    #[tokio::test]
    async fn mark_then_check_round_trip() {
        // mark → check: mark a stable id, verify check returns true.
        // The mark path uses INSERT OR IGNORE so a second mark is a
        // no-op (exercised by the next test).
        let db = fresh_db().await;
        ensure_schema_migrations_table(&db).await.unwrap();
        let mut tx = db.begin().await.unwrap();
        mark_migration_applied(&mut tx, "test_marker_v1")
            .await
            .unwrap();
        tx.commit().await.unwrap();
        assert!(
            migration_already_applied(&db, "test_marker_v1")
                .await
                .unwrap(),
            "marker should report as applied after mark"
        );
    }

    #[tokio::test]
    async fn mark_migration_applied_is_idempotent_via_insert_or_ignore() {
        // INSERT OR IGNORE on the primary key means a second mark of
        // the same id is a no-op — not an error, not a duplicate
        // row. Property: the count of ledger rows for a given id
        // never exceeds 1 regardless of how many times mark fires.
        let db = fresh_db().await;
        ensure_schema_migrations_table(&db).await.unwrap();
        for _ in 0..3 {
            let mut tx = db.begin().await.unwrap();
            mark_migration_applied(&mut tx, "test_dup_marker")
                .await
                .expect("repeated mark should not error");
            tx.commit().await.unwrap();
        }
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM schema_migrations WHERE id = 'test_dup_marker'",
        )
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!(count, 1, "repeated mark must keep ledger rows at 1");
    }

    #[tokio::test]
    async fn different_migration_ids_are_independent() {
        let db = fresh_db().await;
        ensure_schema_migrations_table(&db).await.unwrap();
        let mut tx = db.begin().await.unwrap();
        mark_migration_applied(&mut tx, "migration_a")
            .await
            .unwrap();
        tx.commit().await.unwrap();
        assert!(migration_already_applied(&db, "migration_a").await.unwrap());
        assert!(
            !migration_already_applied(&db, "migration_b").await.unwrap(),
            "marking A must not flip B"
        );
    }
}
