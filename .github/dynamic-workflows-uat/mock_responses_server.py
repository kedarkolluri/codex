#!/usr/bin/env python3

"""Loopback-only Responses API fixture for Dynamic Workflows TUI UAT.

Every route is selected by one standalone ``UAT_ROUTE:<scenario>`` line in
the latest user message. Responses and shell commands come only from the
static table below; unmatched or contract-invalid requests fail closed without
echoing request content.
"""

import argparse
from dataclasses import dataclass
from enum import Enum
import json
import os
import re
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from typing import Any


HOST = "127.0.0.1"
RESPONSES_PATH = "/v1/responses"
MAX_REQUEST_BYTES = 1024 * 1024
ROUTE_SENTINEL_PREFIX = "UAT_ROUTE:"
UNMATCHED_STATUS = 422
REJECTION_STATUS = 422
PIPELINE_GATE_WAIT_SECONDS = 10.0
WORKTREE_OUTPUT_MAX_BYTES = 16 * 1024
WORKTREE_CWD_MAX_CHARS = 2048
IS_WINDOWS = os.name == "nt"

FAILURE_MESSAGE = "Intentional Dynamic Workflows UAT invalid prompt failure."
UNMATCHED_ERROR = {
    "error": {
        "code": "unmatched_uat_scenario",
        "message": "request did not match a configured UAT scenario",
    }
}

FAILURE_SCHEMA = {
    "type": "object",
    "properties": {
        "marker": {"const": "UAT_SCHEMA_OPTIONS_OK"},
        "answer": {"type": "string"},
    },
    "required": ["marker", "answer"],
    "additionalProperties": False,
}
WORKTREE_SCHEMA = {
    "type": "object",
    "properties": {
        "cwd": {"type": "string"},
        "marker": {"const": "UAT_WORKTREE_MARKER"},
        "removed": {"const": True},
        "clean": {"const": True},
    },
    "required": ["cwd", "marker", "removed", "clean"],
    "additionalProperties": False,
}

POSIX_CONTROL_DELAY_COMMAND = "sleep 120"
WINDOWS_CONTROL_DELAY_COMMAND = "Start-Sleep -Seconds 120"
POSIX_DELAY_COMMAND = "sleep 12"
WINDOWS_DELAY_COMMAND = "Start-Sleep -Seconds 12"
POSIX_WORKTREE_COMMAND = (
    "set -euC; "
    "marker_file=uat-worktree-child-marker.txt; "
    'test ! -e "$marker_file" && test ! -L "$marker_file"; '
    "pwd; "
    'printf %s UAT_WORKTREE_MARKER > "$marker_file"; '
    "printf 'UAT_WORKTREE_READ=%s\\n' \"$(cat \"$marker_file\")\"; "
    "sleep 6; "
    'rm "$marker_file"; '
    'test ! -e "$marker_file"; '
    "printf '%s\\n' UAT_WORKTREE_REMOVED=true; "
    'test -z "$(git status --porcelain)"; '
    "printf '%s\\n' UAT_WORKTREE_CLEAN=true"
)
WINDOWS_WORKTREE_COMMAND = (
    "$ErrorActionPreference = 'Stop'; "
    "$markerFile = 'uat-worktree-child-marker.txt'; "
    "if ($null -ne (Get-Item -LiteralPath $markerFile -Force "
    "-ErrorAction SilentlyContinue)) { throw 'pre-existing UAT marker' }; "
    "Write-Output (Get-Location).Path; "
    "$markerBytes = [System.Text.Encoding]::ASCII.GetBytes('UAT_WORKTREE_MARKER'); "
    "$markerStream = [System.IO.File]::Open($markerFile, "
    "[System.IO.FileMode]::CreateNew, [System.IO.FileAccess]::Write, "
    "[System.IO.FileShare]::None); "
    "try { $markerStream.Write($markerBytes, 0, $markerBytes.Length) } "
    "finally { $markerStream.Dispose() }; "
    "Write-Output ('UAT_WORKTREE_READ=' + "
    "[System.IO.File]::ReadAllText($markerFile)); "
    "Start-Sleep -Seconds 6; "
    "Remove-Item -LiteralPath $markerFile; "
    "if ($null -ne (Get-Item -LiteralPath $markerFile -Force "
    "-ErrorAction SilentlyContinue)) { throw 'UAT marker removal failed' }; "
    "Write-Output 'UAT_WORKTREE_REMOVED=true'; "
    "$status = @(git status --porcelain); "
    "if ($LASTEXITCODE -ne 0 -or $status.Count -ne 0) { "
    "throw 'UAT worktree is not clean' }; "
    "Write-Output 'UAT_WORKTREE_CLEAN=true'"
)

