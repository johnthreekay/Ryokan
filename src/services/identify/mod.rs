//! Which franchise entry a release belongs to, decided by comparing the
//! release against the target and every entry it could be confused with.
//!
//! The alias gates answer "does this title name the target?". This
//! answers the next question: given that it does, does another entry of
//! the same franchise fit it better? The target is presumed; a rival has
//! to prove it, on the first of four tiers that holds:
//!
//! - **S (structure)**: the target fails a claim the title makes (season,
//!   part, episode, year, format), the rival fits every claim, and the
//!   rival's names hold every franchise word the target's do.
//!   `Fruits Basket (2019) - 25` for the 2001 series.
//! - **A (names more)**: the rival's names hold the title's franchise
//!   words the target's hold, and more, and either name the rival fully
//!   or explain two more words. `Jujutsu Kaisen: Shimetsu Kaiyuu - Zenpen
//!   - 06` for the first season.
//! - **B (names fully)**: the same franchise words, the title says the
//!   whole of a rival name but not of any target name, and the rival fits
//!   every claim. `JoJo no Kimyou na Bouken - 01-03` for Steel Ball Run.
//! - **C (marker convention)**: the same words, equally complete, but
//!   every name of the target carries a season or part marker the title
//!   doesn't state and the rival's don't, and the rival fits every claim.
//!   `Spy x Family - 05` for Part 2.
//!
//! "Names fully" and "names as well" read a sequel marker the title
//! doesn't state as one more unsaid word on the rival's side
//! (`JUJUTSU KAISEN Season 3` is not said by `Jujutsu Kaisen - 05`); the
//! target's side of B and C reads names without markers, so a target
//! named only by its marker is C's case.
//!
//! With no veto, a rival that names the title at least as well (or fits
//! claims the target fails) makes the verdict `Ambiguous`, unless notes
//! beside the name or a marker the title states point at the target.
//! Several seasons, or an episode range no entry holds alone, is
//! `Spans`. A title with no franchise words, or with words no entry has
//! and no entry named in full, is `Neither`.
//!
//! Missing data (no synonyms, unknown counts, no TVDB row) only removes
//! vetoes: a claim an entry can't judge is `Unknown`, which never
//! confirms a rival, and two entries' names are compared only on the
//! name fields both have. There are no weights.
//!
//! The decision is pure: the franchise comes in as data
//! (`graph::FranchiseGraph`), read out of the metadata caches by
//! `load::load_franchise` or built from a detail's own relations by
//! `Franchise::from_detail`.

mod graph;
mod load;
mod names;
mod structure;

use std::collections::HashSet;

pub use graph::{
    EntryCard, Franchise, FranchiseGraph, GraphEdge, GraphNode, OffsetRow, Relation, TvdbRow,
};
pub use load::{GraphIndex, MAX_CHAIN, load_franchise, load_franchises_for_series, load_graph};

use crate::services::release_title::{self, ParsedTitle};
use graph::{Field, NameFields};
use names::NameForm;
use structure::{Claims, Fits};

/// Which tier a rival's veto rests on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Tier {
    Structure,
    NamesMore,
    NamesFully,
    Marker,
}

