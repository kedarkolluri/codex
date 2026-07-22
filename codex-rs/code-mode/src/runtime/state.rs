use std::collections::HashMap;
use std::sync::mpsc as std_mpsc;

use codex_code_mode_protocol::EnabledToolMetadata;
use codex_code_mode_protocol::ExecuteOutputPolicy;
use serde_json::Value as JsonValue;
use tokio::sync::mpsc;

use super::RuntimeCommand;
use super::RuntimeEvent;
use super::timers;

pub(crate) struct RuntimeState {
    pub(super) event_tx: mpsc::UnboundedSender<RuntimeEvent>,
    pub(super) pending_tool_calls: HashMap<String, v8::Global<v8::PromiseResolver>>,
    pub(super) pending_timeouts: HashMap<u64, timers::ScheduledTimeout>,
    pub(super) stored_values: HashMap<String, JsonValue>,
    pub(super) stored_value_writes: HashMap<String, JsonValue>,
    pub(super) enabled_tools: Vec<EnabledToolMetadata>,
    pub(super) next_tool_call_id: u64,
    pub(super) next_timeout_id: u64,
    pub(super) tool_call_id: String,
    pub(super) runtime_command_tx: std_mpsc::Sender<RuntimeCommand>,
    pub(super) exit_requested: bool,
    pub(super) output_policy: ExecuteOutputPolicy,
}
