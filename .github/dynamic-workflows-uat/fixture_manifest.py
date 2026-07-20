#!/usr/bin/env python3

"""Print or verify the canonical Dynamic Workflows UAT fixture manifest."""

import argparse
import hashlib
import json
from pathlib import Path
import sys
from typing import Any


SCRIPT_PATH = Path(__file__).resolve()
REPO_ROOT = SCRIPT_PATH.parents[2]
MANIFEST_PATH = SCRIPT_PATH.with_name("fixture-manifest.json")

BASE_INPUT_PATHS = (
    ".github/dynamic-workflows-uat/codex-control.config.toml.in",
    ".github/dynamic-workflows-uat/mock_responses_server.py",
    ".github/dynamic-workflows-uat/test_mock_responses_server.py",
    ".github/dynamic-workflows-uat/test_transcript_verifier.py",
    ".github/dynamic-workflows-uat/transcript_verifier.py",
)

ROUTE_EXPECTATIONS = (
    {
        "lane": "baseline/monitor",
        "expected_request_count": 4,
        "routes": (
            {"route": "monitor-a", "phase": "tool", "count": 1},
            {"route": "monitor-a", "phase": "final", "count": 1},
            {"route": "monitor-b", "phase": "tool", "count": 1},
            {"route": "monitor-b", "phase": "final", "count": 1},
        ),
    },
    {
        "lane": "stop",
        "expected_request_count": 1,
        "routes": ({"route": "stop", "phase": "tool", "count": 1},),
    },
    {"lane": "save", "expected_request_count": 0, "routes": ()},
    {
        "lane": "pause/resume",
        "expected_request_count": 3,
        "routes": (
            {"route": "pause", "phase": "tool", "count": 2},
            {"route": "pause", "phase": "final", "count": 1},
        ),
    },
    {
        "lane": "skip",
        "expected_request_count": 3,
        "routes": (
            {"route": "agent-a", "phase": "tool", "count": 1},
            {"route": "agent-b", "phase": "tool", "count": 1},
            {"route": "agent-b", "phase": "final", "count": 1},
        ),
    },
    {
        "lane": "retry",
        "expected_request_count": 5,
        "routes": (
            {"route": "agent-a", "phase": "tool", "count": 2},
            {"route": "agent-a", "phase": "final", "count": 1},
            {"route": "agent-b", "phase": "tool", "count": 1},
            {"route": "agent-b", "phase": "final", "count": 1},
        ),
    },
    {
        "lane": "failure/null",
        "expected_request_count": 3,
        "routes": (
            {"route": "failure-null", "phase": "failed", "count": 1},
            {"route": "failure-sibling", "phase": "final", "count": 1},
            {"route": "failure-schema", "phase": "final", "count": 1},
        ),
    },
    {
        "lane": "budget-36",
        "expected_request_count": 2,
        "routes": (
            {"route": "budget-0", "phase": "final", "count": 1},
            {"route": "budget-1", "phase": "final", "count": 1},
        ),
        "forbidden_routes": ("budget-2", "budget-3"),
    },
    {
        "lane": "parallel barrier",
        "expected_request_count": 3,
        "routes": (
            {"route": "parallel-fast", "phase": "final", "count": 1},
            {"route": "parallel-held", "phase": "tool", "count": 1},
            {"route": "parallel-held", "phase": "final", "count": 1},
        ),
    },
    {
        "lane": "pipeline stagger",
        "expected_request_count": 4,
        "routes": (
            {"route": "pipeline-a0", "phase": "final", "count": 1},
            {"route": "pipeline-a1", "phase": "final", "count": 1},
            {"route": "pipeline-b0", "phase": "tool", "count": 1},
            {"route": "pipeline-b0", "phase": "final", "count": 1},
        ),
    },
    {"lane": "nesting", "expected_request_count": 0, "routes": ()},
    {
        "lane": "worktree",
        "expected_request_count": 2,
        "routes": (
            {"route": "worktree", "phase": "tool", "count": 1},
            {"route": "worktree", "phase": "final", "count": 1},
        ),
    },
)


def _repo_relative(path: Path, repo_root: Path) -> str:
    return path.relative_to(repo_root).as_posix()


def manifest_paths(
    repo_root: Path = REPO_ROOT,
    script_path: Path = SCRIPT_PATH,
) -> list[Path]:
    """Return every input covered by the manifest in canonical path order."""

    paths = [repo_root / path for path in BASE_INPUT_PATHS]
    paths.append(script_path)
    fixture_root = repo_root / ".github/dynamic-workflows-uat/fixtures"
    paths.extend(path for path in fixture_root.rglob("*") if path.is_file())
    return sorted(paths, key=lambda path: _repo_relative(path, repo_root))