CONTROL_DELAY_COMMAND = (
    WINDOWS_CONTROL_DELAY_COMMAND if IS_WINDOWS else POSIX_CONTROL_DELAY_COMMAND
)
DELAY_COMMAND = WINDOWS_DELAY_COMMAND if IS_WINDOWS else POSIX_DELAY_COMMAND
WORKTREE_COMMAND = WINDOWS_WORKTREE_COMMAND if IS_WINDOWS else POSIX_WORKTREE_COMMAND

UUID_PATTERN = (
    r"[0-9a-f]{8}-[0-9a-f]{4}-7[0-9a-f]{3}-"
    r"[89ab][0-9a-f]{3}-[0-9a-f]{12}"
)
WORKTREE_NAMESPACE_PATTERN = re.compile(rf"\.codex-worktrees-{UUID_PATTERN}")
WORKTREE_AGENT_PATTERN = re.compile(r"agent-[0-9]{1,6}")
WINDOWS_INVALID_COMPONENT_CHARS = frozenset('<>:"/\\|?*')


class ResponseMode(Enum):
    DIRECT = "direct"
    TOOL = "tool"
    FAILED = "failed"
    FORBIDDEN = "forbidden"
    PIPELINE_A0 = "pipeline-a0"
    PIPELINE_B0 = "pipeline-b0"
    PIPELINE_A1 = "pipeline-a1"
    WORKTREE = "worktree"


@dataclass(frozen=True)
class RequestContract:
    schema: dict[str, Any] | None = None
    model: str | None = None
    effort: str | None = None


@dataclass(frozen=True)
class Scenario:
    mode: ResponseMode
    final_text: str | None = None
    output_tokens: int = 11
    call_id: str | None = None
    command: str | None = None
    timeout_ms: int | None = None
    login: bool | None = None
    contract: RequestContract | None = None


@dataclass(frozen=True)
class Dispatch:
    status: int
    body: bytes
    content_type: str
    cache_control: str
    transcript: str


