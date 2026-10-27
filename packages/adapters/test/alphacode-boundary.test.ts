import { expect, test } from 'bun:test';
import { validateAlphaIdentity, AlphaTextState, requireObservedStop, permissionReply, type AlphaIdentity } from '../src/native/alphacode';

const expected: AlphaIdentity = { buildDigest: 'a'.repeat(64), effectiveRoute: { connection_id: 'owned', model_id: 'explicit-model', harness: 'alphacode' }, cwd: '/work/owned', allowedTools: ['read', 'write'], profile: '/state/owned', socket: '/state/owned/daemon.sock' };
const native = { ...expected, permissionsVersion: 1, gateActive: true };
test('unpatched_binary_rejected', () => {
  expect(() => validateAlphaIdentity({}, expected)).toThrow('UnpatchedAlphaCode');
  expect(() => validateAlphaIdentity({ ...native, gateActive: false }, expected)).toThrow();
});
test('existing_daemon_route_mismatch_rejected', () => {
  expect(() => validateAlphaIdentity({ ...native, effectiveRoute: { ...native.effectiveRoute, model_id: 'other' } }, expected)).toThrow('SessionIdentityMismatch');
  expect(() => validateAlphaIdentity({ ...native, socket: '/tmp/shared.sock' }, expected)).toThrow();
});
test('text_replace_not_appended', () => {
  const text = new AlphaTextState();
  text.apply('text_delta', 'message', 'old response');
  const nativeReplacementText = 'corrected response';
  text.apply('text_replace', 'message', nativeReplacementText);
  const finalText = text.get('message');
  expect(finalText).toBe(nativeReplacementText);
});
test('cancel_observes_daemon_children', () => {
  expect(() => requireObservedStop({ session_id: 's', cancelled: true, active_tools: 1, children: [] }, 's')).toThrow();
  expect(() => requireObservedStop({ session_id: 's', cancelled: true, active_tools: 0, children: [{ terminated: false }] }, 's')).toThrow();
  requireObservedStop({ session_id: 's', cancelled: true, active_tools: 0, children: [] }, 's');
});
test('changed_native_input_cannot_reuse_permission', () => {
  const action = { session_id: 's', tool_call_id: 't', resolved_name: 'write', canonical_input: { path: '/work/owned/a', content: 'safe' }, cwd: expected.cwd, route: expected.effectiveRoute, policy_version: "1", state_digest: 'b'.repeat(64) };
  expect(() => permissionReply({ request_id: 'p', action }, { request_id: 'p', action: { ...action, canonical_input: { ...action.canonical_input, content: 'changed' } }, allowed: true })).toThrow();
  expect(permissionReply({ request_id: 'p', action }, { request_id: 'p', action, allowed: false }).allowed).toBe(false);
});
