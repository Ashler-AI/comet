#!/usr/bin/env python3
"""Inspect the exact exported IPA before any staging upload; never changes Apple state."""
import hashlib
import json
from pathlib import Path
import plistlib
import stat
import subprocess
import sys
import tempfile
import zipfile

BUNDLE = "ai.ashler.crew.staging"
TEAM = "825LYXGJR6"


def plist_output(*args):
    return plistlib.loads(subprocess.check_output(args, stderr=subprocess.DEVNULL))


def verify(app, bundle, build, push=False):
    info = plistlib.loads((app / "Info.plist").read_bytes())
    if (info["CFBundleIdentifier"], str(info["CFBundleVersion"]), info["CFBundleShortVersionString"]) != (bundle, build, "1.0"):
        raise ValueError("Unexpected bundle/version/build in " + app.name)
    subprocess.run(["codesign", "--verify", "--deep", "--strict", str(app)], check=True)
    signed = plist_output("codesign", "-d", "--entitlements", ":-", "--xml", str(app))
    profile = plist_output("security", "cms", "-D", "-i", str(app / "embedded.mobileprovision"))
    provisioned = profile["Entitlements"]
    if profile["TeamIdentifier"] != [TEAM] or profile.get("ProvisionedDevices") or profile.get("ProvisionsAllDevices"):
        raise ValueError("Expected existing-team App Store distribution profile")
    identifier = TEAM + "." + bundle
    profile_identifier = provisioned.get("application-identifier")
    # Apple's profile entitlement is an allowlist; the signature is concrete.
    # Only the capability-free extension may use an existing wildcard profile.
    matches = profile_identifier == identifier or (
        not push and bundle == BUNDLE + ".LiveActivity"
        and isinstance(profile_identifier, str)
        and profile_identifier.startswith(TEAM + ".")
        and profile_identifier.endswith(".*") and profile_identifier.count("*") == 1
        and identifier.startswith(profile_identifier[:-1])
    )
    if signed.get("application-identifier") != identifier or not matches:
        raise ValueError("Unexpected distribution signing identity")
    for entitlements in (signed, provisioned):
        if entitlements.get("get-task-allow", False):
            raise ValueError("Development entitlement in distribution signature/profile")
        if entitlements.get("com.apple.developer.team-identifier") != TEAM:
            raise ValueError("Unexpected team entitlement")
        if push and entitlements.get("aps-environment") != "production":
            raise ValueError("Missing production APNs entitlement")
    return {"bundleId": bundle, "buildNumber": build, "teamId": TEAM,
            "profileUUID": profile["UUID"], "profileApplicationIdentifier": profile_identifier,
            "apsEnvironment": signed.get("aps-environment")}


def main():
    ipa, build = Path(sys.argv[1]), sys.argv[2]
    with tempfile.TemporaryDirectory() as temporary:
        root = Path(temporary)
        with zipfile.ZipFile(ipa) as archive:
            for member in archive.infolist():
                if Path(member.filename).is_absolute() or ".." in Path(member.filename).parts:
                    raise ValueError("Unsafe IPA member")
                if stat.S_ISLNK(member.external_attr >> 16):
                    target = Path(archive.read(member).decode("utf-8"))
                    if target.is_absolute() or ".." in target.parts:
                        raise ValueError("Unsafe IPA symbolic link")
        # ditto preserves signed bundle modes and framework symlinks.
        subprocess.run(["ditto", "-x", "-k", str(ipa), str(root)], check=True)
        apps = list((root / "Payload").glob("*.app"))
        if len(apps) != 1:
            raise ValueError("Expected exactly one staging app")
        app = apps[0]
        receipt = verify(app, BUNDLE, build, push=True)
        extensions = list((app / "PlugIns").glob("*.appex"))
        if len(extensions) != 1:
            raise ValueError("Expected exactly the upstream LiveActivity extension")
        receipt["extension"] = verify(extensions[0], BUNDLE + ".LiveActivity", build)
        with ipa.open("rb") as source:
            receipt["sha256"] = hashlib.file_digest(source, "sha256").hexdigest()
        print(json.dumps(receipt, indent=2))


if __name__ == "__main__":
    main()
