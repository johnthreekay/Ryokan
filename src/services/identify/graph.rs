//! The franchise as plain data, and its build into entry cards.
//!
//! [`FranchiseGraph`] is what the loader reads out of the metadata caches
//! and what the test fixtures vendor: entries, relation edges, TVDB
//! seasons and curated absolute offsets. [`FranchiseGraph::build`] turns
//! it into a [`Franchise`] with the numbering frames worked out, so the
//! fixtures run the same build production does.

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

use super::names::NameForm;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct FranchiseGraph {
    pub nodes: Vec<GraphNode>,
    #[serde(default)]
    pub edges: Vec<GraphEdge>,
    /// TVDB show and season per entry (anibridge).
    #[serde(default)]
    pub tvdb: Vec<TvdbRow>,
    /// Curated absolute offsets (anime-relations), which override the
    /// TV PREQUEL walk.
    #[serde(default)]
    pub offsets: Vec<OffsetRow>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct GraphNode {
    pub id: i64,
    #[serde(default)]
    pub mal_id: Option<i64>,
    #[serde(default)]
    pub title_romaji: String,
    #[serde(default)]
    pub title_english: String,
    #[serde(default)]
    pub title_native: String,
    #[serde(default)]
    pub synonyms: Vec<String>,
    #[serde(default)]
    pub format: String,
    #[serde(default)]
    pub episodes: Option<i32>,
    #[serde(default)]
    pub season_year: Option<i32>,
    #[serde(default)]
    pub end_year: Option<i32>,
}

/// `to` is `from`'s `relation` (`PREQUEL`: `to` came before `from`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GraphEdge {
    pub from: i64,
    pub to: i64,
    pub relation: String,
}

/// One TVDB season an entry sits in (anibridge's `TvdbPlacement`): TVDB
/// episode = the entry's episode + `episode_offset`, when known.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TvdbRow {
    pub id: i64,
    pub tvdb_id: i64,
    pub season: i32,
    #[serde(default)]
    pub episode_offset: Option<i32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OffsetRow {
    pub id: i64,
    pub episodes: i32,
}

/// A relation, with AniList's and MAL's spellings folded together.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Relation {
    Prequel,
    Sequel,
    Parent,
    SideStory,
    SpinOff,
    Alternative,
    Summary,
    FullStory,
    Other,
}

impl Relation {
    pub fn parse(raw: &str) -> Self {
        let key: String = raw
            .trim()
            .chars()
            .map(|c| {
                if c == '-' || c == ' ' {
                    '_'
                } else {
                    c.to_ascii_uppercase()
                }
            })
            .collect();
        match key.as_str() {
            "PREQUEL" => Self::Prequel,
            "SEQUEL" => Self::Sequel,
            "PARENT" | "PARENT_STORY" => Self::Parent,
            "SIDE_STORY" => Self::SideStory,
            "SPIN_OFF" | "SPINOFF" => Self::SpinOff,
            "ALTERNATIVE" | "ALTERNATIVE_SETTING" | "ALTERNATIVE_VERSION" => Self::Alternative,
            "SUMMARY" => Self::Summary,
            "FULL_STORY" => Self::FullStory,
            _ => Self::Other,
        }
    }

    /// An edge of the main chain (`PREQUEL` / `SEQUEL` / `PARENT`).
    pub fn is_chain(self) -> bool {
        matches!(self, Self::Prequel | Self::Sequel | Self::Parent)
    }

    /// An edge the franchise takes one hop through.
    pub fn is_side(self) -> bool {
        matches!(
            self,
            Self::SideStory | Self::SpinOff | Self::Alternative | Self::Summary | Self::FullStory
        )
    }
}

/// The names of one entry, by field. Comparisons between two entries
/// read only the fields both have, so an entry missing its English title
/// or synonyms can lose a veto but never gain one.
#[derive(Debug, Clone, Default)]
pub(super) struct NameFields {
    pub romaji: Vec<NameForm>,
    pub english: Vec<NameForm>,
    pub native: Vec<NameForm>,
    pub synonyms: Vec<NameForm>,
    /// The library series' alternate titles (target only).
    pub alternates: Vec<NameForm>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Field {
    Romaji,
    English,
    Native,
    Synonyms,
    Alternates,
}

impl NameFields {
    pub fn get(&self, field: Field) -> &[NameForm] {
        match field {
            Field::Romaji => &self.romaji,
            Field::English => &self.english,
            Field::Native => &self.native,
            Field::Synonyms => &self.synonyms,
            Field::Alternates => &self.alternates,
        }
    }

    pub fn has(&self, field: Field) -> bool {
        !self.get(field).is_empty()
    }

