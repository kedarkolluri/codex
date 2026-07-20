export const meta = {
  name: "uat-worktree",
  description: "Disposable write/read/remove proof inside a clean isolated Git worktree",
  phases: ["Isolate", "Report"],
};

const PROOF_SCHEMA = {
  type: "object",
  properties: {
    cwd: { type: "string" },
    marker: { const: "UAT_WORKTREE_MARKER" },
    removed: { const: true },
    clean: { const: true },
  },
  required: ["cwd", "marker", "removed", "clean"],
  additionalProperties: false,
};

phase("Isolate");
const result = await agent(
  'UAT_ROUTE:worktree\nWork only inside the current isolated Git checkout and do not traverse outside it. Create `uat-worktree-child-marker.txt` with exact content `UAT_WORKTREE_MARKER`, read it back, delete it, verify it is absent, and verify `git status --porcelain` is clean. Return a JSON object with the absolute checkout root as `cwd`, `marker` equal to `UAT_WORKTREE_MARKER`, and booleans `removed: true` and `clean: true`. Do not use the network or inspect credentials.',
  {
    label: "isolated-write-cleanup",
    phase: "Isolate",
    isolation: "worktree",
    schema: PROOF_SCHEMA,
  },
);
phase("Report");
log(`UAT_WORKTREE_RESULT=${JSON.stringify(result)}`);
text(JSON.stringify(result));
