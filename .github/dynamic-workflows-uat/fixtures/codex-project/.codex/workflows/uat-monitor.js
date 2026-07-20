export const meta = {
  name: 'uat-monitor',
  description: 'Two harmless readers for workflow monitor and drill UAT',
  phases: ['Inspect', 'Report'],
}

phase('Inspect')
const results = await parallel([
  () => agent(
    'UAT_ROUTE:monitor-a\nRead README.md only. Return its first Markdown heading exactly and do not edit files.',
    { label: 'reader-a', phase: 'Inspect' },
  ),
  () => agent(
    'UAT_ROUTE:monitor-b\nRead README.md only. Report whether it says this fixture is disposable. Do not edit files.',
    { label: 'reader-b', phase: 'Inspect' },
  ),
])

phase('Report')
log('UAT_MONITOR_RESULTS=' + JSON.stringify(results))
text(JSON.stringify(results))
