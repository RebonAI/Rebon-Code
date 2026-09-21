//! The mobile catalogue, in Flutter's ARB format.
//!
//! `flutter gen-l10n` reads these and writes `assets/i18n/mobile/generated/`. It
//! emits its getters in ARB order, which is why the source keeps the curated
//! order instead of sorting.
//!
//! A message with placeholders is followed by its `@key` metadata block, the
//! notation gen-l10n takes the parameter types from. Only the template ARB
//! (`app_en.arb`, per the mobile client's `l10n.yaml`) is read for those,
//! but both files
//! carry them: the two catalogues describing the same message differently is
//! the drift this crate exists to prevent.

use crate::{json_string, Locale, Source, Surface};

/// ARB is JSON with no comment syntax, so the "do not edit" notice lives in
/// the mobile client's own README and in the staleness test rather than at
/// the top of the file.
pub fn render(source: &Source, surface: &Surface, locale: Locale, tag: &str) -> String {
    let mut out = format!("{{\n  \"@@locale\": {},\n", json_string(tag));
    let entries = source.entries(surface, locale);
    for (index, (key, text)) in entries.iter().enumerate() {
        let placeholders = source.placeholders_for(surface, key);
        let last = index + 1 == entries.len() && placeholders.is_none();
        let comma = if last { "" } else { "," };
        out.push_str(&format!(
            "  {}: {}{comma}\n",
            json_string(key),
            json_string(text)
        ));
        if let Some(placeholders) = placeholders {
            let body = placeholders
                .iter()
                .map(|placeholder| {
                    format!(
                        "{}: {{\"type\": {}}}",
                        json_string(&placeholder.name),
                        json_string(&placeholder.kind)
                    )
                })
                .collect::<Vec<_>>()
                .join(", ");
            let comma = if index + 1 == entries.len() { "" } else { "," };
            out.push_str(&format!(
                "  {}: {{\"placeholders\": {{{body}}}}}{comma}\n",
                json_string(&format!("@{key}"))
            ));
        }
    }
    out.push_str("}\n");
    out
}
