//! Crew's wording (docs/crew/zgx-thin-port.md, "zgx Crew mode"): upstream's
//! catalog (`ui/locales`) with Crew's messages over it (`ui/crew/locales`: the
//! same keys, Crew's words), in a Crew build.
//!
//! zgx keeps one catalog a host, from one folder (`ScriptHost::i18n`), so the
//! two are merged into the app's data folder as a window's host is made, and
//! that folder is loaded. Upstream's files are never edited, so an upstream
//! sync never conflicts on Crew's words. What still names Zeron and stays so
//! is listed, with why, in `ui/crew/locales/kept.json`; the tests hold the
//! three to each other, so a message upstream adds or renames is noticed.

use std::path::{Path, PathBuf};

use serde_json::Value as Json;

/// Crew's messages, beside upstream's catalog: `<ui>/crew/locales`.
pub fn overlay_dir(locales: &Path) -> PathBuf {
    locales.parent().map(|ui| ui.join("crew").join("locales")).unwrap_or_default()
}

/// The folder a Crew build's catalog loads from: upstream's `locales` with
/// Crew's messages over it, merged into the app's data folder; `locales`
/// itself in Zeron's build, or when the merge can't be made (said on stderr).
pub fn catalog_dir(locales: &Path) -> PathBuf {
    if !super::MODE {
        return locales.to_path_buf();
    }
    let Some(out) = zgx_script::app_data_dir(crate::APP_ID).map(|dir| dir.join("crew-locales")) else {
        return locales.to_path_buf();
    };
    match merge(locales, &overlay_dir(locales), &out) {
        Ok(()) => out,
        Err(error) => {
            eprintln!("zeron-zgx: Crew's wording isn't applied: {error}");
            locales.to_path_buf()
        }
    }
}

/// Every `<locale>.json` of `locales`, with the same-named file of `overlay`
/// (where there is one) over it, written to `out`, which then holds those
/// alone. A file is replaced whole (written beside it, then renamed), so a
/// window reading the folder never sees half of one.
pub fn merge(locales: &Path, overlay: &Path, out: &Path) -> Result<(), String> {
    let read = |path: &Path| -> Result<Json, String> {
        let text = std::fs::read_to_string(path).map_err(|error| format!("{}: {error}", path.display()))?;
        serde_json::from_str(&text).map_err(|error| format!("{}: {error}", path.display()))
    };
    std::fs::create_dir_all(out).map_err(|error| format!("{}: {error}", out.display()))?;
    let mut written = Vec::new();
    let entries = std::fs::read_dir(locales).map_err(|error| format!("{}: {error}", locales.display()))?;
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().map(|name| name.to_owned()) else { continue };
        if path.extension().is_none_or(|extension| extension != "json") || !path.is_file() {
            continue;
        }
        let mut catalog = read(&path)?;
        let over = overlay.join(&name);
        if over.is_file() {
            overlay_onto(&mut catalog, read(&over)?);
        }
        let text = serde_json::to_string_pretty(&catalog).map_err(|error| error.to_string())?;
        let target = out.join(&name);
        let partial = out.join(format!(".{}.partial", name.to_string_lossy()));
        std::fs::write(&partial, text).map_err(|error| format!("{}: {error}", partial.display()))?;
        std::fs::rename(&partial, &target).map_err(|error| format!("{}: {error}", target.display()))?;
        written.push(name);
    }
    // (A locale upstream dropped goes from the merged folder too.)
    for entry in std::fs::read_dir(out).into_iter().flatten().flatten() {
        let name = entry.file_name();
        if entry.path().extension().is_some_and(|extension| extension == "json") && !written.contains(&name) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
    Ok(())
}

/// `over`'s messages over `catalog`'s, key by key, nested as they are.
fn overlay_onto(catalog: &mut Json, over: Json) {
    match (catalog, over) {
        (Json::Object(catalog), Json::Object(over)) => {
            for (key, value) in over {
                match catalog.get_mut(&key) {
                    Some(slot) if slot.is_object() && value.is_object() => overlay_onto(slot, value),
                    _ => {
                        catalog.insert(key, value);
                    }
                }
            }
        }
        (slot, value) => *slot = value,
    }
}

#[cfg(test)]
#[path = "crew_wording_tests.rs"]
mod tests;
