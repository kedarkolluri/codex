#!/usr/bin/env python3

"""Verify one fresh Dynamic Workflows UAT mock-server transcript."""

import argparse
from collections import Counter
import json
from pathlib import Path
import sys
from typing import Any
from urllib.parse import urlsplit


SCRIPT_PATH = Path(__file__).resolve()
DEFAULT_MANIFEST_PATH = SCRIPT_PATH.with_name("fixture-manifest.json")
MAX_TRANSCRIPT_BYTES = 1024 * 1024
MAX_TRANSCRIPT_LINE_BYTES = 4096
STARTUP_KEYS = frozenset({"base_url", "health"})
REQUEST_KEYS = frozenset({"request", "scenario", "phase"})


class VerificationError(ValueError):
    """The transcript or its manifest expectation is not an exact UAT proof."""


def _read_json_object(path: Path, description: str) -> dict[str, Any]:
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except FileNotFoundError as error:
        raise VerificationError(f"{description} is missing: {path}") from error
    except (OSError, UnicodeError, json.JSONDecodeError) as error:
        raise VerificationError(f"cannot read {description} {path}: {error}") from error
    if not isinstance(value, dict):
        raise VerificationError(f"{description} {path} must contain a JSON object")
    return value


def _read_transcript(path: Path) -> list[dict[str, Any]]:
    try:
        size = path.stat().st_size
        if size > MAX_TRANSCRIPT_BYTES:
            raise VerificationError(
                f"transcript exceeds the {MAX_TRANSCRIPT_BYTES}-byte limit: {path}"
            )
        text = path.read_text(encoding="utf-8")
    except FileNotFoundError as error:
        raise VerificationError(f"transcript is missing: {path}") from error
    except (OSError, UnicodeError) as error:
        raise VerificationError(f"cannot read transcript {path}: {error}") from error

    if not text:
        raise VerificationError("transcript is empty; the fresh-server startup record is required")

    records = []
    for line_number, line in enumerate(text.splitlines(), start=1):
        if not line:
            raise VerificationError(f"transcript line {line_number} is blank")
        if len(line.encode("utf-8")) > MAX_TRANSCRIPT_LINE_BYTES:
            raise VerificationError(
                f"transcript line {line_number} exceeds the "
                f"{MAX_TRANSCRIPT_LINE_BYTES}-byte limit"
            )
        try:
            value = json.loads(line)
        except json.JSONDecodeError as error:
            raise VerificationError(
                f"transcript line {line_number} is not valid JSON: {error}"
            ) from error
        if not isinstance(value, dict):
            raise VerificationError(
                f"transcript line {line_number} must contain a JSON object"
            )
        records.append(value)
    return records


def _loopback_endpoint(value: Any, name: str, expected_path: str) -> int:
    if not isinstance(value, str):
        raise VerificationError(f"startup {name} must be a string")
    try:
        endpoint = urlsplit(value)
        port = endpoint.port
    except ValueError as error:
        raise VerificationError(f"startup {name} is not a valid URL: {error}") from error
    if (
        endpoint.scheme != "http"
        or endpoint.hostname != "127.0.0.1"
        or endpoint.username is not None
        or endpoint.password is not None
        or port is None
        or endpoint.path != expected_path
        or endpoint.query
        or endpoint.fragment
    ):
        raise VerificationError(
            f"startup {name} must be an exact loopback http endpoint at {expected_path}"
        )
    return port


def _validate_startup(record: dict[str, Any]) -> None:
    if set(record) != STARTUP_KEYS:
        raise VerificationError(
            "transcript line 1 must be the exact fresh-server startup record "
            "with base_url and health"
        )
    base_port = _loopback_endpoint(record["base_url"], "base_url", "/v1")
    health_port = _loopback_endpoint(record["health"], "health", "/healthz")
    if base_port != health_port:
        raise VerificationError("startup base_url and health must use the same port")


