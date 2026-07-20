export const meta = {
  name: "uat-stop",
  description: "One harmless long-running child for explicit workflow-stop UAT",
  phases: ["Hold"],
};

phase("Hold");
const result = await agent(
  "UAT_ROUTE:stop\nThis is a disposable control UAT. Run the harmless command `sleep 120`, then reply with exactly UAT_STOP_FINISHED. Do not read or edit files, use the network, inspect credentials, or perform any other action.",
  { label: "stop-target", phase: "Hold" },
);
text(`UAT_STOP_UNEXPECTED_NATURAL_COMPLETION=${JSON.stringify(result)}`);
