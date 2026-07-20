export const meta = {
  name: "uat-parallel-barrier",
  description: "Fast and held siblings proving the parallel completion barrier",
  phases: ["Parallel barrier", "Barrier released"],
};

phase("Parallel barrier");
const results = await parallel([
  () =>
    agent(
      "UAT_ROUTE:parallel-fast\nReply with exactly UAT_PARALLEL_FAST_DONE. Do not use tools, read or edit files, use the network, or inspect credentials.",
      { label: "parallel-fast", phase: "Parallel barrier" },
    ),
  () =>
    agent(
      "UAT_ROUTE:parallel-held\nRun exactly one harmless platform-appropriate local delay command for 12 seconds (`sleep 12` on POSIX or `Start-Sleep -Seconds 12` in PowerShell), then reply with exactly UAT_PARALLEL_HELD_DONE. Do not read or edit files, use the network, inspect credentials, or perform any other action.",
      { label: "parallel-held", phase: "Parallel barrier" },
    ),
]);

phase("Barrier released");
log(`UAT_PARALLEL_BARRIER_RESULTS=${JSON.stringify(results)}`);
text(JSON.stringify(results));