def _validate_fail_closed_policy(manifest: dict[str, Any]) -> None:
    if manifest.get("schema_version") != 1:
        raise VerificationError("fixture manifest schema_version must be exactly 1")
    policy = manifest.get("unmatched_request_policy")
    if not isinstance(policy, dict):
        raise VerificationError("fixture manifest unmatched_request_policy must be an object")
    transcript = policy.get("transcript")
    status = policy.get("http_status")
    if (
        policy.get("mode") != "fail_closed"
        or type(status) is not int
        or not 400 <= status <= 599
        or transcript != {"scenario": "unmatched", "phase": "rejected"}
    ):
        raise VerificationError(
            "fixture manifest unmatched_request_policy is not the supported "
            "fail-closed rejection contract"
        )


def _lane_expectation(manifest: dict[str, Any], lane: str) -> dict[str, Any]:
    expectations = manifest.get("route_expectations")
    if not isinstance(expectations, list):
        raise VerificationError("fixture manifest route_expectations must be an array")

    by_lane = {}
    for index, expectation in enumerate(expectations):
        if not isinstance(expectation, dict):
            raise VerificationError(f"route_expectations[{index}] must be an object")
        expectation_lane = expectation.get("lane")
        if not isinstance(expectation_lane, str) or not expectation_lane:
            raise VerificationError(
                f"route_expectations[{index}].lane must be a non-empty string"
            )
        if expectation_lane in by_lane:
            raise VerificationError(
                f"fixture manifest contains duplicate lane {expectation_lane!r}"
            )
        by_lane[expectation_lane] = expectation

    try:
        return by_lane[lane]
    except KeyError as error:
        raise VerificationError(f"fixture manifest has no lane {lane!r}") from error


def _expected_multiset(
    expectation: dict[str, Any],
) -> tuple[int, Counter[tuple[str, str]], frozenset[str]]:
    allowed_keys = {"lane", "expected_request_count", "routes", "forbidden_routes"}
    unknown_keys = set(expectation) - allowed_keys
    if unknown_keys:
        raise VerificationError(
            f"lane expectation contains unsupported keys: {sorted(unknown_keys)!r}"
        )

    expected_count = expectation.get("expected_request_count")
    if type(expected_count) is not int or expected_count < 0:
        raise VerificationError("lane expected_request_count must be a non-negative integer")
    routes = expectation.get("routes")
    if not isinstance(routes, list):
        raise VerificationError("lane routes must be an array")

    expected: Counter[tuple[str, str]] = Counter()
    for index, route_expectation in enumerate(routes):
        if not isinstance(route_expectation, dict) or set(route_expectation) != {
            "route",
            "phase",
            "count",
        }:
            raise VerificationError(
                f"lane routes[{index}] must contain only route, phase, and count"
            )
        route = route_expectation["route"]
        phase = route_expectation["phase"]
        count = route_expectation["count"]
        if not isinstance(route, str) or not route:
            raise VerificationError(f"lane routes[{index}].route must be a non-empty string")
        if not isinstance(phase, str) or not phase:
            raise VerificationError(f"lane routes[{index}].phase must be a non-empty string")
        if type(count) is not int or count <= 0:
            raise VerificationError(f"lane routes[{index}].count must be a positive integer")
        key = (route, phase)
        if key in expected:
            raise VerificationError(
                f"lane routes contains duplicate route/phase expectation {key!r}"
            )
        expected[key] = count

    forbidden_value = expectation.get("forbidden_routes", [])
    if not isinstance(forbidden_value, list) or any(
        not isinstance(route, str) or not route for route in forbidden_value
    ):
        raise VerificationError("lane forbidden_routes must be an array of non-empty strings")
    if len(forbidden_value) != len(set(forbidden_value)):
        raise VerificationError("lane forbidden_routes must not contain duplicates")
    forbidden = frozenset(forbidden_value)
    overlap = forbidden & {route for route, _phase in expected}
    if overlap:
        raise VerificationError(
            f"lane routes and forbidden_routes overlap: {sorted(overlap)!r}"
        )
    if sum(expected.values()) != expected_count:
        raise VerificationError(
            "lane route counts do not sum to expected_request_count "
            f"({sum(expected.values())} != {expected_count})"
        )
    return expected_count, expected, forbidden


