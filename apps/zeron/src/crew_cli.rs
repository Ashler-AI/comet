//! Crew: a launch's Scaffold authority and device bootstrap, and the
//! `scaffold-authority` probe (docs/crew/zgx-thin-port.md, "Scaffold
//! sandboxes"), ported from Crew's `apps/zeron/src/crew_cli.rs`
//! (Ashler-AI/comet). Scaffold's native runner starts a sandbox host with
//!
//! ```text
//! comet --data-dir D --ipc-port 39400 headless --device-bootstrap-file F \
//!   --edge-url E --scaffold-url S --project-scope P --deployment-id D
//! ```
//!
//! (`COMET_DATA_DIR`, `COMET_EDGE_URL` and `COMET_SCAFFOLD_URL` set to the
//! same), and restarts it the same way less the bootstrap. Here:
//!
//! - A command line that names an authority names all of it (the Edge, the
//!   authority and the project; the deployment if it has one), as https
//!   origins, never another Crew's Edge, and never with a development bearer
//!   or a different `crew-runtime.json`. The runner's variables may only say
//!   the same as the command line: they never name an authority or a data dir
//!   themselves (isolation rule 1).
//! - The bootstrap is Crew's file, read strictly: private, the owner's, a
//!   bounded regular file (never a link), no field Crew doesn't write, and
//!   bound to the authority. Its grant becomes the engine's device
//!   (`zeron_engine::crew_sandbox`), which removes the file once the grant is
//!   verified, and runs OMP by default as Crew's host does.
//! - `scaffold-authority` prints the running host's verified grant as Crew's
//!   does, byte for byte: the desktop's attach probe reads that line.

use std::io::Read as _;
use std::path::Path;

use serde_json::Value;
use zeron_cli::crew_args::Launch;
use zeron_engine::crew_sandbox::{DEVICE_SESSION_FILE, DeviceLaunch, SCAFFOLD_HOST_AUTHORITY};
use zeron_engine::scaffold_auth::{DeviceConfig, ScaffoldConfig, normalized_origin};
use zeron_engine::{DeviceGrantAuthority, EngineConfig, HarnessId};

use crate::crew_runtime;

/// The largest bootstrap Crew reads (as Scaffold writes them).
const MAX_BOOTSTRAP_BYTES: u64 = 16 * 1024;

/// What a bootstrap may say: Crew's fields, `edgeUrl` optional.
const BOOTSTRAP_FIELDS: [&str; 8] = [
    "deviceJoinGrant",
    "projectId",
    "deploymentId",
    "sessionId",
    "deviceId",
    "lifecycleEpoch",
    "sandboxId",
    "edgeUrl",
];

/// The command line's part of `config`: its authority, then its bootstrap.
pub fn apply(config: &mut EngineConfig, launch: &Launch) -> Result<(), String> {
    apply_with_env(config, launch, |name| std::env::var_os(name))
}

