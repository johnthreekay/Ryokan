//! Seeded indexer catalog for the Settings → Indexers picker
//! (issue #28 follow-up).
//!
//! Sonarr / autobrr both ship a curated indexer list so users
//! pick from named cards instead of typing into a blank form.
//! Ryokan does the same, but every indexer reaches Ryokan
//! through Prowlarr or Jackett (Ryokan doesn't speak any
//! tracker's native API directly), so the catalog entries
//! pre-fill *defaults* — the row's display name, the private-
//! tracker flag, sensible priority + min-seeders — and leave
//! the URL + API key for the user to paste from their Prowlarr
//! / Jackett instance. Per-tracker seed ratio / seed time stay
//! blank by default because the right value depends on each
//! tracker's specific HnR / ratio policy and the user's
//! buffer goals; baking a number in would be wrong as often
//! as it's right.
//!
//! The grid sits above the form on the Add path; clicking a
//! card sends `?tab=indexers&template=<slug>` and the handler
//! re-renders the form with `IndexerSeed` populated from the
//! matched entry. Generic Torznab / Generic Newznab serve as
//! the fall-throughs for anything not in the curated list.

/// One curated indexer template. The grid renders a card per
/// entry; the form pre-fills from whichever entry the user
/// clicked.
pub struct SeededIndexer {
    /// URL-safe identifier, used as the `template=<slug>` query
    /// param when the user picks this card. Must be unique.
    pub slug: &'static str,
    /// Card heading + default `name` field on the form.
    pub display_name: &'static str,
    /// One-line description shown under the heading on the
    /// card. Generic per-category text — keep it short enough
    /// to fit on one line at the picker grid's narrowest
    /// breakpoint.
    pub blurb: &'static str,
    /// Optional notes shown above the form once the user picks
    /// this template. Reserved for *real* gotchas (e.g. when
    /// two cards point at the same tracker via different
    /// protocols and the user needs to pick the right side).
    /// Empty string renders no panel — that's the default.
    pub notes: &'static str,
    /// Marks the indexer as a private tracker, which flips the
    /// per-series upgrade-opt-in default and may affect future
    /// seed-rule defaults.
    pub is_private_tracker: bool,
    /// Sonarr-convention priority floor. Lower = preferred.
    /// Range 1..=50, default 25.
    pub default_priority: i64,
    /// Floor for releases the indexer is allowed to surface;
    /// scoring runs only on releases above this seeder count.
    pub default_min_seeders: i64,
    /// Suggested seed ratio passed to the download client at
    /// add time. `None` is the right answer for almost every
    /// tracker — site rules vary widely and the user knows
    /// their own buffer goals better than the catalog does.
    pub default_seed_ratio: Option<f64>,
    /// Suggested seed time floor in minutes. Same `None`-by-
    /// default reasoning as `default_seed_ratio`.
    pub default_seed_time_minutes: Option<i64>,
    /// Default protocol kind for the indexer (`torznab` or
    /// `newznab`). The Add Indexer form defaults its Kind
    /// dropdown to this value when the user picks the seed,
    /// since most curated entries are torznab-shaped (private
    /// and public anime trackers) and the Generic Newznab
    /// seed is the only entry that defaults to newznab.
    pub default_kind: &'static str,
    /// Hint shown in the URL field when the user picks this
    /// card. Should look like a real Prowlarr / Jackett URL so
    /// the user can pattern-match their own.
    pub url_placeholder: &'static str,
    /// `true` for the catch-all entries at the bottom of the
    /// grid (Generic Torznab / Generic Newznab). Renders with
    /// a different visual treatment so the picker doesn't
    /// suggest the user always belongs in the curated list.
    pub is_generic: bool,
}

