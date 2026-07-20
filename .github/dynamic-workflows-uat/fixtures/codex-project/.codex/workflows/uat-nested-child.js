export const meta = {
  name: "uat-nested-child",
  description: "Helper: admitted depth-one run that probes the depth-two guard",
  phases: ["Depth one admitted", "Depth two probe"],
};

phase("Depth one admitted");
const depthOne = `UAT_NEST_DEPTH_ONE_OK:${args?.marker ?? "missing"}`;

phase("Depth two probe");
let depthTwo;
try {
  const grandchild = await workflow("uat-nested-grandchild", {
    marker: "from-child",
  });
  depthTwo = `UAT_NEST_DEPTH_TWO_ADMITTED_UNEXPECTED:${grandchild}`;
} catch (error) {
  depthTwo = `UAT_NEST_DEPTH_TWO_REJECTED:${String(error)}`;
}

const result = JSON.stringify({ depthOne, depthTwo });
log(`UAT_NESTED_CHILD_RESULT=${result}`);
text(result);
