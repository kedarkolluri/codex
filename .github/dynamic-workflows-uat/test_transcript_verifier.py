#!/usr/bin/env python3

import copy
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


MODULE_PATH = Path(__file__).with_name("transcript_verifier.py")
MANIFEST_PATH = Path(__file__).with_name("fixture-manifest.json")
SPEC = importlib.util.spec_from_file_location("transcript_verifier", MODULE_PATH)
assert SPEC is not None and SPEC.loader is not None
transcript_verifier = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(transcript_verifier)


def _startup() -> dict[str, object]:
    return {
        "base_url": "http://127.0.0.1:43123/v1",
        "health": "http://127.0.0.1:43123/healthz",
    }


def _expectation(manifest: dict[str, object], lane: str) -> dict[str, object]:
    expectations = manifest["route_expectations"]
    assert isinstance(expectations, list)
    matching = [value for value in expectations if value["lane"] == lane]
    assert len(matching) == 1
    return matching[0]


def _valid_records(manifest: dict[str, object], lane: str) -> list[dict[str, object]]:
    expectation = _expectation(manifest, lane)
    routes = expectation["routes"]
    assert isinstance(routes, list)
    route_phases = [
        (value["route"], value["phase"])
        for value in routes
        for _index in range(value["count"])
    ]
    if lane == "pipeline stagger":
        route_phases = [
            ("pipeline-a0", "final"),
            ("pipeline-b0", "tool"),
            ("pipeline-a1", "final"),
            ("pipeline-b0", "final"),
        ]
    return [
        _startup(),
        *[
            {"request": number, "scenario": route, "phase": phase}
            for number, (route, phase) in enumerate(route_phases, start=1)
        ],
    ]


def _write_json_lines(path: Path, records: list[dict[str, object]]) -> None:
    path.write_text(
        "".join(f"{json.dumps(record, separators=(',', ':'))}\n" for record in records),
        encoding="utf-8",
    )


class TranscriptVerifierTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.manifest = json.loads(MANIFEST_PATH.read_text(encoding="utf-8"))

    def test_every_manifest_lane_accepts_its_exact_fresh_server_multiset(self) -> None:
        expectations = self.manifest["route_expectations"]
        self.assertIsInstance(expectations, list)
        for expectation in expectations:
            lane = expectation["lane"]
            with self.subTest(lane=lane):
                records = _valid_records(self.manifest, lane)
                self.assertEqual(
                    transcript_verifier.verify_transcript(
                        self.manifest,
                        lane,
                        records,
                    ),
                    expectation["expected_request_count"],
                )

    def test_exact_count_numbers_and_multiset_are_all_required(self) -> None:
        valid = _valid_records(self.manifest, "baseline/monitor")

        with self.assertRaisesRegex(
            transcript_verifier.VerificationError,
            "request count mismatch",
        ):
            transcript_verifier.verify_transcript(
                self.manifest,
                "baseline/monitor",
                valid[:-1],
            )

        duplicate_number = copy.deepcopy(valid)
        duplicate_number[-1]["request"] = 1
        with self.assertRaisesRegex(
            transcript_verifier.VerificationError,
            "duplicate transcript request number 1",
        ):
            transcript_verifier.verify_transcript(
                self.manifest,
                "baseline/monitor",
                duplicate_number,
            )

        reused_server = copy.deepcopy(valid)
        for record in reused_server[1:]:
            record["request"] += 8
        with self.assertRaisesRegex(
            transcript_verifier.VerificationError,
            "exactly contiguous from 1",
        ):
            transcript_verifier.verify_transcript(
                self.manifest,
                "baseline/monitor",
                reused_server,
            )

        wrong_multiset = copy.deepcopy(valid)
        wrong_multiset[-1]["scenario"] = "monitor-a"
        with self.assertRaisesRegex(
            transcript_verifier.VerificationError,
            "request multiset mismatch",
        ):
            transcript_verifier.verify_transcript(
                self.manifest,
                "baseline/monitor",
                wrong_multiset,
            )

    def test_forbidden_routes_and_every_rejection_fail_closed(self) -> None:
        forbidden = _valid_records(self.manifest, "budget-36")
        forbidden[-1]["scenario"] = "budget-2"
        with self.assertRaisesRegex(
            transcript_verifier.VerificationError,
            "contains forbidden requests",
        ):
            transcript_verifier.verify_transcript(
                self.manifest,
                "budget-36",
                forbidden,
            )

        rejected = _valid_records(self.manifest, "budget-36")
        rejected[-1]["phase"] = "rejected"
        with self.assertRaisesRegex(
            transcript_verifier.VerificationError,
            "contains rejected requests",
        ):
            transcript_verifier.verify_transcript(
                self.manifest,
                "budget-36",
                rejected,
            )

        unmatched = _valid_records(self.manifest, "stop")
        unmatched[-1] = {
            "request": 1,
            "scenario": "unmatched",
            "phase": "rejected",
        }
        with self.assertRaisesRegex(
            transcript_verifier.VerificationError,
            "contains rejected requests",
        ):
            transcript_verifier.verify_transcript(
                self.manifest,
                "stop",
                unmatched,
            )

    def test_pipeline_requires_a1_before_b0_final_followup(self) -> None:
        invalid = _valid_records(self.manifest, "pipeline stagger")
        a1 = next(record for record in invalid if record.get("scenario") == "pipeline-a1")
        b0_final = next(
            record
            for record in invalid
            if record.get("scenario") == "pipeline-b0" and record.get("phase") == "final"
        )
        a1["request"], b0_final["request"] = b0_final["request"], a1["request"]

        with self.assertRaisesRegex(
            transcript_verifier.VerificationError,
            "pipeline-b0 tool < pipeline-a1 final < pipeline-b0 final/followup",
        ):
            transcript_verifier.verify_transcript(
                self.manifest,
                "pipeline stagger",
                invalid,
            )

    def test_startup_and_manifest_contracts_are_not_optional(self) -> None:
        missing_startup = _valid_records(self.manifest, "stop")[1:]
        with self.assertRaisesRegex(
            transcript_verifier.VerificationError,
            "fresh-server startup record",
        ):
            transcript_verifier.verify_transcript(
                self.manifest,
                "stop",
                missing_startup,
            )

        non_loopback = _valid_records(self.manifest, "stop")
        non_loopback[0]["base_url"] = "https://provider.example/v1"
        with self.assertRaisesRegex(
            transcript_verifier.VerificationError,
            "exact loopback http endpoint",
        ):
            transcript_verifier.verify_transcript(
                self.manifest,
                "stop",
                non_loopback,
            )

        permissive_manifest = copy.deepcopy(self.manifest)
        permissive_manifest["unmatched_request_policy"]["mode"] = "allow"
        with self.assertRaisesRegex(
            transcript_verifier.VerificationError,
            "fail-closed rejection contract",
        ):
            transcript_verifier.verify_transcript(
                permissive_manifest,
                "stop",
                _valid_records(self.manifest, "stop"),
            )

    def test_cli_reads_complete_jsonl_and_returns_nonzero_on_extra_request(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            transcript = root / "mock.stdout.jsonl"
            _write_json_lines(transcript, _valid_records(self.manifest, "stop"))
            environment = os.environ.copy()
            environment["PYTHONDONTWRITEBYTECODE"] = "1"
            command = [
                sys.executable,
                str(MODULE_PATH),
                "--manifest",
                str(MANIFEST_PATH),
                "--lane",
                "stop",
                str(transcript),
            ]

            verified = subprocess.run(
                command,
                check=False,
                capture_output=True,
                text=True,
                env=environment,
            )
            self.assertEqual(verified.returncode, 0, verified.stderr)
            self.assertEqual(verified.stdout, "verified lane 'stop': 1 request\n")

            records = _valid_records(self.manifest, "stop")
            records.append({"request": 2, "scenario": "stop", "phase": "final"})
            _write_json_lines(transcript, records)
            rejected = subprocess.run(
                command,
                check=False,
                capture_output=True,
                text=True,
                env=environment,
            )
            self.assertEqual(rejected.returncode, 1)
            self.assertIn("lane request count mismatch", rejected.stderr)


if __name__ == "__main__":
    unittest.main()
