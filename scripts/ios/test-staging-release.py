#!/usr/bin/env python3
"""Offline release contract checks; no Xcode, Apple requests or signing."""
import importlib.util
from pathlib import Path
import plistlib
import tempfile
import unittest
from unittest.mock import patch


def load(name):
    spec = importlib.util.spec_from_file_location(name, Path(__file__).with_name(name + ".py"))
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


metadata = load("staging-metadata")
ipa = load("verify-staging-ipa")


class ReleaseContracts(unittest.TestCase):
    def test_build_number_uses_all_metadata_not_source_reservation(self):
        self.assertEqual(metadata.next_build([], 37), 37)
        self.assertEqual(metadata.next_build([{"attributes": {"version": "42"}}, {"attributes": {"version": "36"}}], 37), 43)
        with self.assertRaises(ValueError):
            metadata.next_build([{"attributes": {"version": "1.2"}}], 37)

    def test_distribution_identity_is_staging_only(self):
        with tempfile.TemporaryDirectory() as directory:
            app = Path(directory)
            (app / "Info.plist").write_bytes(plistlib.dumps({"CFBundleIdentifier": ipa.BUNDLE,
                "CFBundleVersion": "42", "CFBundleShortVersionString": "1.0"}))
            signed = {"application-identifier": ipa.TEAM + "." + ipa.BUNDLE,
                      "com.apple.developer.team-identifier": ipa.TEAM, "aps-environment": "production"}
            profile = {"TeamIdentifier": [ipa.TEAM], "UUID": "test", "Entitlements": signed.copy()}
            with patch.object(ipa.subprocess, "run"), patch.object(ipa, "plist_output", side_effect=lambda *args: profile if args[0] == "security" else signed):
                self.assertEqual(ipa.verify(app, ipa.BUNDLE, "42", push=True)["teamId"], ipa.TEAM)
                signed["aps-environment"] = "development"
                with self.assertRaises(ValueError):
                    ipa.verify(app, ipa.BUNDLE, "42", push=True)
                signed["aps-environment"] = "production"
                profile["ProvisionedDevices"] = ["device"]
                with self.assertRaises(ValueError):
                    ipa.verify(app, ipa.BUNDLE, "42", push=True)
                with self.assertRaises(ValueError):
                    ipa.verify(app, "ai.ashler.crew", "42", push=True)

    def test_extension_accepts_authorizing_wildcard_not_wildcard_signature(self):
        with tempfile.TemporaryDirectory() as directory:
            app = Path(directory)
            bundle = ipa.BUNDLE + ".LiveActivity"
            (app / "Info.plist").write_bytes(plistlib.dumps({"CFBundleIdentifier": bundle,
                "CFBundleVersion": "37", "CFBundleShortVersionString": "1.0"}))
            signed = {"application-identifier": ipa.TEAM + "." + bundle,
                      "com.apple.developer.team-identifier": ipa.TEAM}
            profile = {"TeamIdentifier": [ipa.TEAM], "UUID": "existing-wildcard", "Entitlements": signed.copy()}
            with patch.object(ipa.subprocess, "run"), patch.object(ipa, "plist_output", side_effect=lambda *args: profile if args[0] == "security" else signed):
                for allowed in (ipa.TEAM + ".*", ipa.TEAM + "." + ipa.BUNDLE + ".*"):
                    profile["Entitlements"]["application-identifier"] = allowed
                    self.assertEqual(ipa.verify(app, bundle, "37")["profileApplicationIdentifier"], allowed)
                for denied in ("FOREIGN.*", ipa.TEAM + ".other.*", ipa.TEAM + ".*.LiveActivity"):
                    profile["Entitlements"]["application-identifier"] = denied
                    with self.assertRaises(ValueError):
                        ipa.verify(app, bundle, "37")
                profile["Entitlements"]["application-identifier"] = ipa.TEAM + ".*"
                signed["application-identifier"] = ipa.TEAM + ".*"
                with self.assertRaises(ValueError):
                    ipa.verify(app, bundle, "37")


if __name__ == "__main__":
    unittest.main()
