//! Curated absolute-numbering offsets from `anime-relations` (#206).
//!
//! [erengy/anime-relations](https://github.com/erengy/anime-relations) is
//! Taiga's episode-redirect table: one rule per line saying that releases
//! carrying the *source* title and numbered `99-159` are episodes `1-61`
//! of the *destination* entry. That is exactly the number
//! `series.cumulative_prior_episodes` stores, so a rule is a curated
//! answer to the question `models::local_metadata::
//! compute_cumulative_prior_episodes` otherwise guesses by walking
//! AniList's PREQUEL chain. The walk is wrong wherever AniList's graph
//! has no clean TV-to-TV edge (Dragon Ball Kai 2014's only prequel is a
//! one-episode SPECIAL, so the walk says 0 and a `- 105` release lands
//! on a 69-episode series) and it is always 0 for Jikan-sourced series,
//! which have no relation rows at all.
//!
//! The file is vendored at `static/anime-relations.txt` and compiled in
//! with `include_str!` (CC0, ~50 KB, a few hundred lines). There is no
//! network path: a new rule arrives with the next Ryokan release, which
//! is fine because the rules exist for franchises whose numbering is
//! settled, not for this week's premiere. Refresh the snapshot by
//! copying upstream's `anime-relations.txt` over the vendored file; the
//! round-trip test pins the rule count so a bad copy fails loudly.
//!
//! What a rule contributes here is only its offset. Taiga also uses the
//! `!` suffix (emit a self-rule on the destination) to decide which
//! *title* a release may carry; Ryokan keys the offset by destination
//! id and the title gate is `auto_search::aliases`, so the flag is
//! parsed and kept but not acted on. Two rule shapes are deliberately
//! ignored when building the index: a destination range that does not
//! start at 1 (`100245:25-28 -> 100245:21-24` remaps four episodes in
//! the middle of a run, which a single offset cannot express without
//! breaking episodes 1-20) and a non-positive offset (episode-0
//! specials, `:0 -> :1`). When several rules target one destination
//! (Attack on Titan S3 Part 2 from S1 *and* from S3 Part 1) the largest
//! offset wins: that is the root-relative numbering SubsPlease-style
//! groups use and the same thing the PREQUEL walk sums to.

use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

use sqlx::SqlitePool;

use crate::models::{local_metadata, metadata_cache};

/// The vendored rule file. Public so the Credits tab and tests can
/// read the snapshot's own `last_modified` line.
pub const RULES_TEXT: &str = include_str!("../../static/anime-relations.txt");

/// Provider ids on one side of a rule. `None` is upstream's `?`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Ids {
    pub mal: Option<i64>,
    pub kitsu: Option<i64>,
    pub anilist: Option<i64>,
}

/// An episode or a range of episodes. `end == None` is upstream's
/// open-ended `13-?`; a single episode has `end == Some(start)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EpisodeRange {
    pub start: i32,
    pub end: Option<i32>,
}

/// One `- src -> dst` line with `~` already expanded to the source id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rule {
    pub src: Ids,
    pub src_range: EpisodeRange,
    pub dst: Ids,
    pub dst_range: EpisodeRange,
    /// Upstream's trailing `!`: also redirect the destination to itself.
    pub self_rule: bool,
}

/// A parsed rule file. Lines the parser did not understand are listed
/// in `skipped` with their 1-based line number rather than failing the
/// whole file, since one bad upstream edit must not take every rule
/// with it.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct RuleFile {
    pub version: String,
    pub last_modified: String,
    pub rules: Vec<Rule>,
    pub skipped: Vec<(usize, String)>,
}

