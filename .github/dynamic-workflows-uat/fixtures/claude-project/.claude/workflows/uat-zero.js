export const meta = {
  name: 'uat-zero',
  description: 'Harmless zero-agent workflow for terminal UAT',
  phases: ['Prepare', 'Finish'],
}

phase('Prepare')
log('UAT_ZERO_PREPARE')
phase('Finish')
log('UAT_ZERO_FINISH')

return { status: 'ok', marker: 'UAT_ZERO_OK' }
