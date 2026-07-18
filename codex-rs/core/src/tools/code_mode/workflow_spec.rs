use codex_tools::FreeformTool;
use codex_tools::FreeformToolFormat;
use codex_tools::ToolSpec;

use super::WORKFLOW_TOOL_NAME;

/// Freeform grammar for the workflow tool. Identical in shape to the code-mode
/// `exec` grammar (optional `// @exec:` pragma line followed by raw source); the
/// body is a workflow ES module that opens with a static `export const meta`
/// manifest.
const WORKFLOW_FREEFORM_GRAMMAR: &str = r#"
start: pragma_source | plain_source
pragma_source: PRAGMA_LINE NEWLINE SOURCE
plain_source: SOURCE

PRAGMA_LINE: /[ \t]*\/\/ @exec:[^\r\n]*/
NEWLINE: /\r?\n/
SOURCE: /[\s\S]+/
"#;

/// Build the [`ToolSpec`] for the workflow host tool. This is a skeleton spec:
/// it advertises the raw-source freeform interface only. Rich per-workflow
/// descriptions (args schema, phases, budget) arrive in later phases.
pub(crate) fn create_workflow_tool() -> ToolSpec {
    ToolSpec::Freeform(FreeformTool {
        name: WORKFLOW_TOOL_NAME.to_string(),
        description: "Run a dynamic workflow: raw JavaScript source that opens with a static \
             `export const meta = { name, description, phases }` manifest followed by the \
             workflow body. The manifest is validated before execution and the body then runs \
             exactly once in a fresh isolate. Orchestration hooks (agent handoff, journal, \
             budget) arrive in later milestones."
            .to_string(),
        format: FreeformToolFormat {
            r#type: "grammar".to_string(),
            syntax: "lark".to_string(),
            definition: WORKFLOW_FREEFORM_GRAMMAR.to_string(),
        },
    })
}
