use std::collections::BTreeMap;

use codex_tools::JsonSchema;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolSpec;

use super::WORKFLOW_TOOL_NAME;

/// Build the model-callable workflow launcher.
///
/// The model may select a statically discovered saved workflow and provide
/// bounded JSON arguments, but it cannot submit inline source or an arbitrary
/// filesystem path. Workflow source remains host-authored and reviewable.
pub(crate) fn create_workflow_tool() -> ToolSpec {
    let properties = BTreeMap::from([
        (
            "name".to_string(),
            JsonSchema::string(Some(
                "Exact metadata name of a saved workflow discovered by Codex.".to_string(),
            )),
        ),
        (
            "args".to_string(),
            JsonSchema {
                description: Some(
                    "Optional JSON value exposed to the workflow as the read-only `args` global."
                        .to_string(),
                ),
                ..Default::default()
            },
        ),
        (
            "resumeFromRunId".to_string(),
            JsonSchema::string(Some(
                "Optional canonical workflow run UUID whose compatible journal prefix should be replayed."
                    .to_string(),
            )),
        ),
    ]);

    ToolSpec::Function(ResponsesApiTool {
        name: WORKFLOW_TOOL_NAME.to_string(),
        description: "Start a saved Dynamic Workflow in the background. The workflow is resolved by its exact saved metadata name; inline source and filesystem paths are not accepted. Returns after the run is durably initialized, while progress and completion continue through workflow events."
            .to_string(),
        strict: false,
        defer_loading: None,
        parameters: JsonSchema::object(
            properties,
            Some(vec!["name".to_string()]),
            /*additional_properties*/ Some(false.into()),
        ),
        output_schema: None,
    })
}