/// Curated seed list. Order is the render order on the picker
/// grid; the two `is_generic` entries always go last per the
/// Sonarr convention.
pub const SEEDED: &[SeededIndexer] = &[
    SeededIndexer {
        slug: "animebytes",
        display_name: "AnimeBytes",
        blurb: "Anime private tracker",
        notes: "",
        is_private_tracker: true,
        default_priority: 15,
        default_min_seeders: 1,
        default_seed_ratio: None,
        default_seed_time_minutes: None,
        default_kind: "torznab",
        url_placeholder: "https://prowlarr.local/{N}/api",
        is_generic: false,
    },
    SeededIndexer {
        slug: "bakabt",
        display_name: "BakaBT",
        blurb: "Anime private tracker",
        notes: "",
        is_private_tracker: true,
        default_priority: 20,
        default_min_seeders: 1,
        default_seed_ratio: None,
        default_seed_time_minutes: None,
        default_kind: "torznab",
        url_placeholder: "https://prowlarr.local/{N}/api",
        is_generic: false,
    },
    SeededIndexer {
        slug: "u2",
        display_name: "U2",
        blurb: "Anime private tracker",
        notes: "",
        is_private_tracker: true,
        default_priority: 25,
        default_min_seeders: 1,
        default_seed_ratio: None,
        default_seed_time_minutes: None,
        default_kind: "torznab",
        url_placeholder: "https://prowlarr.local/{N}/api",
        is_generic: false,
    },
    SeededIndexer {
        slug: "nekobt",
        display_name: "nekoBT",
        blurb: "Public anime tracker",
        notes: "",
        is_private_tracker: false,
        default_priority: 30,
        default_min_seeders: 2,
        default_seed_ratio: None,
        default_seed_time_minutes: None,
        default_kind: "torznab",
        url_placeholder: "https://prowlarr.local/{N}/api",
        is_generic: false,
    },
    // Sukebei is Nyaa's adult side; Ryokan's built-in Nyaa search
    // never touches it, so this tile is how an adult title (the
    // series page's 18+ banner) gets an indexer at all. Prowlarr and
    // Jackett both ship a definition for it.
    SeededIndexer {
        slug: "sukebei",
        display_name: "Sukebei",
        blurb: "Adult anime tracker",
        notes: "",
        is_private_tracker: false,
        default_priority: 35,
        default_min_seeders: 2,
        default_seed_ratio: None,
        default_seed_time_minutes: None,
        default_kind: "torznab",
        url_placeholder: "https://prowlarr.local/{N}/api",
        is_generic: false,
    },
    SeededIndexer {
        slug: "tokyotosho",
        display_name: "Tokyo Toshokan",
        blurb: "Public anime tracker",
        notes: "",
        is_private_tracker: false,
        default_priority: 40,
        default_min_seeders: 2,
        default_seed_ratio: None,
        default_seed_time_minutes: None,
        default_kind: "torznab",
        url_placeholder: "https://prowlarr.local/{N}/api",
        is_generic: false,
    },
    // NZBGeek — paid newznab Usenet indexer. Carries anime as a
    // side effect of being a general-purpose Usenet source, but
    // it's the most widely-used paid indexer in the *arr ecosystem
    // and shipping a curated entry saves users a Prowlarr trip
    // for the URL pattern. `default_min_seeders` is 0 because
    // Usenet has no peer concept — the field is torrent-only.
    SeededIndexer {
        slug: "nzbgeek",
        display_name: "NZBGeek",
        blurb: "Usenet indexer (paid)",
        notes: "",
        is_private_tracker: false,
        default_priority: 30,
        default_min_seeders: 0,
        default_seed_ratio: None,
        default_seed_time_minutes: None,
        default_kind: "newznab",
        url_placeholder: "https://prowlarr.local/{N}/api",
        is_generic: false,
    },
    SeededIndexer {
        slug: "generic-torznab",
        display_name: "Generic Torznab",
        blurb: "Any torznab-compatible indexer",
        notes: "",
        is_private_tracker: false,
        default_priority: 25,
        default_min_seeders: 1,
        default_seed_ratio: None,
        default_seed_time_minutes: None,
        default_kind: "torznab",
        url_placeholder: "https://prowlarr.local/{N}/api",
        is_generic: true,
    },
    // Generic newznab fall-through for indexers not in the
    // curated list (NZB.cat, NZBPlanet, DrunkenSlug, …). AnimeTosho
    // left the catalog entirely when the site shut down in 2026.
    SeededIndexer {
        slug: "generic-newznab",
        display_name: "Generic Newznab",
        blurb: "Any newznab-compatible Usenet indexer",
        notes: "",
        is_private_tracker: false,
        default_priority: 25,
        default_min_seeders: 0,
        default_seed_ratio: None,
        default_seed_time_minutes: None,
        default_kind: "newznab",
        url_placeholder: "https://nzb.indexer.example/api",
        is_generic: true,
    },
];

