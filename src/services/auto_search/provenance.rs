//! Match provenance: how a search candidate's title matched the series.
//!
//! Every candidate that passes the title gate is stamped with the alias
//! it matched, whether the match was verbatim or fuzzy, and which
//! search phase produced it. Scoring uses it to prefer a verbatim
//! candidate over an otherwise-equal fuzzy one, the "Grabbed:" log line
//! and the grab history carry it so a misgrab is diagnosable after the
//! fact, and the interactive search breakdown shows it as "Title Match
//! Confidence". The string forms below are the contract shared by JSON,
//! the log, and the `episode_grab_history` columns; the round-trip test
//! at the bottom keeps them from drifting.

use serde::{Deserialize, Serialize};

/// How the release title matched an alias.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum MatchKind {
    /// The normalized alias appears verbatim in the normalized title.
    Verbatim,
    /// Enough of the alias's distinctive tokens appear in the title.
    Fuzzy,
    /// The infohash is a SeaDex pick for this series; the title gate
    /// was bypassed on purpose.
    SeadexCurated,
}

impl MatchKind {
    pub fn as_str(self) -> &'static str {
        match self {
            MatchKind::Verbatim => "verbatim",
            MatchKind::Fuzzy => "fuzzy",
            MatchKind::SeadexCurated => "seadex_curated",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "verbatim" => Some(MatchKind::Verbatim),
            "fuzzy" => Some(MatchKind::Fuzzy),
            "seadex_curated" => Some(MatchKind::SeadexCurated),
            _ => None,
        }
    }
}

/// Which query phase surfaced the candidate. The extended and
/// franchise passes run looser aliases (synonyms, colon-split
/// sub-phrases, franchise roots with no sibling guard), which is why
/// scoring treats them as lower confidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum MatchPhase {
    Primary,
    Extended,
    PreferredGroup,
    BdProbe,
    Franchise,
    BatchProbe,
    SeadexSeed,
}

impl MatchPhase {
    pub fn as_str(self) -> &'static str {
        match self {
            MatchPhase::Primary => "primary",
            MatchPhase::Extended => "extended",
            MatchPhase::PreferredGroup => "preferred_group",
            MatchPhase::BdProbe => "bd_probe",
            MatchPhase::Franchise => "franchise",
            MatchPhase::BatchProbe => "batch_probe",
            MatchPhase::SeadexSeed => "seadex_seed",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "primary" => Some(MatchPhase::Primary),
            "extended" => Some(MatchPhase::Extended),
            "preferred_group" => Some(MatchPhase::PreferredGroup),
            "bd_probe" => Some(MatchPhase::BdProbe),
            "franchise" => Some(MatchPhase::Franchise),
            "batch_probe" => Some(MatchPhase::BatchProbe),
            "seadex_seed" => Some(MatchPhase::SeadexSeed),
            _ => None,
        }
    }

    /// Human wording for the breakdown and history lines.
    pub fn display_name(self) -> &'static str {
        match self {
            MatchPhase::Primary => "primary pass",
            MatchPhase::Extended => "extended alias pass",
            MatchPhase::PreferredGroup => "preferred group pass",
            MatchPhase::BdProbe => "BD probe",
            MatchPhase::Franchise => "franchise pass",
            MatchPhase::BatchProbe => "batch probe",
            MatchPhase::SeadexSeed => "SeaDex seed",
        }
    }

    /// The passes whose aliases are looser than the series' own titles.
    pub fn is_fallback(self) -> bool {
        matches!(self, MatchPhase::Extended | MatchPhase::Franchise)
    }
}

/// Result of scanning the alias list for one release title. The
/// collector adds the phase to turn it into a [`MatchProvenance`].
#[derive(Debug, Clone, PartialEq)]
pub struct AliasMatch {
    pub kind: MatchKind,
    /// The alias as it was given (not normalized), so logs read the
    /// way the series page shows it.
    pub alias: String,
    /// Share of the alias's distinctive tokens found in the title;
    /// 1.0 for a verbatim match.
    pub ratio: f32,
}

impl AliasMatch {
    pub fn into_provenance(self, phase: MatchPhase) -> MatchProvenance {
        MatchProvenance {
            phase,
            kind: self.kind,
            alias: self.alias,
            ratio: self.ratio,
        }
    }
}

/// Stamped on every candidate that passes the title gate.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct MatchProvenance {
    pub phase: MatchPhase,
    pub kind: MatchKind,
    pub alias: String,
    pub ratio: f32,
}

impl MatchProvenance {
    /// A SeaDex-curated candidate: the alias is empty and the ratio is
    /// 1.0 because the hash, not the title, is what matched.
    pub fn seadex(phase: MatchPhase) -> Self {
        MatchProvenance {
            phase,
            kind: MatchKind::SeadexCurated,
            alias: String::new(),
            ratio: 1.0,
        }
    }

