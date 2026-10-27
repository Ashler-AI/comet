//! Application storage paths. Provider credentials keep their own locations.

use std::ffi::OsString;
use std::path::PathBuf;


pub fn resolve_data_dir(mut env: impl FnMut(&str) -> Option<OsString>, pinned: bool) -> PathBuf {
    if let Some(dir) = (!pinned).then(|| env("COMET_DATA_DIR")
        .filter(|value| !value.is_empty())
        .or_else(|| env("ZERON_DATA_DIR").filter(|value| !value.is_empty()))).flatten()
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
        local.join(crate::environment::DEFAULTS.windows_data_dir)
    }
    #[cfg(not(windows))]
    {
        let home = PathBuf::from(env("HOME").expect("HOME not set"));
        // Never migrate or adopt an installed profile when selecting a flavor.
        // Importing an existing profile is a separate, explicit operation.
        home.join(crate::environment::DEFAULTS.data_dir)
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
        }, false)
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
            resolve(&[("COMET_DATA_DIR", "explicit"), ("ZERON_DATA_DIR", "upstream")]),
            PathBuf::from("explicit")
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn starting_selected_flavor_does_not_adopt_an_installed_profile() {
        let home = tempfile::tempdir().unwrap();
        let installed = [".comet-native", ".comet-native-staging", ".zeron"];
        for name in installed {
            let profile = home.path().join(name);
            std::fs::create_dir(&profile).unwrap();
            std::fs::write(profile.join("sentinel"), b"retained-session").unwrap();
        }
        let selected = resolve(&[("HOME", home.path().to_str().unwrap())]);
        assert_eq!(selected, home.path().join(crate::environment::DEFAULTS.data_dir));
        assert!(!selected.exists());
        for name in installed {
            assert_eq!(
                std::fs::read(home.path().join(name).join("sentinel")).unwrap(),
                b"retained-session"
            );
        }
    }

    #[cfg(windows)]
    #[test]
    fn selected_flavor_uses_native_local_app_data() {
        assert_eq!(
            resolve(&[("LOCALAPPDATA", "local"), ("HOME", "ignored")]),
            PathBuf::from("local").join(crate::environment::DEFAULTS.windows_data_dir)
        );
    }

}
