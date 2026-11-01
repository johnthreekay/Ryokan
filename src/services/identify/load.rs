//! A target's franchise, read out of the metadata caches.
//!
//! The 12-hour metadata sweep walks every library series' relation graph
//! into `provider_relations_cache` (edges, both directions for the main
//! kinds) and `provider_metadata_cache` (full details with synonyms).
//! [`load_franchise`] walks those level by level from the target, one
//! query per level: chain edges (`PREQUEL` / `SEQUEL` / `PARENT`) up to
//! [`MAX_CHAIN`] entries, and one hop through side stories, spin-offs,
//! alternatives and summaries. TVDB placements come from anibridge's
//! loaded mappings, curated offsets from anime-relations. No network: an
//! entry the caches don't hold is simply not in the franchise, which can
//! only cost a veto.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, LazyLock, Mutex};

use sqlx::{Row, SqlitePool};

use crate::models::local_metadata::same_relation_entry;
use crate::services::anilist::{AnimeDetail, RelatedEntry};
use crate::services::{anibridge, anime_relations};

use super::graph::{Franchise, FranchiseGraph, GraphEdge, GraphNode, OffsetRow, Relation, TvdbRow};

/// Entries on the main chain the walk takes at most.
pub const MAX_CHAIN: usize = 40;

/// Loaded franchises by root and inputs, with the cache fingerprint they
/// were built from. Bounded: cleared when it grows past `CACHE_CAP`.
type LoadCache = HashMap<CacheKey, (Fingerprint, Arc<Franchise>)>;
static CACHE: LazyLock<Mutex<LoadCache>> = LazyLock::new(|| Mutex::new(HashMap::new()));
const CACHE_CAP: usize = 512;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct CacheKey {
    root: i64,
    offset: i32,
    alternates: Vec<String>,
    /// The detail the caller holds may be newer than the cached rows.
    detail: (String, String, usize, Option<i32>),
}

/// Row counts and newest stamps of the two cache tables: any write moves
/// one of them.
type Fingerprint = (i64, String, i64, String);

async fn fingerprint(db: &SqlitePool) -> Option<Fingerprint> {
    let row = sqlx::query(
        "SELECT \
           (SELECT COUNT(*) FROM provider_relations_cache) AS rel_count, \
           (SELECT COALESCE(MAX(cached_at), '') FROM provider_relations_cache) AS rel_at, \
           (SELECT COUNT(*) FROM provider_metadata_cache) AS meta_count, \
           (SELECT COALESCE(MAX(cached_at), '') FROM provider_metadata_cache) AS meta_at",
    )
    .fetch_one(db)
    .await
    .ok()?;
    Some((
        row.get("rel_count"),
        row.get("rel_at"),
        row.get("meta_count"),
        row.get("meta_at"),
    ))
}

/// The franchise around `detail`, with the library row's absolute
/// `offset` (`series.cumulative_prior_episodes`, 0 when unknown) and its
/// alternate titles. Cached per process until the caches change.
pub async fn load_franchise(
    db: &SqlitePool,
    detail: &AnimeDetail,
    offset: i32,
    alternates: &[String],
) -> Arc<Franchise> {
    let print = fingerprint(db).await;
    load_with_print(db, detail, offset, alternates, print.as_ref()).await
}

/// [`load_franchise`] for every library row the metadata cache holds a
/// detail for, keyed by series id: one query for the details and one
/// fingerprint for the lot. Rows with no cached detail are absent.
pub async fn load_franchises_for_series(
    db: &SqlitePool,
    rows: &[crate::models::series::Series],
) -> HashMap<i64, Arc<Franchise>> {
    let mut out = HashMap::new();
    if rows.is_empty() {
        return out;
    }
    let details: HashMap<i64, AnimeDetail> =
        sqlx::query("SELECT series_id, detail_json FROM series_metadata_cache")
            .fetch_all(db)
            .await
            .unwrap_or_default()
            .into_iter()
            .filter_map(|row| {
                let detail =
                    serde_json::from_str::<AnimeDetail>(&row.get::<String, _>("detail_json"))
                        .ok()?;
                Some((row.get::<i64, _>("series_id"), detail))
            })
            .collect();
    let print = fingerprint(db).await;
    for row in rows {
        let Some(detail) = details.get(&row.id) else {
            continue;
        };
        let franchise = load_with_print(
            db,
            detail,
            row.cumulative_prior_episodes,
            &row.alternate_title_list(),
            print.as_ref(),
        )
        .await;
        out.insert(row.id, franchise);
    }
    out
}