SCENARIOS: dict[str, Scenario] = {
    # Existing control and monitor routes.
    "stop": Scenario(
        ResponseMode.TOOL,
        final_text="UAT_STOP_FINISHED",
        call_id="call-control-uat-stop",
        command=CONTROL_DELAY_COMMAND,
        timeout_ms=150_000,
    ),
    "pause": Scenario(
        ResponseMode.TOOL,
        final_text="UAT_PAUSE_FINISHED",
        call_id="call-control-uat-pause",
        command=CONTROL_DELAY_COMMAND,
        timeout_ms=150_000,
    ),
    "agent-a": Scenario(
        ResponseMode.TOOL,
        final_text="UAT_AGENT_A_FINISHED",
        call_id="call-control-uat-agent-a",
        command=CONTROL_DELAY_COMMAND,
        timeout_ms=150_000,
    ),
    "agent-b": Scenario(
        ResponseMode.TOOL,
        final_text="UAT_AGENT_B_FINISHED",
        call_id="call-control-uat-agent-b",
        command=CONTROL_DELAY_COMMAND,
        timeout_ms=150_000,
    ),
    "monitor-a": Scenario(
        ResponseMode.TOOL,
        final_text="# Dynamic Workflows human UAT fixture",
        call_id="call-control-uat-monitor-a",
        command=CONTROL_DELAY_COMMAND,
        timeout_ms=150_000,
    ),
    "monitor-b": Scenario(
        ResponseMode.TOOL,
        final_text="Yes. It says this fixture is disposable.",
        call_id="call-control-uat-monitor-b",
        command=CONTROL_DELAY_COMMAND,
        timeout_ms=150_000,
    ),
    # Failure/null and structured options.
    "failure-null": Scenario(ResponseMode.FAILED),
    "failure-sibling": Scenario(
        ResponseMode.DIRECT,
        final_text="UAT_SIBLING_SURVIVED",
    ),
    "failure-schema": Scenario(
        ResponseMode.DIRECT,
        final_text=json.dumps(
            {"marker": "UAT_SCHEMA_OPTIONS_OK", "answer": "structured"},
            separators=(",", ":"),
        ),
        output_tokens=21,
        contract=RequestContract(
            schema=FAILURE_SCHEMA,
            model="gpt-5.4",
            effort="high",
        ),
    ),
    # Run-local budget. Ordinals 2 and 3 must never reach the provider.
    "budget-0": Scenario(
        ResponseMode.DIRECT,
        final_text="UAT_BUDGET_AGENT_0",
        output_tokens=18,
    ),
    "budget-1": Scenario(
        ResponseMode.DIRECT,
        final_text="UAT_BUDGET_AGENT_1",
        output_tokens=18,
    ),
    "budget-2": Scenario(ResponseMode.FORBIDDEN),
    "budget-3": Scenario(ResponseMode.FORBIDDEN),
    # Parallel barrier.
    "parallel-fast": Scenario(
        ResponseMode.DIRECT,
        final_text="UAT_PARALLEL_FAST_DONE",
    ),
    "parallel-held": Scenario(
        ResponseMode.TOOL,
        final_text="UAT_PARALLEL_HELD_DONE",
        call_id="call-uat-parallel-held",
        command=DELAY_COMMAND,
        timeout_ms=20_000,
        login=False,
    ),
    # Three-child default-cap pipeline.
    "pipeline-a0": Scenario(
        ResponseMode.PIPELINE_A0,
        final_text="UAT_PIPELINE_A0_DONE",
    ),
    "pipeline-b0": Scenario(
        ResponseMode.PIPELINE_B0,
        final_text="UAT_PIPELINE_B0_DONE",
        call_id="call-uat-pipeline-b0",
        command=DELAY_COMMAND,
        timeout_ms=20_000,
        login=False,
    ),
    "pipeline-a1": Scenario(
        ResponseMode.PIPELINE_A1,
        final_text="UAT_PIPELINE_A1_DONE",
    ),
    # Positive local worktree proof.
    "worktree": Scenario(
        ResponseMode.WORKTREE,
        call_id="call-uat-worktree",
        command=WORKTREE_COMMAND,
        timeout_ms=20_000,
        login=False,
        contract=RequestContract(schema=WORKTREE_SCHEMA),
    ),
}


class RouteState:
    """One-shot state for exactly one pipeline-stagger UAT run."""

    def __init__(self) -> None:
        self.pipeline_a0_waiting = threading.Event()
        self.pipeline_b0_seen = threading.Event()
        self.pipeline_a1_seen = threading.Event()
        self._pipeline_lock = threading.Lock()
        self._pipeline_condition = threading.Condition(self._pipeline_lock)
        self._pipeline_a0_claimed = False
        self._pipeline_a0_completed = False
        self._pipeline_b0_claimed = False
        self._pipeline_b0_completed = False
        self._pipeline_a1_claimed = False
        self._pipeline_terminal = False

    def claim_pipeline_a0(self) -> str | None:
        with self._pipeline_condition:
            if self._pipeline_terminal or self._pipeline_a0_claimed:
                return self._poison_pipeline_locked("pipeline_reuse")
            self._pipeline_a0_claimed = True
            self.pipeline_a0_waiting.set()
            return None

    def complete_pipeline_a0_after_b0(self, timeout: float) -> str | None:
        with self._pipeline_condition:
            ready = self._pipeline_condition.wait_for(
                lambda: self._pipeline_b0_claimed or self._pipeline_terminal,
                timeout=max(timeout, 0.0),
            )
            if not ready:
                return self._poison_pipeline_locked("pipeline_gate_timeout")
            if self._pipeline_terminal:
                return "pipeline_reuse"
            if not self._pipeline_a0_claimed or not self._pipeline_b0_claimed:
                return self._poison_pipeline_locked("pipeline_order_violation")
            self._pipeline_a0_completed = True
            return None

    def claim_pipeline_b0(self) -> str | None:
        with self._pipeline_condition:
            if self._pipeline_terminal or self._pipeline_b0_claimed:
                return self._poison_pipeline_locked("pipeline_reuse")
            self._pipeline_b0_claimed = True
            self.pipeline_b0_seen.set()
            self._pipeline_condition.notify_all()
            return None

    def claim_pipeline_a1(self) -> str | None:
        with self._pipeline_condition:
            if self._pipeline_terminal or self._pipeline_a1_claimed:
                return self._poison_pipeline_locked("pipeline_reuse")
            if not self._pipeline_a0_completed or not self._pipeline_b0_claimed:
                return self._poison_pipeline_locked("pipeline_order_violation")
            self._pipeline_a1_claimed = True
            self.pipeline_a1_seen.set()
            return None

    def complete_pipeline_b0(self) -> str | None:
        with self._pipeline_condition:
            if self._pipeline_terminal or self._pipeline_b0_completed:
                return self._poison_pipeline_locked("pipeline_reuse")
            if not self._pipeline_b0_claimed or not self._pipeline_a1_claimed:
                return self._poison_pipeline_locked("pipeline_order_violation")
            self._pipeline_b0_completed = True
            self._pipeline_terminal = True
            self._pipeline_condition.notify_all()
            return None

    def poison_pipeline(self, code: str) -> str:
        with self._pipeline_condition:
            return self._poison_pipeline_locked(code)

    def _poison_pipeline_locked(self, code: str) -> str:
        self._pipeline_terminal = True
        self._pipeline_condition.notify_all()
        return code