def _sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def build_manifest(
    repo_root: Path = REPO_ROOT,
    script_path: Path = SCRIPT_PATH,
) -> dict[str, Any]:
    files = {
        _repo_relative(path, repo_root): {"sha256": _sha256(path)}
        for path in manifest_paths(repo_root, script_path)
    }
    return {
        "schema_version": 1,
        "geometry": {"columns": 120, "rows": 36},
        "unmatched_request_policy": {
            "mode": "fail_closed",
            "http_status": 422,
            "transcript": {"scenario": "unmatched", "phase": "rejected"},
        },
        "route_expectations": json.loads(json.dumps(ROUTE_EXPECTATIONS)),
        "notes": {
            "cancellation_lanes": (
                "Stop, the pre-checkpoint pause attempt, selected-agent skip, "
                "and the pre-retry canceled attempt intentionally end after "
                "their tool phase; they must not be counted as final replies."
            ),
            "selected_agent": "Selected-agent control lanes target agent-a.",
        },
        "files": files,
    }


def _json_value(value: Any) -> str:
    return json.dumps(value, ensure_ascii=False, sort_keys=True, separators=(",", ":"))


def manifest_drift(expected: dict[str, Any], actual: dict[str, Any]) -> list[str]:
    """Return deterministic, human-readable differences between two manifests."""

    drift = []
    expected_files = expected.get("files")
    actual_files = actual.get("files")
    if not isinstance(expected_files, dict):
        drift.append("invalid: recorded files value is not an object")
        expected_files = {}
    if not isinstance(actual_files, dict):
        drift.append("invalid: generated files value is not an object")
        actual_files = {}

    for path in sorted(actual_files.keys() - expected_files.keys()):
        drift.append(f"added: {path} ({_json_value(actual_files[path])})")
    for path in sorted(expected_files.keys() - actual_files.keys()):
        drift.append(f"removed: {path} ({_json_value(expected_files[path])})")
    for path in sorted(actual_files.keys() & expected_files.keys()):
        recorded = expected_files[path]
        generated = actual_files[path]
        if recorded != generated:
            drift.append(
                f"modified: {path} "
                f"(recorded {_json_value(recorded)}, generated {_json_value(generated)})"
            )

    metadata_keys = sorted((actual.keys() | expected.keys()) - {"files"})
    for key in metadata_keys:
        recorded = expected.get(key, "<missing>")
        generated = actual.get(key, "<missing>")
        if recorded != generated:
            drift.append(
                f"metadata changed: {key} "
                f"(recorded {_json_value(recorded)}, generated {_json_value(generated)})"
            )
    return drift


def _render(manifest: dict[str, Any]) -> str:
    return json.dumps(manifest, ensure_ascii=False, indent=2, sort_keys=True) + "\n"


def _check(manifest_path: Path = MANIFEST_PATH) -> int:
    try:
        expected = json.loads(manifest_path.read_text(encoding="utf-8"))
    except FileNotFoundError:
        print(f"fixture manifest is missing: {manifest_path}", file=sys.stderr)
        return 1
    except (OSError, json.JSONDecodeError) as error:
        print(f"cannot read fixture manifest {manifest_path}: {error}", file=sys.stderr)
        return 1
    if not isinstance(expected, dict):
        print(f"fixture manifest {manifest_path} must contain a JSON object", file=sys.stderr)
        return 1

    try:
        actual = build_manifest()
    except (OSError, ValueError) as error:
        print(f"cannot build fixture manifest: {error}", file=sys.stderr)
        return 1

    drift = manifest_drift(expected, actual)
    if drift:
        print("fixture manifest drift:", file=sys.stderr)
        for item in drift:
            print(f"  {item}", file=sys.stderr)
        return 1
    print("fixture manifest is current")
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    action = parser.add_mutually_exclusive_group(required=True)
    action.add_argument("--check", action="store_true", help="verify the checked-in manifest")
    action.add_argument("--print", action="store_true", dest="print_manifest", help="print the current manifest")
    args = parser.parse_args()

    if args.check:
        return _check()
    try:
        print(_render(build_manifest()), end="")
    except (OSError, ValueError) as error:
        print(f"cannot build fixture manifest: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