    /// Key-value fragment for log detail strings.
    pub fn log_fragment(&self) -> String {
        format!(
            "match={} phase={} alias={:?} ratio={:.2}",
            self.kind.as_str(),
            self.phase.as_str(),
            self.alias,
            self.ratio
        )
    }

    /// The suffix appended to a grab log line: `, match=...` or
    /// `, match=unknown` when the candidate was never stamped.
    pub fn log_suffix(provenance: Option<&MatchProvenance>) -> String {
        match provenance {
            Some(p) => format!(", {}", p.log_fragment()),
            None => ", match=unknown".to_string(),
        }
    }

    /// One readable sentence for the score breakdown and the grab
    /// history.
    pub fn summary(&self) -> String {
        history_summary(
            self.kind.as_str(),
            self.phase.as_str(),
            &self.alias,
            f64::from(self.ratio),
        )
    }
}

/// Builds the summary sentence from the stored string forms. Returns an
/// empty string for legacy history rows (written before provenance
/// existed), which have an empty `kind`.
pub fn history_summary(kind: &str, phase: &str, alias: &str, ratio: f64) -> String {
    let Some(kind) = MatchKind::parse(kind) else {
        return String::new();
    };
    let phase_name = MatchPhase::parse(phase)
        .map(MatchPhase::display_name)
        .unwrap_or("unknown pass");
    match kind {
        MatchKind::Verbatim => format!("Verbatim alias match: {:?} ({})", alias, phase_name),
        MatchKind::Fuzzy => format!(
            "Fuzzy alias match: {:?} at {}% ({})",
            alias,
            (ratio * 100.0).round() as i64,
            phase_name
        ),
        MatchKind::SeadexCurated => format!("SeaDex curated release ({})", phase_name),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KINDS: [MatchKind; 3] = [
        MatchKind::Verbatim,
        MatchKind::Fuzzy,
        MatchKind::SeadexCurated,
    ];
    const PHASES: [MatchPhase; 7] = [
        MatchPhase::Primary,
        MatchPhase::Extended,
        MatchPhase::PreferredGroup,
        MatchPhase::BdProbe,
        MatchPhase::Franchise,
        MatchPhase::BatchProbe,
        MatchPhase::SeadexSeed,
    ];

    #[test]
    fn string_forms_round_trip_and_match_serde() {
        for kind in KINDS {
            assert_eq!(MatchKind::parse(kind.as_str()), Some(kind));
            assert_eq!(
                serde_json::to_string(&kind).unwrap(),
                format!("\"{}\"", kind.as_str())
            );
        }
        for phase in PHASES {
            assert_eq!(MatchPhase::parse(phase.as_str()), Some(phase));
            assert_eq!(
                serde_json::to_string(&phase).unwrap(),
                format!("\"{}\"", phase.as_str())
            );
        }
        assert_eq!(MatchKind::parse("nope"), None);
        assert_eq!(MatchPhase::parse(""), None);
    }

    #[test]
    fn log_fragment_and_summary_wording() {
        let fuzzy = AliasMatch {
            kind: MatchKind::Fuzzy,
            alias: "Risa THE ANIMATION".to_string(),
            ratio: 0.67,
        }
        .into_provenance(MatchPhase::Extended);
        assert_eq!(
            fuzzy.log_fragment(),
            "match=fuzzy phase=extended alias=\"Risa THE ANIMATION\" ratio=0.67"
        );
        assert_eq!(
            fuzzy.summary(),
            "Fuzzy alias match: \"Risa THE ANIMATION\" at 67% (extended alias pass)"
        );

        let verbatim = AliasMatch {
            kind: MatchKind::Verbatim,
            alias: "Sousou no Frieren".to_string(),
            ratio: 1.0,
        }
        .into_provenance(MatchPhase::Primary);
        assert_eq!(
            verbatim.summary(),
            "Verbatim alias match: \"Sousou no Frieren\" (primary pass)"
        );
        assert_eq!(
            MatchProvenance::log_suffix(Some(&verbatim)),
            ", match=verbatim phase=primary alias=\"Sousou no Frieren\" ratio=1.00"
        );
        assert_eq!(MatchProvenance::log_suffix(None), ", match=unknown");

        let seadex = MatchProvenance::seadex(MatchPhase::SeadexSeed);
        assert_eq!(seadex.summary(), "SeaDex curated release (SeaDex seed)");
        assert_eq!(seadex.ratio, 1.0);
        assert!(seadex.alias.is_empty());
    }

    #[test]
    fn history_summary_is_empty_for_legacy_rows() {
        assert_eq!(history_summary("", "", "", 0.0), "");
        assert_eq!(
            history_summary("verbatim", "garbage", "X", 1.0),
            "Verbatim alias match: \"X\" (unknown pass)"
        );
    }

    #[test]
    fn fallback_phases_are_extended_and_franchise_only() {
        for phase in PHASES {
            assert_eq!(
                phase.is_fallback(),
                matches!(phase, MatchPhase::Extended | MatchPhase::Franchise),
                "{phase:?}"
            );
        }
    }
}
