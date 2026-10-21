//! Application storage paths. Provider credentials keep their own locations.

use std::ffi::OsString;
use std::path::PathBuf;

pub fn production() -> bool {
    option_env!("COMET_PACKAGE_ENVIRONMENT")
        .or(option_env!("COMET_DEFAULT_ENVIRONMENT")) == Some("production")
}

pub fn ipc_port() -> u16 {
    if production() { 27654 } else { 27655 }
}

pub fn release_defaults() -> (&'static str, &'static str, &'static str, Option<&'static str>) {
    if production() {
        ("https://comet.internal.ashler.com", "https://scaffold.internal.ashler.com", "ashler-production", None)
    } else {
        ("https://comet-staging.internal.ashler.com", "https://scaffold-staging.internal.ashler.com", "ashler-staging", Some("ashler-staging"))
    }
}

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
        local.join(if production() { "Crew" } else { "Crew Staging" })
    }
    #[cfg(not(windows))]
    {
        let home = PathBuf::from(env("HOME").expect("HOME not set"));
        home.join(if production() { ".comet-native" } else { ".comet-native-staging" })
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


}
