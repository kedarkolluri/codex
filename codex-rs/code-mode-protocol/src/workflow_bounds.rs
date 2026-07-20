//! Shared bounds for workflow-authored fields that cross process, UI, journal, or model seams.

use std::io;
use std::io::Write;

use crate::response::FunctionCallOutputContentItem;
use crate::workflow_meta::ParsedWorkflowMeta;
use serde_json::Value as JsonValue;

/// Maximum UTF-8 byte length of a saved or nested workflow name.
pub const WORKFLOW_NAME_MAX_BYTES: usize = 256;
/// Maximum UTF-8 byte length of a saved workflow description.
pub const WORKFLOW_DESCRIPTION_MAX_BYTES: usize = 4 * 1024;
/// Maximum number of statically declared workflow phases.
pub const WORKFLOW_PHASES_MAX_ITEMS: usize = 256;
/// Maximum number of topology nodes one workflow runtime may allocate.
pub const WORKFLOW_TOPOLOGY_MAX_NODES: u64 = 4_000;
/// Maximum number of dynamic log events one workflow runtime may emit.
pub const WORKFLOW_LOG_MAX_EVENTS: u64 = 4_000;
/// Maximum number of dynamic phase events one workflow runtime may emit.
pub const WORKFLOW_PHASE_MAX_EVENTS: u64 = 1_000;
/// Maximum retry generation for one workflow agent after its initial attempt.
pub const WORKFLOW_AGENT_MAX_RETRIES: u32 = 5;
/// Maximum UTF-8 byte length of a static or dynamic workflow phase title.
pub const WORKFLOW_PHASE_TITLE_MAX_BYTES: usize = 512;
/// Maximum UTF-8 byte length of one workflow log event.
pub const WORKFLOW_LOG_MESSAGE_MAX_BYTES: usize = 4 * 1024;
/// Maximum serialized byte length of a workflow argument value.
pub const WORKFLOW_ARGS_MAX_BYTES: usize = 32 * 1024;
/// Maximum raw byte length of one model-authored `workflow_run` argument object.
///
/// The raw function-call arguments remain in parent model history, so this bound includes JSON
/// whitespace and escaping that disappear when the nested `args` value is reserialized.
pub const WORKFLOW_MODEL_CALL_MAX_BYTES: usize = 4 * 1024;
/// Maximum serialized byte length of model-authored workflow `args`.
///
/// The smaller nested-value cap leaves room inside [`WORKFLOW_MODEL_CALL_MAX_BYTES`] for the saved
/// workflow name, optional resume ID, and JSON envelope. Non-model entrypoints retain the larger
/// execution cap above.
pub const WORKFLOW_MODEL_ARGS_MAX_BYTES: usize = 3 * 1024;
/// Maximum UTF-8 byte length of a workflow agent prompt before IPC.
///
/// A byte-level ceiling below 10K also hard-bounds byte-fallback tokenizers; the core applies its
/// normal token estimator independently before creating a child request.
pub const WORKFLOW_AGENT_PROMPT_MAX_BYTES: usize = 8 * 1024;
/// Maximum UTF-8 byte length of a workflow agent display label.
pub const WORKFLOW_AGENT_LABEL_MAX_BYTES: usize = 512;
/// Maximum UTF-8 byte length of a workflow agent string option.
pub const WORKFLOW_AGENT_OPTION_MAX_BYTES: usize = 256;
/// Maximum serialized byte length of a workflow agent structured-output schema.
///
/// Schemas become model response-format context, so this byte ceiling guarantees that one schema
/// cannot cross the repository's 10K-token per-item review boundary even under byte fallback.
pub const WORKFLOW_AGENT_SCHEMA_MAX_BYTES: usize = 8 * 1024;
/// Maximum nesting depth of a workflow agent structured-output schema.
pub const WORKFLOW_AGENT_SCHEMA_MAX_DEPTH: usize = 64;
/// Maximum number of content items a workflow run may return across all runtime yields.
pub const WORKFLOW_OUTPUT_MAX_ITEMS: usize = 256;
/// Maximum aggregate serialized payload bytes a workflow run may return across all runtime yields.
///
/// This intentionally matches the durable workflow return cap. Text and image URL payloads both
/// count toward it, including `data:` URLs, while the separate item cap bounds envelope overhead.
pub const WORKFLOW_OUTPUT_MAX_BYTES: usize = 32 * 1024;

