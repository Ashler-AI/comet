//! Crew: the Scaffold authority an engine of this build signs in with
//! (docs/crew/zgx-thin-port.md, slice 2), from `<data_dir>/crew-runtime.json`
//! in Crew's shape:
//!
//! ```json
//! { "edgeUrl": "http://127.0.0.1:8787", "scaffoldUrl": "https://scaffold.example",
//!   "projectScope": "project", "deploymentId": "optional", "sandboxRegion": "optional" }
//! ```
//!
//! `sandboxRegion` (this port's own) names the Scaffold region whose Edge is
//! `edgeUrl`: the only region this engine creates sandboxes in. Without it,
//! it creates none (crew_cloud.rs).
//!
//! That file, in the data dir `zeron_cli::crew` resolved and allowed, is the
//! only source: no environment variable names the authority or its Edge
//! (not `COMET_*`, `CREW_*`, nor `ZERON_EDGE_URL`, which the file's `edgeUrl`
//! replaces), so a shell that inherited another Crew's environment can't point
//! this engine at that Crew's backend. Fields Crew also writes (`ipcPort`, …)
//! aren't read: the data dir and port stay `zeron_cli::crew`'s.
//!
//! Without the file the engine is as it was: signed out and local, no Edge.
//! A file that can't be used stops the engine (as another install's data dir
//! does) rather than leave it running without the authority it names.
//!
//! Nor may it name another Crew's Edge, whatever else it says (isolation
//! rule 5): an engine of this build syncing there would be a second writer
//! in rooms that other Crew versions use ([`OTHER_EDGES`]).

use std::path::Path;

use zeron_engine::EngineConfig;
use zeron_engine::scaffold_auth::{ScaffoldConfig, normalized_origin};

/// The file's name in the data dir.
pub const FILE: &str = "crew-runtime.json";

/// Other Crews' Edges: Crew Staging's, Crew's (native and legacy), and Crew
/// Canary's local one. Never this build's.
pub const OTHER_EDGES: &[&str] = &[
    "https://comet-staging.internal.ashler.com",
    "https://crew.internal.ashler.com",
    "https://comet.internal.ashler.com",
    "http://127.0.0.1:27941",
    "http://localhost:27941",
    "http://[::1]:27941",
];

/// A data dir's Crew runtime: the Scaffold authority and its Edge.
#[derive(Debug, Clone)]
pub struct CrewRuntime {
    /// The Edge (an exact origin), the engine's and the authority's.
    pub edge_url: String,
    pub scaffold: ScaffoldConfig,
}

/// `data_dir`'s Crew runtime, if it has one.
pub fn load(data_dir: &Path) -> Result<Option<CrewRuntime>, String> {
    let path = data_dir.join(FILE);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("can't read {}: {error}", path.display())),
    };
    let invalid = |why: &str| format!("{} is invalid: {why}", path.display());
    let value: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(|_| invalid("not JSON"))?;
    let fields = value.as_object().ok_or_else(|| invalid("not a JSON object"))?;
    let text = |name: &str| -> Result<Option<String>, String> {
        match fields.get(name) {
            None | Some(serde_json::Value::Null) => Ok(None),
            Some(serde_json::Value::String(text)) if !text.trim().is_empty() => {
                Ok(Some(text.trim().to_string()))
            }
            Some(_) => Err(invalid(&format!("{name} must be a nonblank string"))),
        }
    };
    let required = |name: &str| text(name)?.ok_or_else(|| invalid(&format!("{name} is missing")));
    let origin = |name: &str| -> Result<String, String> {
        normalized_origin(&required(name)?).map_err(|_| {
            invalid(&format!(
                "{name} must be an https origin (or loopback http), with no path, query or credentials"
            ))
        })
    };
    let edge_url = origin("edgeUrl")?;
    if OTHER_EDGES.contains(&edge_url.as_str()) {
        return Err(invalid(&format!("{edge_url} is another Crew's Edge")));
    }
    let mut scaffold = ScaffoldConfig::new(origin("scaffoldUrl")?, &edge_url, required("projectScope")?);
    scaffold.deployment_id = text("deploymentId")?;
    // Platform's region names (`^[a-z0-9][a-z0-9-]{0,62}$`).
    scaffold.sandbox_region = text("sandboxRegion")?;
    if scaffold.sandbox_region.as_deref().is_some_and(|region| {
        region.len() > 63
            || !region.bytes().enumerate().all(|(index, byte)| {
                byte.is_ascii_lowercase() || byte.is_ascii_digit() || (index > 0 && byte == b'-')
            })
    }) {
        return Err(invalid("sandboxRegion must be a Scaffold region name"));
    }
    Ok(Some(CrewRuntime { edge_url, scaffold }))
}