impl Tier {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Structure => "structure",
            Self::NamesMore => "names_more",
            Self::NamesFully => "names_fully",
            Self::Marker => "marker",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rival {
    pub entry: i64,
    pub title: String,
    pub tier: Tier,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Nothing fits better than the target.
    Target,
    /// Another entry fits better: a veto.
    Rival(Rival),
    /// The claims cover several entries (a pack across seasons): never a
    /// veto.
    Spans,
    /// Another entry fits as well as the target.
    Ambiguous { entry: i64, title: String },
    /// The title says nothing about this franchise: no veto.
    Neither,
}

impl Verdict {
    pub fn is_veto(&self) -> bool {
        matches!(self, Self::Rival(_))
    }
}

/// Identify a release title against a franchise.
pub fn identify(franchise: &Franchise, title: &str) -> Verdict {
    identify_parsed(franchise, &release_title::parse(title))
}

/// [`identify`] for a title already parsed.
pub fn identify_parsed(franchise: &Franchise, parsed: &ParsedTitle) -> Verdict {
    identify_prepared(franchise, &PreparedTitle::of(parsed))
}

/// A title read once for judging against many franchises (reading its
/// names runs the release-title parser on each).
pub struct PreparedTitle(TitleView);

impl PreparedTitle {
    pub fn of(parsed: &ParsedTitle) -> Self {
        Self(TitleView::of(parsed))
    }
}

/// [`identify`] for a [`PreparedTitle`].
pub fn identify_prepared(franchise: &Franchise, title: &PreparedTitle) -> Verdict {
    let view = &title.0;
    let target = franchise.target();
    let t = Evidence::own(target, view);

    let all: Vec<Evidence> = franchise
        .entries()
        .iter()
        .map(|e| Evidence::own(e, view))
        .collect();
    if all.iter().all(|e| e.said.is_empty()) {
        return Verdict::Neither;
    }
    if spans(franchise, view, &all) {
        return Verdict::Spans;
    }

    // `against` reads a subset of the fields `own` does: an entry whose
    // own names say nothing of the title says nothing as a rival either.
    let (rivals, unfit): (Vec<Evidence>, Vec<Evidence>) = franchise
        .entries()
        .iter()
        .enumerate()
        .filter(|(i, _)| *i != franchise.target && !all[*i].said.is_empty())
        .map(|(_, r)| Evidence::against(r, target, view))
        .filter(|r| !r.said.is_empty())
        .partition(|r| !r.fits.incompatible());
    // Among equal rivals, the closer one: fewer unsaid words, then an
    // entry on the target's main chain.
    let closeness = |r: &Evidence| (r.unsaid_marked.unwrap_or(usize::MAX), !r.entry.on_chain);
    let veto = rivals
        .iter()
        .filter_map(|r| veto(&t, r).map(|(tier, reason)| (tier, r, reason)))
        .min_by_key(|(tier, r, _)| (*tier, closeness(r)));
    if let Some((tier, r, reason)) = veto {
        return Verdict::Rival(Rival {
            entry: r.entry.id,
            title: r.entry.title.clone(),
            tier,
            reason,
        });
    }

    // A rival the title names in full, with more of its words than the
    // target's, is the title's subject even when its season or episode
    // numbers fit no frame the franchise knows (`BLEACH Thousand-Year
    // Blood War S01E43`: Disney+ numbers the four cours as one season).
    // A year or format it fails still rules it out (`Ranma 1/2 (2024)`).
    if let Some(r) = unfit
        .iter()
        .filter(|r| {
            r.fits.misnumbered_only()
                && r.unsaid_marked == Some(0)
                && r.said.len() > t.said.len()
                && r.said.is_superset(&t.said)
        })
        .min_by_key(|r| closeness(r))
    {
        let mut extra: Vec<&str> = r.said.difference(&t.said).map(String::as_str).collect();
        extra.sort_unstable();
        return Verdict::Rival(Rival {
            entry: r.entry.id,
            title: r.entry.title.clone(),
            tier: Tier::NamesMore,
            reason: format!("{} also names {}", r.entry.title, extra.join(" ")),
        });
    }

    // Franchise words, but no entry's name said in full and words no
    // entry has: another show sharing a word (`Mushoku no Eiyuu` beside
    // `Mushoku Tensei`).
    let vocabulary: HashSet<&str> = franchise
        .entries()
        .iter()
        .flat_map(|e| NameFields::ALL.iter().flat_map(|f| e.names.get(*f)))
        .flat_map(|f| f.latin.iter().map(String::as_str))
        .collect();
    let foreign = view.latin.iter().any(|t| !vocabulary.contains(t.as_str()));
    if t.said.is_empty() || foreign && all.iter().all(|e| e.unsaid.is_none_or(|u| u > 0)) {
        return Verdict::Neither;
    }

    let ties: Vec<&Evidence> = rivals
        .iter()
        .filter(|r| {
            let names_as_well = r.said.is_superset(&t.said)
                && match (r.unsaid_marked, t.unsaid_marked) {
                    (Some(ru), Some(tu)) => ru <= tu,
                    (Some(_), None) => true,
                    _ => false,
                };
            // A rival fits as well only if it shows every claim the target
            // shows: one whose data can't say (an unannounced entry) is no
            // tie for a target that fits.
            let fits_as_well = !(t.fits.confirmed() && !r.fits.confirmed());
            names_as_well && fits_as_well || t.fits.incompatible() && r.said.is_superset(&t.said)
        })
        .collect();
    if ties.is_empty() {
        return Verdict::Target;
    }

    // Sonarr's identity: a title whose name is exactly an entry's (clean
    // titles equal, years and season words kept) is that entry's. It
    // decides a tie either way: `Fruits Basket 1st Season - 01 ~ 25` is
    // the 2019 remake's AniList title, not the 2001 series'.
    let target_exact = view.is_exactly(target);
    let exact_ties: Vec<&&Evidence> = ties.iter().filter(|r| view.is_exactly(r.entry)).collect();
    if !target_exact && let Some(r) = exact_ties.iter().min_by_key(|r| closeness(r)) {
        return Verdict::Rival(Rival {
            entry: r.entry.id,
            title: r.entry.title.clone(),
            tier: Tier::NamesFully,
            reason: format!("the title is {}'s name exactly", r.entry.title),
        });
    }
    if target_exact && exact_ties.is_empty() {
        return Verdict::Target;
    }
    let r = if target_exact {
        exact_ties.into_iter().copied().min_by_key(|r| closeness(r))
    } else {
        ties.into_iter().min_by_key(|r| closeness(r))
    }
    .expect("a tie");

    // What the title writes beside or in its name breaks a remaining tie
    // for the target only, too weak to veto with: notes naming the target,
    // or a marker the target's names carry and the rival's don't
    // (`Spy x Family Part 2 - 05`).
    if view.notes_name(target) && !view.notes_name(r.entry)
        || t.names_stated_marker && !r.names_stated_marker
    {
        return Verdict::Target;
    }
    Verdict::Ambiguous {
        entry: r.entry.id,
        title: r.entry.title.clone(),
    }
}

/// The title's side: its main names' tokens, numbers it writes, notes
/// and claims.
struct TitleView {
    latin: HashSet<String>,
    /// The main names folded, for CJK containment.
    cjk_text: String,
    /// Numbers the title writes outside its claims (`Mob Psycho 100
    /// (BD)` writes 100 though its series title stops before it).
    numbers: HashSet<String>,
    notes: Vec<NameForm>,
    notes_latin: HashSet<String>,
    notes_text: String,
    claims: Claims,
    /// The main names as `names::clean_title` reads them.
    exact: Vec<String>,
}

impl TitleView {
    fn of(parsed: &ParsedTitle) -> Self {
        let zones = parsed.name_zones();
        let mains: Vec<NameForm> = zones.names.iter().map(|n| NameForm::of(n)).collect();
        let latin: HashSet<String> = mains.iter().flat_map(|f| f.latin.iter().cloned()).collect();
        let mut numbers: HashSet<String> = latin
            .iter()
            .filter(|t| t.chars().all(|c| c.is_ascii_digit()))
            .cloned()
            .collect();
        if let Some(n) = parsed.ambiguous_number {
            numbers.insert(n.to_string());
        }
        let notes: Vec<NameForm> = zones.notes.iter().map(|n| NameForm::of(n)).collect();
        Self {
            latin,
            cjk_text: names::fold(&zones.names.join(" ")),
            numbers,
            notes_latin: notes.iter().flat_map(|f| f.latin.iter().cloned()).collect(),
            notes_text: names::fold(&zones.notes.join(" ")),
            notes,
            claims: Claims::of(parsed, mains.iter().find_map(|f| f.part)),
            exact: zones
                .names
                .iter()
                .map(|n| names::clean_title(n))
                .filter(|n| !n.is_empty())
                .collect(),
        }
    }