/// Parse the `anime-relations.txt` grammar: `#` comments, `::meta` /
/// `::rules` sections, `- key: value` meta lines and
/// `- MAL|Kitsu|AniList:episodes -> MAL|Kitsu|AniList:episodes[!]` rules.
pub fn parse(text: &str) -> RuleFile {
    #[derive(PartialEq)]
    enum Section {
        None,
        Meta,
        Rules,
    }
    let mut out = RuleFile::default();
    let mut section = Section::None;
    for (idx, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        match line {
            "::meta" => {
                section = Section::Meta;
                continue;
            }
            "::rules" => {
                section = Section::Rules;
                continue;
            }
            _ => {}
        }
        let Some(body) = line.strip_prefix("- ") else {
            out.skipped.push((idx + 1, raw.to_string()));
            continue;
        };
        match section {
            Section::Meta => {
                if let Some(v) = body.strip_prefix("version:") {
                    out.version = v.trim().to_string();
                } else if let Some(v) = body.strip_prefix("last_modified:") {
                    out.last_modified = v.trim().to_string();
                } else {
                    out.skipped.push((idx + 1, raw.to_string()));
                }
            }
            Section::Rules => match parse_rule(body) {
                Some(rule) => out.rules.push(rule),
                None => out.skipped.push((idx + 1, raw.to_string())),
            },
            Section::None => out.skipped.push((idx + 1, raw.to_string())),
        }
    }
    out
}

fn parse_rule(body: &str) -> Option<Rule> {
    let (body, self_rule) = match body.strip_suffix('!') {
        Some(b) => (b, true),
        None => (body, false),
    };
    let (src, dst) = body.split_once("->")?;
    let (src, src_range) = parse_side(src.trim(), None)?;
    let (dst, dst_range) = parse_side(dst.trim(), Some(src))?;
    Some(Rule {
        src,
        src_range,
        dst,
        dst_range,
        self_rule,
    })
}

/// `MAL|Kitsu|AniList:episodes`. `repeat` is the source side, which
/// `~` copies from; it is `None` while parsing the source itself so a
/// stray `~` there is rejected.
fn parse_side(side: &str, repeat: Option<Ids>) -> Option<(Ids, EpisodeRange)> {
    let (ids, episodes) = side.rsplit_once(':')?;
    let mut parts = ids.split('|');
    let mal = parse_id(parts.next()?, repeat.map(|r| r.mal))?;
    let kitsu = parse_id(parts.next()?, repeat.map(|r| r.kitsu))?;
    let anilist = parse_id(parts.next()?, repeat.map(|r| r.anilist))?;
    if parts.next().is_some() {
        return None;
    }
    Some((
        Ids {
            mal,
            kitsu,
            anilist,
        },
        parse_range(episodes)?,
    ))
}

/// One id cell: digits, `?` (unknown → `None`), or `~` (repeat the
/// source's cell). Returns `Some(None)` for unknown and `None` for a
/// cell that is not any of those.
fn parse_id(cell: &str, repeat: Option<Option<i64>>) -> Option<Option<i64>> {
    match cell.trim() {
        "?" => Some(None),
        "~" => repeat,
        digits => digits.parse::<i64>().ok().filter(|n| *n > 0).map(Some),
    }
}

fn parse_range(spec: &str) -> Option<EpisodeRange> {
    let spec = spec.trim();
    match spec.split_once('-') {
        Some((start, end)) => {
            let start = start.trim().parse::<i32>().ok()?;
            let end = match end.trim() {
                "?" => None,
                e => Some(e.parse::<i32>().ok()?),
            };
            if end.is_some_and(|e| e < start) {
                return None;
            }
            Some(EpisodeRange { start, end })
        }
        None => {
            let start = spec.parse::<i32>().ok()?;
            Some(EpisodeRange {
                start,
                end: Some(start),
            })
        }
    }
}

/// The offset a destination entry takes from its best rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Offset {
    /// Episodes to subtract from a release number: `src_range.start - 1`.
    pub episodes: i32,
    /// The rule's source entry, whose title absolute-numbered releases
    /// carry. Its AniList id, when the rule has one, is what the search
    /// path reads franchise aliases for.
    pub source: Ids,
}

