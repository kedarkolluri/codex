export const meta = {
  name: "uat-agent-control",
  description: "Two harmless held children for explicit selected-attempt control UAT",
  phases: ["Hold", "Finish"],
};

phase("Hold");
const results = await parallel([
  () =>
    agent(
      "UAT_ROUTE:agent-a\nThis is disposable selected-agent control UAT target A. Run only the harmless command `sleep 120`, then reply with exactly UAT_AGENT_A_FINISHED. Do not read or edit files, use the network, inspect credentials, or perform any other action.",
      { label: "control-a", phase: "Hold" },
    ),
  () =>
    agent(
      "UAT_ROUTE:agent-b\nThis is disposable selected-agent control UAT target B. Run only the harmless command `sleep 120`, then reply with exactly UAT_AGENT_B_FINISHED. Do not read or edit files, use the network, inspect credentials, or perform any other action.",
      { label: "control-b", phase: "Hold" },
    ),
]);
phase("Finish");
text(`UAT_AGENT_CONTROL_RESULTS=${JSON.stringify(results)}`);
