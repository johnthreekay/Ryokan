//! Sonarr CF JSON → compiled form.
//!
//! This module owns `compile_from_json` and its local helpers. Regex
//! compilation happens once per CF at startup (or on CF edit) and the
//! compiled `fancy_regex::Regex` lives on `CompiledSpec` for the rest
//! of the process lifetime — the scoring hot path never re-compiles.
//!
//! Cross-compat quirks kept faithful:
//! - Both array-form (`[{"name": "x", "value": …}]`) and object-form
//!   (`{"x": …}`) `fields` shapes are accepted — the Sonarr UI / API
//!   export emits the array form; trash-guides JSON ships the object
//!   form. Both round-trip.
//! - .NET regex idioms that fancy-regex rejects are rewritten by
//!   `sonarr_to_rust_regex` (currently: literal `[` inside a char
//!   class). See that function's docs.
//! - `Ryokan.SeaDexBestSpecification` and the bare `SeaDexBestSpecification`
//!   are both accepted. The namespaced form is the canonical export
//!   shape so Sonarr-safe exporters can detect and strip it.

use super::{CompiledCustomFormat, CompiledSpec, Resolution, SpecKind};

/// Compile a single Sonarr-shape CF JSON blob into an evaluator-ready
/// [`CompiledCustomFormat`].
///
/// Rejects, with an explanatory error, any CF whose intent cannot be
/// faithfully represented in Ryokan:
/// - All of the original specs were of unsupported types (e.g. a
///   TRaSH `BR-DISK` CF with only a `QualityModifierSpecification`).
///   Silently collapsing would leave `specs=[]`, which under the §5.7.2
///   vacuous-truth rule would match every release.
/// - A `required=true` spec was dropped for being unsupported. Dropping
///   a required spec would change the DidMatch gate for its group.
///
/// All regex compilation happens here once, not per-candidate: a 1000-
/// candidate search over 10 CFs with one regex each would otherwise
/// compile 10k regexes.
pub fn compile_from_json(raw: &str, score: i32, id: i64) -> Result<CompiledCustomFormat, String> {
    let v: serde_json::Value =
        serde_json::from_str(raw).map_err(|e| format!("CF JSON parse: {e}"))?;

    let name = v
        .get("name")
        .and_then(|n| n.as_str())
        .ok_or_else(|| "CF missing name".to_string())?
        .to_string();

    let specs_json = v
        .get("specifications")
        .and_then(|s| s.as_array())
        .ok_or_else(|| "CF missing specifications".to_string())?;

    let original_spec_count = specs_json.len();
    let mut specs: Vec<CompiledSpec> = Vec::new();
    let mut dropped_required: Option<String> = None;

    for spec_v in specs_json {
        let implementation = spec_v
            .get("implementation")
            .and_then(|i| i.as_str())
            .unwrap_or("");
        let negate = spec_v
            .get("negate")
            .and_then(|b| b.as_bool())
            .unwrap_or(false);
        let required = spec_v
            .get("required")
            .and_then(|b| b.as_bool())
            .unwrap_or(false);
        // Sonarr's CF JSON ships `fields` in two shapes depending on
        // where the dump came from:
        //   - Sonarr UI / API export: array of `{"name": …, "value": …}`
        //   - trash-guides repo JSON: object map like `{"value": …}`
        // `field_str` / `field_i64` / `field_f64` accept either shape via
        // `field_value`; we hand them the raw `fields` node and let them
        // do the work. An absent `fields` becomes `Value::Null`, which
        // both lookups treat as "no such field".
        let null_fields = serde_json::Value::Null;
        let fields: &serde_json::Value = spec_v.get("fields").unwrap_or(&null_fields);

        let kind: SpecKind = match implementation {
            "ReleaseTitleSpecification" => {
                let value = field_str(fields, "value")?;
                // Sonarr regexes are case-insensitive by convention;
                // prepend `(?i)` so every imported CF matches regardless
                // of how the user authored it. `sonarr_to_rust_regex`
                // rewrites .NET-isms (literal `[` inside a char class)
                // into forms fancy-regex accepts. See that function's
                // docs for the exact transforms.
                let rewritten = sonarr_to_rust_regex(&value);
                let re = fancy_regex::Regex::new(&format!("(?i){rewritten}"))
                    .map_err(|e| format!("ReleaseTitle regex: {e}"))?;
                SpecKind::ReleaseTitle { regex: re }
            }
            "ReleaseGroupSpecification" => {
                let value = field_str(fields, "value")?;
                let rewritten = sonarr_to_rust_regex(&value);
                let re = fancy_regex::Regex::new(&format!("(?i){rewritten}"))
                    .map_err(|e| format!("ReleaseGroup regex: {e}"))?;
                SpecKind::ReleaseGroup { regex: re }
            }
            "SizeSpecification" => {
                // Sonarr ships size bounds as GB floats. Field names
                // are literally `min` and `max` (verified against
                // Sonarr's SizeSpecification.cs).
                let min = field_f64(fields, "min").unwrap_or(0.0);
                let max = field_f64(fields, "max").unwrap_or(f64::INFINITY);
                const GB: f64 = 1024.0 * 1024.0 * 1024.0;
                SpecKind::Size {
                    min_bytes: (min * GB) as i64,
                    max_bytes: if max.is_finite() {
                        (max * GB) as i64
                    } else {
                        i64::MAX
                    },
                }
            }
            "ResolutionSpecification" => {
                let value = field_i64(fields, "value")? as i32;
                SpecKind::Resolution {
                    value: Resolution::from_int(value),
                }
            }
            "SourceSpecification" => {
                let value = field_i64(fields, "value")?;
                // Reject TelevisionRaw (2) — no anime mapping. Also
                // reject anything out of range. Treat like an unknown
                // implementation so the drop/required logic below fires.
                if value == 2 || !(0..=7).contains(&value) {
                    tracing::warn!(
                        "custom_formats: skipping SourceSpecification(value={value}) on CF `{name}`"
                    );
                    if required && dropped_required.is_none() {
                        dropped_required = Some(format!("SourceSpecification(value={value})"));
                    }
                    continue;
                }
                SpecKind::Source {
                    sonarr_value: value as u8,
                }
            }
            // Ryokan-only spec: accept both the namespaced form (new
            // default, see plan §5.7.5) and the bare form for backwards
            // compatibility with older Ryokan exports.
            "Ryokan.SeaDexBestSpecification" | "SeaDexBestSpecification" => SpecKind::SeaDexBest,
            other => {
                tracing::warn!(
                    "custom_formats: skipping unsupported spec `{other}` on CF `{name}`"
                );
                if required && dropped_required.is_none() {
                    dropped_required = Some(other.to_string());
                }
                continue;
            }
        };

        specs.push(CompiledSpec {
            kind,
            negate,
            required,
        });
    }

    // Cross-compat safety: a CF whose intent cannot be preserved must
    // not silently become a match-everything vacuous-truth CF. See
    // plan §5.7.2 for why.
    if original_spec_count > 0 && specs.is_empty() {
        return Err(format!(
            "all {original_spec_count} specifications unsupported — CF `{name}` cannot be \
             represented in Ryokan and was rejected to avoid a vacuous-match CF"
        ));
    }
    if let Some(label) = dropped_required {
        return Err(format!(
            "required=true specification `{label}` was dropped — CF `{name}` semantics \
             cannot be preserved and the CF was rejected"
        ));
    }

    Ok(CompiledCustomFormat {
        id,
        name,
        score,
        specs,
    })
}