fn apply_with_env(
    config: &mut EngineConfig,
    launch: &Launch,
    env: impl Fn(&str) -> Option<std::ffi::OsString>,
) -> Result<(), String> {
    let args = &launch.runtime;
    if args.names_authority() {
        let required = |value: &Option<String>, flag: &str| {
            value
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
                .ok_or_else(|| format!("{flag} is required with a Scaffold authority on the command line"))
        };
        let origin = |value: String, flag: &str| {
            normalized_origin(&value).map_err(|_| {
                format!("{flag} must be an https origin (or loopback http), with no path, query or credentials")
            })
        };
        let edge = origin(required(&args.edge_url, "--edge-url")?, "--edge-url")?;
        if crew_runtime::OTHER_EDGES.contains(&edge.as_str()) {
            return Err(format!("{edge} is another Crew's Edge"));
        }
        let control = origin(required(&args.scaffold_url, "--scaffold-url")?, "--scaffold-url")?;
        let project = required(&args.project_scope, "--project-scope")?;
        let deployment = match &args.deployment_id {
            None => None,
            Some(_) => Some(required(&args.deployment_id, "--deployment-id")?),
        };
        if config.edge_token.as_deref().is_some_and(|token| !token.trim().is_empty()) {
            return Err("a development bearer (ZERON_EDGE_TOKEN) and a Scaffold authority can't be used together".into());
        }
        if let Some(saved) = &config.scaffold
            && (saved.control_origin != control
                || saved.edge_origin != edge
                || saved.project_scope != project
                || saved.deployment_id != deployment)
        {
            return Err(format!("the command line names another authority than {}", crew_runtime::FILE));
        }
        // The runner's variables restate the command line, or nothing at all.
        for (name, expected) in [("COMET_EDGE_URL", &edge), ("COMET_SCAFFOLD_URL", &control)] {
            if let Some(value) = env(name).filter(|value| !value.is_empty()) {
                let value = value.to_string_lossy();
                if normalized_origin(&value).ok().as_ref() != Some(expected) {
                    return Err(format!("{name} names another origin than the command line"));
                }
            }
        }
        if let Some(value) = env("COMET_DATA_DIR").filter(|value| !value.is_empty())
            && std::path::absolute(&value).ok().as_deref() != Some(config.data_dir.as_path())
        {
            return Err("COMET_DATA_DIR names another data dir than the command line".into());
        }
        config.edge_url = edge.clone();
        config.edge_token = None;
        config.workos_client_id = None;
        config.org_id = None;
        let mut scaffold = ScaffoldConfig::new(control, &edge, project);
        scaffold.deployment_id = deployment;
        config.scaffold = Some(scaffold);
    }
    match &launch.device_bootstrap_file {
        Some(path) => configure_device(config, path)?,
        // A host restarted from its durable credential runs as it was launched.
        None if args.names_authority() && config.data_dir.join(DEVICE_SESSION_FILE).exists() => {
            config.default_harness = HarnessId::Omp;
        }
        None => {}
    }
    Ok(())
}

/// Read `path`, Scaffold's device bootstrap, into `config`: the sandbox host
/// its grant names, on the authority `config` already has.
pub fn configure_device(config: &mut EngineConfig, path: &Path) -> Result<(), String> {
    let bytes = read_private(path)?;
    let value: Value = serde_json::from_slice(&bytes).map_err(|_| "invalid device bootstrap".to_owned())?;
    let fields = value.as_object().ok_or("invalid device bootstrap")?;
    if let Some(unknown) = fields.keys().find(|key| !BOOTSTRAP_FIELDS.contains(&key.as_str())) {
        return Err(format!("invalid device bootstrap: unknown field {unknown}"));
    }
    let text = |name: &str| -> Result<String, String> {
        fields
            .get(name)
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| format!("invalid device bootstrap: {name}"))
    };
    let grant = text("deviceJoinGrant")?;
    let project = text("projectId")?;
    let deployment = text("deploymentId")?;
    let session = text("sessionId")?;
    let device = text("deviceId")?;
    let sandbox = text("sandboxId")?;
    let epoch = fields
        .get("lifecycleEpoch")
        .and_then(Value::as_u64)
        .filter(|epoch| *epoch >= 1)
        .ok_or("invalid device bootstrap: lifecycleEpoch")?;
    let edge = match fields.get("edgeUrl") {
        None | Some(Value::Null) => None,
        Some(Value::String(edge)) => Some(edge.clone()),
        Some(_) => return Err("invalid device bootstrap: edgeUrl".into()),
    };
    // Scaffold's ids (its runtime profile's pattern), its device, and Crew's
    // grant shape: `cg1.<32 hex>.<secret>`.
    if let Some(name) = [("projectId", &project), ("deploymentId", &deployment), ("sessionId", &session),
        ("sandboxId", &sandbox), ("deviceId", &device)]
        .into_iter()
        .find_map(|(name, id)| (!scaffold_id(id)).then_some(name))
    {
        return Err(format!("invalid device bootstrap: {name}"));
    }
    if device != format!("comet-scaffold-{sandbox}-e{epoch}") {
        return Err("device bootstrap names another device than its sandbox's".into());
    }
    if !join_grant(&grant) {
        return Err("invalid device bootstrap: deviceJoinGrant".into());
    }
    let authority = config
        .scaffold
        .as_ref()
        .ok_or("device bootstrap requires a configured Scaffold authority")?;
    if project != authority.project_scope
        || authority.deployment_id.as_deref().is_some_and(|expected| expected != deployment)
        || edge.is_some_and(|edge| normalized_origin(&edge).ok().as_deref() != Some(config.edge_url.as_str()))
    {
        return Err("device bootstrap does not match the configured authority".into());
    }
    config.device = Some(DeviceLaunch {
        grant: DeviceConfig {
            join_grant: grant,
            device_id: device,
            session_id: session,
            deployment_id: deployment,
            sandbox_id: sandbox,
            lifecycle_epoch: epoch,
        },
        bootstrap_file: Some(path.to_owned()),
    });
    config.default_harness = HarnessId::Omp;
    Ok(())
}

