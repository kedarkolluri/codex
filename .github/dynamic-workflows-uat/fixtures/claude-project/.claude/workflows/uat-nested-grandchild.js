export const meta = {
  name: "uat-nested-grandchild",
  description: "Helper sentinel that must not run when depth two is rejected",
  phases: ["Unexpected"],
};

phase("Unexpected");
return `UAT_NEST_GRANDCHILD_UNEXPECTED:${args?.marker ?? "missing"}`;
