//! One copy source, three message catalogues.
//!
//! `i18n/{en,ja,zh-CN}.json` holds every user-visible string the desktop app,
//! the browser page and the mobile app show, keyed by a surface prefix
//! (`app.` / `web.` / `mobile.`). This crate turns that source into the shape
//! each surface loads:
//!
//! | surface | output | locales |
//! | --- | --- | --- |
//! | desktop | `assets/i18n/desktop/{en,ja,zh-CN}.json` | en, ja, zh-CN |
//! | browser | `assets/i18n/web/i18n.generated.ts` | en, zh |
//! | mobile | `assets/i18n/mobile/app_{en,zh}.arb` | en, zh |
//!
//! The prefixes exist because the three surfaces disagree: `newSession` is
//! "New task" on the page and "New" on the phone, and folding those into one
//! key would silently rewrite one of them. What the shared source buys is a
//! shared *shape* — one format, one place to add a locale, one test that every
//! surface has every string in every language it ships.
//!
//! Japanese is desktop-only, because that is the only surface with a Japanese
//! option today. `i18n/ja.json` therefore holds the `app.` keys and nothing
//! else, and [`check`] holds each surface to the locales it actually ships
//! rather than to a union nobody translated.
//!
//! Regenerate with `cargo run -p rebon-i18n-gen`.

mod arb;
mod ordered;
mod typescript;

use std::collections::BTreeMap;

pub use ordered::OrderedStrings;

/// The command that rewrites every catalogue.
pub const REGENERATE_COMMAND: &str = "cargo run -p rebon-i18n-gen";

/// A locale as the source names it. Each surface maps these onto its own tags.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Locale {
    En,
    Ja,
    ZhCn,
}

impl Locale {
    pub const ALL: [Self; 3] = [Self::En, Self::Ja, Self::ZhCn];

    /// The source file's stem, which is also the desktop catalogue's name.
    pub const fn source_stem(self) -> &'static str {
        match self {
            Self::En => "en",
            Self::Ja => "ja",
            Self::ZhCn => "zh-CN",
        }
    }
}

/// A surface's prefix and the locales it ships.
struct Surface {
    prefix: &'static str,
    locales: &'static [Locale],
}

const APP: Surface = Surface {
    prefix: "app.",
    locales: &[Locale::En, Locale::Ja, Locale::ZhCn],
};
const WEB: Surface = Surface {
    prefix: "web.",
    locales: &[Locale::En, Locale::ZhCn],
};
const MOBILE: Surface = Surface {
    prefix: "mobile.",
    locales: &[Locale::En, Locale::ZhCn],
};

/// One interpolated value in a message, with the type the target language
/// gives it. Carried in `i18n/placeholders.json` rather than inferred from the
/// text, because the browser dictionary needs a typed signature and the mobile
/// ARB needs the same fact in its own notation.
#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
pub struct Placeholder {
    pub name: String,
    #[serde(rename = "type")]
    pub kind: String,
}

/// The whole source, as read off disk.
pub struct Source {
    /// Key → text, per locale, in source order.
    catalogs: BTreeMap<&'static str, OrderedStrings>,
    placeholders: BTreeMap<String, Vec<Placeholder>>,
}

impl Source {
    /// Read `i18n/` under `root`.
    pub fn read(root: &std::path::Path) -> Result<Self, String> {
        let mut catalogs = BTreeMap::new();
        for locale in Locale::ALL {
            let path = root
                .join("i18n")
                .join(format!("{}.json", locale.source_stem()));
            let text = std::fs::read_to_string(&path)
                .map_err(|error| format!("read {}: {error}", path.display()))?;
            let parsed: OrderedStrings = serde_json::from_str(&text)
                .map_err(|error| format!("parse {}: {error}", path.display()))?;
            catalogs.insert(locale.source_stem(), parsed);
        }
        let path = root.join("i18n/placeholders.json");
        let text = std::fs::read_to_string(&path)
            .map_err(|error| format!("read {}: {error}", path.display()))?;
        let placeholders = serde_json::from_str(&text)
            .map_err(|error| format!("parse {}: {error}", path.display()))?;
        Ok(Self {
            catalogs,
            placeholders,
        })
    }

