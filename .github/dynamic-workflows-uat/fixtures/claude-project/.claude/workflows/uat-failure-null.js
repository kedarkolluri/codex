export const meta = {
  name: "uat-failure-null",
  description: "Natural null, sibling survival, and structured agent options UAT",
  phases: ["Failure isolation", "Structured result"],
};

const RESULT_SCHEMA = {
  type: "object",
  properties: {
    marker: { const: "UAT_SCHEMA_OPTIONS_OK" },
    answer: { type: "string" },
  },
  required: ["marker", "answer"],
  additionalProperties: false,
};

phase("Failure isolation");
const [naturalFailure, sibling] = await parallel([
  () =>
    agent(
      "UAT_ROUTE:failure-null\nReply with exactly UAT_NATURAL_NULL_UNEXPECTED_SUCCESS. Do not use tools, read or edit files, use the network, or inspect credentials.",
      { label: "natural-null", phase: "Failure isolation" },
    ),
  () =>
    agent(
      "UAT_ROUTE:failure-sibling\nReply with exactly UAT_SIBLING_SURVIVED. Do not use tools, read or edit files, use the network, or inspect credentials.",
      { label: "sibling-survivor", phase: "Failure isolation" },
    ),
]);

phase("Structured result");
const structured = await agent(
  'UAT_ROUTE:failure-schema\nReturn exactly {"marker":"UAT_SCHEMA_OPTIONS_OK","answer":"structured"}. Do not use tools, read or edit files, use the network, or inspect credentials.',
  {
    label: "schema-options",
    phase: "Structured result",
    schema: RESULT_SCHEMA,
    model: "haiku",
  },
);

const result = { naturalFailure, sibling, structured };
log(`UAT_FAILURE_NULL_RESULTS=${JSON.stringify(result)}`);
return result;