    /// Fields that name the entry: romaji, English, native, alternates.
    pub const CANONICAL: [Field; 4] = [
        Field::Romaji,
        Field::English,
        Field::Native,
        Field::Alternates,
    ];
    pub const ALL: [Field; 5] = [
        Field::Romaji,
        Field::English,
        Field::Native,
        Field::Synonyms,
        Field::Alternates,
    ];
}

/// Where one entry's episodes sit in each numbering a release may use.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct Frames {
    /// Episodes before this entry along its TV PREQUEL chain (a curated
    /// offset when there is one).
    pub absolute: i32,
    /// The TVDB seasons (never season 0, the specials), each with the
    /// offset from the entry's episodes to the season's when known.
    pub tvdb: Vec<(i32, Option<i32>)>,
    /// The first year of the TVDB show (`JoJo's Bizarre Adventure (2012)`
    /// for every JoJo TV entry).
    pub tvdb_start_year: Option<i32>,
}

#[derive(Debug, Clone)]
pub struct EntryCard {
    pub id: i64,
    pub mal_id: Option<i64>,
    /// A display title: romaji, else English, else native.
    pub title: String,
    pub format: String,
    pub episodes: Option<i32>,
    pub season_year: Option<i32>,
    pub end_year: Option<i32>,
    pub(super) names: NameFields,
    /// Seasons the entry answers to: its names' markers and its TVDB
    /// season; season 1 for a first TV entry with neither.
    pub(super) seasons: Vec<i32>,
    pub(super) parts: Vec<i32>,
    /// A movie series' part number (`Kizumonogatari II`).
    pub(super) sequence: Option<i32>,
    pub(super) frames: Frames,
    /// Reached from the target through `PREQUEL` / `SEQUEL` / `PARENT`
    /// edges alone (the target included).
    pub on_chain: bool,
}

impl EntryCard {
    pub(super) fn is_tv(&self) -> bool {
        matches!(self.format.as_str(), "TV" | "TV_SHORT")
    }

    /// One release is the whole entry: a movie, or one episode.
    pub(super) fn is_single_entry(&self) -> bool {
        self.format == "MOVIE" || self.episodes == Some(1)
    }
}

/// A target and the entries it could be confused with.
#[derive(Debug, Clone)]
pub struct Franchise {
    pub(super) target: usize,
    pub(super) entries: Vec<EntryCard>,
}

impl Franchise {
    pub fn target(&self) -> &EntryCard {
        &self.entries[self.target]
    }

    pub fn entries(&self) -> &[EntryCard] {
        &self.entries
    }

    pub fn entry(&self, id: i64) -> Option<&EntryCard> {
        self.entries.iter().find(|e| e.id == id)
    }
}

impl FranchiseGraph {
    /// The franchise as seen from `target`, every node an entry. `None`
    /// when the target is not a node.
    pub fn build(&self, target: i64, alternates: &[String]) -> Option<Franchise> {
        let target_index = self.nodes.iter().position(|n| n.id == target)?;
        let by_id: HashMap<i64, &GraphNode> = self.nodes.iter().map(|n| (n.id, n)).collect();
        let prequels = self.tv_prequels(&by_id);
        let curated: HashMap<i64, i32> = self.offsets.iter().map(|o| (o.id, o.episodes)).collect();

        let mut entries: Vec<EntryCard> = self
            .nodes
            .iter()
            .enumerate()
            .map(|(i, node)| {
                let alternates = if i == target_index { alternates } else { &[] };
                let absolute = curated
                    .get(&node.id)
                    .copied()
                    .unwrap_or_else(|| walk_offset(node.id, &prequels, &by_id));
                card(node, alternates, absolute)
            })
            .collect();

        // The first year of each TVDB show, from its numbered seasons
        // (a pilot filed under specials is no start).
        let mut start_years: HashMap<i64, i32> = HashMap::new();
        for row in self.tvdb.iter().filter(|r| r.season > 0) {
            if let Some(year) = by_id.get(&row.id).and_then(|n| n.season_year) {
                start_years
                    .entry(row.tvdb_id)
                    .and_modify(|y| *y = (*y).min(year))
                    .or_insert(year);
            }
        }
        for entry in &mut entries {
            for row in self
                .tvdb
                .iter()
                .filter(|r| r.id == entry.id && r.season > 0)
            {
                entry.frames.tvdb.push((row.season, row.episode_offset));
                if !entry.seasons.contains(&row.season) {
                    entry.seasons.push(row.season);
                }
                if let Some(year) = start_years.get(&row.tvdb_id) {
                    let start = entry.frames.tvdb_start_year.get_or_insert(*year);
                    *start = (*start).min(*year);
                }
            }
        }
        let chain = self.chain_from(target);
        for entry in &mut entries {
            entry.on_chain = chain.contains(&entry.id);
        }
        // A first TV entry with no season marker and no TVDB row is
        // season 1 by convention.
        for entry in &mut entries {
            if entry.seasons.is_empty() && entry.is_tv() && entry.frames.absolute == 0 {
                entry.seasons.push(1);
            }
        }
        Some(Franchise {
            target: target_index,
            entries,
        })
    }