    /// The title's name is exactly one of the entry's titles.
    fn is_exactly(&self, entry: &EntryCard) -> bool {
        self.exact.iter().any(|n| entry.exact_names.contains(n))
    }

    fn says_latin(&self, token: &str) -> bool {
        self.latin.contains(token)
            || self.numbers.contains(token.trim_start_matches('0'))
            || self.numbers.contains(token)
    }

    fn says_cjk(&self, run: &str) -> bool {
        self.cjk_text.contains(run)
    }

    /// Whether the notes say one of the entry's canonical names in full.
    fn notes_name(&self, entry: &EntryCard) -> bool {
        !self.notes.is_empty()
            && NameFields::CANONICAL
                .iter()
                .flat_map(|f| entry.names.get(*f))
                .any(|form| {
                    !form.is_empty()
                        && form.latin.iter().all(|t| self.notes_latin.contains(t))
                        && form
                            .cjk
                            .iter()
                            .all(|c| self.notes_text.contains(c.as_str()))
                })
    }
}

/// What one entry's names and data say about the title.
struct Evidence<'a> {
    entry: &'a EntryCard,
    fits: Fits,
    /// X: the title's franchise words the entry's names hold.
    said: HashSet<String>,
    /// U: the fewest distinctive words of a canonical name the title
    /// leaves unsaid (`None`: no canonical name to judge). Markers are no
    /// words here.
    unsaid: Option<usize>,
    /// U counting an unstated season or part marker, and an unstated
    /// format word, as one more unsaid word each: `JUJUTSU KAISEN Season
    /// 3` and `HUNTER×HUNTER OVA` are not said in full by a title that
    /// names neither.
    unsaid_marked: Option<usize>,
    /// M: every Latin canonical name carries a season or part marker the
    /// title doesn't state.
    marked: bool,
    /// A Latin canonical name carries the marker the title states.
    names_stated_marker: bool,
}

impl<'a> Evidence<'a> {
    /// Against its own names, every field.
    fn own(entry: &'a EntryCard, view: &TitleView) -> Self {
        Self::with_fields(entry, view, |_| true)
    }

