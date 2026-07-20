export const meta = {
  name: "uat-pause",
  description: "One harmless held child for durable pause and exact resume UAT",
  phases: ["Hold", "Finish"],
};

phase("Hold");
log(`UAT_PAUSE_ARG_VALID=${args?.marker === "resume-marker"}`);
const result = await agent(
  "UAT_ROUTE:pause\nThis is a disposable pause/resume UAT. Run the harmless command `sleep 120`, then reply with exactly UAT_PAUSE_FINISHED. Do not read or edit files, use the network, inspect credentials, or perform any other action.",
  { label: "pause-target", phase: "Hold" },
);
phase("Finish");
text(`UAT_PAUSE_RESULT=${JSON.stringify(result)}`);