async fn load_with_print(
    db: &SqlitePool,
    detail: &AnimeDetail,
    offset: i32,
    alternates: &[String],
    print: Option<&Fingerprint>,
) -> Arc<Franchise> {
    let key = CacheKey {
        root: detail.id,
        offset,
        alternates: alternates.to_vec(),
        detail: (
            detail.title_romaji.clone(),
            detail.title_english.clone(),
            detail.synonyms.len(),
            detail.episodes,
        ),
    };
    if let Some(print) = print
        && let Some((cached_print, franchise)) = CACHE.lock().unwrap().get(&key)
        && cached_print == print
    {
        return franchise.clone();
    }
    let graph = load_graph(db, detail, offset).await;
    // Nothing cached beyond the target (a series added before its first
    // metadata sweep): the detail's own relations are better than none.
    let franchise = Arc::new(if graph.nodes.len() <= 1 {
        Franchise::from_detail(detail, alternates)
    } else {
        graph
            .build(detail.id, alternates)
            .unwrap_or_else(|| Franchise::from_detail(detail, alternates))
    });
    if let Some(print) = print {
        let mut cache = CACHE.lock().unwrap();
        if cache.len() >= CACHE_CAP {
            cache.clear();
        }
        cache.insert(key, (print.clone(), franchise.clone()));
    }
    franchise
}

/// The graph [`load_franchise`] builds from.
pub async fn load_graph(db: &SqlitePool, detail: &AnimeDetail, offset: i32) -> FranchiseGraph {
    let mut nodes: BTreeMap<i64, GraphNode> = BTreeMap::new();
    let mut edges: Vec<GraphEdge> = Vec::new();
    nodes.insert(detail.id, node_from_detail(detail));
    let mut chain: HashSet<i64> = HashSet::from([detail.id]);
    let mut walked: HashSet<i64> = HashSet::new();
    let mut level: Vec<i64> = vec![detail.id];
    while !level.is_empty() {
        level.retain(|id| walked.insert(*id));
        if level.is_empty() {
            break;
        }
        let mut next: Vec<i64> = Vec::new();
        for row in relation_rows(db, &level).await {
            let relation = Relation::parse(&row.relation);
            if !(relation.is_chain() || relation.is_side()) {
                continue;
            }
            // The neighbor on the far side of this edge from the level.
            let (near, far) = if level.contains(&row.from) {
                (row.from, row.to)
            } else {
                (row.to, row.from)
            };
            // A side entry's own edges are never walked.
            if !chain.contains(&near) {
                continue;
            }
            edges.push(GraphEdge {
                from: row.from,
                to: row.to,
                relation: row.relation.clone(),
            });
            if far == row.to {
                nodes.entry(far).or_insert_with(|| node_from_row(&row));
            } else {
                nodes.entry(far).or_insert_with(|| GraphNode {
                    id: far,
                    ..GraphNode::default()
                });
            }
            // `far`'s parent is `near`: a child, a side entry.
            let child = relation == Relation::Parent && far == row.from;
            if relation.is_chain() && !child && chain.len() < MAX_CHAIN && chain.insert(far) {
                next.push(far);
            }
        }
        level = next;
    }

    // Full details (synonyms, end year) where the sweep cached them.
    let ids: Vec<i64> = nodes
        .keys()
        .copied()
        .filter(|id| *id != detail.id)
        .collect();
    for cached in metadata_rows(db, &ids).await {
        if let Some(node) = nodes.get_mut(&cached.id) {
            *node = node_from_detail(&cached);
        }
    }
    merge_cross_provider(&mut nodes, &mut edges);
    edges.retain(|e| nodes.contains_key(&e.from) && nodes.contains_key(&e.to));
    edges.sort_by(|a, b| (a.from, a.to, &a.relation).cmp(&(b.from, b.to, &b.relation)));
    edges.dedup();

    let mut tvdb = Vec::new();
    let mut offsets = Vec::new();
    for node in nodes.values() {
        if node.id > 0 {
            for p in anibridge::tvdb_placements(node.id).await {
                tvdb.push(TvdbRow {
                    id: node.id,
                    tvdb_id: p.tvdb_id,
                    season: p.season,
                    episode_offset: p.episode_offset,
                });
            }
        }
        let curated = anime_relations::offset_for(node.id, node.mal_id).map(|o| o.episodes);
        // The library row's stored offset is the same decision
        // (`anime_relations::cumulative_prior_episodes`) for the target.
        let stored = (node.id == detail.id && offset > 0).then_some(offset);
        if let Some(episodes) = curated.or(stored) {
            offsets.push(OffsetRow {
                id: node.id,
                episodes,
            });
        }
    }
    FranchiseGraph {
        nodes: nodes.into_values().collect(),
        edges,
        tvdb,
        offsets,
    }
}