def _usage(output_tokens: int) -> dict[str, Any]:
    return {
        "input_tokens": 0,
        "input_tokens_details": None,
        "output_tokens": output_tokens,
        "output_tokens_details": None,
        "total_tokens": output_tokens,
    }


def _created(response_id: str) -> dict[str, Any]:
    return {"type": "response.created", "response": {"id": response_id}}


def _completed(response_id: str, output_tokens: int) -> dict[str, Any]:
    return {
        "type": "response.completed",
        "response": {"id": response_id, "usage": _usage(output_tokens)},
    }


def _failed(response_id: str) -> dict[str, Any]:
    return {
        "type": "response.failed",
        "response": {
            "id": response_id,
            "error": {"code": "invalid_prompt", "message": FAILURE_MESSAGE},
        },
    }


def _function_call(spec: Scenario) -> dict[str, Any]:
    if spec.call_id is None or spec.command is None or spec.timeout_ms is None:
        raise ValueError("tool scenario is missing its static call configuration")
    arguments: dict[str, Any] = {
        "command": spec.command,
        "timeout_ms": spec.timeout_ms,
    }
    if spec.login is not None:
        arguments["login"] = spec.login
    return {
        "type": "response.output_item.done",
        "item": {
            "type": "function_call",
            "call_id": spec.call_id,
            "name": "shell_command",
            "arguments": json.dumps(arguments, separators=(",", ":")),
        },
    }


def _assistant_message(message_id: str, text: str) -> dict[str, Any]:
    return {
        "type": "response.output_item.done",
        "item": {
            "type": "message",
            "role": "assistant",
            "id": message_id,
            "content": [{"type": "output_text", "text": text}],
        },
    }


def _sse(events: list[dict[str, Any]]) -> bytes:
    chunks = []
    for event in events:
        event_type = event["type"]
        payload = json.dumps(event, ensure_ascii=False, separators=(",", ":"))
        chunks.append(f"event: {event_type}\ndata: {payload}\n\n")
    return "".join(chunks).encode("utf-8")


def _function_call_outputs(body: dict[str, Any]) -> list[dict[str, Any]]:
    inputs = body.get("input")
    if not isinstance(inputs, list):
        return []
    return [
        item
        for item in inputs
        if isinstance(item, dict) and item.get("type") == "function_call_output"
    ]


def _latest_user_text(body: dict[str, Any]) -> list[str]:
    inputs = body.get("input")
    if not isinstance(inputs, list):
        return []
    for item in reversed(inputs):
        if not isinstance(item, dict) or item.get("role") != "user":
            continue
        content = item.get("content")
        if isinstance(content, str):
            return [content]
        if not isinstance(content, list):
            return []
        return [
            part["text"]
            for part in content
            if isinstance(part, dict)
            and part.get("type") == "input_text"
            and isinstance(part.get("text"), str)
        ]
    return []


