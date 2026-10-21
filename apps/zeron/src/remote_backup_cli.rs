//! Provider-local offline tools; never start the legacy or destination daemon.
//! The provider authenticates and fences before invoking these commands. JSON
//! context binds files; it must never be accepted by a network API as proof.
use anyhow::{Context, bail};
use std::path::PathBuf;
use zeron_sync::crew_remote_backup::{RemoteBackupContext, export_remote_crew_backup};

#[derive(Debug, clap::Args)]
pub struct Args {
    #[arg(long)]
    context_file: PathBuf,
    #[arg(long)]
    source_profile_root: PathBuf,
    #[arg(long)]
    source_backup_root: PathBuf,
    #[arg(long)]
    database_relative: PathBuf,
    #[arg(long)]
    destination: PathBuf,
    /// Exact frozen legacy-session-sql-v1 envelope obtained by the authenticated coordinator.
    /// This file is evidence, never authentication or proof of the server writer barrier.
    #[arg(long)]
    authoritative_storage: PathBuf,
    /// Independently copied agent directories containing sessions/ and blobs/.
    #[arg(long = "native-history-root")]
    native_history_roots: Vec<PathBuf>,
    /// Independently staged, hash-named fetched objects; never the live profile.
    #[arg(long)]
    attachment_root: Option<PathBuf>,
    /// Print unresolved attachment acquisition work without publishing a backup.
    #[arg(long)]
    missing_attachments: bool,
    #[arg(long, required = true)]
    writers_stopped: bool,
}

#[derive(Debug, clap::Args)]
pub struct ImportArgs {
    #[arg(long)]
    context_file: PathBuf,
    #[arg(long)]
    backup: PathBuf,
    /// Native COMET_DATA_DIR; the shared runtime key selects its exact profile.
    #[arg(long)]
    destination_root: PathBuf,
    #[arg(long)]
    resource: String,
    #[arg(long)]
    edge_origin: String,
}

fn context(path: &PathBuf) -> anyhow::Result<RemoteBackupContext> {
    let context: RemoteBackupContext = serde_json::from_slice(
        &std::fs::read(path).context("read provider-selected remote context")?,
    )?;
    context.validate()?;
    Ok(context)
}

pub fn run(args: Args) -> anyhow::Result<()> {
    if !args.writers_stopped {
        bail!("provider must stop all source writers before export");
    }
    let context = context(&args.context_file)?;
    if args.missing_attachments {
        let inventory = zeron_sync::crew_remote_backup::inventory_remote_crew_backup(
            &args.source_profile_root,
            &args.source_backup_root,
            &args.database_relative,
            &args.destination,
            &args.native_history_roots,
            &args.authoritative_storage,
            args.attachment_root.as_deref(),
            &context,
        )?;
        println!("{}", serde_json::to_string(&inventory)?);
        return Ok(());
    }
    let manifest = export_remote_crew_backup(
        &args.source_profile_root,
        &args.source_backup_root,
        &args.database_relative,
        &args.destination,
        &args.native_history_roots,
        &args.authoritative_storage,
        args.attachment_root.as_deref(),
        &context,
    )?;
    println!("{}", serde_json::to_string(&manifest)?);
    Ok(())
}

fn destination(args: &ImportArgs, context: &RemoteBackupContext) -> anyhow::Result<PathBuf> {
    context.validate()?;
    if !args.destination_root.is_absolute() {
        bail!("absolute native data root required");
    }
    let resource = zeron_sync::scaffold_auth::normalized_origin(&args.resource)?;
    let edge_origin = zeron_sync::scaffold_auth::normalized_origin(&args.edge_origin)?;
    Ok(zeron_engine::profile::scaffold_profile_directory(
        &args.destination_root,
        &resource,
        &edge_origin,
        &context.owner_subject,
        &context.project_id,
        Some(&context.deployment_id),
        &context.deployment_id,
        &context.logical_session_id,
        &context.sandbox_id,
    ))
}

