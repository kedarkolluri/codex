#!/usr/bin/env python3

import hashlib
import importlib.util
from pathlib import Path
import tempfile
import unittest


MODULE_PATH = Path(__file__).with_name("fixture_manifest.py")
SPEC = importlib.util.spec_from_file_location("fixture_manifest", MODULE_PATH)
assert SPEC is not None and SPEC.loader is not None
fixture_manifest = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(fixture_manifest)


class FixtureManifestTests(unittest.TestCase):
    def test_path_coverage_is_repo_relative_sorted_and_includes_hidden_fixtures(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            uat_root = root / ".github/dynamic-workflows-uat"
            fixture_root = uat_root / "fixtures"
            files = {
                uat_root / "codex-control.config.toml.in": b"config\n",
                uat_root / "mock_responses_server.py": b"mock\n",
                uat_root / "test_mock_responses_server.py": b"test\n",
                uat_root / "test_transcript_verifier.py": b"verifier test\n",
                uat_root / "transcript_verifier.py": b"verifier\n",
                uat_root / "fixture_manifest.py": b"manifest\n",
                fixture_root / "plain.txt": b"plain\n",
                fixture_root / ".hidden/workflow.js": b"hidden\n",
            }
            for path, content in files.items():
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_bytes(content)
            (uat_root / "fixture-manifest.json").write_text("not an input\n")

            manifest = fixture_manifest.build_manifest(
                root,
                uat_root / "fixture_manifest.py",
            )

            self.assertEqual(
                list(manifest["files"]),
                sorted(path.relative_to(root).as_posix() for path in files),
            )
            hidden_path = ".github/dynamic-workflows-uat/fixtures/.hidden/workflow.js"
            self.assertEqual(
                manifest["files"][hidden_path],
                {"sha256": hashlib.sha256(b"hidden\n").hexdigest()},
            )
            self.assertNotIn(
                ".github/dynamic-workflows-uat/fixture-manifest.json",
                manifest["files"],
            )

    def test_drift_reports_added_removed_modified_and_metadata_in_sorted_order(self) -> None:
        expected = {
            "schema_version": 1,
            "geometry": {"columns": 80, "rows": 24},
            "files": {
                "removed": {"sha256": "old"},
                "modified": {"sha256": "before"},
            },
        }
        actual = {
            "schema_version": 1,
            "geometry": {"columns": 120, "rows": 36},
            "files": {
                "added": {"sha256": "new"},
                "modified": {"sha256": "after"},
            },
        }

        self.assertEqual(
            fixture_manifest.manifest_drift(expected, actual),
            [
                'added: added ({"sha256":"new"})',
                'removed: removed ({"sha256":"old"})',
                'modified: modified (recorded {"sha256":"before"}, generated {"sha256":"after"})',
                'metadata changed: geometry (recorded {"columns":80,"rows":24}, generated {"columns":120,"rows":36})',
            ],
        )


if __name__ == "__main__":
    unittest.main()
