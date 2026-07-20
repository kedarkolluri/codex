export const meta = {
  name: "uat-budget",
  description: "Bounded hard-ceiling UAT; launch with args budget.total=36",
  phases: ["Meter", "Ceiling"],
};

if (args?.budget?.total !== 36) {
  throw new Error('uat-budget requires args.budget.total=36');
}

phase("Meter");
const attempts = [];
const readings = [
  {
    point: "start",
    total: budget.total,
    spent: budget.spent(),
    remaining: budget.remaining(),
  },
];

for (let i = 0; i < 4; i++) {
  try {
    const value = await agent(
      `UAT_ROUTE:budget-${i}\nReply with exactly UAT_BUDGET_AGENT_${i}. Do not use tools, read or edit files, use the network, or inspect credentials.`,
      { label: `budget-${i}`, phase: "Meter" },
    );
    attempts.push({ ordinal: i, value });
    readings.push({
      point: `after-${i}`,
      total: budget.total,
      spent: budget.spent(),
      remaining: budget.remaining(),
    });
  } catch (error) {
    attempts.push({ ordinal: i, error: String(error) });
    readings.push({
      point: `rejected-${i}`,
      total: budget.total,
      spent: budget.spent(),
      remaining: budget.remaining(),
    });
    break;
  }
}

phase("Ceiling");
const result = {
  requestedTotal: args?.budget?.total ?? null,
  attempts,
  readings,
};
log(`UAT_BUDGET_RESULTS=${JSON.stringify(result)}`);
return result;
