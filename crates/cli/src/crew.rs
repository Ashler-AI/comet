//! Crew's test flavour of Zeron's paths and ports (docs/crew/zgx-thin-port.md).
//!
//! This build runs Crew on upstream Zeron beside installs it must never touch:
//!
//! | install                 | data dir                       | IPC port     |
//! |-------------------------|--------------------------------|--------------|
//! | Crew                    | `~/.crew`                      | 27958        |
//! | Crew Staging            | `~/.crew-staging`              | 27957        |
//! | Crew Canary (0.2.x)     | `~/.crew-upstream-canary`      | 27954        |
//! | legacy native profiles  | `~/.comet-native*`             | 27655        |
//! | Zeron, Zeron staging    | `~/.zeron`, `~/.zeron-staging` | 27654, 27871 |
//!
//! Its own folders are `.crew-zgx-*`: `~/.crew-zgx-dev` (port 27960) for a
//! development build, `~/.crew-zgx-canary` (27959) for the packaged canary.
//! `CREW_FLAVOR=zgx-canary` at build time selects the canary.
//!
//! A data dir is refused when any folder on its path is named `.crew…`,
//! `.comet…` or `.zeron…` (other than `.crew-zgx-…`), whatever HOME or the
//! environment says: agent shells on a Crew machine inherit that Crew's
//! `ZERON_DATA_DIR` and `ZERON_IPC_PORT`, and an engine of this build opening
//! another install's store would migrate it in place.

use std::path::{Component, Path, PathBuf};

/// The build's flavour: `zgx-canary` or `zgx-dev` (the default).
pub const FLAVOR: &str = match option_env!("CREW_FLAVOR") {
    Some(flavor) => flavor,
    None => "zgx-dev",
};

const CANARY: bool = matches!(FLAVOR.as_bytes(), b"zgx-canary");

/// The data dir's name under HOME.
pub const DATA_DIR_NAME: &str = if CANARY { ".crew-zgx-canary" } else { ".crew-zgx-dev" };

/// The engine's IPC port when nothing names one.
pub const DEFAULT_IPC_PORT: u16 = if CANARY { 27959 } else { 27960 };

/// The app's own data (zgx's `app_data_dir`: its setup, windows and
/// `ui.persist` state), apart for each flavour as the engine's is.
pub const APP_ID: &str = if CANARY { "crew-zgx-canary" } else { "crew-zgx-dev" };

/// What the packaged canary is called: its bundle (scripts/crew/package-zgx-app),
/// its windows and Settings → About.
pub const CANARY_PRODUCT: &str = "Crew zgx Canary";

/// What this build is called where it says.
pub const PRODUCT: &str = if CANARY { CANARY_PRODUCT } else { "Crew (zgx dev)" };

/// The packaged canary's bundle id (scripts/crew/package-zgx-app).
pub const CANARY_BUNDLE_ID: &str = "ai.ashler.crew.zgx-canary";

/// The id this build takes its links under (zgx-desktop's application id):
/// the packaged canary's bundle id.
pub const BUNDLE_ID: &str = if CANARY { CANARY_BUNDLE_ID } else { "ai.ashler.crew.zgx-dev" };

/// The scheme the packaged canary registers (scripts/crew/package-zgx-app).
pub const CANARY_URL_SCHEME: &str = "crew-zgx-canary";

/// The scheme of this build's links (a chat's "Copy link", a clicked
/// banner): no link meant for another Crew, or for Zeron, opens here, and
/// none of this build's opens there.
pub const URL_SCHEME: &str = if CANARY { CANARY_URL_SCHEME } else { "crew-zgx-dev" };

/// No Edge until Crew's sign-in lands: a loopback port nothing serves
/// (discard), so an engine of this build stays signed out and local and
/// never reaches Zeron's servers. `ZERON_EDGE_URL` names a real one (a local
/// `wrangler dev` of this branch's Edge).
pub const DEFAULT_EDGE_URL: &str = "http://127.0.0.1:9";

/// Folder-name prefixes that belong to other installs.
const OTHER_INSTALLS: &[&str] = &[".crew", ".comet", ".zeron"];

/// The prefix of this build's own folders.
const OURS: &str = ".crew-zgx-";

/// Why `port` can't be this build's engine's, if it can't.
pub fn port_refusal(port: u16) -> Option<String> {
    let whose = match port {
        27654 | 27655 => "Zeron's",
        27871 => "Zeron staging's",
        27954..=27958 => "another Crew's",
        _ => return None,
    };
    Some(format!("port {port} is {whose} engine's"))
}

/// Why `dir` can't be this build's data dir, if it can't: a folder on its
/// path (symlinks resolved) is another install's.
pub fn data_dir_refusal(dir: &Path) -> Option<String> {
    let dir = normalize(dir);
    dir.components().find_map(|component| {
        let Component::Normal(name) = component else {
            return None;
        };
        let name = name.to_string_lossy();
        let theirs = OTHER_INSTALLS.iter().any(|prefix| name.starts_with(prefix));
        (theirs && !name.starts_with(OURS)).then(|| {
            format!(
                "{} is inside {name}, another Crew or Zeron install's",
                dir.display()
            )
        })
    })
}

/// Why an engine at `dir` on `port` can't be this build's, if it can't.
pub fn refusal(dir: &Path, port: u16) -> Option<String> {
    port_refusal(port).or_else(|| data_dir_refusal(dir))
}