/// Look up a seed by slug. `None` for unknown slugs; the
/// caller treats that the same as no template selected.
pub fn find_seed(slug: &str) -> Option<&'static SeededIndexer> {
    SEEDED.iter().find(|s| s.slug == slug)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    /// Slug uniqueness is load-bearing — `find_seed` returns the
    /// first match, so a duplicate would silently shadow the
    /// later entry on the picker.
    #[test]
    fn slugs_are_unique() {
        let mut seen: HashSet<&'static str> = HashSet::new();
        for s in SEEDED {
            assert!(seen.insert(s.slug), "duplicate slug: {}", s.slug);
        }
    }

    /// Slugs land in a `?template=<slug>` query param, so they
    /// must be URL-safe without escaping. ASCII alnum + `-` is
    /// the safe set.
    #[test]
    fn slugs_are_url_safe() {
        for s in SEEDED {
            assert!(
                s.slug
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-'),
                "slug {:?} contains a non-URL-safe character",
                s.slug
            );
            assert!(!s.slug.is_empty(), "empty slug not allowed");
        }
    }

    /// Display names are user-facing — empty would render a
    /// blank card heading.
    #[test]
    fn display_names_are_non_empty() {
        for s in SEEDED {
            assert!(
                !s.display_name.is_empty(),
                "empty display_name for slug {:?}",
                s.slug
            );
        }
    }

    /// Picker convention (mirrors Sonarr): `is_generic` entries
    /// always sit at the end of the grid so the curated list
    /// reads first. A reorder that buried Generic Torznab in
    /// the middle would break the picker UX.
    #[test]
    fn generic_entries_come_last() {
        let mut seen_generic = false;
        for s in SEEDED {
            if s.is_generic {
                seen_generic = true;
            } else {
                assert!(
                    !seen_generic,
                    "non-generic seed {:?} appears after a generic entry; \
                     Generic cards must sit at the end of the grid",
                    s.slug
                );
            }
        }
        assert!(
            seen_generic,
            "catalog must include at least one generic fallback (Generic Torznab / Newznab)"
        );
    }

    /// The picker grid renders both PRIVATE and PUBLIC pills;
    /// having neither would mean a tester wouldn't exercise the
    /// pill-class branch.
    #[test]
    fn catalog_covers_private_and_public_and_generic() {
        let private_count = SEEDED
            .iter()
            .filter(|s| s.is_private_tracker && !s.is_generic)
            .count();
        let public_count = SEEDED
            .iter()
            .filter(|s| !s.is_private_tracker && !s.is_generic)
            .count();
        let generic_count = SEEDED.iter().filter(|s| s.is_generic).count();
        assert!(private_count > 0, "expected at least one private tracker");
        assert!(public_count > 0, "expected at least one public tracker");
        assert!(generic_count >= 2, "expected the two generic fall-throughs");
    }

    /// `default_kind` must be `torznab` or `newznab` — the
    /// upsert handler validates the same enum, so a catalog
    /// entry with another value would fail the form roundtrip.
    #[test]
    fn default_kind_is_torznab_or_newznab() {
        for s in SEEDED {
            assert!(
                matches!(s.default_kind, "torznab" | "newznab"),
                "seed {:?} has invalid kind {:?}",
                s.slug,
                s.default_kind
            );
        }
    }

    /// `default_priority` must fall in the form's accepted
    /// range (1..=50) — same validation the handler applies on
    /// upsert.
    #[test]
    fn default_priority_in_range() {
        for s in SEEDED {
            assert!(
                (1..=50).contains(&s.default_priority),
                "seed {:?} priority {} out of [1, 50]",
                s.slug,
                s.default_priority
            );
        }
    }

    #[test]
    fn find_seed_resolves_known_slugs() {
        let ab = find_seed("animebytes").expect("animebytes seed exists");
        assert_eq!(ab.display_name, "AnimeBytes");
        assert!(ab.is_private_tracker);
        assert!(!ab.is_generic);

        let nekobt = find_seed("nekobt").expect("nekobt seed exists");
        // nekoBT is the only catalog entry whose public/private
        // status doesn't match a widely-cited source — the
        // upstream classification was set per the project owner's
        // direct knowledge of the tracker. If the assertion fails,
        // verify nekoBT's current registration model (open vs
        // invite-only) before flipping the catalog entry; the
        // is_private_tracker flag drives the per-series upgrade-
        // opt-in default and shouldn't toggle on a stale rumor.
        assert!(!nekobt.is_private_tracker, "nekoBT is public");

        let generic = find_seed("generic-torznab").expect("generic torznab seed exists");
        assert!(generic.is_generic);
        assert_eq!(generic.default_kind, "torznab");

        let generic_nzb = find_seed("generic-newznab").expect("generic newznab seed exists");
        assert!(generic_nzb.is_generic);
        assert_eq!(generic_nzb.default_kind, "newznab");
    }

    #[test]
    fn find_seed_returns_none_for_unknown_slug() {
        assert!(find_seed("does-not-exist").is_none());
        assert!(find_seed("").is_none());
        // Case-sensitive — slugs are URL-safe lowercase by
        // convention. A user hand-typing `?template=AnimeBytes`
        // (capital) should fall through to the picker rather
        // than silently match.
        assert!(find_seed("AnimeBytes").is_none());
    }
}