impl Franchise {
    /// A franchise from a detail's own relation list, for callers with no
    /// database: no synonyms for the relatives, no TVDB rows, no walk past
    /// one hop.
    pub fn from_detail(detail: &AnimeDetail, alternates: &[String]) -> Franchise {
        let mut nodes = vec![node_from_detail(detail)];
        let mut edges = Vec::new();
        for rel in &detail.relations {
            if rel.media_type != "ANIME"
                || same_relation_entry((rel.id, rel.id_mal), (detail.id, detail.id_mal))
            {
                continue;
            }
            let relation = Relation::parse(&rel.relation_type);
            if !(relation.is_chain() || relation.is_side()) {
                continue;
            }
            if !nodes.iter().any(|n| n.id == rel.id) {
                nodes.push(node_from_related(rel));
            }
            edges.push(GraphEdge {
                from: detail.id,
                to: rel.id,
                relation: rel.relation_type.clone(),
            });
        }
        let offsets = anime_relations::offset_for(detail.id, detail.id_mal)
            .map(|o| OffsetRow {
                id: detail.id,
                episodes: o.episodes,
            })
            .into_iter()
            .collect();
        FranchiseGraph {
            nodes,
            edges,
            tvdb: Vec::new(),
            offsets,
        }
        .build(detail.id, alternates)
        .expect("the target is the first node")
    }
}

/// Entries a MAL-sourced walk reached as `-mal_id` that the graph also
/// holds under their AniList id are one entry: keep the AniList node and
/// point the edges at it.
fn merge_cross_provider(nodes: &mut BTreeMap<i64, GraphNode>, edges: &mut [GraphEdge]) {
    let mut alias: HashMap<i64, i64> = HashMap::new();
    for (&id, node) in nodes.iter().filter(|(id, _)| **id < 0) {
        let mal = node.mal_id.or(Some(-id));
        if let Some(positive) = nodes
            .values()
            .find(|n| n.id > 0 && same_relation_entry((n.id, n.mal_id), (id, mal)))
        {
            alias.insert(id, positive.id);
        }
    }
    for id in alias.keys() {
        nodes.remove(id);
    }
    for edge in edges.iter_mut() {
        edge.from = *alias.get(&edge.from).unwrap_or(&edge.from);
        edge.to = *alias.get(&edge.to).unwrap_or(&edge.to);
    }
}

struct RelationRow {
    from: i64,
    to: i64,
    to_mal: Option<i64>,
    relation: String,
    title_romaji: String,
    title_english: String,
    title_native: String,
    format: String,
    episodes: Option<i32>,
    season_year: Option<i32>,
}

async fn relation_rows(db: &SqlitePool, ids: &[i64]) -> Vec<RelationRow> {
    let mut out = Vec::new();
    for chunk in ids.chunks(200) {
        let marks = vec!["?"; chunk.len()].join(",");
        let sql = format!(
            "SELECT provider_id, related_provider_id, related_mal_id, relation_type, \
             title_romaji, title_english, title_native, format, episodes, season_year \
             FROM provider_relations_cache \
             WHERE media_type IN ('ANIME', '') \
               AND (provider_id IN ({marks}) OR related_provider_id IN ({marks}))"
        );
        let mut query = sqlx::query(sqlx::AssertSqlSafe(sql));
        for id in chunk.iter().chain(chunk.iter()) {
            query = query.bind(id);
        }
        let Ok(rows) = query.fetch_all(db).await else {
            continue;
        };
        out.extend(rows.into_iter().map(|row| RelationRow {
            from: row.get("provider_id"),
            to: row.get("related_provider_id"),
            to_mal: row.get("related_mal_id"),
            relation: row.get("relation_type"),
            title_romaji: row.get("title_romaji"),
            title_english: row.get("title_english"),
            title_native: row.get("title_native"),
            format: row.get("format"),
            episodes: row.get("episodes"),
            season_year: row.get("season_year"),
        }));
    }
    out
}

