use super::*;
use sha2::{Digest, Sha256};
#[cfg(not(unix))]
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct IsolatedRecoveryReceipt {
    pub descriptor_sha256: String,
    pub cwd: String,
    pub native_session_id: String,
    pub launch_selector: String,
    pub source_native_session_id: String,
    pub source_sha256: String,
    pub rebound_sha256: String,
    pub rebound_bytes: u64,
}

fn namespace(operation_id: &str) -> String {
    format!("{:x}", Sha256::digest(operation_id.as_bytes()))
}

pub(crate) fn isolated_recovery_native_id(operation_id: &str) -> String {
    let hash = Sha256::digest(operation_id.as_bytes());
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&hash[..16]);
    bytes[6] = (bytes[6] & 15) | 0x80;
    bytes[8] = (bytes[8] & 63) | 0x80;
    uuid::Uuid::from_bytes(bytes).to_string()
}

#[cfg(unix)]
mod unix;

#[cfg(not(unix))]
fn validate_non_unix_operation(operation_id: &str) -> Result<(), ScaffoldError> {
    if !operation_id.starts_with("crew-handoff:") || !operation_id.ends_with(":isolated-recovery") || operation_id.len() > 4096 {
        return Err(ScaffoldError::InvalidResponse("isolated recovery operation ID must use crew-handoff:<command>:isolated-recovery and be at most 4096 bytes".into()));
    }
    Ok(())
}

#[cfg(not(unix))]
impl ScaffoldRuntime {
    pub(crate) async fn recover_native_handoff(
        &self,
        operation_id: &str,
        _environment: &SessionEnvironment,
        _source_selector: &str,
        _source_cwd: &str,
        _session_only: bool,
        _capsule_root: &Path,
        _cancellation: &CancellationToken,
    ) -> Result<IsolatedRecoveryReceipt, ScaffoldError> {
        validate_non_unix_operation(operation_id)?;
        Err(ScaffoldError::UnsupportedPlatform(
            "New isolated native recovery preparation requires a macOS or Linux controller; existing completed handoffs remain available for read-only replay.",
        ))
    }

    pub(crate) fn cleanup_native_handoff_recovery(&self, operation_id: &str, capsule_root: &Path) -> Result<(), ScaffoldError> {
        validate_non_unix_operation(operation_id)?;
        let directory = capsule_root.join(namespace(operation_id));
        match std::fs::symlink_metadata(&directory) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Ok(_) => Err(ScaffoldError::UnsupportedPlatform(
                "An existing isolated native recovery capsule requires cleanup from a macOS or Linux controller; its contents were left unchanged.",
            )),
            Err(error) => Err(ScaffoldError::InvalidResponse(format!(
                "Could not inspect retained isolated recovery capsule {}: {error}. Use a macOS or Linux controller for capsule cleanup.", directory.display(),
            ))),
        }
    }
}

#[cfg(all(test, not(unix)))]
mod non_unix_tests {
    use super::*;
    use std::net::TcpListener;

