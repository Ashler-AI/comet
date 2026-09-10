//! Distribution trust is pinned at build time, never taken from the update feed.

use std::path::Path;
use std::process::{Command, Output};

use anyhow::{Context as _, bail};

const BUNDLE_ID: &str = "ai.ashler.comet";

pub(super) fn expected_team() -> anyhow::Result<&'static str> {
    validate_team(option_env!("COMET_MACOS_SIGNING_TEAM_ID"))
}

fn validate_team(team: Option<&str>) -> anyhow::Result<&str> {
    team.filter(|team| {
        team.len() == 10
            && team
                .bytes()
                .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit())
    })
    .context("Crew macOS updates require a build-time COMET_MACOS_SIGNING_TEAM_ID pin (10 uppercase letters/digits)")
}

pub(super) struct Policy {
    requirement: String,
    compiled_requirement: Vec<u8>,
}

impl Policy {
    pub(super) fn pinned() -> anyhow::Result<Self> {
        Self::for_team(expected_team()?)
    }

    fn for_team(team: &str) -> anyhow::Result<Self> {
        let team = validate_team(Some(team))?;
        let requirement = format!(
            "identifier \"{BUNDLE_ID}\" and anchor apple generic and certificate 1[field.1.2.840.113635.100.6.2.6] exists and certificate leaf[field.1.2.840.113635.100.6.1.13] exists and certificate leaf[subject.OU] = \"{team}\""
        );
        let compiled_requirement = compile_requirement(&requirement)?;
        Ok(Self {
            requirement,
            compiled_requirement,
        })
    }

    pub(super) fn verify_distribution(&self, bundle: &Path) -> anyhow::Result<()> {
        verify_bundle_shape(bundle)?;
        self.verify_identity(bundle)?;
        let display = checked(
            Command::new("/usr/bin/codesign")
                .args(["--display", "-r-"])
                .arg(bundle),
        )?;
        // codesign versions differ in which stream carries requirement text.
        let stdout = String::from_utf8_lossy(&display.stdout);
        let stderr = String::from_utf8_lossy(&display.stderr);
        let requirement = stdout
            .lines()
            .chain(stderr.lines())
            .find_map(|line| line.trim().strip_prefix("designated => "))
            .context("Crew bundle has no designated requirement")?;
        if compile_requirement(requirement)? != self.compiled_requirement {
            bail!("Crew bundle does not use the stable Developer ID designated requirement");
        }
        // Gatekeeper is part of macOS. stapler/xcrun belong only in release CI:
        // customers must not need Xcode or Command Line Tools to update Crew.
        checked(
            Command::new("/usr/sbin/spctl")
                .args([
                    "--assess",
                    "--type",
                    "execute",
                    "--ignore-cache",
                    "--no-cache",
                ])
                .arg(bundle),
        )?;
        Ok(())
    }

    fn verify_identity(&self, bundle: &Path) -> anyhow::Result<()> {
        verify_signature(bundle, &self.requirement)
    }

    pub(super) fn verify_installed(&self, bundle: &Path) -> anyhow::Result<()> {
        verify_bundle_shape(bundle)?;
        let identity_error = match self.verify_identity(bundle) {
            Ok(()) => return Ok(()),
            Err(error) => error,
        };
        // Never turn a bad Developer ID signature or a different team into a
        // migration exception. Only an intact, explicitly ad-hoc same-ID bundle
        // may be migrated, and only by an already trusted running distribution.
        let display = checked(
            Command::new("/usr/bin/codesign")
                .args(["--display", "--verbose=4"])
                .arg(bundle),
        )?;
        if !String::from_utf8_lossy(&display.stderr)
            .lines()
            .any(|line| line == "Signature=adhoc")
        {
            return Err(identity_error).context("installed Crew bundle has an untrusted identity");
        }
        verify_signature(bundle, &format!("identifier \"{BUNDLE_ID}\""))?;
        let executable =
            std::env::current_exe().context("locating the running Crew distribution")?;
        let super::InstallKind::MacApp { bundle: running } =
            super::detect_install_from(&executable, None)
        else {
            bail!("legacy ad-hoc Crew migration requires a running signed Crew distribution");
        };
        self.verify_distribution(&running)
            .context("legacy ad-hoc Crew migration requires a running trusted Crew distribution")
    }
}

