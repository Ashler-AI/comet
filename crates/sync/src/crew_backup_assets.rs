//! One source-derived attachment plan for inventory and export. Missing paths
//! never become guessed hashes; fetched objects must match a persisted digest.
use crate::crew_import::CrewImportError;
use serde::Serialize;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};

fn unsafe_data(message: impl Into<String>) -> CrewImportError {
    CrewImportError::Unsafe(message.into())
}
fn digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn referenced_digest(value: &str) -> Option<&str> {
    let hash = value
        .strip_prefix("blob:sha256:")
        .or_else(|| {
            value
                .strip_prefix("data:")?
                .split_once(";base64,blob:sha256:")
                .map(|(_, hash)| hash)
        })
        .or_else(|| value.strip_prefix("/attachments/"))?;
    digest(hash).then_some(hash)
}
fn relative(value: &Path) -> Result<String, CrewImportError> {
    if value.as_os_str().is_empty()
        || !value
            .components()
            .all(|part| matches!(part, Component::Normal(_)))
        || !value
            .components()
            .any(|part| matches!(part.as_os_str().to_str(), Some("uploads" | "attachments")))
    {
        return Err(unsafe_data(
            "attachment alias is not an exact profile-relative asset",
        ));
    }
    value
        .to_str()
        .map(str::to_owned)
        .ok_or_else(|| unsafe_data("non-UTF8 attachment alias"))
}

#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AttachmentInventory {
    pub missing_attachments: BTreeSet<String>,
    pub missing_mappings: Vec<MissingAsset>,
    pub unresolved_references: BTreeSet<String>,
    pub native_missing_blobs: Vec<MissingAsset>,
}
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MissingAsset {
    pub sha256: String,
    pub relative_path: String,
    pub reference: String,
}
impl AttachmentInventory {
    pub(crate) fn complete(&self) -> bool {
        self.missing_attachments.is_empty() && self.unresolved_references.is_empty()
    }
}