    fn runtime() -> (ScaffoldRuntime, TcpListener) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let client = ScaffoldClient::new(&origin, "project-a", Arc::new(zeron_rpc::StaticToken("test-token".into()))).unwrap();
        (ScaffoldRuntime::new(client, "https://edge.example", Arc::new(UnavailableDeviceJoinGrantProvider)), listener)
    }

    fn environment() -> SessionEnvironment {
        SessionEnvironment {
            source: SessionEnvironmentSource::Scaffold {
                sandbox_id: "sandbox-a".into(), region: None, lifecycle: zeron_proto::ScaffoldLifecycle::Ready,
                lifecycle_epoch: Some(1), links: Box::default(),
            },
            name: None, owner_principal: "owner-a".into(),
            scope: CollaborationScope { project_id: "project-a".into(), deployment_id: Some("deployment-a".into()), session_id: Some("session-a".into()), unknown: Default::default() },
            source_ref: None, last_activity_at: None, database_environment: Some(ScaffoldDatabaseEnvironment::Local), unknown: Default::default(),
        }
    }

    fn no_requests(listener: &TcpListener) {
        assert_eq!(listener.accept().unwrap_err().kind(), std::io::ErrorKind::WouldBlock);
    }

    #[tokio::test]
    async fn unsupported_preparation_leaves_source_capsule_and_guest_untouched() {
        let (runtime, listener) = runtime();
        let private = tempfile::tempdir().unwrap();
        let root = private.path().join("native-handoff-recovery");
        let environment = environment();
        let cancellation = CancellationToken::new();
        let result = tokio::time::timeout(Duration::from_secs(1), runtime.recover_native_handoff(
            "crew-handoff:original:isolated-recovery", &environment,
            r"C:\private\fork.jsonl", r"C:\source", false, &root, &cancellation,
        )).await.expect("unsupported preparation must return without guest I/O");
        let ScaffoldError::UnsupportedPlatform(_) = result.unwrap_err() else { panic!("explicit unsupported-platform error required") };
        assert!(!root.exists());
        no_requests(&listener);
    }

    #[test]
    fn absent_cleanup_succeeds_but_existing_capsule_is_preserved() {
        let (runtime, listener) = runtime();
        let private = tempfile::tempdir().unwrap();
        let root = private.path().join("native-handoff-recovery");
        let operation = "crew-handoff:completed:isolated-recovery";
        runtime.cleanup_native_handoff_recovery(operation, &root).unwrap();
        assert!(!root.exists());
        std::fs::create_dir(&root).unwrap();
        let sibling = root.join(namespace("crew-handoff:sibling:isolated-recovery"));
        std::fs::create_dir(&sibling).unwrap();
        std::fs::write(sibling.join("sentinel"), b"keep sibling").unwrap();
        runtime.cleanup_native_handoff_recovery(operation, &root).unwrap();
        let owned = root.join(namespace(operation));
        std::fs::create_dir(&owned).unwrap();
        std::fs::write(owned.join("native.tar"), b"keep exact capsule").unwrap();
        let error = runtime.cleanup_native_handoff_recovery(operation, &root).unwrap_err();
        assert!(matches!(error, ScaffoldError::UnsupportedPlatform(_)));
        assert_eq!(std::fs::read(owned.join("native.tar")).unwrap(), b"keep exact capsule");
        std::fs::remove_file(owned.join("native.tar")).unwrap();
        std::fs::remove_dir(&owned).unwrap();
        std::fs::write(&owned, b"keep existing namespace entry").unwrap();
        assert!(matches!(runtime.cleanup_native_handoff_recovery(operation, &root), Err(ScaffoldError::UnsupportedPlatform(_))));
        assert_eq!(std::fs::read(&owned).unwrap(), b"keep existing namespace entry");
        assert_eq!(std::fs::read(sibling.join("sentinel")).unwrap(), b"keep sibling");
        no_requests(&listener);
    }

    #[tokio::test]
    async fn unsupported_controller_rejects_invalid_operation_before_side_effects() {
        let (runtime, listener) = runtime();
        let private = tempfile::tempdir().unwrap();
        let root = private.path().join("native-handoff-recovery");
        let environment = environment();
        let cancellation = CancellationToken::new();
        for operation in ["other:original:isolated-recovery".to_owned(), "crew-handoff:original:transfer".to_owned(), format!("crew-handoff:{}:isolated-recovery", "x".repeat(4096))] {
            assert!(matches!(runtime.recover_native_handoff(&operation, &environment, "source", "cwd", true, &root, &cancellation).await, Err(ScaffoldError::InvalidResponse(_))));
            assert!(matches!(runtime.cleanup_native_handoff_recovery(&operation, &root), Err(ScaffoldError::InvalidResponse(_))));
        }
        assert!(!root.exists());
        no_requests(&listener);
    }
}

