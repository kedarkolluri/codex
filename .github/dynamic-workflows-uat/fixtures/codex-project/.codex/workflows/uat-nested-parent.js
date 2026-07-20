export const meta = {
  name: "uat-nested-parent",
  description: "Depth-one success followed by a caught depth-two nested rejection",
  phases: ["Depth one", "Report"],
};

phase("Depth one");
const result = await workflow("uat-nested-child", {
  marker: "from-parent",
});
phase("Report");
log(`UAT_NESTED_PARENT_RESULT=${JSON.stringify(result)}`);
text(result);
