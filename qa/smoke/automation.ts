// Live smoke test for automation tool.
//
// This test exercises the automation adapter and broker with real temporary storage.
// It is guarded by a BLOCKED status because it requires a human presence on a KDE/Wayland
// workstation (the native window backend and Firefox profile cannot be automated in CI).
// See docs/superpowers/plans/2026-10-12-03-harnesses-orchestration.md §Task H8.

import { AutomationAdapter } from '../../packages/adapters/src/tools/automation';

// Export the smoke test metadata.
export const metadata = {
  name: 'automation',
  description: 'Action‑scoped browser and desktop automation with real execution',
  blocked: true,
  blockReason: 'Live automation requires human presence on a KDE/Wayland workstation',
};

// Placeholder: real smoke would launch a headless Firefox profile, create a NativeWindow,
// open a browser scope, and verify that a click or other operation works as expected.
// This implementation is a stub that passes compilation but does not run in CI.

export async function runSmoke(): Promise<void> {
  console.log('Automation smoke test BLOCKED – human presence required');
  console.log('Test skipped.');
}