fn prepare_import_parent(
    destination: &std::path::Path,
    backup: &std::path::Path,
    original: &std::path::Path,
) -> anyhow::Result<()> {
    use std::path::Component;
    if [destination, original].iter().any(|path| {
        !path.is_absolute()
            || path
                .components()
                .any(|part| matches!(part, Component::ParentDir | Component::CurDir))
    }) {
        bail!("native destination and original alias must be absolute normalized paths");
    }
    let backup = backup.canonicalize()?;
    let original = match original.canonicalize() {
        Ok(path) => path,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => original.to_path_buf(),
        Err(error) => return Err(error.into()),
    };
    if destination.starts_with(&backup)
        || backup.starts_with(destination)
        || destination.starts_with(&original)
        || original.starts_with(destination)
    {
        bail!("native destination overlaps backup or original source profile");
    }
    let parent = destination
        .parent()
        .context("native destination parent missing")?;
    for ancestor in parent.ancestors() {
        match std::fs::symlink_metadata(ancestor) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
                bail!("native destination parent is not an independent directory")
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(parent)?;
    Ok(())
}

fn import_or_verify(args: ImportArgs, verify: bool) -> anyhow::Result<()> {
    let context = context(&args.context_file)?;
    let manifest = serde_json::from_slice(&std::fs::read(args.backup.join("crew-backup.json"))?)?;
    context.manifest(&manifest)?;
    let principal = context.principal()?;
    let destination = destination(&args, &context)?;
    if !verify {
        prepare_import_parent(
            &destination,
            &args.backup,
            std::path::Path::new(&manifest.source_profile_root),
        )?;
    }
    let operation = if verify {
        zeron_sync::crew_import::verify_legacy_crew_import
    } else {
        zeron_sync::crew_import::import_legacy_crew_backup
    };
    let report = operation(&args.backup, &destination, &principal, &context.device_id)?;
    let mut output = serde_json::to_value(report)?;
    output["destinationProfile"] = serde_json::to_value(destination)?;
    println!("{}", serde_json::to_string(&output)?);
    Ok(())
}

pub fn import(args: ImportArgs) -> anyhow::Result<()> {
    import_or_verify(args, false)
}
pub fn verify(args: ImportArgs) -> anyhow::Result<()> {
    import_or_verify(args, true)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn offline_destination_matches_runtime_origins_and_rejects_unsafe_authority() {
        let context = RemoteBackupContext {
            migration_id: "migration".into(),
            owner_subject: "owner".into(),
            project_id: "project".into(),
            deployment_id: "deployment".into(),
            logical_session_id: "11111111-1111-4111-8111-111111111111".into(),
            sandbox_id: "sandbox".into(),
            device_id: "device".into(),
            lifecycle_epoch: 7,
            source_generation: "session".into(),
            writers_stopped: true,
        };
        let mut args = ImportArgs {
            context_file: "/context".into(),
            backup: "/backup".into(),
            destination_root: "/native-data".into(),
            resource: "https://control.example/".into(),
            edge_origin: "https://edge.example/".into(),
        };
        let expected = zeron_engine::profile::scaffold_profile_directory(
            &args.destination_root,
            "https://control.example",
            "https://edge.example",
            "owner",
            "project",
            Some("deployment"),
            "deployment",
            &context.logical_session_id,
            "sandbox",
        );
        assert_eq!(destination(&args, &context).unwrap(), expected);
        args.resource = "https://control.example".into();
        args.edge_origin = "https://edge.example".into();
        assert_eq!(destination(&args, &context).unwrap(), expected);
        for invalid in [
            "",
            "http://remote.example",
            "https://user:password@control.example",
            "https://control.example/path",
            "https://control.example?query",
        ] {
            args.resource = invalid.into();
            assert!(destination(&args, &context).is_err());
        }
        args.resource = "https://control.example".into();
        args.edge_origin = "https://edge.example/path".into();
        assert!(destination(&args, &context).is_err());
        args.edge_origin = "https://edge.example".into();
        let mut unbound = context;
        unbound.owner_subject.clear();
        assert!(destination(&args, &unbound).is_err());
    }
    #[test]
    fn import_parent_is_private_and_never_creates_the_final_profile() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();
        let backup = root.join("backup");
        std::fs::create_dir(&backup).unwrap();
        let destination = root.join("native/profiles/scaffold/exact-profile");
        prepare_import_parent(&destination, &backup, &root.join("original")).unwrap();
        assert!(destination.parent().unwrap().is_dir());
        assert!(!destination.exists());
        assert!(
            prepare_import_parent(&backup.join("overlap"), &backup, &root.join("original"))
                .is_err()
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::{PermissionsExt, symlink};
            assert_eq!(
                std::fs::metadata(destination.parent().unwrap())
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
            symlink(&root, root.join("linked")).unwrap();
            assert!(
                prepare_import_parent(
                    &root.join("linked/profile"),
                    &backup,
                    &root.join("original")
                )
                .is_err()
            );
        }
    }
}