/// Incremental, transactional accounting for workflow runtime output across yield boundaries.
///
/// [`Self::admit`] validates a prospective chunk without cloning it or serializing the already
/// accumulated output. State changes only after every item in the chunk fits, so callers can fail
/// closed without partially appending an oversized response.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct WorkflowOutputBounds {
    item_count: usize,
    serialized_bytes: usize,
}

impl WorkflowOutputBounds {
    /// Admit one runtime-response chunk against the aggregate workflow output caps.
    pub fn admit(&mut self, items: &[FunctionCallOutputContentItem]) -> Result<(), String> {
        let item_count = self
            .item_count
            .checked_add(items.len())
            .ok_or_else(output_item_limit_error)?;
        if item_count > WORKFLOW_OUTPUT_MAX_ITEMS {
            return Err(output_item_limit_error());
        }

        let mut serialized_bytes = self.serialized_bytes;
        for item in items {
            let payload = match item {
                FunctionCallOutputContentItem::InputText { text } => text,
                FunctionCallOutputContentItem::InputImage { image_url, .. } => image_url,
            };
            let remaining = WORKFLOW_OUTPUT_MAX_BYTES
                .checked_sub(serialized_bytes)
                .ok_or_else(output_byte_limit_error)?;
            let payload_bytes = serialized_payload_len(payload, remaining)?;
            serialized_bytes = serialized_bytes
                .checked_add(payload_bytes)
                .ok_or_else(output_byte_limit_error)?;
        }

        self.item_count = item_count;
        self.serialized_bytes = serialized_bytes;
        Ok(())
    }

    /// Number of content items admitted so far.
    pub fn item_count(self) -> usize {
        self.item_count
    }

    /// Aggregate serialized text/image payload bytes admitted so far.
    pub fn serialized_bytes(self) -> usize {
        self.serialized_bytes
    }
}

fn serialized_payload_len(payload: &str, max_bytes: usize) -> Result<usize, String> {
    let mut writer = BoundedCountingWriter::new(max_bytes);
    if let Err(error) = serde_json::to_writer(&mut writer, payload) {
        return if writer.exceeded {
            Err(output_byte_limit_error())
        } else {
            Err(format!("failed to serialize workflow output: {error}"))
        };
    }
    Ok(writer.len)
}

struct BoundedCountingWriter {
    len: usize,
    max_bytes: usize,
    exceeded: bool,
}

impl BoundedCountingWriter {
    fn new(max_bytes: usize) -> Self {
        Self {
            len: 0,
            max_bytes,
            exceeded: false,
        }
    }
}

impl Write for BoundedCountingWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let Some(next_len) = self.len.checked_add(buf.len()) else {
            self.exceeded = true;
            return Err(io::Error::other("workflow output byte cap exceeded"));
        };
        if next_len > self.max_bytes {
            self.exceeded = true;
            return Err(io::Error::other("workflow output byte cap exceeded"));
        }
        self.len = next_len;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn output_item_limit_error() -> String {
    format!("workflow output item cap exceeded (maximum {WORKFLOW_OUTPUT_MAX_ITEMS})")
}

fn output_byte_limit_error() -> String {
    format!("workflow output byte cap exceeded (maximum {WORKFLOW_OUTPUT_MAX_BYTES})")
}

/// Validate a saved or nested workflow name.
pub fn ensure_workflow_name(name: &str) -> Result<(), String> {
    ensure_nonempty_bounded_text("workflow name", name, WORKFLOW_NAME_MAX_BYTES)
}

/// Validate a static or dynamic workflow phase title.
pub fn ensure_workflow_phase_title(title: &str) -> Result<(), String> {
    ensure_nonempty_bounded_text(
        "workflow phase title",
        title,
        WORKFLOW_PHASE_TITLE_MAX_BYTES,
    )
}

/// Validate one workflow log message.
pub fn ensure_workflow_log_message(message: &str) -> Result<(), String> {
    ensure_nonempty_bounded_text(
        "workflow log message",
        message,
        WORKFLOW_LOG_MESSAGE_MAX_BYTES,
    )
}

/// Validate the serialized size of a workflow argument value.
pub fn ensure_workflow_args(args: &JsonValue) -> Result<(), String> {
    ensure_workflow_args_with_cap(args, WORKFLOW_ARGS_MAX_BYTES, "execution")
}

/// Validate model-authored workflow arguments before starting a saved workflow.
pub fn ensure_workflow_model_args(args: &JsonValue) -> Result<(), String> {
    ensure_workflow_args_with_cap(args, WORKFLOW_MODEL_ARGS_MAX_BYTES, "model-context")
}

