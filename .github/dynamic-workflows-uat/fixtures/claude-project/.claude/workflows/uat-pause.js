export const meta = {
  name: "uat-pause",
  description: "One harmless held child for checkpoint pause and same-session resume UAT",
  phases: ["Hold", "Finish"],
};

phase("Hold");
log(`UAT_PAUSE_ARG_VALID=${args?.marker === "resume-marker"}`);
const result = await agent(
  "UAT_ROUTE:pause\nThis is a disposable pause/resume UAT. Run only the harmless command `sleep 120`, then reply with exactly UAT_PAUSE_FINISHED. Do not read or edit files, use the network, inspect credentials, or perform any other action.",
  { label: "pause-target", phase: "Hold" },
);
phase("Finish");
const output = { markerValid: args?.marker === "resume-marker", result };
log(`UAT_PAUSE_RESULT=${JSON.stringify(output)}`);
return output;
