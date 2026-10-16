//! Explicit, copy-only migration of a user-selected legacy iOS CometDocs backup.
//! Old snapshots/intents are evidence, never fed into upstream's live writer.
use loro::ToJson;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;
use serde::Serialize;
use sha2::{Digest, Sha256};
use crate::{ClientError, error::Result};

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LegacyImportReport {
    pub schema_version: u32,
    pub directory: String,
    pub files: Vec<LegacyFile>,
    pub pending_files: usize,
    pub requires_reconciliation: bool,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LegacyFile {
    pub name: String,
    pub bytes: u64,
    pub sha256: String,
    pub disposition: String,
}
fn storage(error: impl std::fmt::Display) -> ClientError { ClientError::Storage(error.to_string()) }
fn save(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path).map_err(storage)?;
    file.write_all(bytes).map_err(storage)?;
    file.sync_all().map_err(storage)
}

pub fn preserve_backup(source: &Path, profile: &Path) -> Result<LegacyImportReport> {
    if fs::symlink_metadata(source).map_err(storage)?.file_type().is_symlink() || !source.is_dir() {
        return Err(ClientError::InvalidArgument("Select a copied CometDocs folder, not a symlink".into()));
    }
    // Inventory first: never partially accept unknown nested directories or unbounded input.
    let mut entries = Vec::new();
    let mut total = 0u64;
    for entry in fs::read_dir(source).map_err(storage)? {
        let entry = entry.map_err(storage)?;
        let kind = entry.file_type().map_err(storage)?;
        let size = entry.metadata().map_err(storage)?.len();
        if !kind.is_file() || size > 64 * 1024 * 1024 {
            return Err(ClientError::InvalidArgument("Legacy backup must contain regular files of at most 64 MiB".into()));
        }
        total = total.checked_add(size).ok_or_else(|| storage("Backup size overflow"))?;
        if total > 512 * 1024 * 1024 || entries.len() >= 4096 {
            return Err(ClientError::InvalidArgument("Legacy backup exceeds 512 MiB or 4096 files; split a copy before importing".into()));
        }
        entries.push(entry.path());
    }
    if entries.is_empty() { return Err(ClientError::InvalidArgument("Legacy backup is empty".into())); }
    let destination = profile.join("legacy-imports").join(uuid::Uuid::new_v4().to_string());
    fs::create_dir_all(destination.join("originals")).map_err(storage)?;
    fs::create_dir(destination.join("read-only-json")).map_err(storage)?;
    let mut report = LegacyImportReport { schema_version: 1, directory: destination.to_string_lossy().into_owned(),
        files: Vec::new(), pending_files: 0, requires_reconciliation: true };
    for path in entries {
        let name = path.file_name().and_then(|s| s.to_str()).ok_or_else(|| storage("Invalid backup filename"))?;
        let bytes = fs::read(&path).map_err(storage)?;
        let sha256 = format!("{:x}", Sha256::digest(&bytes));
        save(&destination.join("originals").join(name), &bytes)?;
        let mut disposition = "preserved-unknown-format".to_owned();
        if name.ends_with(".loro") || name.ends_with(".loro.recovery") {
            let doc = loro::LoroDoc::new();
            match doc.import(&bytes) {
                Ok(status) if status.pending.is_none() && doc.state_vv() == doc.oplog_vv() => {
                    let json = serde_json::to_vec_pretty(&doc.get_deep_value().to_json_value()).map_err(storage)?;
                    save(&destination.join("read-only-json").join(format!("{name}.json")), &json)?;
                    disposition = "preserved-cache-only-read-only".into();
                }
                _ => disposition = "preserved-unreadable-snapshot".into(),
            }
        } else if name.contains(".intents") || name.ends_with(".outcome") {
            report.pending_files += 1;
            disposition = "preserved-pending-never-replayed".into();
        }
        report.files.push(LegacyFile { name: name.into(), bytes: bytes.len() as u64, sha256, disposition });
    }
    save(&destination.join("manifest.json"), &serde_json::to_vec_pretty(&report).map_err(storage)?)?;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn migration_preserves_pending_without_replay_or_source_changes() {
        let source = tempfile::tempdir().unwrap();
        let target = tempfile::tempdir().unwrap();
        let pending = br#"[{"commandId":"unknown","prompt":"retain me"}]"#;
        fs::write(source.path().join("session.intents"), pending).unwrap();
        let report = preserve_backup(source.path(), target.path()).unwrap();
        assert_eq!(report.pending_files, 1);
        assert!(report.requires_reconciliation);
        assert_eq!(fs::read(source.path().join("session.intents")).unwrap(), pending);
        assert_eq!(fs::read(Path::new(&report.directory).join("originals/session.intents")).unwrap(), pending);
        assert!(!target.path().join("registry.db").exists());
    }
}
