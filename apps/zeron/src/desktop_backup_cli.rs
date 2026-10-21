//! Offline bulk source preparation, not authentication or activation. A trusted
//! operator captures every frozen room after verifying the context identity.
use anyhow::bail;
use std::path::PathBuf;
use zeron_sync::crew_desktop_backup::DesktopSourceContext;

#[derive(Debug, clap::Args)]
pub struct Args {
    /// Credential-free, freshly verified exact source/target identity context.
    #[arg(long)]
    context_file: PathBuf,
    #[arg(long)]
    source_profile_root: PathBuf,
    #[arg(long)]
    source_backup_root: PathBuf,
    #[arg(long)]
    database_relative: PathBuf,
    /// Frozen workspace.json and sessions/<UUID>.json; every source room is required.
    #[arg(long)]
    source_storage_root: PathBuf,
    #[arg(long)]
    destination: PathBuf,
    #[arg(long = "native-history-root")]
    native_history_roots: Vec<PathBuf>,
    #[arg(long)]
    attachment_root: Option<PathBuf>,
    /// Coordinator-authenticated original-epoch responses in sessions/<UUID>.json.
    #[arg(long)]
    terminal_authority_root: Option<PathBuf>,
    #[arg(long)]
    missing_attachments: bool,
    #[arg(long, required = true)]
    writers_stopped: bool,
}

pub fn run(args: Args) -> anyhow::Result<()> {
    if !args.writers_stopped {
        bail!("all desktop and remote source writers must be fenced before preparation");
    }
    let context: DesktopSourceContext =
        serde_json::from_slice(&std::fs::read(&args.context_file)?)?;
    let principal = zeron_proto::CollaborationPrincipal {
        subject: context.owner_subject.clone(),
        email: None,
        project_id: context.project_id.clone(),
        deployment_id: Some(context.deployment_id.clone()),
        session_id: None,
        capabilities: Vec::new(),
        unknown: Default::default(),
    };
    context.validate(&principal)?;
    let output = if args.missing_attachments {
        serde_json::to_value(zeron_sync::crew_backup::inventory_desktop_crew_backup(
            &args.source_profile_root,
            &args.source_backup_root,
            &args.database_relative,
            &args.destination,
            &args.native_history_roots,
            &principal,
            &context,
            &args.source_storage_root,
            args.attachment_root.as_deref(),
            args.terminal_authority_root.as_deref(),
        )?)?
    } else {
        serde_json::to_value(zeron_sync::crew_backup::export_desktop_crew_backup(
            &args.source_profile_root,
            &args.source_backup_root,
            &args.database_relative,
            &args.destination,
            &args.native_history_roots,
            &principal,
            &context,
            &args.source_storage_root,
            args.attachment_root.as_deref(),
            args.terminal_authority_root.as_deref(),
        )?)?
    };
    println!("{}", serde_json::to_string(&output)?);
    Ok(())
}