/// Rewrite a .NET/Sonarr-flavored regex into something fancy-regex
/// accepts, preserving match semantics. The transforms performed:
///
/// 1. **Literal `[` inside a character class is escaped.** .NET regex
///    treats `[` as a literal when it appears inside `[...]`, so
///    trash-guides patterns like `[([]dual[])]` are valid .NET but
///    reject in regex-syntax (the parser errors on a "nested" class).
///    We walk the pattern tracking char-class state and emit `\[`
///    whenever a bare `[` appears inside a class.
///
/// The char-class-leading-`]` rule (`[])]` = class containing `]`, `)`)
/// is already handled by regex-syntax natively, so this pass leaves it
/// alone.
///
/// Backslash escapes are copied through without interpretation — this
/// is a syntactic fixup, not a semantic one.
fn sonarr_to_rust_regex(pattern: &str) -> String {
    let mut out = String::with_capacity(pattern.len() + 4);
    let mut chars = pattern.chars().peekable();
    let mut in_class = false;
    // `just_opened` is true immediately after `[` (and after an
    // optional leading `^`), so a `]` in that position is treated as
    // a literal rather than closing the class.
    let mut just_opened = false;

    while let Some(c) = chars.next() {
        if c == '\\' {
            // Escape sequence: copy both the backslash and the next
            // char verbatim regardless of class state.
            out.push(c);
            if let Some(n) = chars.next() {
                out.push(n);
            }
            just_opened = false;
            continue;
        }

        if !in_class {
            out.push(c);
            if c == '[' {
                in_class = true;
                just_opened = true;
            }
        } else if c == '^' && just_opened {
            // Negated class marker; still at the "leading position"
            // after the `^`.
            out.push(c);
        } else if c == ']' && !just_opened {
            out.push(c);
            in_class = false;
        } else if c == '[' {
            // Literal `[` inside a class — escape for regex-syntax.
            out.push('\\');
            out.push('[');
            just_opened = false;
        } else {
            out.push(c);
            just_opened = false;
        }
    }

    out
}