#[derive(Default)]
struct Requirement {
    relative: Option<String>,
    sha256: Option<String>,
}
pub(crate) struct RequiredAssets {
    original: PathBuf,
    references: BTreeMap<String, Requirement>,
}
impl RequiredAssets {
    pub(crate) fn new(original: &Path) -> Self {
        Self {
            original: original.into(),
            references: BTreeMap::new(),
        }
    }
    fn add(
        &mut self,
        reference: &str,
        path: Option<&str>,
        sha: Option<&str>,
    ) -> Result<(), CrewImportError> {
        if reference.is_empty() {
            return Err(unsafe_data("empty persisted attachment reference"));
        }
        if reference.starts_with("data:") && !reference.contains("blob:sha256:") {
            return Ok(());
        }
        let mut hash = referenced_digest(reference).map(str::to_owned);
        if let Some(sha) = sha {
            if !digest(sha) || hash.as_deref().is_some_and(|known| known != sha) {
                return Err(unsafe_data("conflicting persisted attachment digest"));
            }
            hash = Some(sha.into());
        }
        let local = path.unwrap_or(reference);
        let output = Path::new(local)
            .strip_prefix(&self.original)
            .ok()
            .map(relative)
            .transpose()?;
        let entry = self.references.entry(reference.into()).or_default();
        if entry
            .relative
            .as_ref()
            .zip(output.as_ref())
            .is_some_and(|(a, b)| a != b)
            || entry
                .sha256
                .as_ref()
                .zip(hash.as_ref())
                .is_some_and(|(a, b)| a != b)
        {
            return Err(unsafe_data("conflicting attachment alias binding"));
        }
        if output.is_some() {
            entry.relative = output;
        }
        if hash.is_some() {
            entry.sha256 = hash;
        }
        Ok(())
    }
    pub(crate) fn collect(&mut self, value: &Value) -> Result<(), CrewImportError> {
        self.collect_inner(value, true)
    }
    pub(crate) fn collect_native(&mut self, value: &Value) -> Result<(), CrewImportError> {
        self.collect_inner(value, false)
    }
    fn collect_inner(
        &mut self,
        value: &Value,
        include_native_blobs: bool,
    ) -> Result<(), CrewImportError> {
        match value {
            Value::String(text) => {
                if let Some((_, refs)) = zeron_proto::attachment_trailer(text) {
                    for path in refs
                        .lines()
                        .filter_map(|line| line.trim().strip_prefix("- ").map(str::trim))
                        .filter(|path| !path.is_empty())
                    {
                        self.add(path, None, None)?;
                    }
                }
                if include_native_blobs && referenced_digest(text).is_some() {
                    self.add(text, None, None)?;
                }
            }
            Value::Array(values) => {
                for value in values {
                    self.collect_inner(value, include_native_blobs)?;
                }
            }
            Value::Object(fields) => {
                if fields.get("kind").and_then(Value::as_str) == Some("attachment") {
                    let record: zeron_proto::PublicationRecord =
                        serde_json::from_value(value.clone())?;
                    let zeron_proto::PublicationValue::Attachment(attachment) = record.value else {
                        return Err(unsafe_data("invalid attachment publication"));
                    };
                    let path = attachment.metadata.get("localPath").map(String::as_str);
                    if path.is_none() || Path::new(&attachment.blob_key).is_absolute() {
                        self.add(&attachment.blob_key, path, attachment.sha256.as_deref())?;
                    }
                    if let Some(path) = path {
                        self.add(
                            path,
                            Some(path),
                            attachment
                                .sha256
                                .as_deref()
                                .or_else(|| referenced_digest(&attachment.blob_key)),
                        )?;
                    }
                }
                if fields
                    .get("kind")
                    .and_then(Value::as_str)
                    .is_some_and(|kind| matches!(kind, "image" | "visual"))
                {
                    if let Some(path) = fields.get("path").and_then(Value::as_str) {
                        self.add(path, None, None)?;
                    }
                }
                if let Some(paths) = fields.get("attachments").and_then(Value::as_array) {
                    for path in paths.iter().filter_map(Value::as_str) {
                        self.add(path, None, None)?;
                    }
                }
                for value in fields.values() {
                    self.collect_inner(value, include_native_blobs)?;
                }
            }
            _ => {}
        }
        Ok(())
    }
    pub(crate) fn resolve(
        &self,
        source: &Path,
        fetched: Option<&Path>,
        plan: &mut BTreeMap<String, PathBuf>,
        aliases: &mut BTreeMap<String, String>,
        report: &mut AttachmentInventory,
    ) -> Result<(), CrewImportError> {
        for (reference, requirement) in &self.references {
            if !Path::new(reference).is_absolute() {
                report.unresolved_references.insert(reference.clone());
                if let Some(hash) = &requirement.sha256 {
                    report.missing_attachments.insert(hash.clone());
                }
                continue;
            }
            let copied = requirement.relative.as_ref().map(|path| source.join(path));
            if let Some(path) = copied.as_ref().filter(|path| path.exists()) {
                let hash = crate::crew_backup::hash(path)?;
                if requirement
                    .sha256
                    .as_ref()
                    .is_some_and(|expected| expected != &hash)
                {
                    return Err(unsafe_data(format!(
                        "copied attachment digest mismatch: {reference}"
                    )));
                }
                let output = requirement.relative.as_ref().unwrap();
                crate::crew_backup::plan_file(plan, Path::new(output), path.clone())?;
                aliases.insert(reference.clone(), output.clone());
            } else if let Some(hash) = &requirement.sha256 {
                let output = requirement
                    .relative
                    .clone()
                    .unwrap_or_else(|| format!("attachments/{hash}"));
                if let Some(path) = fetched_file(fetched, hash)? {
                    crate::crew_backup::plan_file(plan, Path::new(&output), path)?;
                    aliases.insert(reference.clone(), output);
                } else {
                    report.missing_attachments.insert(hash.clone());
                    report.missing_mappings.push(MissingAsset {
                        sha256: hash.clone(),
                        relative_path: output,
                        reference: reference.clone(),
                    });
                }
            } else {
                report.unresolved_references.insert(reference.clone());
            }
        }
        Ok(())
    }
}

pub(crate) fn fetched_file(
    root: Option<&Path>,
    hash: &str,
) -> Result<Option<PathBuf>, CrewImportError> {
    let Some(root) = root else {
        return Ok(None);
    };
    let path = root.join(hash);
    match std::fs::symlink_metadata(&path) {
        Ok(_) => {
            if crate::crew_backup::hash(&path)? != hash {
                return Err(unsafe_data(format!(
                    "fetched attachment digest mismatch: {hash}"
                )));
            }
            Ok(Some(path))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn legacy_upload_name_never_becomes_a_guessed_content_hash() {
        let source = tempfile::tempdir().unwrap();
        let original = Path::new("/original-profile");
        let reference = "/original-profile/uploads/abcd1234-image.png";
        let mut required = RequiredAssets::new(original);
        required
            .collect(&Value::String(format!(
                "Inspect.\n\nAttached images (local files — open them to view):\n- {reference}"
            )))
            .unwrap();
        let mut plan = BTreeMap::new();
        let mut aliases = BTreeMap::new();
        let mut report = AttachmentInventory::default();
        required
            .resolve(source.path(), None, &mut plan, &mut aliases, &mut report)
            .unwrap();
        assert_eq!(
            report.unresolved_references,
            BTreeSet::from([reference.to_owned()])
        );
        assert!(report.missing_attachments.is_empty());
        assert!(plan.is_empty() && aliases.is_empty());
    }
}
