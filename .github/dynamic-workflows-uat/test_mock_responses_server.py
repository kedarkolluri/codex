#!/usr/bin/env python3

import copy
import importlib.util
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import threading
import unittest


MODULE_PATH = Path(__file__).with_name("mock_responses_server.py")
SPEC = importlib.util.spec_from_file_location("mock_responses_server", MODULE_PATH)
assert SPEC is not None and SPEC.loader is not None
mock_server = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = mock_server
SPEC.loader.exec_module(mock_server)


EXPECTED_SCENARIOS = {
    "stop",
    "pause",
    "agent-a",
    "agent-b",
    "monitor-a",
    "monitor-b",
    "failure-null",
    "failure-sibling",
    "failure-schema",
    "budget-0",
    "budget-1",
    "budget-2",
    "budget-3",
    "parallel-fast",
    "parallel-held",
    "pipeline-a0",
    "pipeline-b0",
    "pipeline-a1",
    "worktree",
}

CONTROL_CASES = {
    "stop": ("call-control-uat-stop", "UAT_STOP_FINISHED"),
    "pause": ("call-control-uat-pause", "UAT_PAUSE_FINISHED"),
    "agent-a": ("call-control-uat-agent-a", "UAT_AGENT_A_FINISHED"),
    "agent-b": ("call-control-uat-agent-b", "UAT_AGENT_B_FINISHED"),
    "monitor-a": (
        "call-control-uat-monitor-a",
        "# Dynamic Workflows human UAT fixture",
    ),
    "monitor-b": (
        "call-control-uat-monitor-b",
        "Yes. It says this fixture is disposable.",
    ),
}

VALID_WORKTREE_CWD = (
    "/tmp/fixture/.codex-worktrees-"
    "0190cba5-3f1a-7abc-8def-0123456789ab/agent-2"
)
VALID_WINDOWS_WORKTREE_CWD = (
    "C:\\tmp\\fixture\\.codex-worktrees-"
    "0190cba5-3f1a-7abc-8def-0123456789ab\\agent-2"
)


def _request_with_text(text: str) -> dict[str, object]:
    return {
        "input": [
            {
                "role": "user",
                "content": [{"type": "input_text", "text": text}],
            }
        ]
    }


def _request_for_scenario(scenario: str) -> dict[str, object]:
    return _request_with_text(f"UAT_ROUTE:{scenario}\nfixture instruction")


def _add_contract(
    body: dict[str, object],
    schema: dict[str, object],
    *,
    model: str | None = None,
    effort: str | None = None,
) -> None:
    body["text"] = {
        "format": {
            "type": "json_schema",
            "name": "codex_output_schema",
            "strict": True,
            "schema": copy.deepcopy(schema),
        }
    }
    if model is not None:
        body["model"] = model
    if effort is not None:
        body["reasoning"] = {"effort": effort}


def _add_function_output(
    body: dict[str, object],
    call_id: str,
    output: object,
) -> None:
    inputs = body["input"]
    assert isinstance(inputs, list)
    inputs.append(
        {
            "type": "function_call_output",
            "call_id": call_id,
            "output": output,
        }
    )


def _dispatch(
    scenario: str,
    request_number: int = 1,
    state: object | None = None,
    gate_wait_seconds: float = 0.0,
) -> object:
    route_state = state if state is not None else mock_server.RouteState()
    return mock_server._dispatch_request(
        _request_for_scenario(scenario),
        request_number,
        route_state,
        gate_wait_seconds=gate_wait_seconds,
    )


def _events(dispatch: object) -> list[dict[str, object]]:
    body = dispatch.body.decode("utf-8")
    return [
        json.loads(line.removeprefix("data: "))
        for line in body.splitlines()
        if line.startswith("data: ")
    ]


def _assistant_text(dispatch: object) -> str:
    messages = [
        event["item"]
        for event in _events(dispatch)
        if event.get("type") == "response.output_item.done"
        and isinstance(event.get("item"), dict)
        and event["item"].get("type") == "message"
    ]
    assert len(messages) == 1
    content = messages[0]["content"]
    assert isinstance(content, list) and len(content) == 1
    text = content[0]["text"]
    assert isinstance(text, str)
    return text