    /// Every key one locale declares, in source order.
    pub fn keys(&self, locale: Locale) -> Vec<&String> {
        self.catalog(locale).keys().collect()
    }

    fn catalog(&self, locale: Locale) -> &OrderedStrings {
        self.catalogs
            .get(locale.source_stem())
            .expect("every locale was read in `read`")
    }

    /// The `(key without prefix, text)` pairs one surface takes from one
    /// locale, in source order.
    fn entries(&self, surface: &Surface, locale: Locale) -> Vec<(&str, &str)> {
        self.catalog(locale)
            .iter()
            .filter_map(|(key, text)| {
                key.strip_prefix(surface.prefix)
                    .map(|stripped| (stripped, text.as_str()))
            })
            .collect()
    }

    fn placeholders_for(&self, surface: &Surface, key: &str) -> Option<&[Placeholder]> {
        self.placeholders
            .get(&format!("{}{key}", surface.prefix))
            .map(Vec::as_slice)
    }
}

/// Everything the source has to satisfy before it is worth generating from.
///
/// The one that matters is the last: a surface must have every one of its keys
/// in every locale it ships. Without it a missing translation is a key that
/// silently disappears from one language's catalogue, which is how a button
/// ends up blank in Chinese and nowhere else.
pub fn check(source: &Source) -> Result<(), String> {
    let english = source.catalog(Locale::En);
    if english.is_empty() {
        return Err("i18n/en.json is empty".to_string());
    }
    for (key, text) in english.iter() {
        if !(key.starts_with(APP.prefix)
            || key.starts_with(WEB.prefix)
            || key.starts_with(MOBILE.prefix))
        {
            return Err(format!("{key} has no surface prefix"));
        }
        if text.trim().is_empty() {
            return Err(format!("{key} is empty in English"));
        }
    }
    for surface in [&APP, &WEB, &MOBILE] {
        let expected: Vec<&str> = source
            .entries(surface, Locale::En)
            .into_iter()
            .map(|(key, _)| key)
            .collect();
        if expected.is_empty() {
            return Err(format!("no keys carry the prefix {}", surface.prefix));
        }
        for locale in surface.locales {
            let actual: Vec<&str> = source
                .entries(surface, *locale)
                .into_iter()
                .map(|(key, _)| key)
                .collect();
            if actual != expected {
                let missing: Vec<&&str> = expected.iter().filter(|k| !actual.contains(k)).collect();
                let extra: Vec<&&str> = actual.iter().filter(|k| !expected.contains(k)).collect();
                return Err(format!(
                    "{} does not match English in {}: missing {missing:?}, unexpected {extra:?}",
                    surface.prefix,
                    locale.source_stem(),
                ));
            }
        }
    }
    Ok(())
}

/// Every generated file, keyed by its path relative to the repository root.
pub fn artefacts(source: &Source) -> Result<Vec<(String, String)>, String> {
    check(source)?;
    let mut out = Vec::new();
    for locale in APP.locales {
        out.push((
            format!("assets/i18n/desktop/{}.json", locale.source_stem()),
            json_catalog(&source.entries(&APP, *locale)),
        ));
    }
    out.push((
        "assets/i18n/web/i18n.generated.ts".to_string(),
        typescript::render(source, &WEB),
    ));
    for (locale, tag) in [(Locale::En, "en"), (Locale::ZhCn, "zh")] {
        out.push((
            format!("assets/i18n/mobile/app_{tag}.arb"),
            arb::render(source, &MOBILE, locale, tag),
        ));
    }
    Ok(out)
}

/// The desktop catalogue: a flat JSON object, two-space indent, source order.
fn json_catalog(entries: &[(&str, &str)]) -> String {
    let mut out = String::from("{\n");
    for (index, (key, text)) in entries.iter().enumerate() {
        let comma = if index + 1 == entries.len() { "" } else { "," };
        out.push_str(&format!(
            "  {}: {}{comma}\n",
            json_string(key),
            json_string(text)
        ));
    }
    out.push_str("}\n");
    out
}

/// A JSON string literal with non-ASCII left as itself, matching the
/// catalogues these files replace — an escaped `\u5close` catalogue is
/// unreadable to the person translating it.
pub(crate) fn json_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}