fn verify_bundle_shape(bundle: &Path) -> anyhow::Result<()> {
    if !std::fs::symlink_metadata(bundle)?.file_type().is_dir()
        || !std::fs::symlink_metadata(bundle.join("Contents/Info.plist"))?
            .file_type()
            .is_file()
        || !std::fs::symlink_metadata(bundle.join("Contents/MacOS/comet"))?
            .file_type()
            .is_file()
    {
        bail!("{} is not a Crew app bundle directory", bundle.display());
    }
    let identity = checked(
        Command::new("/usr/libexec/PlistBuddy")
            .args([
                "-c",
                "Print :CFBundleIdentifier",
                "-c",
                "Print :CFBundleExecutable",
            ])
            .arg(bundle.join("Contents/Info.plist")),
    )?;
    // Bundle-root codesign verification must authenticate the executable that
    // Crew actually launches, not a different plist-selected main executable.
    if !String::from_utf8_lossy(&identity.stdout)
        .lines()
        .eq([BUNDLE_ID, "comet"])
    {
        bail!(
            "Crew update bundle must have CFBundleIdentifier {BUNDLE_ID} and CFBundleExecutable comet"
        );
    }
    Ok(())
}

fn verify_signature(bundle: &Path, requirement: &str) -> anyhow::Result<()> {
    checked(
        Command::new("/usr/bin/codesign")
            .args(["--verify", "--deep", "--strict", "--test-requirement"])
            .arg(format!("={requirement}"))
            .arg(bundle),
    )?;
    Ok(())
}

fn compile_requirement(requirement: &str) -> anyhow::Result<Vec<u8>> {
    Ok(checked(
        Command::new("/usr/bin/csreq")
            .arg("-r")
            .arg(format!("={requirement}"))
            .args(["-b", "/dev/stdout"]),
    )?
    .stdout)
}

fn checked(command: &mut Command) -> anyhow::Result<Output> {
    let output = command
        .output()
        .with_context(|| format!("running {command:?}"))?;
    if !output.status.success() {
        bail!(
            "{command:?} failed ({}): {}{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_or_malformed_team_pin_fails_closed() {
        for team in [
            None,
            Some(""),
            Some("abcdefghij"),
            Some("ABCDE1234"),
            Some("ABCDE1234\""),
        ] {
            assert!(validate_team(team).is_err());
        }
        assert_eq!(validate_team(Some("ABCDE12345")).unwrap(), "ABCDE12345");
    }

    #[test]
    fn intact_adhoc_bundle_cannot_be_a_distribution_or_authorize_migration() {
        let tmp = tempfile::tempdir().unwrap();
        let bundle = tmp.path().join("Crew.app");
        let binary = bundle.join("Contents/MacOS/comet");
        std::fs::create_dir_all(binary.parent().unwrap()).unwrap();
        std::fs::copy("/usr/bin/true", &binary).unwrap();
        std::fs::write(
            bundle.join("Contents/Info.plist"),
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?><plist version=\"1.0\"><dict><key>CFBundleIdentifier</key><string>ai.ashler.comet</string><key>CFBundleExecutable</key><string>comet</string><key>CFBundlePackageType</key><string>APPL</string></dict></plist>",
        )
        .unwrap();
        let resource = bundle.join("Contents/Resources/sealed.txt");
        std::fs::create_dir_all(resource.parent().unwrap()).unwrap();
        std::fs::write(&resource, b"original").unwrap();
        checked(
            Command::new("/usr/bin/codesign")
                .args(["--force", "--sign", "-", "--identifier", BUNDLE_ID])
                .arg(&bundle),
        )
        .unwrap();
        verify_signature(&bundle, "identifier \"ai.ashler.comet\"").unwrap();
        let policy = Policy::for_team("ABCDE12345").unwrap();
        assert!(policy.verify_distribution(&bundle).is_err());
        assert!(policy.verify_installed(&bundle).is_err());
        // An intact legacy ad-hoc app is not enough to authorize migration;
        // tampering must also fail integrity verification, even before trust.
        std::fs::write(&resource, b"tampered").unwrap();
        assert!(verify_signature(&bundle, "identifier \"ai.ashler.comet\"").is_err());
        assert!(policy.verify_installed(&bundle).is_err());
        assert_eq!(std::fs::read(&resource).unwrap(), b"tampered");
    }
}