def _tool_call(dispatch: object) -> tuple[str, dict[str, object]]:
    calls = [
        event["item"]
        for event in _events(dispatch)
        if event.get("type") == "response.output_item.done"
        and isinstance(event.get("item"), dict)
        and event["item"].get("type") == "function_call"
    ]
    assert len(calls) == 1
    call_id = calls[0]["call_id"]
    arguments = calls[0]["arguments"]
    assert isinstance(call_id, str) and isinstance(arguments, str)
    return call_id, json.loads(arguments)


def _completed_usage(dispatch: object) -> dict[str, object]:
    completions = [
        event["response"]["usage"]
        for event in _events(dispatch)
        if event.get("type") == "response.completed"
    ]
    assert len(completions) == 1
    return completions[0]


def _error_code(dispatch: object) -> str:
    payload = json.loads(dispatch.body)
    return payload["error"]["code"]


def _transcript(dispatch: object) -> dict[str, object]:
    return json.loads(dispatch.transcript)


def _worktree_request() -> dict[str, object]:
    body = _request_for_scenario("worktree")
    _add_contract(body, mock_server.WORKTREE_SCHEMA)
    return body


def _valid_worktree_output(cwd: str = VALID_WORKTREE_CWD) -> str:
    return "\n".join(
        [
            cwd,
            "UAT_WORKTREE_READ=UAT_WORKTREE_MARKER",
            "UAT_WORKTREE_REMOVED=true",
            "UAT_WORKTREE_CLEAN=true",
        ]
    )