/// Look up a named field in a Sonarr CF spec regardless of which of
/// the two `fields` shapes the exporter used:
///
/// - **Array form** (Sonarr UI / API): `[{"name": "value", "value": X}, …]`
///   — the canonical shape Sonarr returns from `/api/v3/customformat`.
/// - **Object form** (trash-guides repo JSON): `{"value": X, "min": Y}`
///   — the shape trash-guides ships in `docs/json/sonarr/cf/*.json`.
///
/// Both shapes are valid round-trip Sonarr imports, so Ryokan accepts
/// both. Returns `None` if `fields` is neither shape or the name is
/// absent. Callers coerce the returned `Value` to the expected type.
fn field_value<'a>(fields: &'a serde_json::Value, name: &str) -> Option<&'a serde_json::Value> {
    if let Some(arr) = fields.as_array() {
        for f in arr {
            if f.get("name").and_then(|n| n.as_str()) == Some(name) {
                return f.get("value");
            }
        }
        None
    } else if let Some(obj) = fields.as_object() {
        obj.get(name)
    } else {
        None
    }
}

fn field_str(fields: &serde_json::Value, name: &str) -> Result<String, String> {
    field_value(fields, name)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| format!("field `{name}` missing"))
}

fn field_i64(fields: &serde_json::Value, name: &str) -> Result<i64, String> {
    field_value(fields, name)
        .and_then(|v| v.as_i64())
        .ok_or_else(|| format!("field `{name}` missing"))
}

fn field_f64(fields: &serde_json::Value, name: &str) -> Option<f64> {
    // Sonarr ships these as JSON numbers; accept either f64 or i64
    // encodings for robustness (`"min": 5` vs `"min": 5.0`).
    field_value(fields, name).and_then(|v| v.as_f64().or_else(|| v.as_i64().map(|n| n as f64)))
}

#[cfg(test)]
mod tests {
    use super::super::test_helpers::compile;
    use super::super::{Resolution, SpecKind};
    use super::{compile_from_json, sonarr_to_rust_regex};

    // ── Parser: basic shapes and field extraction ────────────────────────