/// Stops this process when `dir`/`port` belong to another install: the
/// guard every engine start of this build goes through.
pub fn exit_if_refused(dir: &Path, port: u16) {
    if let Some(why) = refusal(dir, port) {
        eprintln!("refusing to start: {why} (a Crew {FLAVOR} build; docs/crew/zgx-thin-port.md)");
        std::process::exit(78);
    }
}

/// `path` with symlinks resolved where it exists, and `.`/`..` folded
/// lexically where it doesn't (a data dir is often not made yet).
fn normalize(path: &Path) -> PathBuf {
    if let Ok(real) = std::fs::canonicalize(path) {
        return real;
    }
    let mut folded = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                folded.pop();
            }
            other => folded.push(other),
        }
    }
    // The nearest existing ancestor, resolved, with the rest put back on.
    let mut existing = folded.clone();
    let mut rest = Vec::new();
    while !existing.exists() {
        let Some(name) = existing.file_name().map(ToOwned::to_owned) else {
            return folded;
        };
        rest.push(name);
        if !existing.pop() {
            return folded;
        }
    }
    let mut real = std::fs::canonicalize(&existing).unwrap_or(existing);
    real.extend(rest.into_iter().rev());
    real
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn other_installs_folders_and_their_insides_are_refused() {
        let home = tempfile::tempdir().unwrap();
        let home = home.path();
        for name in [
            ".crew",
            ".crew-staging",
            ".crew-upstream-canary",
            ".crew-staging-mcp-95e5",
            ".comet-native",
            ".comet-native-staging",
            ".zeron",
            ".zeron-staging",
        ] {
            assert!(data_dir_refusal(&home.join(name)).is_some(), "{name}");
            assert!(data_dir_refusal(&home.join(name).join("profiles/x")).is_some(), "{name}/…");
        }
    }

    #[test]
    fn relative_tricks_dont_get_past_the_refusal() {
        let home = tempfile::tempdir().unwrap();
        let sneaky = home.path().join(".crew-zgx-dev/../.crew-staging");
        assert!(data_dir_refusal(&sneaky).is_some());
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_to_another_install_is_refused() {
        let home = tempfile::tempdir().unwrap();
        let home = home.path();
        std::fs::create_dir(home.join(".crew-staging")).unwrap();
        std::os::unix::fs::symlink(home.join(".crew-staging"), home.join("innocent")).unwrap();
        assert!(data_dir_refusal(&home.join("innocent")).is_some());
        assert!(data_dir_refusal(&home.join("innocent/sub")).is_some());
    }

    #[test]
    fn this_builds_folders_and_unrelated_ones_are_allowed() {
        let home = tempfile::tempdir().unwrap();
        let home = home.path();
        for name in [".crew-zgx-dev", ".crew-zgx-canary", "elsewhere", "crew-notes"] {
            assert!(data_dir_refusal(&home.join(name)).is_none(), "{name}");
        }
    }

    #[test]
    fn other_engines_ports_are_refused() {
        for port in [27654, 27655, 27871, 27954, 27955, 27956, 27957, 27958] {
            assert!(port_refusal(port).is_some(), "{port}");
        }
        for port in [DEFAULT_IPC_PORT, 27959, 27960, 27961] {
            assert!(port_refusal(port).is_none(), "{port}");
        }
    }

    /// The packaged canary's bundle is named, and built, as this flavour,
    /// without Zeron's member-card scenes.
    #[test]
    fn the_packaged_canary_is_the_canary_flavour() {
        let script = include_str!("../../../scripts/crew/package-zgx-app");
        assert!(script.contains(&format!("APP_NAME=\"{CANARY_PRODUCT}\"")), "the bundle's name");
        assert!(script.contains("FLAVOR=zgx-canary"), "built as the canary");
        assert!(script.contains(&format!("BUNDLE_ID=\"{CANARY_BUNDLE_ID}\"")), "the bundle's id");
        assert!(script.contains(&format!("URL_SCHEME=\"{CANARY_URL_SCHEME}\"")), "the links' scheme registered");
        assert!(script.contains("--exclude '/parallax-scenes/'"), "no member-card scenes");
    }

    /// The ids this build's links come under are reverse-DNS, as
    /// zgx-desktop's `application_id` requires (else the app wouldn't start).
    #[test]
    fn the_links_id_is_reverse_dns() {
        for id in [BUNDLE_ID, CANARY_BUNDLE_ID] {
            let pieces: Vec<&str> = id.split('.').collect();
            assert!(pieces.len() >= 2, "{id}");
            assert!(
                pieces.iter().all(|p| p.starts_with(|c: char| c.is_ascii_alphabetic())
                    && p.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')),
                "{id}"
            );
        }
    }

    #[test]
    fn the_defaults_are_this_builds_own() {
        let home = tempfile::tempdir().unwrap();
        assert!(refusal(&home.path().join(DATA_DIR_NAME), DEFAULT_IPC_PORT).is_none());
        assert!(DATA_DIR_NAME.starts_with(OURS));
        // The app's own data follows the flavour too: its name is the data
        // dir's, less the dot (`crew-zgx-dev`, `crew-zgx-canary`).
        assert_eq!(Some(APP_ID), DATA_DIR_NAME.strip_prefix('.'));
    }
}
