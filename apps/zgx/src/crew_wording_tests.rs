//! Crew's wording (src/crew_wording.rs): the merge, and the three files held
//! to each other: every message upstream's catalog has that names Zeron is
//! Crew's (ui/crew/locales/<locale>.json) or kept, with why
//! (ui/crew/locales/kept.json), and Crew's are still upstream's messages,
//! naming the same arguments.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use super::{merge, overlay_dir};
use crate::i18n_tests::{arguments, catalog};

const LOCALES: &[&str] = &["en.json", "zh-Hans.json"];

fn ui() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("ui")
}

/// A JSON file's messages by their dotted keys.
fn flat(path: &std::path::Path) -> BTreeMap<String, String> {
    fn walk(prefix: &str, value: &serde_json::Value, out: &mut BTreeMap<String, String>) {
        match value {
            serde_json::Value::Object(map) => {
                for (key, value) in map {
                    let key = if prefix.is_empty() { key.clone() } else { format!("{prefix}.{key}") };
                    walk(&key, value, out);
                }
            }
            serde_json::Value::String(text) => {
                out.insert(prefix.to_owned(), text.clone());
            }
            _ => {}
        }
    }
    let text = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let json: serde_json::Value = serde_json::from_str(&text).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let mut out = BTreeMap::new();
    walk("", &json, &mut out);
    out
}

fn overlay(name: &str) -> BTreeMap<String, String> {
    flat(&overlay_dir(&ui().join("locales")).join(name))
}

fn kept() -> BTreeMap<String, String> {
    flat(&overlay_dir(&ui().join("locales")).join("kept.json"))
}

/// Every message of upstream's that names Zeron is Crew's, or kept with a
/// reason: one upstream adds is noticed here.
#[test]
fn every_message_naming_zeron_is_crew_s_or_kept() {
    let kept = kept();
    assert!(kept.values().all(|why| !why.trim().is_empty()), "every kept message says why");
    for name in LOCALES {
        let crew = overlay(name);
        let upstream = catalog(name);
        let missing: Vec<&String> = upstream
            .iter()
            .filter(|(key, text)| text.contains("Zeron") && !crew.contains_key(*key) && !kept.contains_key(*key))
            .map(|(key, _)| key)
            .collect();
        assert!(missing.is_empty(), "{name}: messages naming Zeron with no Crew words and not kept: {missing:#?}");
        // Crew's say Crew. (A file format's name, `.zeron-plugin`, is no
        // product name.)
        for (key, text) in &crew {
            assert!(!text.contains("Zeron"), "{name} {key}: {text}");
        }
    }
}

/// Crew's messages are still upstream's: the same keys, naming the same
/// arguments. One upstream renames or drops is noticed here; a kept one too.
#[test]
fn crew_s_messages_are_upstream_s() {
    let english = catalog("en.json");
    for key in kept().keys() {
        assert!(english.contains_key(key), "kept.json names {key}, which upstream no longer has");
    }
    for name in LOCALES {
        let upstream = catalog(name);
        let crew = overlay(name);
        assert!(!crew.is_empty(), "{name}");
        for (key, text) in &crew {
            let Some(theirs) = upstream.get(key) else {
                panic!("{name}: Crew's {key} isn't upstream's any more");
            };
            let ours: BTreeSet<String> = arguments(text).unwrap_or_else(|e| panic!("{name} {key}: {e}"));
            assert_eq!(ours, arguments(theirs).unwrap(), "{name} {key}: the same arguments as upstream's");
        }
    }
}

/// The merge: each locale of upstream's with Crew's over it, nested keys
/// replaced one by one, the rest as it was; a locale upstream drops goes.
#[test]
fn the_merge_puts_crew_s_words_over_upstream_s() {
    let dir = tempfile::tempdir().unwrap();
    let (locales, over, out) = (dir.path().join("locales"), dir.path().join("crew"), dir.path().join("out"));
    std::fs::create_dir_all(&locales).unwrap();
    std::fs::create_dir_all(&over).unwrap();
    std::fs::create_dir_all(&out).unwrap();
    std::fs::write(locales.join("en.json"), r#"{"a":{"b":"Zeron is up","c":"stays"},"d":"also stays"}"#).unwrap();
    std::fs::write(locales.join("fr.json"), r#"{"a":{"b":"Zeron est là"}}"#).unwrap();
    std::fs::write(over.join("en.json"), r#"{"a":{"b":"Crew is up"}}"#).unwrap();
    std::fs::write(out.join("gone.json"), "{}").unwrap();
    merge(&locales, &over, &out).unwrap();
    let en = flat(&out.join("en.json"));
    assert_eq!(en["a.b"], "Crew is up");
    assert_eq!(en["a.c"], "stays");
    assert_eq!(en["d"], "also stays");
    assert_eq!(flat(&out.join("fr.json"))["a.b"], "Zeron est là", "no Crew words for it: upstream's");
    assert!(!out.join("gone.json").exists());

    // The real catalogs merge, and show Crew's words.
    let merged = tempfile::tempdir().unwrap();
    let ui_locales = ui().join("locales");
    merge(&ui_locales, &overlay_dir(&ui_locales), merged.path()).unwrap();
    let en = flat(&merged.path().join("en.json"));
    assert_eq!(en["settings.about.state.up_to_date"], "Crew is up to date");
    assert_eq!(en["shell.palette.default_interface"], "Use Zeron v3", "kept");
    assert_eq!(en.len(), catalog("en.json").len(), "no message added or lost");
}