def _route_sentinel(body: dict[str, Any]) -> str | None:
    sentinels = [
        line.strip()
        for text in _latest_user_text(body)
        for line in text.splitlines()
        if line.strip().startswith(ROUTE_SENTINEL_PREFIX)
    ]
    if len(sentinels) != 1:
        return None
    return sentinels[0]


def _scenario(body: dict[str, Any]) -> tuple[str, Scenario] | None:
    sentinel = _route_sentinel(body)
    if sentinel is None:
        return None
    name = sentinel.removeprefix(ROUTE_SENTINEL_PREFIX)
    spec = SCENARIOS.get(name)
    if spec is None or sentinel != f"{ROUTE_SENTINEL_PREFIX}{name}":
        return None
    return name, spec


def _request_matches_contract(body: dict[str, Any], contract: RequestContract) -> bool:
    if contract.model is not None and body.get("model") != contract.model:
        return False
    if contract.effort is not None:
        reasoning = body.get("reasoning")
        if not isinstance(reasoning, dict) or reasoning.get("effort") != contract.effort:
            return False
    if contract.schema is not None:
        text = body.get("text")
        if not isinstance(text, dict):
            return False
        output_format = text.get("format")
        if not isinstance(output_format, dict):
            return False
        expected_format = {
            "type": "json_schema",
            "name": "codex_output_schema",
            "strict": True,
            "schema": contract.schema,
        }
        if output_format != expected_format:
            return False
    return True


def _valid_worktree_components(components: list[str], *, windows: bool) -> bool:
    if not components:
        return False
    for component in components:
        if not component or component in {".", ".."}:
            return False
        if len(component.encode("utf-8")) > 255:
            return False
        if any(ord(char) < 32 or ord(char) == 127 for char in component):
            return False
        if windows:
            if component.endswith((" ", ".")):
                return False
            if any(char in WINDOWS_INVALID_COMPONENT_CHARS for char in component):
                return False
    return True


def _validated_worktree_cwd(line: str) -> str | None:
    if not line or len(line) > WORKTREE_CWD_MAX_CHARS:
        return None

    if line.startswith("/"):
        components = line[1:].split("/")
        if not _valid_worktree_components(components, windows=False):
            return None
    elif (
        len(line) >= 4
        and line[0].isascii()
        and line[0].isalpha()
        and line[1:3] == ":\\"
        and "/" not in line
    ):
        components = line[3:].split("\\")
        if not _valid_worktree_components(components, windows=True):
            return None
    else:
        return None

    if len(components) < 2:
        return None
    if WORKTREE_NAMESPACE_PATTERN.fullmatch(components[-2]) is None:
        return None
    if WORKTREE_AGENT_PATTERN.fullmatch(components[-1]) is None:
        return None
    return line


def _worktree_result(function_output: str) -> str | None:
    if len(function_output.encode("utf-8")) > WORKTREE_OUTPUT_MAX_BYTES:
        return None
    lines = function_output.splitlines()
    required_lines = {
        "UAT_WORKTREE_READ=UAT_WORKTREE_MARKER",
        "UAT_WORKTREE_REMOVED=true",
        "UAT_WORKTREE_CLEAN=true",
    }
    if not required_lines.issubset(lines):
        return None

    matches = [cwd for line in lines if (cwd := _validated_worktree_cwd(line))]
    if len(matches) != 1:
        return None
    cwd = matches[0]
    return json.dumps(
        {
            "cwd": cwd,
            "marker": "UAT_WORKTREE_MARKER",
            "removed": True,
            "clean": True,
        },
        separators=(",", ":"),
    )


def _transcript_line(request_number: int, scenario: str, phase: str) -> str:
    return json.dumps(
        {"request": request_number, "scenario": scenario, "phase": phase},
        separators=(",", ":"),
    )


def _json_rejection(
    request_number: int,
    scenario: str,
    code: str,
    message: str,
) -> Dispatch:
    response = json.dumps(
        {"error": {"code": code, "message": message}},
        separators=(",", ":"),
    ).encode("utf-8")
    return Dispatch(
        status=REJECTION_STATUS,
        body=response,
        content_type="application/json; charset=utf-8",
        cache_control="no-store",
        transcript=_transcript_line(request_number, scenario, "rejected"),
    )