/// Reverse index: destination id → offset. Built once from the vendored
/// file (see [`INDEX`]); tests build their own from hand-written rules.
#[derive(Debug, Default)]
pub struct Index {
    by_anilist: HashMap<i64, Offset>,
    by_mal: HashMap<i64, Offset>,
}

impl Index {
    pub fn build(rules: &[Rule]) -> Index {
        let mut index = Index::default();
        for rule in rules {
            if rule.dst_range.start != 1 {
                continue;
            }
            let episodes = rule.src_range.start - 1;
            if episodes <= 0 {
                continue;
            }
            let offset = Offset {
                episodes,
                source: rule.src,
            };
            if let Some(al) = rule.dst.anilist {
                index
                    .by_anilist
                    .entry(al)
                    .and_modify(|o| {
                        if offset.episodes > o.episodes {
                            *o = offset;
                        }
                    })
                    .or_insert(offset);
            }
            if let Some(mal) = rule.dst.mal {
                index
                    .by_mal
                    .entry(mal)
                    .and_modify(|o| {
                        if offset.episodes > o.episodes {
                            *o = offset;
                        }
                    })
                    .or_insert(offset);
            }
        }
        index
    }

    /// Look a series up by its AniList id first, then by MAL id. A
    /// negative `anilist_id` is the Jikan-sourced sentinel (`-mal_id`)
    /// and doubles as the MAL id when the row carries none.
    pub fn lookup(&self, anilist_id: i64, mal_id: Option<i64>) -> Option<Offset> {
        if anilist_id > 0
            && let Some(o) = self.by_anilist.get(&anilist_id)
        {
            return Some(*o);
        }
        let mal = mal_id
            .filter(|m| *m > 0)
            .or_else(|| (anilist_id < 0).then_some(-anilist_id));
        mal.and_then(|m| self.by_mal.get(&m).copied())
    }

    pub fn len(&self) -> usize {
        self.by_anilist.len().max(self.by_mal.len())
    }

    pub fn is_empty(&self) -> bool {
        self.by_anilist.is_empty() && self.by_mal.is_empty()
    }
}

/// The compiled-in index. Parsing ~550 lines once at first use is
/// microseconds; nothing here is refreshed at runtime.
static INDEX: LazyLock<Index> = LazyLock::new(|| Index::build(&parse(RULES_TEXT).rules));

/// The curated offset for a series, if the vendored file has a rule
/// whose destination is this entry.
pub fn offset_for(anilist_id: i64, mal_id: Option<i64>) -> Option<Offset> {
    INDEX.lookup(anilist_id, mal_id)
}

/// The `last_modified` date of the vendored snapshot, for the Credits
/// tab and logs.
pub fn snapshot_date() -> String {
    parse(RULES_TEXT).last_modified
}

/// Where a series' offset came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OffsetSource {
    Rule(Offset),
    Walk,
}

/// The offset to store on `series.cumulative_prior_episodes`: the
/// curated rule when one exists, else the PREQUEL-chain walk. This is
/// the single decision point both writers (the metadata refresh and the
/// grab-time hydration) go through, so they can never disagree.
pub async fn cumulative_prior_episodes(
    db: &SqlitePool,
    anilist_id: i64,
    mal_id: Option<i64>,
) -> (i32, OffsetSource) {
    if let Some(rule) = offset_for(anilist_id, mal_id) {
        return (rule.episodes, OffsetSource::Rule(rule));
    }
    (
        local_metadata::compute_cumulative_prior_episodes(db, anilist_id).await,
        OffsetSource::Walk,
    )
}

