use codex_code_mode_protocol::CellId;
use codex_code_mode_protocol::RuntimeResponse;
use codex_code_mode_protocol::WaitOutcome;
use codex_code_mode_protocol::WaitRequest;
use codex_code_mode_protocol::host::DelegateRequest;
use codex_code_mode_protocol::host::HostResponse;
use codex_code_mode_protocol::host::HostToClient;
use codex_code_mode_protocol::host::InvalidWireCellId;
use codex_code_mode_protocol::host::WireCellId;
use codex_code_mode_protocol::host::WireResult;
use codex_code_mode_protocol::host::WireRuntimeResponse;
use codex_code_mode_protocol::host::WireWaitOutcome;
use codex_code_mode_protocol::host::WireWaitRequest;

use super::RemoteSession;

pub(super) fn public_cell_id(generation: u64, cell_id: &WireCellId) -> CellId {
    if generation == 1 {
        CellId::new(cell_id.as_str().to_string())
    } else {
        CellId::new(format!("g{generation}:{}", cell_id.as_str()))
    }
}

pub(super) fn remote_cell_id(
    session: &RemoteSession,
    cell_id: &CellId,
) -> Result<WireCellId, String> {
    if session.generation == 1 {
        if cell_id.as_str().starts_with('g') && cell_id.as_str().contains(':') {
            return Err(format!(
                "cell {cell_id} belongs to a stale code-mode host generation"
            ));
        }
        return WireCellId::try_new(cell_id.as_str()).map_err(|err| err.to_string());
    }
    let prefix = format!("g{}:", session.generation);
    let Some(remote_id) = cell_id.as_str().strip_prefix(&prefix) else {
        return Err(format!(
            "cell {cell_id} belongs to a stale code-mode host generation"
        ));
    };
    WireCellId::try_new(remote_id).map_err(|err| err.to_string())
}

pub(super) fn remote_wait_request(
    session: &RemoteSession,
    request: WaitRequest,
) -> Result<WireWaitRequest, String> {
    Ok(WireWaitRequest {
        cell_id: remote_cell_id(session, &request.cell_id)?,
        yield_time_ms: request.yield_time_ms,
    })
}

pub(super) fn public_runtime_response(
    cell_id: CellId,
    response: RuntimeResponse,
) -> RuntimeResponse {
    match response {
        RuntimeResponse::Yielded { content_items, .. } => RuntimeResponse::Yielded {
            cell_id,
            content_items,
        },
        RuntimeResponse::Terminated { content_items, .. } => RuntimeResponse::Terminated {
            cell_id,
            content_items,
        },
        RuntimeResponse::Result {
            content_items,
            error_text,
            ..
        } => RuntimeResponse::Result {
            cell_id,
            content_items,
            error_text,
        },
    }
}

pub(super) fn public_wait_outcome(cell_id: CellId, outcome: WaitOutcome) -> WaitOutcome {
    match outcome {
        WaitOutcome::LiveCell(response) => {
            WaitOutcome::LiveCell(public_runtime_response(cell_id, response))
        }
        WaitOutcome::MissingCell(response) => {
            WaitOutcome::MissingCell(public_runtime_response(cell_id, response))
        }
    }
}

pub(super) fn runtime_response_cell_id(response: &WireRuntimeResponse) -> &WireCellId {
    match response {
        WireRuntimeResponse::Yielded { cell_id, .. }
        | WireRuntimeResponse::Terminated { cell_id, .. }
        | WireRuntimeResponse::Result { cell_id, .. } => cell_id,
    }
}

pub(super) fn wait_outcome_cell_id(outcome: &WireWaitOutcome) -> &WireCellId {
    match outcome {
        WireWaitOutcome::LiveCell(response) | WireWaitOutcome::MissingCell(response) => {
            runtime_response_cell_id(response)
        }
    }
}

pub(super) fn validate_host_cell_ids(message: &HostToClient) -> Result<(), InvalidWireCellId> {
    match message {
        HostToClient::Response {
            result: WireResult::Ok { value },
            ..
        } => match value {
            HostResponse::ExecutionStarted { cell_id } => cell_id.validate(),
            HostResponse::WaitCompleted { outcome } => wait_outcome_cell_id(outcome).validate(),
            HostResponse::SessionReady { .. } | HostResponse::SessionClosed { .. } => Ok(()),
        },
        HostToClient::InitialResponse {
            result: WireResult::Ok { value },
            ..
        } => runtime_response_cell_id(value).validate(),
        HostToClient::DelegateRequest { request, .. } => match request {
            DelegateRequest::InvokeTool { invocation } => invocation.cell_id.validate(),
            DelegateRequest::Notify { cell_id, .. } => cell_id.validate(),
        },
        HostToClient::CellClosed { cell_id, .. } => cell_id.validate(),
        HostToClient::HostHello(_)
        | HostToClient::HandshakeRejected { .. }
        | HostToClient::Response {
            result: WireResult::Err { .. },
            ..
        }
        | HostToClient::InitialResponse {
            result: WireResult::Err { .. },
            ..
        }
        | HostToClient::CancelDelegateRequest { .. } => Ok(()),
    }
}

#[cfg(test)]
#[path = "cell_ids_tests.rs"]
mod tests;