async fn metadata_rows(db: &SqlitePool, ids: &[i64]) -> Vec<AnimeDetail> {
    let mut out = Vec::new();
    for chunk in ids.chunks(200) {
        if chunk.is_empty() {
            continue;
        }
        let marks = vec!["?"; chunk.len()].join(",");
        let sql = format!(
            "SELECT detail_json FROM provider_metadata_cache WHERE provider_id IN ({marks})"
        );
        let mut query = sqlx::query(sqlx::AssertSqlSafe(sql));
        for id in chunk {
            query = query.bind(id);
        }
        let Ok(rows) = query.fetch_all(db).await else {
            continue;
        };
        out.extend(rows.into_iter().filter_map(|row| {
            serde_json::from_str::<AnimeDetail>(&row.get::<String, _>("detail_json")).ok()
        }));
    }
    out
}

fn node_from_detail(detail: &AnimeDetail) -> GraphNode {
    GraphNode {
        id: detail.id,
        mal_id: detail.id_mal,
        title_romaji: detail.title_romaji.clone(),
        title_english: detail.title_english.clone(),
        title_native: detail.title_native.clone(),
        synonyms: detail.synonyms.clone(),
        format: detail.format.clone(),
        episodes: detail.episodes,
        season_year: detail.season_year,
        end_year: detail.end_year,
    }
}

fn node_from_related(rel: &RelatedEntry) -> GraphNode {
    GraphNode {
        id: rel.id,
        mal_id: rel.id_mal,
        title_romaji: rel.title_romaji.clone(),
        title_english: rel.title_english.clone(),
        title_native: rel.title_native.clone(),
        synonyms: Vec::new(),
        format: rel.format.clone(),
        episodes: rel.episodes,
        season_year: rel.season_year,
        end_year: None,
    }
}