    /// A rival against the target: only the name fields the target has.
    fn against(entry: &'a EntryCard, target: &EntryCard, view: &TitleView) -> Self {
        Self::with_fields(entry, view, |f| target.names.has(f))
    }

    fn with_fields(entry: &'a EntryCard, view: &TitleView, keep: impl Fn(Field) -> bool) -> Self {
        let mut said = HashSet::new();
        for field in NameFields::ALL.into_iter().filter(|f| keep(*f)) {
            for form in entry.names.get(field) {
                said.extend(form.latin.iter().filter(|t| view.says_latin(t)).cloned());
                said.extend(form.cjk.iter().filter(|c| view.says_cjk(c)).cloned());
            }
        }
        let counts: Vec<(usize, usize)> = NameFields::CANONICAL
            .into_iter()
            .filter(|f| keep(*f))
            .flat_map(|f| entry.names.get(f))
            .filter_map(|form| {
                let u = unsaid_in(form, view)?;
                let marker = form.sequel_marker().is_some_and(|m| !view.claims.states(m));
                let format = form.format_word && !(view.claims.special || view.claims.movie);
                Some((u, u + usize::from(marker) + usize::from(format)))
            })
            .collect();
        let unsaid = counts.iter().map(|c| c.0).min();
        let unsaid_marked = counts.iter().map(|c| c.1).min();
        let latin_names: Vec<&NameForm> = [Field::Romaji, Field::English, Field::Alternates]
            .into_iter()
            .flat_map(|f| entry.names.get(f))
            .collect();
        let marked = !latin_names.is_empty()
            && latin_names
                .iter()
                .all(|form| form.sequel_marker().is_some_and(|m| !view.claims.states(m)));
        let names_stated_marker = latin_names
            .iter()
            .any(|form| form.sequel_marker().is_some_and(|m| view.claims.states(m)));
        Self {
            entry,
            fits: structure::fit(entry, &view.claims),
            said,
            unsaid,
            unsaid_marked,
            marked,
            names_stated_marker,
        }
    }
}

/// How many of a name's distinctive Latin words the title leaves unsaid.
/// CJK runs never count against a name, but a name whose CJK the title
/// doesn't hold can't be judged by its Latin words alone.
fn unsaid_in(form: &NameForm, view: &TitleView) -> Option<usize> {
    if form.is_empty() || form.cjk.iter().any(|c| !view.says_cjk(c)) {
        return None;
    }
    Some(form.latin.iter().filter(|t| !view.says_latin(t)).count())
}

fn veto(t: &Evidence, r: &Evidence) -> Option<(Tier, String)> {
    let r_confirmed = r.fits.confirmed();
    if t.fits.incompatible() && r_confirmed && r.said.is_superset(&t.said) {
        let failed = t.fits.first_no().unwrap_or("claim");
        return Some((
            Tier::Structure,
            format!(
                "the title's {failed} fits {} and not the target",
                r.entry.title
            ),
        ));
    }
    if r.said.len() > t.said.len() && r.said.is_superset(&t.said) {
        let mut extra: Vec<&str> = r.said.difference(&t.said).map(String::as_str).collect();
        extra.sort_unstable();
        if r.unsaid_marked == Some(0) || extra.len() >= 2 {
            return Some((
                Tier::NamesMore,
                format!("{} also names {}", r.entry.title, extra.join(" ")),
            ));
        }
    }
    if r.said == t.said
        && r_confirmed
        && r.unsaid_marked == Some(0)
        && t.unsaid.is_some_and(|u| u > 0)
    {
        return Some((
            Tier::NamesFully,
            format!("the title says {} in full", r.entry.title),
        ));
    }
    if r.said == t.said
        && r_confirmed
        && r.unsaid.is_some()
        && r.unsaid == t.unsaid
        && t.marked
        && !r.marked
    {
        return Some((
            Tier::Marker,
            format!(
                "every name of the target carries a marker the title doesn't state; {} has none",
                r.entry.title
            ),
        ));
    }
    None
}

/// A pack across entries: several seasons, or an episode range no single
/// entry holds that two or more entries' numberings overlap.
fn spans(franchise: &Franchise, view: &TitleView, all: &[Evidence]) -> bool {
    if view.claims.multi_season {
        return true;
    }
    let Some((first, last)) = view.claims.episodes.filter(|(f, l)| l > f) else {
        return false;
    };
    if all
        .iter()
        .any(|e| e.fits.episode == Some(structure::Fit::Yes))
    {
        return false;
    }
    franchise
        .entries()
        .iter()
        .filter(|e| {
            structure::episode_frames(e)
                .iter()
                .any(|&(lo, hi)| lo <= last && hi.is_none_or(|hi| first <= hi))
        })
        .count()
        >= 2
}

#[cfg(test)]
mod tests;