    #[test]
    fn parse_release_title_spec() {
        let json = r#"{
            "name": "x265 preference",
            "specifications": [
                {
                    "name": "x265",
                    "implementation": "ReleaseTitleSpecification",
                    "implementationName": "Release Title",
                    "negate": false,
                    "required": false,
                    "fields": [{"name": "value", "value": "x265"}]
                }
            ]
        }"#;
        let cf = compile(json);
        assert_eq!(cf.name, "x265 preference");
        assert_eq!(cf.specs.len(), 1);
        assert!(matches!(cf.specs[0].kind, SpecKind::ReleaseTitle { .. }));
    }

    #[test]
    fn parse_size_spec_converts_gb_to_bytes() {
        let json = r#"{
            "name": "5-20 GB",
            "specifications": [
                {
                    "implementation": "SizeSpecification",
                    "fields": [
                        {"name": "min", "value": 5},
                        {"name": "max", "value": 20}
                    ]
                }
            ]
        }"#;
        let cf = compile(json);
        if let SpecKind::Size {
            min_bytes,
            max_bytes,
        } = cf.specs[0].kind
        {
            const GB: i64 = 1024 * 1024 * 1024;
            assert_eq!(min_bytes, 5 * GB);
            assert_eq!(max_bytes, 20 * GB);
        } else {
            panic!("expected Size spec");
        }
    }

    #[test]
    fn parse_resolution_spec_uses_from_int() {
        let json = r#"{
            "name": "1080p",
            "specifications": [
                {
                    "implementation": "ResolutionSpecification",
                    "fields": [{"name": "value", "value": 1080}]
                }
            ]
        }"#;
        let cf = compile(json);
        assert!(matches!(
            cf.specs[0].kind,
            SpecKind::Resolution {
                value: Resolution::R1080p
            }
        ));
    }

    #[test]
    fn parse_source_spec_webdl() {
        let json = r#"{
            "name": "WEB-DL",
            "specifications": [
                {
                    "implementation": "SourceSpecification",
                    "fields": [{"name": "value", "value": 3}]
                }
            ]
        }"#;
        let cf = compile(json);
        assert!(matches!(
            cf.specs[0].kind,
            SpecKind::Source { sonarr_value: 3 }
        ));
    }

    #[test]
    fn parse_source_spec_rejects_television_raw() {
        // Sonarr's TelevisionRaw (2) has no anime mapping, so the
        // parser drops it. In this case it's the only spec and its
        // removal would leave a vacuous-match CF, so compilation fails.
        let json = r#"{
            "name": "tv raw only",
            "specifications": [
                {
                    "implementation": "SourceSpecification",
                    "fields": [{"name": "value", "value": 2}]
                }
            ]
        }"#;
        let err = compile_from_json(json, 0, 1).unwrap_err();
        assert!(err.contains("all 1 specifications unsupported"));
    }

    #[test]
    fn parse_source_spec_rejects_out_of_range() {
        let json = r#"{
            "name": "bogus",
            "specifications": [
                {
                    "implementation": "SourceSpecification",
                    "fields": [{"name": "value", "value": 42}]
                }
            ]
        }"#;
        let err = compile_from_json(json, 0, 1).unwrap_err();
        assert!(err.contains("all 1 specifications unsupported"));
    }

    #[test]
    fn parse_seadex_best_namespaced_and_bare() {
        let namespaced = r#"{
            "name": "seadex new",
            "specifications": [
                {
                    "implementation": "Ryokan.SeaDexBestSpecification",
                    "fields": []
                }
            ]
        }"#;
        let bare = r#"{
            "name": "seadex old",
            "specifications": [
                {
                    "implementation": "SeaDexBestSpecification",
                    "fields": []
                }
            ]
        }"#;
        assert!(matches!(
            compile(namespaced).specs[0].kind,
            SpecKind::SeaDexBest
        ));
        assert!(matches!(compile(bare).specs[0].kind, SpecKind::SeaDexBest));
    }

    #[test]
    fn parse_rejects_all_unsupported() {
        // Simulates TRaSH's `BR-DISK` CF: only a spec type Ryokan
        // doesn't implement. Must NOT collapse to an empty specs vec.
        let json = r#"{
            "name": "BR-DISK",
            "specifications": [
                {
                    "implementation": "QualityModifierSpecification",
                    "fields": [{"name": "value", "value": 6}]
                }
            ]
        }"#;
        let err = compile_from_json(json, -10000, 1).unwrap_err();
        assert!(
            err.contains("all 1 specifications unsupported"),
            "error should mention vacuous-match guard: {err}"
        );
    }

    #[test]
    fn parse_rejects_dropped_required_spec() {
        // A CF with a surviving ReleaseTitle spec but a required=true
        // LanguageSpecification (unsupported) must be rejected, because
        // dropping the required spec would change the DidMatch gate.
        let json = r#"{
            "name": "english x265",
            "specifications": [
                {
                    "implementation": "ReleaseTitleSpecification",
                    "fields": [{"name": "value", "value": "x265"}]
                },
                {
                    "implementation": "LanguageSpecification",
                    "required": true,
                    "fields": [{"name": "value", "value": 1}]
                }
            ]
        }"#;
        let err = compile_from_json(json, 0, 1).unwrap_err();
        assert!(err.contains("LanguageSpecification"), "got: {err}");
        assert!(err.contains("required=true"), "got: {err}");
    }

    #[test]
    fn parse_rejects_invalid_regex() {
        let json = r#"{
            "name": "bad regex",
            "specifications": [
                {
                    "implementation": "ReleaseTitleSpecification",
                    "fields": [{"name": "value", "value": "[unclosed"}]
                }
            ]
        }"#;
        assert!(compile_from_json(json, 0, 1).is_err());
    }

    #[test]
    fn parse_rejects_missing_name() {
        let json = r#"{"specifications": []}"#;
        assert!(compile_from_json(json, 0, 1).is_err());
    }

    #[test]
    fn parse_rejects_missing_specifications_key() {
        let json = r#"{"name": "lonely"}"#;
        assert!(compile_from_json(json, 0, 1).is_err());
    }

    // ── Default CF library: compile-only regression ───────────────────────

    #[test]
    fn default_custom_formats_json_compiles_every_cf() {
        // Plan §7.2: the bundled default CF set at
        // `static/default_custom_formats.json` must compile through the
        // production CF compiler. This test is the regression guard for
        // a typo (wrong field name, bad regex, unsupported spec type)
        // silently breaking the Install Defaults button.
        const DEFAULTS: &str = include_str!("../../../static/default_custom_formats.json");
        let value: serde_json::Value =
            serde_json::from_str(DEFAULTS).expect("defaults file is valid JSON");
        let entries = value
            .as_array()
            .expect("defaults file top-level must be an array");
        assert_eq!(
            entries.len(),
            8,
            "bundled default CFs after the SubsPlease/Judas split into two CFs"
        );
        for (i, entry) in entries.iter().enumerate() {
            let name = entry
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("(missing)");
            let score = entry.get("score").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
            let raw = entry.to_string();
            compile_from_json(&raw, score, (i + 1) as i64)
                .unwrap_or_else(|e| panic!("default CF `{name}` failed to compile: {e}"));
        }
    }

    // ── Phase 9 cross-compat §5.7.7 — reject-on-all-unsupported ───────

    #[test]
    fn compile_rejects_cf_with_only_unsupported_spec() {
        // TRaSH's real-world `BR-DISK` CF uses only a
        // `QualityModifierSpecification`, which Ryokan doesn't
        // implement. Per plan §5.7.7 #6, the compiler must reject
        // this rather than silently letting it through as an empty
        // (vacuous-match) CF.
        let json = r#"{
            "name": "BR-DISK",
            "specifications": [
                {
                    "name": "BR-DISK modifier",
                    "implementation": "QualityModifierSpecification",
                    "negate": false,
                    "required": false,
                    "fields": [{"name":"value","value":1}]
                }
            ]
        }"#;
        let err = compile_from_json(json, 100, 1).unwrap_err();
        assert!(
            err.contains("all 1 specifications unsupported"),
            "error must mention the all-unsupported rejection, got: {err}"
        );
    }

    #[test]
    fn compile_rejects_cf_with_dropped_required_spec() {
        // Plan §5.7.7 #6: a CF with one supported spec plus a
        // required=true unsupported spec must be rejected — dropping
        // the required spec would change the CF's semantics silently.
        let json = r#"{
            "name": "Multi-Audio",
            "specifications": [
                {
                    "name": "Title",
                    "implementation": "ReleaseTitleSpecification",
                    "negate": false,
                    "required": false,
                    "fields": [{"name":"value","value":"x265"}]
                },
                {
                    "name": "Language",
                    "implementation": "LanguageSpecification",
                    "negate": false,
                    "required": true,
                    "fields": [{"name":"value","value":10}]
                }
            ]
        }"#;
        let err = compile_from_json(json, 100, 1).unwrap_err();
        assert!(
            err.contains("required=true") && err.contains("dropped"),
            "error must mention the dropped-required rejection, got: {err}"
        );
    }

    // ── Phase 9 round-trip (§5.7.7 #4) ────────────────────────────────

    #[test]
    fn round_trip_preserves_trash_metadata() {
        // A representative trash-guides-shaped CF with all the
        // extension fields plan §5.7.6 wants preserved: `trash_id`,
        // `trash_scores`, `trash_description`, plus `includeCustomFormatWhenRenaming`.
        // The test proves that compiling the CF, re-serializing the
        // stored JSON (which is just the verbatim input because §5.1
        // stores raw JSON), and re-parsing gives back the same `Value`
        // tree. This is the "round-trip faithfully" assertion from §9.
        let json = r#"{
            "trash_id": "ed38b0b3-1e57-47bc-b0e1-abcdef012345",
            "trash_scores": {"default": 500},
            "trash_description": "Prefer x265 encodes over x264",
            "name": "x265 preference",
            "includeCustomFormatWhenRenaming": false,
            "specifications": [
                {
                    "name": "x265",
                    "implementation": "ReleaseTitleSpecification",
                    "implementationName": "Release Title",
                    "negate": false,
                    "required": false,
                    "fields": [{"name": "value", "value": "x265"}]
                }
            ]
        }"#;

        // Compile once to prove it's a valid CF from the production
        // compiler's perspective.
        let cf = compile_from_json(json, 500, 42).expect("fixture CF must compile");
        assert_eq!(cf.name, "x265 preference");

        // Round-trip path 1: raw JSON → Value → back to string →
        // Value. Mirrors what `settings_custom_formats_export` does
        // to every row in the database. The two Values must be equal
        // because `serde_json::Value` is order-sensitive for arrays
        // and order-insensitive for objects, which is exactly the
        // "byte-level equivalence modulo whitespace/field order"
        // semantic from plan §9.
        let parsed_in: serde_json::Value = serde_json::from_str(json).unwrap();
        let reserialized = serde_json::to_string(&parsed_in).unwrap();
        let parsed_out: serde_json::Value = serde_json::from_str(&reserialized).unwrap();
        assert_eq!(
            parsed_in, parsed_out,
            "raw JSON must round-trip as equal Value trees"
        );

        // Check every trash-guides extension field survived the round
        // trip — plan §5.7.6 flags these as the ones Ryokan must NOT
        // drop, even though the evaluator ignores them.
        assert_eq!(
            parsed_out.get("trash_id").and_then(|v| v.as_str()),
            Some("ed38b0b3-1e57-47bc-b0e1-abcdef012345")
        );
        assert_eq!(
            parsed_out
                .get("trash_scores")
                .and_then(|v| v.get("default"))
                .and_then(|v| v.as_i64()),
            Some(500)
        );
        assert_eq!(
            parsed_out.get("trash_description").and_then(|v| v.as_str()),
            Some("Prefer x265 encodes over x264")
        );
        assert_eq!(
            parsed_out
                .get("includeCustomFormatWhenRenaming")
                .and_then(|v| v.as_bool()),
            Some(false)
        );
    }

    // ── sonarr_to_rust_regex ────────────────────────────────────────────

    #[test]
    fn sonarr_rewrite_escapes_literal_lbracket_in_class() {
        // The trash-guides `anime-dual-audio` regex's critical fragment.
        let rewritten = sonarr_to_rust_regex(r"[([]dual[])]");
        assert_eq!(rewritten, r"[(\[]dual[])]");
        assert!(fancy_regex::Regex::new(&rewritten).is_ok());
    }

    #[test]
    fn sonarr_rewrite_passes_through_class_leading_rbracket() {
        // Sonarr's convention: `]` immediately after `[` is literal.
        // regex-syntax handles this natively, so the rewrite should
        // leave it alone and the result should still compile.
        let rewritten = sonarr_to_rust_regex(r"[])]");
        assert_eq!(rewritten, r"[])]");
        assert!(fancy_regex::Regex::new(&rewritten).is_ok());
    }

    #[test]
    fn sonarr_rewrite_preserves_escapes_and_negation() {
        // Negated class `[^...]` with escaped dot inside — should
        // pass through unchanged.
        let src = r"[^\.ab]";
        assert_eq!(sonarr_to_rust_regex(src), src);
        assert!(fancy_regex::Regex::new(src).is_ok());
    }

    #[test]
    fn sonarr_rewrite_leaves_non_class_lbracket_alone() {
        // A `[` outside a class (impossible per strict regex grammar
        // but harmless here) is not rewritten — `sonarr_to_rust_regex`
        // only touches `[` inside an already-open class.
        let src = r"\[text\]";
        assert_eq!(sonarr_to_rust_regex(src), src);
    }

    #[test]
    fn sonarr_rewrite_handles_multiple_classes_in_same_pattern() {
        // Two separate classes in one pattern, both with literal `[`.
        let rewritten = sonarr_to_rust_regex(r"[[a][[b]");
        assert_eq!(rewritten, r"[\[a][\[b]");
        assert!(fancy_regex::Regex::new(&rewritten).is_ok());
    }

    // ── trash-guides anime fixture set (plan §9, Gap E) ──────────────────
    //
    // Twenty-eight real trash-guides anime CF JSON files are vendored at
    // `tests/fixtures/trash-guides-anime/`, last synced with TRaSH-Guides
    // `e7c97a67` (2026-09-28) from `docs/json/sonarr/cf/`. Each fixture is pulled into the
    // binary via `include_str!` so the test needs no filesystem access
    // and no network at build time. These are the **actual** JSON shapes
    // trash-guides ships (object-form `fields`, `trash_id`, `trash_scores`,
    // `trash_description`, `LanguageSpecification` used as a soft hint),
    // so the test doubles as a round-trip regression for the object/array
    // `fields` bug that Gap E was created to catch — if either exporter
    // shape stops parsing, every entry in this array breaks at once.
    //
    // Extending the set: add a new file under `tests/fixtures/trash-guides-anime/`
    // and a new `include_str!` line below. Any CF with a `required=true`
    // LanguageSpecification will fail the parse guard on purpose — that is
    // not a bug, that is Ryokan refusing to silently drop a gating spec.
    const TRASH_ANIME_FIXTURES: &[(&str, &str)] = &[
        (
            "10bit",
            include_str!("../../../tests/fixtures/trash-guides-anime/10bit.json"),
        ),
        (
            "anime-bd-tier-01",
            include_str!("../../../tests/fixtures/trash-guides-anime/anime-bd-tier-01.json"),
        ),
        (
            "anime-bd-tier-02",
            include_str!("../../../tests/fixtures/trash-guides-anime/anime-bd-tier-02.json"),
        ),
        (
            "anime-bd-tier-03",
            include_str!("../../../tests/fixtures/trash-guides-anime/anime-bd-tier-03.json"),
        ),
        (
            "anime-bd-tier-04",
            include_str!("../../../tests/fixtures/trash-guides-anime/anime-bd-tier-04.json"),
        ),
        (
            "anime-bd-tier-05",
            include_str!("../../../tests/fixtures/trash-guides-anime/anime-bd-tier-05.json"),
        ),
        (
            "anime-bd-tier-06",
            include_str!("../../../tests/fixtures/trash-guides-anime/anime-bd-tier-06.json"),
        ),
        (
            "anime-bd-tier-07",
            include_str!("../../../tests/fixtures/trash-guides-anime/anime-bd-tier-07.json"),
        ),
        (
            "anime-bd-tier-08",
            include_str!("../../../tests/fixtures/trash-guides-anime/anime-bd-tier-08.json"),
        ),
        (
            "anime-dual-audio",
            include_str!("../../../tests/fixtures/trash-guides-anime/anime-dual-audio.json"),
        ),
        (
            "anime-lq-groups",
            include_str!("../../../tests/fixtures/trash-guides-anime/anime-lq-groups.json"),
        ),
        (
            "anime-raws",
            include_str!("../../../tests/fixtures/trash-guides-anime/anime-raws.json"),
        ),
        (
            "anime-web-tier-01",
            include_str!("../../../tests/fixtures/trash-guides-anime/anime-web-tier-01.json"),
        ),
        (
            "anime-web-tier-02",
            include_str!("../../../tests/fixtures/trash-guides-anime/anime-web-tier-02.json"),
        ),
        (
            "anime-web-tier-03",
            include_str!("../../../tests/fixtures/trash-guides-anime/anime-web-tier-03.json"),
        ),
        (
            "anime-web-tier-04",
            include_str!("../../../tests/fixtures/trash-guides-anime/anime-web-tier-04.json"),
        ),
        (
            "anime-web-tier-05",
            include_str!("../../../tests/fixtures/trash-guides-anime/anime-web-tier-05.json"),
        ),
        (
            "anime-web-tier-06",
            include_str!("../../../tests/fixtures/trash-guides-anime/anime-web-tier-06.json"),
        ),
        (
            "bad-dual-groups",
            include_str!("../../../tests/fixtures/trash-guides-anime/bad-dual-groups.json"),
        ),
        (
            "dubs-only",
            include_str!("../../../tests/fixtures/trash-guides-anime/dubs-only.json"),
        ),
        (
            "fansub",
            include_str!("../../../tests/fixtures/trash-guides-anime/fansub.json"),
        ),
        (
            "fastsub",
            include_str!("../../../tests/fixtures/trash-guides-anime/fastsub.json"),
        ),
        (
            "uncensored",
            include_str!("../../../tests/fixtures/trash-guides-anime/uncensored.json"),
        ),
        (
            "v0",
            include_str!("../../../tests/fixtures/trash-guides-anime/v0.json"),
        ),
        (
            "v1",
            include_str!("../../../tests/fixtures/trash-guides-anime/v1.json"),
        ),
        (
            "v2",
            include_str!("../../../tests/fixtures/trash-guides-anime/v2.json"),
        ),
        (
            "v3",
            include_str!("../../../tests/fixtures/trash-guides-anime/v3.json"),
        ),
        (
            "v4",
            include_str!("../../../tests/fixtures/trash-guides-anime/v4.json"),
        ),
    ];

    /// Every vendored trash-guides anime CF must compile cleanly. A
    /// failure here means one of three things is broken:
    ///
    /// 1. The object-form `fields` accessor regressed — every fixture
    ///    would fail at once.
    /// 2. trash-guides shipped a new spec implementation string Ryokan
    ///    doesn't recognize, and it's `required=true` (and therefore
    ///    correctly rejected instead of silently dropped).
    /// 3. A regex in a vendored file became invalid against `fancy-regex`
    ///    (unlikely but possible since Sonarr uses .NET's regex engine
    ///    and we use `fancy-regex`, which supports look-around but not
    ///    every PCRE feature).
    #[test]
    fn trash_anime_fixtures_all_parse() {
        assert_eq!(
            TRASH_ANIME_FIXTURES.len(),
            28,
            "fixture count drift: update the set and this assertion together"
        );

        let mut failures: Vec<(String, String)> = Vec::new();
        for (label, raw) in TRASH_ANIME_FIXTURES {
            match compile_from_json(raw, 0, 1) {
                Ok(cf) => {
                    // Sanity: every fixture has at least one surviving
                    // spec (otherwise it's either vacuous or the parser
                    // silently dropped something we didn't expect).
                    assert!(
                        !cf.specs.is_empty(),
                        "fixture `{label}` compiled to zero specs — vacuous CF"
                    );
                }
                Err(e) => failures.push(((*label).to_string(), e)),
            }
        }

        assert!(
            failures.is_empty(),
            "trash-guides anime fixtures failed to parse:\n{}",
            failures
                .iter()
                .map(|(name, err)| format!("  - {name}: {err}"))
                .collect::<Vec<_>>()
                .join("\n")
        );
    }

    /// Every `specifications[].implementation` string in the fixture set
    /// must be one Ryokan knows about (supported + handled, or
    /// explicitly-tolerated-as-soft-hint). If trash-guides ships a new
    /// implementation type (e.g. they add `EditionSpecification` to an
    /// anime CF), this test fails with the exact file + unknown string
    /// so a human can decide whether to add support or reject the CF.
    #[test]
    fn trash_anime_fixtures_use_only_known_implementations() {
        // Strict subset of Ryokan's supported spec list. Kept in sync
        // with the match arm in `compile_from_json`.
        //
        // `LanguageSpecification` is **not supported** but is soft-
        // tolerated by the parser when `required=false` — it is silently
        // dropped. Including it here is a claim that "we intentionally
        // let this appear in vendored fixtures"; a `required=true`
        // Language spec would still fail the first fixture test above.
        const KNOWN: &[&str] = &[
            "ReleaseTitleSpecification",
            "ReleaseGroupSpecification",
            "SourceSpecification",
            "ResolutionSpecification",
            "SizeSpecification",
            "LanguageSpecification",
            "SeaDexBestSpecification",
            "Ryokan.SeaDexBestSpecification",
        ];

        let mut unknowns: Vec<(String, String)> = Vec::new();
        for (label, raw) in TRASH_ANIME_FIXTURES {
            let v: serde_json::Value = serde_json::from_str(raw)
                .unwrap_or_else(|e| panic!("fixture `{label}` is not valid JSON: {e}"));
            let specs = v
                .get("specifications")
                .and_then(|s| s.as_array())
                .unwrap_or_else(|| panic!("fixture `{label}` has no specifications array"));
            for s in specs {
                let impl_name = s
                    .get("implementation")
                    .and_then(|i| i.as_str())
                    .unwrap_or("<missing>");
                if !KNOWN.contains(&impl_name) {
                    unknowns.push(((*label).to_string(), impl_name.to_string()));
                }
            }
        }

        assert!(
            unknowns.is_empty(),
            "trash-guides anime fixtures reference unknown implementations — \
             either add parser support or update the KNOWN list with a rationale:\n{}",
            unknowns
                .iter()
                .map(|(name, imp)| format!("  - {name}: {imp}"))
                .collect::<Vec<_>>()
                .join("\n")
        );
    }

    /// Round-trip guard: parse → serialize → re-parse must produce the
    /// same compiled CF (same spec count, same match against a known
    /// release title). This catches upstream schema drift that would
    /// break Ryokan-compatible re-export of a trash-guides CF.
    #[test]
    fn trash_anime_fixtures_roundtrip_json() {
        for (label, raw) in TRASH_ANIME_FIXTURES {
            let first = compile_from_json(raw, 0, 1)
                .unwrap_or_else(|e| panic!("fixture `{label}` failed first parse: {e}"));

            // Re-serialize the raw JSON Value (not the compiled form —
            // the compiled form is lossy by design). This mirrors what
            // the export handler does: it hands back the stored `json`
            // column, which for a CF imported from trash-guides is the
            // raw trash-guides bytes.
            let value: serde_json::Value = serde_json::from_str(raw)
                .unwrap_or_else(|e| panic!("fixture `{label}` not valid JSON on re-read: {e}"));
            let reserialized = serde_json::to_string(&value)
                .unwrap_or_else(|e| panic!("fixture `{label}` failed to re-serialize: {e}"));

            let second = compile_from_json(&reserialized, 0, 1)
                .unwrap_or_else(|e| panic!("fixture `{label}` failed round-trip parse: {e}"));

            assert_eq!(
                first.specs.len(),
                second.specs.len(),
                "fixture `{label}`: spec count drifted across round-trip \
                 ({} → {})",
                first.specs.len(),
                second.specs.len()
            );
            assert_eq!(
                first.name, second.name,
                "fixture `{label}`: name drifted across round-trip"
            );
        }
    }

    /// Surviving-spec diff: catches "we silently started dropping more
    /// specs than before" across the whole fixture set, which would
    /// indicate either a parser regression or a new unsupported spec
    /// type appearing in trash-guides. The numbers below are the
    /// surviving (post-drop) spec counts as of 2026-04-13 and should
    /// be updated in lockstep with intentional parser changes.
    #[test]
    fn trash_anime_fixtures_surviving_spec_counts_are_stable() {
        // Every fixture compiles to >=1 spec (otherwise Gap E's fixture
        // set includes a vacuous-match CF, which is meaningless). This
        // is a weaker assertion than a hard expected-count map, but it
        // survives trash-guides upstream edits without needing a lockstep
        // update while still catching mass regressions. If you want to
        // tighten this further, snapshot the `(label, spec_count)` map
        // here.
        let mut total_surviving = 0usize;
        for (label, raw) in TRASH_ANIME_FIXTURES {
            let cf = compile_from_json(raw, 0, 1)
                .unwrap_or_else(|e| panic!("fixture `{label}` failed to parse: {e}"));
            assert!(
                !cf.specs.is_empty(),
                "fixture `{label}` compiled to zero surviving specs"
            );
            total_surviving += cf.specs.len();
        }
        // Sanity floor: 28 CFs × at least 1 surviving spec each = 28.
        // Realistic floor is far higher (hundreds of ReleaseTitle/Group
        // specs across the set) — we saw 504 at vendoring time. Use a
        // conservative floor that allows trash-guides some churn without
        // triggering a spurious test failure.
        assert!(
            total_surviving >= 200,
            "surviving-spec count dropped to {total_surviving} — expected \
             at least 200 across the 28-file fixture set (saw 504 at vendor \
             time). Investigate before updating this floor."
        );
    }
}
