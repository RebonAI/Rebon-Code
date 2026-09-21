//! The checked-in catalogues have to match a fresh run of the generator.
//!
//! Without this the shared source is a suggestion: someone edits
//! `assets/i18n/desktop/en.json` directly, the source still says the old text, and
//! the next regeneration silently reverts them.

use std::path::{Path, PathBuf};

use rebon_i18n_gen::{artefacts, check, Locale, Source, REGENERATE_COMMAND};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("the crate lives two levels under the repository root")
        .to_path_buf()
}

fn source() -> Source {
    Source::read(&repo_root()).unwrap_or_else(|error| panic!("{error}"))
}

#[test]
fn generated_catalogues_are_current() {
    let root = repo_root();
    for (relative, expected) in artefacts(&source()).expect("the source passes its own checks") {
        let path = root.join(&relative);
        let actual = std::fs::read_to_string(&path).unwrap_or_else(|error| {
            panic!("{relative} is missing ({error}); run `{REGENERATE_COMMAND}`")
        });
        // Compare on content, not on the checkout's line endings — see the
        // same note in `rebon-schema-gen`.
        let normalize = |text: &str| text.replace("\r\n", "\n");
        let (actual, expected) = (normalize(&actual), normalize(&expected));
        if actual != expected {
            panic!(
                "{relative} is out of date — run `{REGENERATE_COMMAND}`\n{}",
                first_difference(&actual, &expected)
            );
        }
    }
}

fn first_difference(actual: &str, expected: &str) -> String {
    let mut actual_lines = actual.lines();
    let mut expected_lines = expected.lines();
    let mut line = 0;
    loop {
        line += 1;
        match (actual_lines.next(), expected_lines.next()) {
            (None, None) => return "the files differ only in trailing newlines".to_string(),
            (a, e) if a == e => continue,
            (a, e) => {
                return format!(
                    "first difference at line {line}\n  on disk:    {}\n  generated:  {}",
                    a.unwrap_or("<end of file>"),
                    e.unwrap_or("<end of file>"),
                )
            }
        }
    }
}

/// Every surface has every one of its keys in every language it ships.
///
/// This is the property the shared source exists for: before it, a key could
/// be in the English catalogue and missing from the Chinese one with nothing
/// to say so, and the button rendered blank in Chinese alone.
#[test]
fn every_surface_has_every_key_in_every_language_it_ships() {
    check(&source()).unwrap_or_else(|error| panic!("{error}"));
}

/// A locale file is a flat object of strings, and the two full ones declare
/// the same keys in the same order.
///
/// Order is load-bearing downstream: `flutter gen-l10n` emits its getters in
/// ARB order, so a reordered source rewrites three committed Dart files.
#[test]
fn full_locales_agree_on_key_order() {
    let source = source();
    let english = source.keys(Locale::En);
    let chinese = source.keys(Locale::ZhCn);
    assert_eq!(
        english, chinese,
        "i18n/en.json and i18n/zh-CN.json must declare the same keys in the same order"
    );
    let japanese = source.keys(Locale::Ja);
    let app_keys: Vec<&String> = english
        .into_iter()
        .filter(|key| key.starts_with("app."))
        .collect();
    assert_eq!(
        japanese, app_keys,
        "Japanese is desktop-only: i18n/ja.json holds the app keys, in order, and no others"
    );
}

#[test]
fn generation_is_reproducible() {
    let source = source();
    assert_eq!(
        artefacts(&source).expect("first run"),
        artefacts(&source).expect("second run"),
        "two runs must produce the same bytes"
    );
}