def _request_records(records: list[dict[str, Any]]) -> list[dict[str, Any]]:
    _validate_startup(records[0])
    requests = records[1:]
    seen_numbers = set()
    for line_number, record in enumerate(requests, start=2):
        if set(record) != REQUEST_KEYS:
            raise VerificationError(
                f"transcript line {line_number} must contain only request, scenario, and phase"
            )
        request_number = record["request"]
        scenario = record["scenario"]
        phase = record["phase"]
        if type(request_number) is not int or request_number <= 0:
            raise VerificationError(
                f"transcript line {line_number} request must be a positive integer"
            )
        if request_number in seen_numbers:
            raise VerificationError(f"duplicate transcript request number {request_number}")
        seen_numbers.add(request_number)
        if not isinstance(scenario, str) or not scenario:
            raise VerificationError(
                f"transcript line {line_number} scenario must be a non-empty string"
            )
        if not isinstance(phase, str) or not phase:
            raise VerificationError(
                f"transcript line {line_number} phase must be a non-empty string"
            )
    return requests


def _counter_json(counter: Counter[tuple[str, str]]) -> str:
    value = [
        {"route": route, "phase": phase, "count": count}
        for (route, phase), count in sorted(counter.items())
    ]
    return json.dumps(value, separators=(",", ":"))


def _verify_pipeline_order(requests: list[dict[str, Any]]) -> None:
    by_route_phase = {
        (record["scenario"], record["phase"]): record["request"] for record in requests
    }
    b0_tool = by_route_phase[("pipeline-b0", "tool")]
    a1_final = by_route_phase[("pipeline-a1", "final")]
    b0_final = by_route_phase[("pipeline-b0", "final")]
    if not b0_tool < a1_final < b0_final:
        raise VerificationError(
            "pipeline stagger requires pipeline-b0 tool < pipeline-a1 final < "
            "pipeline-b0 final/followup by fresh-server request number"
        )


def verify_transcript(
    manifest: dict[str, Any],
    lane: str,
    records: list[dict[str, Any]],
) -> int:
    """Validate ``records`` against one exact manifest lane and return its request count."""

    if not records:
        raise VerificationError("fresh-server startup record is missing")
    _validate_fail_closed_policy(manifest)
    expectation = _lane_expectation(manifest, lane)
    expected_count, expected, forbidden = _expected_multiset(expectation)
    requests = _request_records(records)

    rejected = [record for record in requests if record["phase"] == "rejected"]
    if rejected:
        details = [(record["request"], record["scenario"]) for record in rejected]
        raise VerificationError(f"transcript contains rejected requests: {details!r}")
    forbidden_seen = [
        (record["request"], record["scenario"])
        for record in requests
        if record["scenario"] in forbidden
    ]
    if forbidden_seen:
        raise VerificationError(f"transcript contains forbidden requests: {forbidden_seen!r}")

    actual_count = len(requests)
    if actual_count != expected_count:
        raise VerificationError(
            f"lane request count mismatch: expected {expected_count}, got {actual_count}"
        )
    numbers = sorted(record["request"] for record in requests)
    expected_numbers = list(range(1, expected_count + 1))
    if numbers != expected_numbers:
        raise VerificationError(
            "fresh-server request numbers must be exactly contiguous from 1: "
            f"expected {expected_numbers!r}, got {numbers!r}"
        )

    actual = Counter((record["scenario"], record["phase"]) for record in requests)
    if actual != expected:
        raise VerificationError(
            "lane request multiset mismatch: "
            f"expected {_counter_json(expected)}, got {_counter_json(actual)}"
        )
    if lane == "pipeline stagger":
        _verify_pipeline_order(requests)
    return actual_count


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--lane", required=True, help="exact lane name from fixture-manifest.json")
    parser.add_argument(
        "--manifest",
        type=Path,
        default=DEFAULT_MANIFEST_PATH,
        help="fixture manifest path",
    )
    parser.add_argument("transcript", type=Path, help="complete fresh mock-server stdout JSONL")
    args = parser.parse_args()

    try:
        manifest = _read_json_object(args.manifest, "fixture manifest")
        records = _read_transcript(args.transcript)
        request_count = verify_transcript(manifest, args.lane, records)
    except VerificationError as error:
        print(f"transcript verification failed: {error}", file=sys.stderr)
        return 1

    noun = "request" if request_count == 1 else "requests"
    print(f"verified lane {args.lane!r}: {request_count} {noun}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
