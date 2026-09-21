//! The wording a front-end puts on a tool row that auto mode let through, and
//! the reader for the transcript sidecar that records which calls those were.
//!
//! Display-only in both directions: the notes are never written to a
//! transcript and the sidecar is never read by a model. Each front-end picks
//! its own presentation: a terminal prints these strings, a localized desktop
//! client maps the same allow source onto its own label.

/// The note a front-end puts on a tool row that auto mode let through without
/// a permission dialog, when the gate did not say which part of it decided. A
/// localized front-end says the same thing through its own string.
///
/// Display-only: it is never written into a transcript entry. The engine
/// records the tool_use ids and their source in a sidecar the model never
/// reads, and each front-end renders the wording itself.
pub const AUTO_MODE_ALLOWED_NOTE: &str = "Auto allowed by rebon's auto mode";

/// A fresh classifier verdict cleared the call.
pub const AUTO_MODE_ALLOWED_BY_CLASSIFIER_NOTE: &str = "Allowed by auto mode classifier";

/// A classifier verdict remembered from an identical earlier call.
pub const AUTO_MODE_ALLOWED_BY_CACHE_NOTE: &str = "Allowed by a cached auto mode verdict";

/// The user's own approval, replayed by the gate — not a machine verdict, so
/// the row must not credit auto mode for it.
pub const AUTO_MODE_ALLOWED_BY_USER_NOTE: &str = "Allowed by your earlier approval";

/// The note for one allow source.
pub fn auto_mode_allowed_note(source: rebon_types::AutoModeAllowSource) -> &'static str {
    use rebon_types::AutoModeAllowSource as Source;
    match source {
        Source::Classifier => AUTO_MODE_ALLOWED_BY_CLASSIFIER_NOTE,
        Source::CachedVerdict => AUTO_MODE_ALLOWED_BY_CACHE_NOTE,
        Source::UserExemption => AUTO_MODE_ALLOWED_BY_USER_NOTE,
        Source::Unspecified => AUTO_MODE_ALLOWED_NOTE,
    }
}

/// Read a transcript entry's display-only `autoModeAllowed` sidecar.
///
/// Two shapes, one reader: `{"id": …, "source": …}` objects since the note
/// began naming its source, and the bare id strings older transcripts hold —
/// those come back `Unspecified` and render the unattributed note rather than
/// guessing at an attribution the transcript never recorded.
pub fn parse_auto_mode_allowed_sidecar(
    sidecar: &serde_json::Value,
) -> Vec<(&str, rebon_types::AutoModeAllowSource)> {
    let Some(entries) = sidecar.as_array() else {
        return Vec::new();
    };
    entries
        .iter()
        .filter_map(|entry| match entry {
            serde_json::Value::String(id) => {
                Some((id.as_str(), rebon_types::AutoModeAllowSource::Unspecified))
            }
            serde_json::Value::Object(map) => {
                let id = map.get("id")?.as_str()?;
                let source = map
                    .get("source")
                    .and_then(|value| serde_json::from_value(value.clone()).ok())
                    .unwrap_or_default();
                Some((id, source))
            }
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod auto_mode_allowed_tests {
    use super::*;
    use rebon_types::AutoModeAllowSource as Source;

    #[test]
    fn every_source_has_its_own_note_and_unspecified_keeps_the_old_wording() {
        assert_eq!(
            auto_mode_allowed_note(Source::Classifier),
            "Allowed by auto mode classifier"
        );
        assert_eq!(
            auto_mode_allowed_note(Source::UserExemption),
            "Allowed by your earlier approval"
        );
        assert_eq!(
            auto_mode_allowed_note(Source::Unspecified),
            AUTO_MODE_ALLOWED_NOTE
        );
        let notes = [
            Source::Classifier,
            Source::CachedVerdict,
            Source::UserExemption,
            Source::Unspecified,
        ]
        .map(auto_mode_allowed_note);
        let unique: std::collections::BTreeSet<_> = notes.iter().collect();
        assert_eq!(unique.len(), notes.len(), "{notes:?}");
    }

    #[test]
    fn the_sidecar_reader_takes_objects_and_legacy_bare_ids() {
        let sidecar = serde_json::json!([
            { "id": "toolu_a", "source": "classifier" },
            { "id": "toolu_b", "source": "userExemption" },
            { "id": "toolu_c" },
            "toolu_legacy",
            { "id": "toolu_d", "source": "somethingNewer" },
            42
        ]);
        assert_eq!(
            parse_auto_mode_allowed_sidecar(&sidecar),
            vec![
                ("toolu_a", Source::Classifier),
                ("toolu_b", Source::UserExemption),
                ("toolu_c", Source::Unspecified),
                ("toolu_legacy", Source::Unspecified),
                ("toolu_d", Source::Unspecified),
            ]
        );
        assert!(parse_auto_mode_allowed_sidecar(&serde_json::json!({})).is_empty());
    }
}
