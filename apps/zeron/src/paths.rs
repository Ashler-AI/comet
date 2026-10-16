//! Application storage paths. Provider credentials keep their own locations.

use std::ffi::OsString;
use std::path::PathBuf;

pub fn data_dir() -> PathBuf {
    resolve_data_dir(|name| std::env::var_os(name))
}

fn resolve_data_dir(mut env: impl FnMut(&str) -> Option<OsString>) -> PathBuf {
    if let Some(dir) = env("COMET_DATA_DIR")
        .or_else(|| env("ZERON_DATA_DIR"))
        .filter(|value| !value.is_empty())
    {
        return PathBuf::from(dir);
    }
    #[cfg(windows)]
    {
        // Explorer does not set HOME. Do not let a shell-specific HOME select
        // a different workspace from a desktop launch, or migrate credentials
        // between Unix-style and native Windows directories implicitly.
        let local = env("LOCALAPPDATA")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .or_else(|| {
                env("USERPROFILE")
                    .filter(|value| !value.is_empty())
                    .map(|home| PathBuf::from(home).join("AppData").join("Local"))
            })
            .expect("LOCALAPPDATA and USERPROFILE not set; set COMET_DATA_DIR");
        local.join("Crew Canary")
    }
    #[cfg(not(windows))]
    {
        let home = PathBuf::from(env("HOME").expect("HOME not set"));
        // Canary startup never renames or opens the installed Crew profile.
        // Importing an existing profile is a separate, explicit operation.
        home.join(".crew-upstream-canary")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolve(vars: &[(&str, &str)]) -> PathBuf {
        resolve_data_dir(|name| {
            vars.iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| value.into())
        })
    }

    #[test]
    fn explicit_data_dir_needs_no_home() {
        assert_eq!(
            resolve(&[("ZERON_DATA_DIR", "custom data")]),
            PathBuf::from("custom data")
        );
    }

    #[test]
    fn native_runtime_data_dir_takes_precedence() {
        assert_eq!(
            resolve(&[("COMET_DATA_DIR", "canary"), ("ZERON_DATA_DIR", "upstream")]),
            PathBuf::from("canary")
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn starting_canary_does_not_adopt_an_installed_profile() {
        let home = tempfile::tempdir().unwrap();
        let installed = home.path().join(".comet-native");
        std::fs::create_dir(&installed).unwrap();
        std::fs::write(installed.join("sentinel"), b"retained-session").unwrap();
        let selected = resolve(&[("HOME", home.path().to_str().unwrap())]);
        assert_ne!(selected, installed);
        assert!(!selected.exists());
        assert_eq!(
            std::fs::read(installed.join("sentinel")).unwrap(),
            b"retained-session"
        );
    }

}