    /// Every node reached from `start` through chain edges, either way.
    fn chain_from(&self, start: i64) -> HashSet<i64> {
        let mut seen: HashSet<i64> = HashSet::from([start]);
        let mut queue = vec![start];
        while let Some(id) = queue.pop() {
            for edge in &self.edges {
                if !Relation::parse(&edge.relation).is_chain() {
                    continue;
                }
                let next = if edge.from == id {
                    edge.to
                } else if edge.to == id {
                    edge.from
                } else {
                    continue;
                };
                if seen.insert(next) {
                    queue.push(next);
                }
            }
        }
        seen
    }

    /// Each node's TV prequels (the related node's format TV), from
    /// `PREQUEL` edges and reversed `SEQUEL` edges.
    fn tv_prequels(&self, by_id: &HashMap<i64, &GraphNode>) -> HashMap<i64, Vec<i64>> {
        let mut out: HashMap<i64, Vec<i64>> = HashMap::new();
        let is_tv = |id: i64| by_id.get(&id).is_some_and(|n| n.format == "TV");
        for edge in &self.edges {
            let (entry, prequel) = match Relation::parse(&edge.relation) {
                Relation::Prequel => (edge.from, edge.to),
                Relation::Sequel => (edge.to, edge.from),
                _ => continue,
            };
            if is_tv(prequel) && by_id.contains_key(&entry) {
                let list = out.entry(entry).or_default();
                if !list.contains(&prequel) {
                    list.push(prequel);
                }
            }
        }
        out
    }
}

/// The TV PREQUEL walk of `local_metadata::compute_cumulative_prior_episodes`:
/// the longest TV prequel at each step, at most 20 hops.
fn walk_offset(
    id: i64,
    prequels: &HashMap<i64, Vec<i64>>,
    by_id: &HashMap<i64, &GraphNode>,
) -> i32 {
    let mut offset = 0i32;
    let mut current = id;
    let mut visited: HashSet<i64> = HashSet::from([id]);
    for _ in 0..20 {
        let Some(prev) = prequels.get(&current).and_then(|list| {
            list.iter()
                .copied()
                .max_by_key(|p| by_id.get(p).and_then(|n| n.episodes).unwrap_or(0))
        }) else {
            break;
        };
        if !visited.insert(prev) {
            break;
        }
        let episodes = by_id.get(&prev).and_then(|n| n.episodes).unwrap_or(0);
        offset = offset.saturating_add(episodes.max(0));
        current = prev;
    }
    offset
}

fn forms(names: &[String]) -> Vec<NameForm> {
    names
        .iter()
        .filter(|n| !n.trim().is_empty())
        .map(|n| NameForm::of(n))
        .filter(|f| !f.is_empty())
        .collect()
}