def _pipeline_rejection(
    request_number: int,
    scenario: str,
    code: str,
) -> Dispatch:
    messages = {
        "pipeline_gate_timeout": (
            "pipeline B0 did not arrive before the bounded A0 gate expired"
        ),
        "pipeline_order_violation": (
            "pipeline requests did not follow the one-shot stage-one/A1/B0-final contract"
        ),
        "pipeline_reuse": (
            "pipeline proof state is terminal or this one-shot route was already used"
        ),
    }
    message = messages.get(code)
    if message is None:
        raise ValueError(f"unknown pipeline rejection code: {code}")
    return _json_rejection(request_number, scenario, code, message)


def _unmatched_rejection(request_number: int) -> Dispatch:
    response = json.dumps(UNMATCHED_ERROR, separators=(",", ":")).encode("utf-8")
    return Dispatch(
        status=UNMATCHED_STATUS,
        body=response,
        content_type="application/json; charset=utf-8",
        cache_control="no-store",
        transcript=_transcript_line(request_number, "unmatched", "rejected"),
    )


def _sse_dispatch(
    request_number: int,
    scenario: str,
    phase: str,
    events: list[dict[str, Any]],
) -> Dispatch:
    return Dispatch(
        status=200,
        body=_sse(events),
        content_type="text/event-stream",
        cache_control="no-cache",
        transcript=_transcript_line(request_number, scenario, phase),
    )


def _direct_dispatch(
    request_number: int,
    scenario: str,
    final_text: str,
    output_tokens: int,
) -> Dispatch:
    response_id = f"resp-uat-{request_number}"
    return _sse_dispatch(
        request_number,
        scenario,
        "final",
        [
            _created(response_id),
            _assistant_message(f"msg-uat-{request_number}", final_text),
            _completed(response_id, output_tokens),
        ],
    )


def _dispatch_request(
    body: dict[str, Any],
    request_number: int,
    state: RouteState,
    gate_wait_seconds: float = PIPELINE_GATE_WAIT_SECONDS,
) -> Dispatch:
    matched = _scenario(body)
    if matched is None:
        return _unmatched_rejection(request_number)
    scenario, spec = matched

    if spec.contract is not None and not _request_matches_contract(body, spec.contract):
        return _json_rejection(
            request_number,
            scenario,
            "uat_request_contract_violation",
            "request did not satisfy the fixed UAT scenario contract",
        )

    if spec.mode is ResponseMode.FORBIDDEN:
        return _json_rejection(
            request_number,
            scenario,
            "unexpected_uat_spawn",
            "workflow issued a child request that this UAT requires to remain unspawned",
        )

    if spec.mode is ResponseMode.FAILED:
        response_id = f"resp-uat-{request_number}"
        return _sse_dispatch(
            request_number,
            scenario,
            "failed",
            [_failed(response_id)],
        )

    if spec.mode is ResponseMode.PIPELINE_A0:
        if _function_call_outputs(body):
            code = state.poison_pipeline("pipeline_order_violation")
            return _pipeline_rejection(request_number, scenario, code)
        code = state.claim_pipeline_a0()
        if code is not None:
            return _pipeline_rejection(request_number, scenario, code)
        code = state.complete_pipeline_a0_after_b0(gate_wait_seconds)
        if code is not None:
            return _pipeline_rejection(request_number, scenario, code)
    elif spec.mode is ResponseMode.PIPELINE_A1:
        if _function_call_outputs(body):
            code = state.poison_pipeline("pipeline_order_violation")
            return _pipeline_rejection(request_number, scenario, code)
        code = state.claim_pipeline_a1()
        if code is not None:
            return _pipeline_rejection(request_number, scenario, code)

    tool_mode = spec.mode in {
        ResponseMode.TOOL,
        ResponseMode.PIPELINE_B0,
        ResponseMode.WORKTREE,
    }
    if tool_mode:
        outputs = _function_call_outputs(body)
        if outputs:
            if spec.call_id is None:
                raise ValueError("tool scenario is missing its static call id")
            exact = [item for item in outputs if item.get("call_id") == spec.call_id]
            if len(outputs) != 1 or len(exact) != 1:
                if spec.mode is ResponseMode.PIPELINE_B0:
                    state.poison_pipeline("pipeline_order_violation")
                return _json_rejection(
                    request_number,
                    scenario,
                    "unexpected_uat_tool_output",
                    "request did not contain exactly one output for the expected UAT tool call",
                )
            if spec.mode is ResponseMode.PIPELINE_B0:
                code = state.complete_pipeline_b0()
                if code is not None:
                    return _pipeline_rejection(request_number, scenario, code)

            final_text = spec.final_text
            if spec.mode is ResponseMode.WORKTREE:
                output = exact[0].get("output")
                final_text = _worktree_result(output) if isinstance(output, str) else None
                if final_text is None:
                    return _json_rejection(
                        request_number,
                        scenario,
                        "invalid_worktree_uat_output",
                        "worktree tool output did not satisfy the bounded UAT proof contract",
                    )
            if final_text is None:
                raise ValueError("tool scenario is missing its static final text")
            return _direct_dispatch(
                request_number,
                scenario,
                final_text,
                spec.output_tokens,
            )

        if spec.mode is ResponseMode.PIPELINE_B0:
            code = state.claim_pipeline_b0()
            if code is not None:
                return _pipeline_rejection(request_number, scenario, code)
        response_id = f"resp-uat-{request_number}"
        return _sse_dispatch(
            request_number,
            scenario,
            "tool",
            [
                _created(response_id),
                _function_call(spec),
                _completed(response_id, 7),
            ],
        )

    if spec.final_text is None:
        raise ValueError("direct scenario is missing its static final text")
    return _direct_dispatch(
        request_number,
        scenario,
        spec.final_text,
        spec.output_tokens,
    )


