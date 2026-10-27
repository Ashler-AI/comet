//! Build-selected defaults only; explicit runtime coordinates retain precedence.

pub struct Defaults {
    pub data_dir: &'static str,
    #[cfg(windows)]
    pub windows_data_dir: &'static str,
    pub edge_url: &'static str,
    pub scaffold_url: Option<&'static str>,
    pub project_scope: Option<&'static str>,
    pub deployment_id: Option<&'static str>,
    pub ipc_port: u16,
    pub launchd_label: &'static str,
    pub systemd_unit: &'static str,
}

#[cfg(any(comet_default_environment = "canary", test))]
const CANARY: Defaults = Defaults {
    data_dir: ".crew-upstream-canary",
    #[cfg(windows)]
    windows_data_dir: "Crew Canary",
    edge_url: "http://127.0.0.1:27941",
    scaffold_url: None,
    project_scope: None,
    deployment_id: None,
    ipc_port: 27954,
    launchd_label: "ai.ashler.crew.upstream-canary",
    systemd_unit: "crew-upstream-canary.service",
};

#[cfg(any(comet_default_environment = "staging", test))]
const STAGING: Defaults = Defaults {
    data_dir: ".crew-staging",
    #[cfg(windows)]
    windows_data_dir: "Crew Staging",
    edge_url: "https://comet-staging.internal.ashler.com",
    scaffold_url: Some("https://scaffold-staging.internal.ashler.com"),
    project_scope: Some("ashler-staging"),
    deployment_id: Some("ashler-staging"),
    ipc_port: 27957,
    launchd_label: "ai.ashler.crew.staging",
    systemd_unit: "crew-staging.service",
};

#[cfg(any(comet_default_environment = "production", test))]
const PRODUCTION: Defaults = Defaults {
    data_dir: ".crew",
    #[cfg(windows)]
    windows_data_dir: "Crew",
    edge_url: "https://comet.internal.ashler.com",
    scaffold_url: Some("https://scaffold.internal.ashler.com"),
    project_scope: Some("ashler-production"),
    // Canonical main leaves deployment selection to validated device bootstrap.
    deployment_id: None,
    ipc_port: 27958,
    launchd_label: "ai.ashler.crew.production",
    systemd_unit: "crew-production.service",
};

#[cfg(comet_default_environment = "canary")]
pub const DEFAULTS: Defaults = CANARY;
#[cfg(comet_default_environment = "staging")]
pub const DEFAULTS: Defaults = STAGING;
#[cfg(comet_default_environment = "production")]
pub const DEFAULTS: Defaults = PRODUCTION;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flavors_have_separate_profiles_ports_and_services() {
        let flavors = [&CANARY, &STAGING, &PRODUCTION];
        for (index, flavor) in flavors.iter().enumerate() {
            // Existing Canary Dev and the fixture controller retain these ports.
            assert!(![27955, 27956].contains(&flavor.ipc_port));
            assert!(![".comet-native", ".comet-native-staging", ".zeron"].contains(&flavor.data_dir));
            for other in &flavors[index + 1..] {
                assert_ne!(flavor.data_dir, other.data_dir);
                #[cfg(windows)]
                assert_ne!(flavor.windows_data_dir, other.windows_data_dir);
                assert_ne!(flavor.ipc_port, other.ipc_port);
                assert_ne!(flavor.launchd_label, other.launchd_label);
                assert_ne!(flavor.systemd_unit, other.systemd_unit);
            }
        }
    }
}