/// Put `config`'s data dir's Crew runtime, if it has one, in charge: Scaffold
/// mode on its Edge, with no WorkOS client and no development bearer.
pub fn apply(config: &mut EngineConfig) -> Result<(), String> {
    let Some(runtime) = load(&config.data_dir)? else {
        return Ok(());
    };
    if config
        .edge_token
        .as_deref()
        .is_some_and(|token| !token.trim().is_empty())
    {
        return Err(format!(
            "a development bearer (ZERON_EDGE_TOKEN) and the Scaffold authority in {} can't be used together",
            config.data_dir.join(FILE).display()
        ));
    }
    if config.edge_url.trim_end_matches('/') != runtime.edge_url {
        tracing::info!(edge = %runtime.edge_url, "{FILE} names the Edge; ZERON_EDGE_URL is not used");
    }
    config.edge_url = runtime.edge_url;
    config.edge_token = None;
    config.workos_client_id = None;
    config.org_id = None;
    config.scaffold = Some(runtime.scaffold);
    // (A sandbox host's device is the command line's: crew_cli.rs.)
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, json: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join(FILE), json).unwrap();
    }

    fn config(data_dir: &Path) -> EngineConfig {
        EngineConfig {
            data_dir: data_dir.to_path_buf(),
            edge_url: "https://comet-staging.internal.example".into(),
            edge_token: None,
            ipc_port: 27960,
            default_harness: zeron_engine::HarnessId::Mock,
            org_id: Some("inherited-org".into()),
            workos_client_id: Some("client_inherited".into()),
            scaffold: None,
            device: None,
        }
    }

    #[test]
    fn without_the_file_the_engine_is_as_it_was() {
        let dir = tempfile::tempdir().unwrap();
        let mut resolved = config(dir.path());
        apply(&mut resolved).unwrap();
        assert!(resolved.scaffold.is_none());
        assert_eq!(resolved.edge_url, "https://comet-staging.internal.example");
        assert_eq!(resolved.workos_client_id.as_deref(), Some("client_inherited"));
    }

    #[test]
    fn the_file_names_the_authority_and_its_edge() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            r#"{"edgeUrl":"http://127.0.0.1:8787/","scaffoldUrl":"https://scaffold.example",
                "projectScope":" project-a ","deploymentId":"deployment-1","ipcPort":27957,
                "sandboxRegion":"harness-zgx","scaffoldRuntimeArtifact":{"unknown":"ignored"}}"#,
        );
        let mut resolved = config(dir.path());
        apply(&mut resolved).unwrap();
        assert_eq!(resolved.edge_url, "http://127.0.0.1:8787");
        let scaffold = resolved.scaffold.expect("Scaffold mode");
        assert_eq!(scaffold.control_origin, "https://scaffold.example");
        assert_eq!(scaffold.edge_origin, "http://127.0.0.1:8787");
        assert_eq!(scaffold.project_scope, "project-a");
        assert_eq!(scaffold.deployment_id.as_deref(), Some("deployment-1"));
        assert_eq!(scaffold.sandbox_region.as_deref(), Some("harness-zgx"));
        assert!(resolved.workos_client_id.is_none() && resolved.org_id.is_none());
        assert!(resolved.edge_token.is_none() && resolved.device.is_none());
        // The data dir and port are slice 1's, whatever the file says.
        assert_eq!(resolved.ipc_port, 27960);
        assert_eq!(resolved.data_dir, dir.path());
    }

    #[test]
    fn a_file_it_cant_use_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let good = serde_json::json!({
            "edgeUrl": "https://edge.example", "scaffoldUrl": "https://scaffold.example",
            "projectScope": "project",
        });
        let mut cases = vec!["{".to_string(), "[]".to_string()];
        for (field, value) in [
            ("edgeUrl", serde_json::Value::Null),
            ("scaffoldUrl", serde_json::json!(" ")),
            ("projectScope", serde_json::json!(7)),
            ("deploymentId", serde_json::json!("")),
            // A region is Platform's name for one, nothing else.
            ("sandboxRegion", serde_json::json!("Harness ZGX")),
            ("sandboxRegion", serde_json::json!("-harness")),
            ("sandboxRegion", serde_json::json!(["harness-zgx"])),
            // Only https origins (or loopback http) are authority configuration.
            ("edgeUrl", serde_json::json!("http://edge.example")),
            ("scaffoldUrl", serde_json::json!("https://scaffold.example/oauth")),
            ("scaffoldUrl", serde_json::json!("https://user@scaffold.example")),
            // Never another Crew's Edge, however it's spelled.
            ("edgeUrl", serde_json::json!("https://comet-staging.internal.ashler.com")),
            ("edgeUrl", serde_json::json!("https://Crew.Internal.Ashler.com:443/")),
            ("edgeUrl", serde_json::json!("https://comet.internal.ashler.com")),
            ("edgeUrl", serde_json::json!("http://127.0.0.1:27941")),
            ("edgeUrl", serde_json::json!("http://localhost:27941/")),
        ] {
            let mut file = good.clone();
            file[field] = value;
            cases.push(file.to_string());
        }
        for case in cases {
            write(dir.path(), &case);
            assert!(load(dir.path()).is_err(), "{case}");
            let mut resolved = config(dir.path());
            assert!(apply(&mut resolved).is_err(), "{case}");
        }
        write(dir.path(), &good.to_string());
        let mut resolved = config(dir.path());
        resolved.edge_token = Some("dev@org".into());
        assert!(apply(&mut resolved).is_err(), "never with a development bearer");
    }

    /// Only the data dir's own file counts: not one beside it, above it, or in
    /// the data dir another Crew's environment names — and no `COMET_*`,
    /// `CREW_*` or `ZERON_EDGE_URL` names an authority or its Edge. Checked
    /// in a child process ([`child`]) given a Crew Staging-like environment.
    #[test]
    fn only_the_data_dirs_file_counts_whatever_the_environment_says() {
        let root = tempfile::tempdir().unwrap();
        let data = root.path().join("data");
        let elsewhere = root.path().join("parent-crew");
        std::fs::create_dir_all(&data).unwrap();
        let theirs = r#"{"edgeUrl":"https://their-edge.example","scaffoldUrl":"https://their-scaffold.example","projectScope":"theirs"}"#;
        write(root.path(), theirs);
        write(&elsewhere, theirs);
        let resolve = || {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "crew_runtime::tests::child", "--nocapture", "--test-threads=1"])
                .env_clear()
                .env("CREW_RUNTIME_TEST_CHILD", "1")
                .env("HOME", root.path())
                .env("PATH", std::env::var_os("PATH").unwrap_or_default())
                .env("ZERON_DATA_DIR", &data)
                .env("ZERON_IPC_PORT", "27961")
                .env("ZERON_EDGE_URL", "https://zeron-edge.example")
                .env("COMET_DATA_DIR", &elsewhere)
                .env("COMET_EDGE_URL", "https://comet-edge.example")
                .env("COMET_SCAFFOLD_URL", "https://comet-scaffold.example")
                .env("CREW_CONTROL_ORIGIN", "https://crew-scaffold.example")
                .env("COMET_PROJECT_SCOPE", "comet-project")
                .env("CREW_PROJECT_SCOPE", "crew-project")
                .env("COMET_DEPLOYMENT_ID", "comet-deployment")
                .env("CREW_DEPLOYMENT_ID", "crew-deployment")
                .output()
                .unwrap();
            let stdout = String::from_utf8_lossy(&output.stdout);
            // With --nocapture, libtest prints the child's line after its own
            // "test crew_runtime::tests::child ... " on the same line.
            let line = stdout
                .lines()
                .find_map(|line| line.split_once("resolved: ").map(|(_, json)| json))
                .unwrap_or_else(|| panic!("child: {output:?}"));
            serde_json::from_str::<serde_json::Value>(line).unwrap()
        };
        let none = resolve();
        assert_eq!(none["scaffold"], serde_json::Value::Null, "{none}");
        assert_eq!(none["dataDir"], data.to_string_lossy().as_ref());

        write(
            &data,
            r#"{"edgeUrl":"http://127.0.0.1:8787","scaffoldUrl":"https://scaffold.example","projectScope":"ours"}"#,
        );
        let ours = resolve();
        assert_eq!(ours["edge"], "http://127.0.0.1:8787", "{ours}");
        assert_eq!(
            ours["scaffold"],
            serde_json::json!(["https://scaffold.example", "http://127.0.0.1:8787", "ours", null]),
        );
        assert_eq!(ours["workos"], serde_json::Value::Null);
    }

    /// [`only_the_data_dirs_file_counts_whatever_the_environment_says`]'s child:
    /// the configuration `zeron-engine` would start with, as JSON.
    #[test]
    fn child() {
        if std::env::var_os("CREW_RUNTIME_TEST_CHILD").is_none() {
            return;
        }
        let config = crate::engine_config_from_env();
        let scaffold = config.scaffold.map(|scaffold| {
            serde_json::json!([scaffold.control_origin, scaffold.edge_origin, scaffold.project_scope, scaffold.deployment_id])
        });
        let resolved = serde_json::json!({
            "dataDir": config.data_dir, "edge": config.edge_url, "scaffold": scaffold,
            "workos": config.workos_client_id,
        });
        println!("resolved: {resolved}");
    }
}