class _Server(ThreadingHTTPServer):
    daemon_threads = True

    def __init__(self, address: tuple[str, int]) -> None:
        super().__init__(address, _Handler)
        self.route_state = RouteState()
        self._request_count = 0
        self._request_count_lock = threading.Lock()

    def next_request_number(self) -> int:
        with self._request_count_lock:
            self._request_count += 1
            return self._request_count


class _Handler(BaseHTTPRequestHandler):
    server: _Server

    def do_GET(self) -> None:
        if self.path != "/healthz":
            self.send_error(404)
            return
        body = b"ok\n"
        self.send_response(200)
        self.send_header("Content-Type", "text/plain; charset=utf-8")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_POST(self) -> None:
        if self.path.split("?", 1)[0] != RESPONSES_PATH:
            self.send_error(404)
            return
        try:
            content_length = int(self.headers.get("Content-Length", "0"))
        except ValueError:
            self.send_error(400, "invalid content length")
            return
        if content_length <= 0 or content_length > MAX_REQUEST_BYTES:
            self.send_error(413, "request body is missing or too large")
            return
        try:
            body = json.loads(self.rfile.read(content_length))
        except (json.JSONDecodeError, UnicodeDecodeError):
            self.send_error(400, "request body is not valid JSON")
            return
        if not isinstance(body, dict):
            self.send_error(400, "request body must be a JSON object")
            return

        request_number = self.server.next_request_number()
        dispatch = _dispatch_request(body, request_number, self.server.route_state)
        print(dispatch.transcript, flush=True)
        self.send_response(dispatch.status)
        self.send_header("Content-Type", dispatch.content_type)
        self.send_header("Cache-Control", dispatch.cache_control)
        self.send_header("Connection", "close")
        self.send_header("Content-Length", str(len(dispatch.body)))
        self.end_headers()
        self.wfile.write(dispatch.body)

    def log_message(self, _format: str, *_args: Any) -> None:
        return


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--port", type=int, default=0)
    args = parser.parse_args()
    if not 0 <= args.port <= 65_535:
        parser.error("--port must be between 0 and 65535")

    server = _Server((HOST, args.port))
    port = server.server_address[1]
    print(
        json.dumps(
            {"base_url": f"http://{HOST}:{port}/v1", "health": f"http://{HOST}:{port}/healthz"},
            separators=(",", ":"),
        ),
        flush=True,
    )
    try:
        server.serve_forever(poll_interval=0.1)
    except KeyboardInterrupt:
        pass
    finally:
        server.server_close()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