/// Titles of a rule's source entry, read from `provider_metadata_cache`
/// only. The search path must not fan out to AniList per series; the
/// metadata refresh caches the source (`metadata_sync::
/// hydrate_rule_source`) so this is a plain cache read by the time a
/// search runs. Empty when the rule has no AniList id for its source
/// or the entry is not cached yet.
pub async fn source_titles(db: &SqlitePool, offset: &Offset) -> Vec<String> {
    let Some(source_id) = offset.source.anilist else {
        return Vec::new();
    };
    let Ok(Some(cached)) = metadata_cache::get_by_provider_id(db, source_id).await else {
        return Vec::new();
    };
    let mut seen: HashSet<String> = HashSet::new();
    [
        cached.detail.title_romaji,
        cached.detail.title_english,
        cached.detail.title_native,
    ]
    .into_iter()
    .map(|t| t.trim().to_string())
    .filter(|t| !t.is_empty())
    .filter(|t| seen.insert(t.to_lowercase()))
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(mal: i64, kitsu: i64, anilist: i64) -> Ids {
        Ids {
            mal: Some(mal),
            kitsu: Some(kitsu),
            anilist: Some(anilist),
        }
    }

    fn rule(line: &str) -> Rule {
        parse_rule(line).unwrap_or_else(|| panic!("rule should parse: {line}"))
    }

    #[test]
    fn vendored_file_round_trips() {
        let file = parse(RULES_TEXT);
        assert_eq!(file.version, "1.3.0");
        assert!(
            file.last_modified.len() == 10 && file.last_modified.as_bytes()[4] == b'-',
            "last_modified should be a date, got {:?}",
            file.last_modified
        );
        assert!(
            file.skipped.is_empty(),
            "every line of the vendored file must parse; skipped: {:?}",
            file.skipped
        );
        // Pinned to the snapshot: a refresh bumps this deliberately, a
        // truncated copy fails here.
        assert_eq!(file.rules.len(), 543);
        assert!(file.rules.iter().any(|r| r.self_rule));
        assert!(
            file.rules
                .iter()
                .any(|r| r.src.anilist.is_none() && r.src.mal.is_some()),
            "the file carries rules with an unknown AniList id"
        );
    }

    #[test]
    fn parses_every_rule_shape() {
        let r = rule("16498|7442|16498:26-37 -> 25777|8671|20958:1-12!");
        assert_eq!(r.src, ids(16498, 7442, 16498));
        assert_eq!(
            r.src_range,
            EpisodeRange {
                start: 26,
                end: Some(37)
            }
        );
        assert_eq!(r.dst, ids(25777, 8671, 20958));
        assert_eq!(
            r.dst_range,
            EpisodeRange {
                start: 1,
                end: Some(12)
            }
        );
        assert!(r.self_rule);

        // `~` repeats the source id; single episode; no `!`.
        let r = rule("33820|12478|21898:0 -> ~|~|~:1");
        assert_eq!(r.dst, r.src);
        assert_eq!(
            r.src_range,
            EpisodeRange {
                start: 0,
                end: Some(0)
            }
        );
        assert!(!r.self_rule);

        // `?` is an unknown id on either side.
        let r = rule("?|7205|15061:51-101 -> ?|7972|20181:1-51");
        assert_eq!(r.src.mal, None);
        assert_eq!(r.src.anilist, Some(15061));
        assert_eq!(r.dst.mal, None);
        assert_eq!(r.dst.anilist, Some(20181));

        // Open-ended range.
        let r = rule("?|?|1:13-? -> ?|?|2:1-?!");
        assert_eq!(
            r.src_range,
            EpisodeRange {
                start: 13,
                end: None
            }
        );
        assert_eq!(
            r.dst_range,
            EpisodeRange {
                start: 1,
                end: None
            }
        );
    }

    #[test]
    fn malformed_lines_are_skipped_not_fatal() {
        let text = "::rules\n\
                    - 1|2|3:14-26 -> 4|5|6:1-13\n\
                    - 1|2:14 -> 4|5|6:1\n\
                    - ~|~|~:5 -> 1|2|3:1\n\
                    - 1|2|3:26-14 -> 4|5|6:1-13\n\
                    - 1|2|3:x -> 4|5|6:1\n\
                    not a rule\n\
                    - 1|2|3:5 -> 4|5|6:1\n";
        let file = parse(text);
        assert_eq!(file.rules.len(), 2, "{:?}", file.skipped);
        let lines: Vec<usize> = file.skipped.iter().map(|(n, _)| *n).collect();
        assert_eq!(lines, vec![3, 4, 5, 6, 7]);
    }

    #[test]
    fn meta_lines_are_read_and_rules_outside_a_section_are_skipped() {
        let text = "- 1|2|3:5 -> 4|5|6:1\n::meta\n- version: 9.9.9\n- last_modified: 2030-01-02\n- other: x\n";
        let file = parse(text);
        assert_eq!(file.version, "9.9.9");
        assert_eq!(file.last_modified, "2030-01-02");
        assert!(file.rules.is_empty());
        assert_eq!(file.skipped.len(), 2);
    }

    #[test]
    fn index_keeps_only_offsets_a_single_number_can_express() {
        let rules = vec![
            // Plain continuation: offset 25.
            rule("16498|7442|16498:26-37 -> 25777|8671|20958:1-12!"),
            // Episode-0 special: negative offset, skipped.
            rule("19759|7973|19759:0 -> 28405|10020|21303:1"),
            // Mid-run remap: destination starts at 21, skipped.
            rule("?|?|100245:25-28 -> ?|?|100245:21-24"),
            // Self-rule (Beastars S2 released as 13-24): offset 12.
            rule("40935|42904|114194:13-24 -> ~|~|~:1-12"),
        ];
        let index = Index::build(&rules);
        assert_eq!(index.lookup(20958, None).map(|o| o.episodes), Some(25));
        assert_eq!(index.lookup(21303, None), None);
        assert_eq!(index.lookup(100245, None), None);
        assert_eq!(index.lookup(114194, None).map(|o| o.episodes), Some(12));
        assert_eq!(index.len(), 2);
    }

    #[test]
    fn largest_offset_wins_when_several_rules_share_a_destination() {
        // Attack on Titan S3 Part 2 from S1 (49) and from S3 Part 1 (12).
        let rules = vec![
            rule("38524|41982|104578:1-10 -> ~|~|~:1-10"),
            rule("35760|13569|99147:13-22 -> 38524|41982|104578:1-10!"),
            rule("16498|7442|16498:50-59 -> 38524|41982|104578:1-10!"),
        ];
        let index = Index::build(&rules);
        let hit = index.lookup(104578, None).expect("rule");
        assert_eq!(hit.episodes, 49);
        assert_eq!(hit.source.anilist, Some(16498));
        // Same answer through the MAL key.
        assert_eq!(index.lookup(0, Some(38524)).map(|o| o.episodes), Some(49));
    }

    #[test]
    fn lookup_falls_back_to_mal_for_sentinel_and_unmapped_rows() {
        let rules = vec![
            // No AniList id on either side: only reachable by MAL id.
            rule("32015|11615|?:3-6 -> 36625|41634|?:1-4"),
            rule("6033|4394|6033:99-159 -> 22777|8351|20635:1-61!"),
        ];
        let index = Index::build(&rules);
        // Jikan-sourced series store anilist_id = -mal_id.
        assert_eq!(index.lookup(-36625, None).map(|o| o.episodes), Some(2));
        // AniList-sourced series whose AL id has no rule but whose MAL
        // id does.
        assert_eq!(
            index.lookup(999_999, Some(36625)).map(|o| o.episodes),
            Some(2)
        );
        // The AL key wins when present.
        assert_eq!(
            index.lookup(20635, Some(36625)).map(|o| o.episodes),
            Some(98)
        );
        assert_eq!(index.lookup(1, None), None);
        assert_eq!(index.lookup(0, None), None);
    }

    #[test]
    fn acceptance_cases_from_the_vendored_snapshot() {
        // Dragon Ball Kai (2014), whose only AniList PREQUEL is a
        // one-episode SPECIAL, so the walk says 0.
        let kai = offset_for(20635, None).expect("Kai 2014 rule");
        assert_eq!(kai.episodes, 98);
        assert_eq!(kai.source.anilist, Some(6033));
        // Attack on Titan Season 2.
        assert_eq!(offset_for(20958, None).map(|o| o.episodes), Some(25));
        // A first season has no rule.
        assert_eq!(offset_for(16498, None), None);
        assert!(!INDEX.is_empty());
    }

    fn detail(
        id: i64,
        romaji: &str,
        english: &str,
        native: &str,
    ) -> crate::services::anilist::AnimeDetail {
        crate::services::anilist::AnimeDetail {
            is_adult: false,
            id,
            id_mal: Some(id),
            title_romaji: romaji.to_string(),
            title_english: english.to_string(),
            title_native: native.to_string(),
            cover_url: String::new(),
            banner_url: String::new(),
            format: "TV".to_string(),
            status: "FINISHED".to_string(),
            status_display: "Finished".to_string(),
            episodes: Some(97),
            duration: Some(24),
            season: String::new(),
            season_year: Some(2009),
            end_year: Some(2011),
            description: String::new(),
            genres: Vec::new(),
            average_score: None,
            average_score_display: None,
            score_is_ten_point: false,
            score_class: String::new(),
            next_airing_episode: None,
            next_airing_at: None,
            synonyms: Vec::new(),
            streaming_episodes: Vec::new(),
            relations: Vec::new(),
        }
    }

    async fn test_pool() -> SqlitePool {
        let db = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("in-memory sqlite");
        crate::models::migrate(&db).await.expect("migrate");
        db
    }

    async fn insert_prequel(db: &SqlitePool, provider_id: i64, related_id: i64, episodes: i32) {
        sqlx::query(
            "INSERT INTO provider_relations_cache \
             (provider_id, related_provider_id, episodes, format, relation_type, media_type) \
             VALUES (?, ?, ?, 'TV', 'PREQUEL', 'ANIME')",
        )
        .bind(provider_id)
        .bind(related_id)
        .bind(episodes)
        .execute(db)
        .await
        .expect("insert prequel");
    }

    #[tokio::test]
    async fn rule_overrides_the_walk_and_the_walk_covers_the_rest() {
        let db = test_pool().await;
        // A cached PREQUEL chain that disagrees with the rule for AoT S2.
        insert_prequel(&db, 20958, 16498, 26).await;
        let (offset, source) = cumulative_prior_episodes(&db, 20958, None).await;
        assert_eq!(offset, 25);
        assert!(matches!(source, OffsetSource::Rule(o) if o.episodes == 25));

        // An id with no rule takes the walk.
        insert_prequel(&db, 3, 2, 23).await;
        insert_prequel(&db, 2, 1, 24).await;
        assert_eq!(
            cumulative_prior_episodes(&db, 3, None).await,
            (47, OffsetSource::Walk)
        );
        assert_eq!(
            cumulative_prior_episodes(&db, 1, None).await,
            (0, OffsetSource::Walk)
        );
    }

    #[tokio::test]
    async fn source_titles_read_the_provider_cache_only() {
        let db = test_pool().await;
        let hit = offset_for(20635, None).expect("Kai 2014 rule");
        assert!(source_titles(&db, &hit).await.is_empty());

        metadata_cache::upsert_provider(
            &db,
            6033,
            Some(6033),
            &detail(
                6033,
                "Dragon Ball Kai",
                "Dragon Ball Z Kai",
                "ドラゴンボール改",
            ),
        )
        .await
        .expect("cache the source entry");

        let titles = source_titles(&db, &hit).await;
        assert_eq!(
            titles,
            vec!["Dragon Ball Kai", "Dragon Ball Z Kai", "ドラゴンボール改"]
        );

        // A rule whose source has no AniList id yields nothing.
        let no_al = Offset {
            episodes: 2,
            source: Ids {
                mal: Some(32015),
                kitsu: None,
                anilist: None,
            },
        };
        assert!(source_titles(&db, &no_al).await.is_empty());
    }
}
