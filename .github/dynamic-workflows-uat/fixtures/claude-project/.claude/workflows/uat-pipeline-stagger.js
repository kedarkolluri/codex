export const meta = {
  name: "uat-pipeline-stagger",
  description: "Three bounded children proving pipeline no-barrier staggering",
  phases: ["Pipeline stagger", "Report"],
};

const stageOne = async (item) => {
  if (item === "held") {
    await agent(
      "UAT_ROUTE:pipeline-b0\nRun exactly one harmless platform-appropriate local delay command for 12 seconds (`sleep 12` on POSIX or `Start-Sleep -Seconds 12` in PowerShell), then reply with exactly UAT_PIPELINE_B0_DONE. Do not read or edit files, use the network, inspect credentials, or perform any other action.",
      { label: "pipeline-b0", phase: "Pipeline stagger" },
    );
  } else {
    await agent(
      "UAT_ROUTE:pipeline-a0\nReply with exactly UAT_PIPELINE_A0_DONE. Do not use tools, read or edit files, use the network, or inspect credentials.",
      { label: "pipeline-a0", phase: "Pipeline stagger" },
    );
  }
  return item;
};

const stageTwo = async (item) => {
  if (item === "fast") {
    await agent(
      "UAT_ROUTE:pipeline-a1\nReply with exactly UAT_PIPELINE_A1_DONE. Do not use tools, read or edit files, use the network, or inspect credentials.",
      { label: "pipeline-a1", phase: "Pipeline stagger" },
    );
  }
  return `${item}-done`;
};

phase("Pipeline stagger");
const results = await pipeline(["fast", "held"], stageOne, stageTwo);
phase("Report");
log(`UAT_PIPELINE_STAGGER_RESULTS=${JSON.stringify(results)}`);
return results;