fn ensure_workflow_args_with_cap(
    args: &JsonValue,
    max_bytes: usize,
    cap_name: &str,
) -> Result<(), String> {
    let serialized_len = serde_json::to_vec(args)
        .map_err(|error| format!("failed to serialize workflow arguments: {error}"))?
        .len();
    if serialized_len > max_bytes {
        return Err(format!(
            "workflow args exceed the {max_bytes}-byte {cap_name} cap"
        ));
    }
    Ok(())
}

/// Validate a workflow agent display label.
pub fn ensure_workflow_agent_label(label: &str) -> Result<(), String> {
    ensure_bounded_text(
        "workflow agent label",
        label,
        WORKFLOW_AGENT_LABEL_MAX_BYTES,
    )
}

/// Validate a workflow agent prompt before it crosses the runtime transport.
pub fn ensure_workflow_agent_prompt(prompt: &str) -> Result<(), String> {
    ensure_bounded_text(
        "workflow agent prompt",
        prompt,
        WORKFLOW_AGENT_PROMPT_MAX_BYTES,
    )
}

/// Validate a workflow agent structured-output schema before it crosses host dispatch.
pub fn ensure_workflow_agent_schema(schema: &JsonValue) -> Result<(), String> {
    let serialized_len = serde_json::to_vec(schema)
        .map_err(|error| format!("opts.schema could not be serialized: {error}"))?
        .len();
    if serialized_len > WORKFLOW_AGENT_SCHEMA_MAX_BYTES {
        return Err(format!(
            "opts.schema is too large ({serialized_len} bytes > {WORKFLOW_AGENT_SCHEMA_MAX_BYTES} byte cap)"
        ));
    }
    let depth = json_depth(schema, WORKFLOW_AGENT_SCHEMA_MAX_DEPTH);
    if depth > WORKFLOW_AGENT_SCHEMA_MAX_DEPTH {
        return Err(format!(
            "opts.schema nesting is too deep (exceeds the {WORKFLOW_AGENT_SCHEMA_MAX_DEPTH}-level cap)"
        ));
    }
    Ok(())
}

/// Validate a named workflow agent string option such as model, effort, role, or isolation.
pub fn ensure_workflow_agent_option(field: &str, value: &str) -> Result<(), String> {
    ensure_bounded_text(field, value, WORKFLOW_AGENT_OPTION_MAX_BYTES)
}

pub(crate) fn ensure_parsed_workflow_meta(meta: &ParsedWorkflowMeta) -> Result<(), String> {
    ensure_workflow_name(&meta.name).map_err(|error| format!("`meta.name` {error}"))?;
    ensure_bounded_text(
        "`meta.description`",
        &meta.description,
        WORKFLOW_DESCRIPTION_MAX_BYTES,
    )?;
    if meta.phases.len() > WORKFLOW_PHASES_MAX_ITEMS {
        return Err(format!(
            "`meta.phases` has {} entries; at most {WORKFLOW_PHASES_MAX_ITEMS} are allowed",
            meta.phases.len()
        ));
    }
    for (index, title) in meta.phases.iter().enumerate() {
        ensure_workflow_phase_title(title)
            .map_err(|error| format!("`meta.phases[{index}]` {error}"))?;
    }
    Ok(())
}

fn ensure_nonempty_bounded_text(field: &str, value: &str, max_bytes: usize) -> Result<(), String> {
    if value.trim().is_empty() {
        return Err(format!("{field} must not be empty"));
    }
    ensure_bounded_text(field, value, max_bytes)
}

fn ensure_bounded_text(field: &str, value: &str, max_bytes: usize) -> Result<(), String> {
    if value.len() > max_bytes {
        return Err(format!(
            "{field} exceeds the {max_bytes}-byte limit (got {} bytes)",
            value.len()
        ));
    }
    Ok(())
}

fn json_depth(value: &JsonValue, limit: usize) -> usize {
    let mut max_depth = 0usize;
    let mut stack = vec![(value, 1usize)];
    while let Some((node, depth)) = stack.pop() {
        max_depth = max_depth.max(depth);
        if depth > limit {
            return max_depth;
        }
        match node {
            JsonValue::Array(items) => {
                for item in items {
                    stack.push((item, depth + 1));
                }
            }
            JsonValue::Object(object) => {
                for value in object.values() {
                    stack.push((value, depth + 1));
                }
            }
            JsonValue::Null | JsonValue::Bool(_) | JsonValue::Number(_) | JsonValue::String(_) => {}
        }
    }
    max_depth
}

#[cfg(test)]
#[path = "workflow_bounds_tests.rs"]
mod tests;