/// A bounded regular file that only its owner (this user) can read, opened
/// without following a link. Nothing is read from anything else.
fn read_private(path: &Path) -> Result<Vec<u8>, String> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options.open(path).map_err(|_| "device bootstrap unavailable".to_owned())?;
    let metadata = file.metadata().map_err(|_| "device bootstrap unavailable".to_owned())?;
    if !metadata.is_file() || metadata.len() == 0 || metadata.len() > MAX_BOOTSTRAP_BYTES {
        return Err("device bootstrap must be a bounded regular file".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        // SAFETY: geteuid takes nothing and can't fail.
        if metadata.mode() & 0o077 != 0 || metadata.uid() != unsafe { libc::geteuid() } {
            return Err("device bootstrap must be private and owned by the current user".into());
        }
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(MAX_BOOTSTRAP_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "device bootstrap unavailable".to_owned())?;
    if bytes.len() as u64 > MAX_BOOTSTRAP_BYTES {
        return Err("device bootstrap exceeded its size limit".into());
    }
    Ok(bytes)
}

/// Scaffold's id pattern (`^[A-Za-z0-9][A-Za-z0-9._:-]{0,127}$`).
fn scaffold_id(id: &str) -> bool {
    id.len() <= 128
        && id.bytes().enumerate().all(|(index, byte)| {
            byte.is_ascii_alphanumeric() || (index > 0 && matches!(byte, b'.' | b'_' | b':' | b'-'))
        })
        && !id.is_empty()
}

/// Crew's join grant: `cg1.<32 hex>.<secret>`.
fn join_grant(grant: &str) -> bool {
    let mut parts = grant.split('.');
    parts.next() == Some("cg1")
        && parts.next().is_some_and(|id| id.len() == 32 && id.bytes().all(|byte| byte.is_ascii_hexdigit()))
        && parts.next().is_some_and(|secret| !secret.is_empty())
        && parts.next().is_none()
}

/// `scaffold-authority`: the running host's verified grant, as one line.
pub async fn scaffold_authority(ipc_port: u16, data_dir: &Path) -> anyhow::Result<()> {
    let client = zeron_rpc::connect_ipc(ipc_port, Some(data_dir))
        .await
        .map_err(|error| anyhow::anyhow!("no engine listening on 127.0.0.1:{ipc_port} ({error})"))?;
    let reply = client
        .call(SCAFFOLD_HOST_AUTHORITY, serde_json::json!({}))
        .await
        .map_err(|error| anyhow::anyhow!("{SCAFFOLD_HOST_AUTHORITY}: {error}"))?;
    println!("{}", authority_line(reply)?);
    Ok(())
}

/// The probe's line as Crew's prints it: the grant's fields in its own
/// order (Crew's JSON keeps it; this build's maps would sort the keys).
fn authority_line(reply: Value) -> anyhow::Result<String> {
    let authority: DeviceGrantAuthority = serde_json::from_value(reply)?;
    Ok(serde_json::to_string(&authority)?)
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;

    use serde_json::json;
    use zeron_cli::crew_args::RuntimeArgs;

    use super::*;

    const GRANT: &str = "cg1.0123456789abcdef0123456789abcdef.enrollment-secret";

    fn config(data_dir: &Path) -> EngineConfig {
        EngineConfig {
            data_dir: data_dir.to_path_buf(),
            edge_url: "http://127.0.0.1:9".into(),
            edge_token: None,
            ipc_port: 39400,
            default_harness: HarnessId::ClaudeCode,
            org_id: Some("inherited".into()),
            workos_client_id: Some("client_inherited".into()),
            scaffold: None,
            device: None,
        }
    }

    /// The runner's launch, less the bootstrap.
    fn runner(data_dir: &Path) -> Launch {
        Launch {
            runtime: RuntimeArgs {
                data_dir: Some(data_dir.to_path_buf()),
                ipc_port: Some(39400),
                edge_url: Some("https://crew-zgx-edge-staging.cgoggins.workers.dev".into()),
                scaffold_url: Some("https://scaffold-staging.internal.ashler.com/".into()),
                project_scope: Some("ashler-staging".into()),
                deployment_id: Some("ashler-staging".into()),
            },
            device_bootstrap_file: None,
        }
    }

    fn resolve(launch: &Launch, vars: &[(&str, &str)]) -> Result<EngineConfig, String> {
        let dir = launch.runtime.data_dir.clone().unwrap_or_default();
        let mut resolved = config(&dir);
        apply_with_env(&mut resolved, launch, |name| {
            vars.iter().find(|(key, _)| *key == name).map(|(_, value)| (*value).into())
        })?;
        Ok(resolved)
    }

    fn bootstrap() -> Value {
        json!({
            "deviceJoinGrant": GRANT, "projectId": "ashler-staging", "deploymentId": "ashler-staging",
            "sessionId": "session-1", "deviceId": "comet-scaffold-sbx-1-e3", "lifecycleEpoch": 3,
            "sandboxId": "sbx-1",
        })
    }

    fn write_private(dir: &Path, value: &Value) -> std::path::PathBuf {
        let path = dir.join("enrollment.json");
        let mut file = tempfile::NamedTempFile::new_in(dir).unwrap();
        file.write_all(value.to_string().as_bytes()).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(file.path(), std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        file.persist(&path).unwrap();
        path
    }

    /// The native runner's coordinates are the engine's authority: Scaffold
    /// mode on its Edge, no WorkOS, organization or bearer; and its
    /// variables, saying the same, change nothing.
    #[test]
    fn the_runner_s_command_line_is_the_authority() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("crew-data");
        let vars = [
            ("COMET_DATA_DIR", data.to_str().unwrap()),
            ("COMET_EDGE_URL", "https://crew-zgx-edge-staging.cgoggins.workers.dev"),
            ("COMET_SCAFFOLD_URL", "https://scaffold-staging.internal.ashler.com"),
        ];
        let resolved = resolve(&runner(&data), &vars).unwrap();
        assert_eq!(resolved.edge_url, "https://crew-zgx-edge-staging.cgoggins.workers.dev");
        let scaffold = resolved.scaffold.unwrap();
        assert_eq!(scaffold.control_origin, "https://scaffold-staging.internal.ashler.com");
        assert_eq!(scaffold.edge_origin, "https://crew-zgx-edge-staging.cgoggins.workers.dev");
        assert_eq!(scaffold.project_scope, "ashler-staging");
        assert_eq!(scaffold.deployment_id.as_deref(), Some("ashler-staging"));
        assert!(resolved.workos_client_id.is_none() && resolved.org_id.is_none() && resolved.edge_token.is_none());
        assert!(resolved.device.is_none());
        assert_eq!(resolved.default_harness, HarnessId::ClaudeCode, "no host yet");
        // A host restarted from its credential runs OMP, as it was launched.
        std::fs::create_dir_all(&data).unwrap();
        std::fs::write(data.join(DEVICE_SESSION_FILE), b"{}").unwrap();
        assert_eq!(resolve(&runner(&data), &[]).unwrap().default_harness, HarnessId::Omp);
        // Nothing named: nothing changes, whatever the variables say.
        let plain = resolve(&Launch::default(), &vars).unwrap();
        assert!(plain.scaffold.is_none());
        assert_eq!(plain.edge_url, "http://127.0.0.1:9");
    }

    #[test]
    fn an_authority_it_can_t_use_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("crew-data");
        let mut cases: Vec<(Launch, Vec<(&str, &str)>)> = Vec::new();
        for (field, value) in [
            ("edge", None),
            ("scaffold", None),
            ("project", None),
            ("project", Some(" ")),
            ("deployment", Some("")),
            ("edge", Some("http://edge.example")),
            ("edge", Some("https://edge.example/path")),
            ("scaffold", Some("https://user@scaffold.example")),
            // Never another Crew's Edge.
            ("edge", Some("https://comet-staging.internal.ashler.com")),
            ("edge", Some("http://127.0.0.1:27941")),
        ] {
            let mut launch = runner(&data);
            let slot = match field {
                "edge" => &mut launch.runtime.edge_url,
                "scaffold" => &mut launch.runtime.scaffold_url,
                "project" => &mut launch.runtime.project_scope,
                _ => &mut launch.runtime.deployment_id,
            };
            *slot = value.map(str::to_owned);
            cases.push((launch, Vec::new()));
        }
        // The runner's variables naming anything else.
        for var in [
            ("COMET_EDGE_URL", "https://comet-staging.internal.ashler.com"),
            ("COMET_SCAFFOLD_URL", "https://scaffold.example"),
            ("COMET_DATA_DIR", "/elsewhere"),
        ] {
            cases.push((runner(&data), vec![var]));
        }
        for (launch, vars) in &cases {
            assert!(resolve(launch, vars).is_err(), "{launch:?} {vars:?}");
        }
        // Not beside a development bearer, nor another crew-runtime.json's.
        let mut with_bearer = config(&data);
        with_bearer.edge_token = Some("dev@org".into());
        assert!(apply_with_env(&mut with_bearer, &runner(&data), |_| None).is_err());
        let mut with_file = config(&data);
        with_file.scaffold = Some(ScaffoldConfig::new("https://other.example", "https://edge.example", "p"));
        assert!(apply_with_env(&mut with_file, &runner(&data), |_| None).is_err());
    }

    /// The bootstrap Platform's runner hands over: the host it names, with
    /// OMP by default, and the file kept for the engine to remove.
    #[test]
    fn a_bootstrap_names_the_host() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_private(dir.path(), &bootstrap());
        let mut launch = runner(&dir.path().join("crew-data"));
        launch.device_bootstrap_file = Some(path.clone());
        let resolved = resolve(&launch, &[]).unwrap();
        let device = resolved.device.expect("a host");
        assert_eq!(device.grant.join_grant, GRANT);
        assert_eq!(device.grant.device_id, "comet-scaffold-sbx-1-e3");
        assert_eq!(device.grant.session_id, "session-1");
        assert_eq!(device.grant.sandbox_id, "sbx-1");
        assert_eq!(device.grant.deployment_id, "ashler-staging");
        assert_eq!(device.grant.lifecycle_epoch, 3);
        assert_eq!(device.bootstrap_file.as_deref(), Some(path.as_path()));
        assert_eq!(resolved.default_harness, HarnessId::Omp);
        assert!(path.exists(), "the engine removes it, once its grant is verified");
        // The Edge Scaffold names, when it does, is the command line's.
        let mut with_edge = bootstrap();
        with_edge["edgeUrl"] = json!("https://crew-zgx-edge-staging.cgoggins.workers.dev");
        write_private(dir.path(), &with_edge);
        assert!(resolve(&launch, &[]).is_ok());
    }

    /// Crew's `bootstrap_requires_the_configured_deployment`, and what else
    /// binds it: the project, the Edge, the device, the grant, and Crew's
    /// fields alone.
    #[test]
    fn a_bootstrap_bound_elsewhere_or_unknown_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let mut launch = runner(&dir.path().join("crew-data"));
        launch.device_bootstrap_file = Some(dir.path().join("enrollment.json"));
        for (field, patch) in [
            ("deploymentId", json!("other")),
            ("projectId", json!("other")),
            ("edgeUrl", json!("https://other-edge.example")),
            ("edgeUrl", json!(7)),
            ("deviceId", json!("comet-scaffold-sbx-1-e4")),
            ("deviceId", json!("device")),
            ("sandboxId", json!("sbx 1")),
            ("lifecycleEpoch", json!(0)),
            ("lifecycleEpoch", json!("3")),
            ("deviceJoinGrant", json!("cs1.0123456789abcdef0123456789abcdef.secret")),
            ("deviceJoinGrant", json!("cg1.short.secret")),
            ("deviceJoinGrant", json!("cg1.0123456789abcdef0123456789abcdef.")),
            ("sessionId", json!(null)),
            ("unexpected", json!("field")),
        ] {
            let mut value = bootstrap();
            value[field] = patch;
            write_private(dir.path(), &value);
            let refused = resolve(&launch, &[]);
            assert!(refused.is_err(), "{field}: {value}");
        }
        // Without an authority there's nothing to bind it to.
        write_private(dir.path(), &bootstrap());
        let mut bare = config(&dir.path().join("crew-data"));
        assert!(configure_device(&mut bare, launch.device_bootstrap_file.as_deref().unwrap()).is_err());
        assert!(bare.device.is_none());
    }

    /// Crew's `bootstrap_rejects_symlinks_without_consuming_credentials`, and
    /// what else a bootstrap file must be.
    #[cfg(unix)]
    #[test]
    fn a_bootstrap_is_a_private_regular_file_and_nothing_else() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let mut launch = runner(&dir.path().join("crew-data"));
        let source = write_private(dir.path(), &bootstrap());
        let link = dir.path().join("link.json");
        std::os::unix::fs::symlink(&source, &link).unwrap();
        launch.device_bootstrap_file = Some(link);
        assert!(resolve(&launch, &[]).is_err(), "a link is never followed");
        assert!(source.exists(), "and nothing is consumed");
        launch.device_bootstrap_file = Some(source.clone());
        std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(resolve(&launch, &[]).is_err(), "readable by others");
        std::fs::write(&source, vec![b' '; MAX_BOOTSTRAP_BYTES as usize + 1]).unwrap();
        std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(resolve(&launch, &[]).is_err(), "too large");
        std::fs::write(&source, b"").unwrap();
        assert!(resolve(&launch, &[]).is_err(), "empty");
        std::fs::write(&source, b"[]").unwrap();
        assert!(resolve(&launch, &[]).is_err(), "not an object");
        launch.device_bootstrap_file = Some(dir.path().to_path_buf());
        assert!(resolve(&launch, &[]).is_err(), "a folder");
        launch.device_bootstrap_file = Some(dir.path().join("missing.json"));
        assert!(resolve(&launch, &[]).is_err(), "missing");
    }

    /// The probe's line is Crew's, whatever order the reply's keys came in:
    /// this build's maps sort them, Crew's keep the grant's own order.
    #[test]
    fn the_probe_prints_crew_s_line() {
        let sorted = json!({
            "capabilities": ["session.read", "session.sync.write", "session.chat", "session.control",
                "session.annotate", "session.files", "session.environment"],
            "deviceId": "comet-scaffold-sbx-1-e3", "expiresAt": 1_800_000_000_000_i64,
            "grantId": "0123456789abcdef0123456789abcdef", "lifecycleEpoch": 3,
            "principalSubject": "owner@example.com", "sandboxId": "sbx-1",
            "scope": {"deploymentId": "ashler-staging", "projectId": "ashler-staging", "sessionId": "session-1"},
        });
        assert_eq!(
            authority_line(sorted).unwrap(),
            r#"{"grantId":"0123456789abcdef0123456789abcdef","expiresAt":1800000000000,"principalSubject":"owner@example.com","scope":{"projectId":"ashler-staging","deploymentId":"ashler-staging","sessionId":"session-1"},"sandboxId":"sbx-1","deviceId":"comet-scaffold-sbx-1-e3","lifecycleEpoch":3,"capabilities":["session.read","session.sync.write","session.chat","session.control","session.annotate","session.files","session.environment"]}"#
        );
        assert!(authority_line(json!({"grantId": "g"})).is_err(), "only a whole grant is printed");
    }
}