fn card(node: &GraphNode, alternates: &[String], absolute: i32) -> EntryCard {
    let names = NameFields {
        romaji: forms(std::slice::from_ref(&node.title_romaji)),
        english: forms(std::slice::from_ref(&node.title_english)),
        native: forms(std::slice::from_ref(&node.title_native)),
        synonyms: forms(&node.synonyms),
        alternates: forms(alternates),
    };
    let mut seasons: Vec<i32> = Vec::new();
    let mut parts: Vec<i32> = Vec::new();
    let mut sequence = None;
    for field in NameFields::CANONICAL {
        for form in names.get(field) {
            if let Some(s) = form.season
                && !seasons.contains(&s)
            {
                seasons.push(s);
            }
            if let Some(p) = form.part
                && !parts.contains(&p)
            {
                parts.push(p);
            }
            if sequence.is_none() {
                sequence = form.sequence;
            }
        }
    }
    let title = [&node.title_romaji, &node.title_english, &node.title_native]
        .into_iter()
        .find(|t| !t.trim().is_empty())
        .cloned()
        .unwrap_or_else(|| node.id.to_string());
    EntryCard {
        id: node.id,
        mal_id: node.mal_id,
        title,
        format: node.format.clone(),
        episodes: node.episodes.filter(|n| *n > 0),
        season_year: node.season_year,
        end_year: node.end_year,
        names,
        seasons,
        parts,
        sequence,
        frames: Frames {
            absolute,
            ..Frames::default()
        },
        on_chain: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(id: i64, romaji: &str, format: &str, episodes: i32, year: i32) -> GraphNode {
        GraphNode {
            id,
            title_romaji: romaji.to_string(),
            format: format.to_string(),
            episodes: Some(episodes),
            season_year: Some(year),
            ..GraphNode::default()
        }
    }

    fn prequel(from: i64, to: i64) -> GraphEdge {
        GraphEdge {
            from,
            to,
            relation: "PREQUEL".to_string(),
        }
    }

    #[test]
    fn relation_spellings_fold_together() {
        assert_eq!(Relation::parse("PARENT_STORY"), Relation::Parent);
        assert_eq!(Relation::parse("SPIN-OFF"), Relation::SpinOff);
        assert_eq!(
            Relation::parse("ALTERNATIVE_VERSION"),
            Relation::Alternative
        );
        assert_eq!(
            Relation::parse("alternative setting"),
            Relation::Alternative
        );
        assert_eq!(Relation::parse("FULL_STORY"), Relation::FullStory);
        assert!(Relation::parse("SEQUEL").is_chain());
        assert!(Relation::parse("SUMMARY").is_side());
        assert_eq!(Relation::parse("CHARACTER"), Relation::Other);
    }

    #[test]
    fn absolute_offsets_follow_the_tv_chain_and_curated_rules_win() {
        let graph = FranchiseGraph {
            nodes: vec![
                node(1, "Jujutsu Kaisen", "TV", 24, 2020),
                node(2, "Jujutsu Kaisen 2nd Season", "TV", 23, 2023),
                node(
                    3,
                    "Jujutsu Kaisen: Shimetsu Kaiyuu - Zenpen",
                    "TV",
                    12,
                    2026,
                ),
                node(4, "Jujutsu Kaisen 0", "MOVIE", 1, 2021),
            ],
            // S3's prequel through a reversed SEQUEL edge; the movie is no
            // part of the count.
            edges: vec![
                prequel(2, 1),
                GraphEdge {
                    from: 2,
                    to: 3,
                    relation: "SEQUEL".to_string(),
                },
                prequel(2, 4),
            ],
            ..FranchiseGraph::default()
        };
        let f = graph.build(3, &[]).expect("target is a node");
        let abs = |id| f.entry(id).unwrap().frames.absolute;
        assert_eq!((abs(1), abs(2), abs(3), abs(4)), (0, 24, 47, 0));
        assert_eq!(f.entry(1).unwrap().seasons, [1]);
        assert_eq!(f.entry(2).unwrap().seasons, [2]);

        let mut curated = graph.clone();
        curated.offsets.push(OffsetRow {
            id: 3,
            episodes: 50,
        });
        let f = curated.build(3, &[]).unwrap();
        assert_eq!(f.entry(3).unwrap().frames.absolute, 50);
        assert!(graph.build(99, &[]).is_none());
    }

    #[test]
    fn tvdb_rows_give_seasons_offsets_and_the_shows_first_year() {
        let row = |id, season, episode_offset| TvdbRow {
            id,
            tvdb_id: 262954,
            season,
            episode_offset,
        };
        let graph = FranchiseGraph {
            nodes: vec![
                node(1, "JoJo no Kimyou na Bouken", "TV", 26, 2012),
                node(
                    2,
                    "JoJo no Kimyou na Bouken: Stardust Crusaders",
                    "TV",
                    24,
                    2014,
                ),
                node(
                    3,
                    "JoJo no Kimyou na Bouken: Stardust Crusaders - Egypt-hen",
                    "TV",
                    24,
                    2015,
                ),
                node(4, "Kishibe Rohan wa Ugokanai", "OVA", 4, 2007),
            ],
            edges: vec![prequel(2, 1), prequel(3, 2)],
            tvdb: vec![
                row(1, 1, Some(0)),
                row(2, 2, Some(0)),
                row(3, 2, Some(24)),
                // A special filed under season 0 starts nothing.
                row(4, 0, None),
            ],
            ..FranchiseGraph::default()
        };
        let f = graph.build(3, &[]).unwrap();
        let egypt = f.entry(3).unwrap();
        assert_eq!(egypt.frames.tvdb, [(2, Some(24))]);
        assert_eq!(egypt.frames.tvdb_start_year, Some(2012));
        assert_eq!(egypt.seasons, [2]);
        let rohan = f.entry(4).unwrap();
        assert!(rohan.frames.tvdb.is_empty() && rohan.seasons.is_empty());
    }
}