fn node_from_row(row: &RelationRow) -> GraphNode {
    GraphNode {
        id: row.to,
        mal_id: row.to_mal,
        title_romaji: row.title_romaji.clone(),
        title_english: row.title_english.clone(),
        title_native: row.title_native.clone(),
        synonyms: Vec::new(),
        format: row.format.clone(),
        episodes: row.episodes,
        season_year: row.season_year,
        end_year: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{local_metadata, metadata_cache};

    async fn test_pool() -> SqlitePool {
        let db = SqlitePool::connect("sqlite::memory:")
            .await
            .expect("in-memory sqlite");
        crate::models::migrate(&db).await.expect("migrate");
        db
    }

    /// `(id, relation, format, episodes)`.
    type Rel<'a> = (i64, &'a str, &'a str, i32);

    fn detail(
        id: i64,
        romaji: &str,
        format: &str,
        episodes: i32,
        synonyms: &[&str],
        relations: &[Rel<'_>],
    ) -> AnimeDetail {
        let relations: Vec<serde_json::Value> = relations
            .iter()
            .map(|(rid, rel, format, episodes)| {
                serde_json::json!({
                    "id": rid, "id_mal": null, "title_romaji": format!("R{rid}"),
                    "title_english": "", "title_native": "", "cover_url": "",
                    "format": format, "status": "FINISHED", "status_display": "",
                    "episodes": episodes, "relation_type": rel, "season_year": null,
                    "media_type": "ANIME"
                })
            })
            .collect();
        serde_json::from_value(serde_json::json!({
            "id": id, "id_mal": null, "title_romaji": romaji,
            "title_english": "", "title_native": "", "cover_url": "", "banner_url": "",
            "format": format, "status": "FINISHED", "status_display": "",
            "episodes": episodes, "duration": null, "season": "", "season_year": null,
            "end_year": null, "description": "", "genres": [], "average_score": null,
            "average_score_display": null, "score_is_ten_point": false, "score_class": "",
            "next_airing_episode": null, "next_airing_at": null, "synonyms": synonyms,
            "streaming_episodes": [], "relations": relations
        }))
        .unwrap()
    }

    async fn cache(db: &SqlitePool, d: &AnimeDetail) {
        local_metadata::replace_relations_for_provider(db, d.id, d)
            .await
            .unwrap();
        metadata_cache::upsert_provider(db, d.id, d.id_mal, d)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn the_walk_follows_the_chain_both_ways_and_one_side_hop() {
        let db = test_pool().await;
        let s1 = detail(1, "Show", "TV", 12, &[], &[(2, "SEQUEL", "TV", 12)]);
        let s2 = detail(
            2,
            "Show 2nd Season",
            "TV",
            12,
            &["Show S2"],
            &[(3, "SEQUEL", "TV", 12), (10, "SIDE_STORY", "OVA", 2)],
        );
        let s3 = detail(3, "Show 3rd Season", "TV", 12, &[], &[]);
        // A side story's own relations are never walked.
        let ova = detail(10, "Show OVA", "OVA", 2, &[], &[(11, "SPIN_OFF", "TV", 12)]);
        for d in [&s1, &s2, &s3, &ova] {
            cache(&db, d).await;
        }
        // From the third season, the prequels come through the reverse
        // edges the cache writes.
        let graph = load_graph(&db, &s3, 0).await;
        let mut ids: Vec<i64> = graph.nodes.iter().map(|n| n.id).collect();
        ids.sort_unstable();
        assert_eq!(ids, [1, 2, 3, 10]);
        let s2_node = graph.nodes.iter().find(|n| n.id == 2).unwrap();
        assert_eq!(s2_node.synonyms, ["Show S2"], "full details from the cache");
        let franchise = graph.build(3, &[]).unwrap();
        assert_eq!(franchise.entry(3).unwrap().frames.absolute, 24);
        assert!(franchise.entry(1).unwrap().on_chain);
        assert!(!franchise.entry(10).unwrap().on_chain);
    }

    #[tokio::test]
    async fn a_loaded_franchise_vetoes_like_a_built_one() {
        let db = test_pool().await;
        let s1 = detail(
            1,
            "Jujutsu Kaisen",
            "TV",
            24,
            &[],
            &[(2, "SEQUEL", "TV", 12)],
        );
        let s3 = detail(
            2,
            "Jujutsu Kaisen: Shimetsu Kaiyuu - Zenpen",
            "TV",
            12,
            &[],
            &[],
        );
        cache(&db, &s1).await;
        cache(&db, &s3).await;
        let franchise = load_franchise(&db, &s1, 0, &[]).await;
        let verdict = super::super::identify(
            &franchise,
            "[SubsPlease] Jujutsu Kaisen: Shimetsu Kaiyuu - Zenpen - 06 (1080p)",
        );
        assert!(verdict.is_veto(), "{verdict:?}");
        // The cached franchise is reused until the caches change.
        let again = load_franchise(&db, &s1, 0, &[]).await;
        assert!(Arc::ptr_eq(&franchise, &again));
        let s4 = detail(3, "Jujutsu Kaisen 4th Season", "TV", 12, &[], &[]);
        cache(&db, &s4).await;
        let after = load_franchise(&db, &s1, 0, &[]).await;
        assert!(!Arc::ptr_eq(&franchise, &after));
    }

    #[tokio::test]
    async fn a_mal_sourced_entry_merges_into_its_anilist_twin() {
        let db = test_pool().await;
        let mut s1 = detail(1, "Show", "TV", 12, &[], &[(2, "SEQUEL", "TV", 12)]);
        s1.id_mal = Some(100);
        let mut s2 = detail(2, "Show 2nd Season", "TV", 12, &[], &[]);
        s2.id_mal = Some(200);
        cache(&db, &s1).await;
        cache(&db, &s2).await;
        // A Jikan-sourced detail of the second season points back at the
        // first by its MAL sentinel.
        let mut mal_s2 = detail(-200, "Show 2nd Season", "TV", 12, &[], &[]);
        mal_s2.id_mal = Some(200);
        sqlx::query(
            "INSERT INTO provider_relations_cache \
             (provider_id, related_provider_id, related_mal_id, relation_type, format, media_type) \
             VALUES (-200, 1, 100, 'PREQUEL', 'TV', 'ANIME'), (1, -200, 200, 'SEQUEL', 'TV', 'ANIME')",
        )
        .execute(&db)
        .await
        .unwrap();
        let graph = load_graph(&db, &s1, 0).await;
        let mut ids: Vec<i64> = graph.nodes.iter().map(|n| n.id).collect();
        ids.sort_unstable();
        assert_eq!(ids, [1, 2]);
        assert!(graph.edges.iter().all(|e| e.from > 0 && e.to > 0));
    }

    #[test]
    fn from_detail_builds_one_hop_without_a_database() {
        let s2 = detail(
            2,
            "Show 2nd Season",
            "TV",
            12,
            &[],
            &[(1, "PREQUEL", "TV", 12), (5, "CHARACTER", "TV", 3)],
        );
        let franchise = Franchise::from_detail(&s2, &[]);
        let mut ids: Vec<i64> = franchise.entries().iter().map(|e| e.id).collect();
        ids.sort_unstable();
        assert_eq!(ids, [1, 2]);
        assert_eq!(franchise.target().id, 2);
        assert_eq!(franchise.entry(2).unwrap().frames.absolute, 12);
    }
}