class MockResponsesServerTests(unittest.TestCase):
    def test_registry_and_exact_sentinel_routes_cover_current_fixtures(self) -> None:
        self.assertEqual(mock_server.HOST, "127.0.0.1")
        self.assertEqual(set(mock_server.SCENARIOS), EXPECTED_SCENARIOS)
        for scenario in EXPECTED_SCENARIOS:
            with self.subTest(scenario=scenario):
                body = _request_for_scenario(scenario)
                matched = mock_server._scenario(body)
                self.assertIsNotNone(matched)
                self.assertEqual(matched[0], scenario)

        self.assertIsNone(
            mock_server._scenario(_request_for_scenario("nesting-parent"))
        )

    def test_static_commands_are_host_selected_and_refuse_preexisting_markers(self) -> None:
        expected_control = (
            mock_server.WINDOWS_CONTROL_DELAY_COMMAND
            if mock_server.IS_WINDOWS
            else mock_server.POSIX_CONTROL_DELAY_COMMAND
        )
        expected_delay = (
            mock_server.WINDOWS_DELAY_COMMAND
            if mock_server.IS_WINDOWS
            else mock_server.POSIX_DELAY_COMMAND
        )
        expected_worktree = (
            mock_server.WINDOWS_WORKTREE_COMMAND
            if mock_server.IS_WINDOWS
            else mock_server.POSIX_WORKTREE_COMMAND
        )
        self.assertEqual(mock_server.CONTROL_DELAY_COMMAND, expected_control)
        self.assertEqual(mock_server.DELAY_COMMAND, expected_delay)
        self.assertEqual(mock_server.WORKTREE_COMMAND, expected_worktree)

        for scenario in CONTROL_CASES:
            with self.subTest(scenario=scenario):
                self.assertEqual(
                    mock_server.SCENARIOS[scenario].command,
                    expected_control,
                )
        self.assertEqual(
            mock_server.SCENARIOS["parallel-held"].command,
            expected_delay,
        )
        self.assertEqual(
            mock_server.SCENARIOS["pipeline-b0"].command,
            expected_delay,
        )
        self.assertEqual(mock_server.SCENARIOS["worktree"].command, expected_worktree)

        self.assertLess(
            mock_server.POSIX_WORKTREE_COMMAND.index(
                'test ! -e "$marker_file" && test ! -L "$marker_file"'
            ),
            mock_server.POSIX_WORKTREE_COMMAND.index(
                'printf %s UAT_WORKTREE_MARKER > "$marker_file"'
            ),
        )
        self.assertLess(
            mock_server.WINDOWS_WORKTREE_COMMAND.index(
                "if ($null -ne (Get-Item -LiteralPath $markerFile"
            ),
            mock_server.WINDOWS_WORKTREE_COMMAND.index(
                "[System.IO.FileMode]::CreateNew"
            ),
        )

    def test_host_worktree_command_proves_cleanup_and_preserves_existing_marker(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            cwd = Path(directory)
            subprocess.run(
                ["git", "init", "--quiet"],
                cwd=cwd,
                check=True,
                capture_output=True,
                text=True,
            )

            def run_worktree_command() -> subprocess.CompletedProcess[str]:
                command = (
                    [
                        "powershell.exe",
                        "-NoLogo",
                        "-NoProfile",
                        "-NonInteractive",
                        "-Command",
                        mock_server.WORKTREE_COMMAND,
                    ]
                    if mock_server.IS_WINDOWS
                    else ["/bin/sh", "-c", mock_server.WORKTREE_COMMAND]
                )
                return subprocess.run(
                    command,
                    cwd=cwd,
                    check=False,
                    capture_output=True,
                    text=True,
                )

            completed = run_worktree_command()
            self.assertEqual(completed.returncode, 0, completed.stderr)
            self.assertIn("UAT_WORKTREE_READ=UAT_WORKTREE_MARKER", completed.stdout)
            self.assertIn("UAT_WORKTREE_REMOVED=true", completed.stdout)
            self.assertIn("UAT_WORKTREE_CLEAN=true", completed.stdout)

            marker = cwd / "uat-worktree-child-marker.txt"
            self.assertFalse(marker.exists())
            marker.write_text("PREEXISTING", encoding="utf-8")
            rejected = run_worktree_command()
            self.assertNotEqual(rejected.returncode, 0)
            self.assertEqual(marker.read_text(encoding="utf-8"), "PREEXISTING")

    def test_latest_user_and_collision_rules_fail_closed(self) -> None:
        body = _request_for_scenario("stop")
        inputs = body["input"]
        self.assertIsInstance(inputs, list)
        inputs.extend(
            [
                {
                    "role": "assistant",
                    "content": [
                        {"type": "input_text", "text": "UAT_ROUTE:agent-a"}
                    ],
                },
                {
                    "type": "function_call_output",
                    "call_id": "unrelated",
                    "output": "UAT_ROUTE:agent-b",
                },
                {
                    "role": "user",
                    "content": [
                        {"type": "input_text", "text": "UAT_ROUTE:pause\nlatest"}
                    ],
                },
            ]
        )
        self.assertEqual(mock_server._route_sentinel(body), "UAT_ROUTE:pause")

        body = _request_for_scenario("stop")
        body["input"].append(
            {
                "role": "user",
                "content": [{"type": "input_text", "text": "latest has no route"}],
            }
        )
        self.assertIsNone(mock_server._scenario(body))

        rejected_texts = [
            "prefix UAT_ROUTE:stop is inline",
            "UAT_ROUTE:stop trailing-text",
            "UAT_ROUTE:stop\nUAT_ROUTE:pause",
            "UAT_ROUTE:budget-1-extra",
        ]
        for text in rejected_texts:
            with self.subTest(text=text):
                self.assertIsNone(mock_server._scenario(_request_with_text(text)))

        assistant_only = {
            "input": [
                {
                    "role": "assistant",
                    "content": [{"type": "input_text", "text": "UAT_ROUTE:stop"}],
                }
            ]
        }
        self.assertIsNone(mock_server._scenario(assistant_only))

    def test_existing_control_routes_keep_static_tool_and_final_phases(self) -> None:
        for index, (scenario, expected) in enumerate(CONTROL_CASES.items(), start=10):
            expected_call_id, expected_final = expected
            with self.subTest(scenario=scenario, phase="tool"):
                body = _request_for_scenario(scenario)
                state = mock_server.RouteState()
                initial = mock_server._dispatch_request(body, index, state)
                self.assertEqual(initial.status, 200)
                self.assertEqual(
                    _tool_call(initial),
                    (
                        expected_call_id,
                        {
                            "command": mock_server.CONTROL_DELAY_COMMAND,
                            "timeout_ms": 150_000,
                        },
                    ),
                )
                initial_events = _events(initial)
                self.assertEqual(initial_events[0]["response"]["id"], f"resp-uat-{index}")
                self.assertEqual(
                    _transcript(initial),
                    {"request": index, "scenario": scenario, "phase": "tool"},
                )

            with self.subTest(scenario=scenario, phase="final"):
                _add_function_output(body, expected_call_id, "fixture output")
                final = mock_server._dispatch_request(body, index + 100, state)
                self.assertEqual(final.status, 200)
                self.assertEqual(_assistant_text(final), expected_final)
                message_events = [
                    event
                    for event in _events(final)
                    if event.get("type") == "response.output_item.done"
                ]
                self.assertEqual(
                    message_events[0]["item"]["id"], f"msg-uat-{index + 100}"
                )
                self.assertEqual(_completed_usage(final)["output_tokens"], 11)

    def test_tool_outputs_must_be_exact_and_rejections_are_sanitized(self) -> None:
        secret = "TOP-SECRET-TOOL-OUTPUT"
        wrong = _request_for_scenario("parallel-held")
        _add_function_output(wrong, "wrong-call-id", secret)
        rejected = mock_server._dispatch_request(
            wrong, 40, mock_server.RouteState()
        )
        self.assertEqual(rejected.status, 422)
        self.assertEqual(_error_code(rejected), "unexpected_uat_tool_output")
        self.assertNotIn(secret, rejected.body.decode("utf-8"))
        self.assertNotIn(secret, rejected.transcript)

        multiple = _request_for_scenario("parallel-held")
        _add_function_output(multiple, "call-uat-parallel-held", "expected")
        _add_function_output(multiple, "wrong-call-id", secret)
        rejected = mock_server._dispatch_request(
            multiple, 41, mock_server.RouteState()
        )
        self.assertEqual(rejected.status, 422)
        self.assertEqual(_error_code(rejected), "unexpected_uat_tool_output")
        self.assertNotIn(secret, rejected.body.decode("utf-8"))
        self.assertNotIn(secret, rejected.transcript)

    def test_failure_and_sibling_routes_are_deterministic(self) -> None:
        failed = _dispatch("failure-null", request_number=51)
        self.assertEqual(failed.status, 200)
        self.assertEqual(
            _events(failed),
            [
                {
                    "type": "response.failed",
                    "response": {
                        "id": "resp-uat-51",
                        "error": {
                            "code": "invalid_prompt",
                            "message": mock_server.FAILURE_MESSAGE,
                        },
                    },
                }
            ],
        )
        self.assertEqual(
            _transcript(failed),
            {"request": 51, "scenario": "failure-null", "phase": "failed"},
        )

        sibling = _dispatch("failure-sibling", request_number=52)
        self.assertEqual(_assistant_text(sibling), "UAT_SIBLING_SURVIVED")
        self.assertEqual(_completed_usage(sibling)["output_tokens"], 11)

    def test_schema_route_accepts_only_the_fixed_request_contract(self) -> None:
        body = _request_for_scenario("failure-schema")
        _add_contract(
            body,
            mock_server.FAILURE_SCHEMA,
            model="gpt-5.4",
            effort="high",
        )
        accepted = mock_server._dispatch_request(
            body, 60, mock_server.RouteState()
        )
        self.assertEqual(accepted.status, 200)
        self.assertEqual(
            _assistant_text(accepted),
            '{"marker":"UAT_SCHEMA_OPTIONS_OK","answer":"structured"}',
        )
        self.assertEqual(_completed_usage(accepted)["output_tokens"], 21)

    def test_schema_contract_variants_are_rejected_without_request_echo(self) -> None:
        base = _request_for_scenario("failure-schema")
        _add_contract(
            base,
            mock_server.FAILURE_SCHEMA,
            model="gpt-5.4",
            effort="high",
        )
        variants = []

        body = copy.deepcopy(base)
        body["model"] = "wrong-model"
        variants.append(("model", body))
        body = copy.deepcopy(base)
        body["reasoning"] = {"effort": "low"}
        variants.append(("effort", body))
        body = copy.deepcopy(base)
        del body["text"]
        variants.append(("missing-text", body))
        for field, value in [
            ("type", "text"),
            ("name", "wrong-name"),
            ("strict", False),
            ("schema", {"type": "object"}),
        ]:
            body = copy.deepcopy(base)
            body["text"]["format"][field] = value
            variants.append((field, body))
        body = copy.deepcopy(base)
        body["text"]["format"]["unexpected"] = "PRIVATE-CONTRACT-VALUE"
        variants.append(("extra-format-field", body))

        for index, (name, body) in enumerate(variants, start=61):
            body["private"] = "PRIVATE-CONTRACT-VALUE"
            with self.subTest(name=name):
                rejected = mock_server._dispatch_request(
                    body, index, mock_server.RouteState()
                )
                self.assertEqual(rejected.status, 422)
                self.assertEqual(
                    _error_code(rejected), "uat_request_contract_violation"
                )
                self.assertNotIn(
                    "PRIVATE-CONTRACT-VALUE", rejected.body.decode("utf-8")
                )
                self.assertNotIn("PRIVATE-CONTRACT-VALUE", rejected.transcript)

    def test_budget_routes_account_exactly_and_forbid_excess_spawns(self) -> None:
        provider_requests = []
        for ordinal in range(2):
            dispatch = _dispatch(f"budget-{ordinal}", request_number=70 + ordinal)
            provider_requests.append(dispatch)
            self.assertEqual(_assistant_text(dispatch), f"UAT_BUDGET_AGENT_{ordinal}")
            self.assertEqual(_completed_usage(dispatch)["output_tokens"], 18)
            self.assertEqual(_completed_usage(dispatch)["total_tokens"], 18)
        self.assertEqual(len(provider_requests), 2)
        self.assertEqual(
            sum(_completed_usage(item)["output_tokens"] for item in provider_requests),
            36,
        )

        for ordinal in (2, 3):
            with self.subTest(forbidden_ordinal=ordinal):
                body = _request_for_scenario(f"budget-{ordinal}")
                body["private"] = "PRIVATE-BUDGET-CONTENT"
                rejected = mock_server._dispatch_request(
                    body, 70 + ordinal, mock_server.RouteState()
                )
                self.assertEqual(rejected.status, 422)
                self.assertEqual(_error_code(rejected), "unexpected_uat_spawn")
                self.assertNotIn(
                    "PRIVATE-BUDGET-CONTENT", rejected.body.decode("utf-8")
                )
                self.assertNotIn("PRIVATE-BUDGET-CONTENT", rejected.transcript)

    def test_parallel_routes_require_three_provider_requests(self) -> None:
        state = mock_server.RouteState()
        fast = mock_server._dispatch_request(
            _request_for_scenario("parallel-fast"), 80, state
        )
        held_body = _request_for_scenario("parallel-held")
        held_tool = mock_server._dispatch_request(held_body, 81, state)
        self.assertEqual(_assistant_text(fast), "UAT_PARALLEL_FAST_DONE")
        self.assertEqual(
            _tool_call(held_tool),
            (
                "call-uat-parallel-held",
                {
                    "command": mock_server.DELAY_COMMAND,
                    "timeout_ms": 20_000,
                    "login": False,
                },
            ),
        )

        _add_function_output(
            held_body, "call-uat-parallel-held", "static shell completion"
        )
        held_final = mock_server._dispatch_request(held_body, 82, state)
        self.assertEqual(_assistant_text(held_final), "UAT_PARALLEL_HELD_DONE")
        self.assertEqual(len([fast, held_tool, held_final]), 3)
        self.assertNotIn("parallel-after", mock_server.SCENARIOS)

    def test_pipeline_gate_proves_a1_arrives_while_b0_is_held(self) -> None:
        state = mock_server.RouteState()
        a0_body = _request_for_scenario("pipeline-a0")
        a0_results = []

        def dispatch_a0() -> None:
            a0_results.append(
                mock_server._dispatch_request(
                    a0_body,
                    90,
                    state,
                    gate_wait_seconds=1.0,
                )
            )

        thread = threading.Thread(target=dispatch_a0)
        thread.start()
        self.assertTrue(state.pipeline_a0_waiting.wait(timeout=0.25))
        self.assertTrue(thread.is_alive())

        b0_body = _request_for_scenario("pipeline-b0")
        b0_tool = mock_server._dispatch_request(b0_body, 91, state)
        self.assertEqual(
            _tool_call(b0_tool),
            (
                "call-uat-pipeline-b0",
                {
                    "command": mock_server.DELAY_COMMAND,
                    "timeout_ms": 20_000,
                    "login": False,
                },
            ),
        )
        thread.join(timeout=0.25)
        self.assertFalse(thread.is_alive())
        self.assertEqual(len(a0_results), 1)
        self.assertEqual(_assistant_text(a0_results[0]), "UAT_PIPELINE_A0_DONE")

        a1 = mock_server._dispatch_request(
            _request_for_scenario("pipeline-a1"), 92, state
        )
        self.assertEqual(_assistant_text(a1), "UAT_PIPELINE_A1_DONE")
        _add_function_output(b0_body, "call-uat-pipeline-b0", "held call finished")
        b0_final = mock_server._dispatch_request(b0_body, 93, state)
        self.assertEqual(_assistant_text(b0_final), "UAT_PIPELINE_B0_DONE")
        self.assertEqual(len([a0_results[0], b0_tool, a1, b0_final]), 4)

        reused_bodies = {
            "pipeline-a0": _request_for_scenario("pipeline-a0"),
            "pipeline-b0": _request_for_scenario("pipeline-b0"),
            "pipeline-a1": _request_for_scenario("pipeline-a1"),
        }
        for offset, (scenario, body) in enumerate(reused_bodies.items(), start=94):
            with self.subTest(reused_scenario=scenario):
                reused = mock_server._dispatch_request(
                    body,
                    offset,
                    state,
                    gate_wait_seconds=0.0,
                )
                self.assertEqual(reused.status, 422)
                self.assertEqual(_error_code(reused), "pipeline_reuse")

    def test_pipeline_accepts_either_stage_one_arrival_order(self) -> None:
        state = mock_server.RouteState()
        b0_body = _request_for_scenario("pipeline-b0")
        b0_tool = mock_server._dispatch_request(b0_body, 97, state)
        self.assertEqual(b0_tool.status, 200)
        self.assertTrue(state.pipeline_b0_seen.is_set())

        a0 = mock_server._dispatch_request(
            _request_for_scenario("pipeline-a0"),
            98,
            state,
            gate_wait_seconds=0.0,
        )
        self.assertEqual(_assistant_text(a0), "UAT_PIPELINE_A0_DONE")
        a1 = mock_server._dispatch_request(
            _request_for_scenario("pipeline-a1"), 99, state
        )
        self.assertEqual(_assistant_text(a1), "UAT_PIPELINE_A1_DONE")
        _add_function_output(b0_body, "call-uat-pipeline-b0", "held call finished")
        b0_final = mock_server._dispatch_request(b0_body, 100, state)
        self.assertEqual(_assistant_text(b0_final), "UAT_PIPELINE_B0_DONE")

    def test_pipeline_timeouts_order_violations_and_state_reset_fail_closed(self) -> None:
        timed_out_state = mock_server.RouteState()
        timed_out = _dispatch(
            "pipeline-a0",
            request_number=100,
            state=timed_out_state,
            gate_wait_seconds=0.0,
        )
        self.assertEqual(timed_out.status, 422)
        self.assertEqual(_error_code(timed_out), "pipeline_gate_timeout")
        after_timeout = _dispatch(
            "pipeline-b0",
            request_number=101,
            state=timed_out_state,
        )
        self.assertEqual(_error_code(after_timeout), "pipeline_reuse")

        first_state = mock_server.RouteState()
        b0_body = _request_for_scenario("pipeline-b0")
        initial = mock_server._dispatch_request(b0_body, 102, first_state)
        self.assertEqual(initial.status, 200)
        _add_function_output(b0_body, "call-uat-pipeline-b0", "completed too soon")
        out_of_order = mock_server._dispatch_request(b0_body, 103, first_state)
        self.assertEqual(out_of_order.status, 422)
        self.assertEqual(_error_code(out_of_order), "pipeline_order_violation")
        poisoned = mock_server._dispatch_request(
            _request_for_scenario("pipeline-a0"),
            104,
            first_state,
            gate_wait_seconds=0.0,
        )
        self.assertEqual(_error_code(poisoned), "pipeline_reuse")

        fresh_state = mock_server.RouteState()
        self.assertFalse(fresh_state.pipeline_a0_waiting.is_set())
        self.assertFalse(fresh_state.pipeline_b0_seen.is_set())
        self.assertFalse(fresh_state.pipeline_a1_seen.is_set())

    def test_worktree_initial_request_uses_only_the_host_static_command(self) -> None:
        body = _worktree_request()
        body["private_command"] = "rm -rf PRIVATE-WORKTREE-CONTENT"
        initial = mock_server._dispatch_request(body, 110, mock_server.RouteState())
        self.assertEqual(initial.status, 200)
        call_id, arguments = _tool_call(initial)
        self.assertEqual(call_id, "call-uat-worktree")
        self.assertEqual(arguments["command"], mock_server.WORKTREE_COMMAND)
        self.assertEqual(arguments["timeout_ms"], 20_000)
        self.assertIs(arguments["login"], False)
        self.assertIn("UAT_WORKTREE_MARKER", arguments["command"])
        self.assertIn("git status --porcelain", arguments["command"])
        self.assertNotIn("PRIVATE-WORKTREE-CONTENT", arguments["command"])
        if mock_server.IS_WINDOWS:
            self.assertIn("Write-Output (Get-Location).Path", arguments["command"])
            self.assertIn("Start-Sleep -Seconds 6", arguments["command"])
            self.assertIn("Remove-Item -LiteralPath $markerFile", arguments["command"])
        else:
            self.assertIn("pwd", arguments["command"])
            self.assertIn("sleep 6", arguments["command"])
            self.assertIn('rm "$marker_file"', arguments["command"])

    def test_worktree_followup_returns_only_validated_structured_proof(self) -> None:
        for index, cwd in enumerate(
            (VALID_WORKTREE_CWD, VALID_WINDOWS_WORKTREE_CWD),
            start=111,
        ):
            with self.subTest(cwd=cwd):
                body = _worktree_request()
                output = _valid_worktree_output(cwd)
                _add_function_output(body, "call-uat-worktree", output)
                final = mock_server._dispatch_request(
                    body, index, mock_server.RouteState()
                )
                self.assertEqual(final.status, 200)
                self.assertEqual(
                    json.loads(_assistant_text(final)),
                    {
                        "cwd": cwd,
                        "marker": "UAT_WORKTREE_MARKER",
                        "removed": True,
                        "clean": True,
                    },
                )
                self.assertEqual(_completed_usage(final)["output_tokens"], 11)

    def test_worktree_output_parser_is_bounded_and_fail_closed(self) -> None:
        private = "PRIVATE-WORKTREE-OUTPUT"
        uuid_v4_cwd = (
            "/tmp/fixture/.codex-worktrees-"
            "0190cba5-3f1a-4abc-8def-0123456789ab/agent-2"
        )
        uppercase_cwd = VALID_WORKTREE_CWD.replace("cba", "CBA")
        traversing_cwd = VALID_WORKTREE_CWD.replace(
            "/.codex-worktrees-", "/../.codex-worktrees-"
        )
        windows_traversing_cwd = VALID_WINDOWS_WORKTREE_CWD.replace(
            "\\.codex-worktrees-", "\\..\\.codex-worktrees-"
        )
        windows_invalid_cwd = VALID_WINDOWS_WORKTREE_CWD.replace(
            "\\fixture\\", "\\fixture?private\\"
        )
        windows_mixed_separators = VALID_WINDOWS_WORKTREE_CWD.replace(
            "C:\\tmp\\", "C:/tmp/"
        )
        too_long_cwd = (
            "/tmp/"
            + "a" * mock_server.WORKTREE_CWD_MAX_CHARS
            + "/.codex-worktrees-0190cba5-3f1a-7abc-8def-0123456789ab/agent-2"
        )
        invalid_outputs = {
            "relative": _valid_worktree_output(VALID_WORKTREE_CWD.removeprefix("/")),
            "uuid-v4": _valid_worktree_output(uuid_v4_cwd),
            "uppercase": _valid_worktree_output(uppercase_cwd),
            "traversal": _valid_worktree_output(traversing_cwd),
            "windows-traversal": _valid_worktree_output(windows_traversing_cwd),
            "windows-invalid-character": _valid_worktree_output(windows_invalid_cwd),
            "windows-mixed-separators": _valid_worktree_output(
                windows_mixed_separators
            ),
            "windows-relative": _valid_worktree_output(
                VALID_WINDOWS_WORKTREE_CWD.removeprefix("C:\\")
            ),
            "duplicate-cwd": f"{_valid_worktree_output()}\n{VALID_WORKTREE_CWD}",
            "overlong-cwd": _valid_worktree_output(too_long_cwd),
            "missing-marker": f"{VALID_WORKTREE_CWD}\n{private}",
            "oversized-output": private
            + "x" * (mock_server.WORKTREE_OUTPUT_MAX_BYTES + 1),
        }
        for index, (name, output) in enumerate(invalid_outputs.items(), start=120):
            with self.subTest(name=name):
                body = _worktree_request()
                _add_function_output(body, "call-uat-worktree", output)
                rejected = mock_server._dispatch_request(
                    body, index, mock_server.RouteState()
                )
                self.assertEqual(rejected.status, 422)
                self.assertEqual(_error_code(rejected), "invalid_worktree_uat_output")
                self.assertNotIn(private, rejected.body.decode("utf-8"))
                self.assertNotIn(private, rejected.transcript)

        body = _worktree_request()
        _add_function_output(body, "call-uat-worktree", 123)
        rejected = mock_server._dispatch_request(
            body, 130, mock_server.RouteState()
        )
        self.assertEqual(_error_code(rejected), "invalid_worktree_uat_output")

        body = _worktree_request()
        _add_function_output(body, "wrong-call-id", private)
        rejected = mock_server._dispatch_request(
            body, 131, mock_server.RouteState()
        )
        self.assertEqual(_error_code(rejected), "unexpected_uat_tool_output")
        self.assertNotIn(private, rejected.body.decode("utf-8"))
        self.assertNotIn(private, rejected.transcript)

    def test_unmatched_and_all_transcripts_are_sanitized_bounded_records(self) -> None:
        private = "TOP-SECRET-UNMATCHED-UAT-CONTENT"
        first = mock_server._dispatch_request(
            _request_with_text(private), 140, mock_server.RouteState()
        )
        second = mock_server._dispatch_request(
            _request_with_text(private), 140, mock_server.RouteState()
        )
        self.assertEqual(first, second)
        self.assertEqual(first.status, 422)
        self.assertEqual(json.loads(first.body), mock_server.UNMATCHED_ERROR)
        self.assertNotIn(private, first.body.decode("utf-8"))
        self.assertNotIn(private, first.transcript)

        dispatches = [
            first,
            _dispatch("failure-null", request_number=141),
            _dispatch("failure-sibling", request_number=142),
            _dispatch("parallel-held", request_number=143),
            _dispatch("budget-2", request_number=144),
        ]
        expected_phases = ["rejected", "failed", "final", "tool", "rejected"]
        for dispatch, expected_phase in zip(dispatches, expected_phases):
            transcript = _transcript(dispatch)
            self.assertEqual(set(transcript), {"request", "scenario", "phase"})
            self.assertEqual(transcript["phase"], expected_phase)
            self.assertLess(len(dispatch.transcript), 100)


if __name__ == "__main__":
    unittest.main()
