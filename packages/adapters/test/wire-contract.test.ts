import { expect, test } from 'bun:test';
import { formatWireI64, formatWireU64, parseWireI64, parseWireU64 } from '../../contracts/src/wire';
import { validateCommandResult, validateResult } from '../../contracts/src/validate';
import type { CommandResult, CoreCommand, EventEnvelope, ResultOf, StatusReport } from '../../contracts/src/generated';

test.each([
  ['0', 0n], ['9007199254740993', 9007199254740993n], ['18446744073709551615', 18446744073709551615n],
] as const)('unsigned_wire_%s_retains_the_original_integer', (wire, integer) => {
  expect(parseWireU64(wire)).toBe(integer);
  expect(formatWireU64(integer)).toBe(wire);
});

test.each([
  ['-9223372036854775808', -9223372036854775808n], ['0', 0n], ['9223372036854775807', 9223372036854775807n],
] as const)('signed_wire_%s_retains_the_original_integer', (wire, integer) => {
  expect(parseWireI64(wire)).toBe(integer);
  expect(formatWireI64(integer)).toBe(wire);
});

test.each([0, 1, 9007199254740992, null, true, '', '00', '01', '+1', '-0', '-1', ' 1', '1\n',
  '1.0', '1e3', '١', '18446744073709551616'])('unsigned_wire_rejects_noncanonical_or_out_of_range_%j', value => {
  expect(() => parseWireU64(value)).toThrow();
});

test.each([0, -1, null, false, '', '00', '01', '+1', '-0', '-01', ' -1', '1\n', '1.0', '1e3',
  '9223372036854775808', '-9223372036854775809'])('signed_wire_rejects_noncanonical_or_out_of_range_%j', value => {
  expect(() => parseWireI64(value)).toThrow();
});

test('formatters_reject_overflow_and_runtime_number_coercion', () => {
  expect(() => formatWireU64(-1n)).toThrow();
  expect(() => formatWireU64(18446744073709551616n)).toThrow();
  expect(() => formatWireI64(-9223372036854775809n)).toThrow();
  expect(() => formatWireI64(9223372036854775808n)).toThrow();
  expect(() => Reflect.apply(formatWireU64, undefined, [1])).toThrow();
  expect(() => Reflect.apply(formatWireI64, undefined, [1])).toThrow();
});

// ── Typed CommandResult envelope: ResultOf<C> unwraps only the matching
//    command variant; mismatched, echoed, unknown-field and out-of-range
//    results are rejected by the local-schema validator. ────────────────

const statusReport: StatusReport = {
  protocol: 1,
  projects: '2',
  last_seq: '9007199254740993',
  abandoned_requests: '0',
};

const eventEnvelope: EventEnvelope = {
  protocol: 1,
  seq: '5',
  timestamp: '1700000000000',
  payload: { type: 'state_changed', entity_id: 'project/1', state: 'project_created' },
};

test('result_of_unwraps_only_the_matching_command_variant', () => {
  const envelope: CommandResult = { command: 'status', value: statusReport };
  const leaf = validateResult({ type: 'status' }, envelope);
  expect(leaf).toBe(statusReport);
  const typed: ResultOf<{ type: 'status' }> = leaf;
  expect(typed.projects).toBe('2');
});

test('result_of_rejects_a_mismatched_echoed_result_with_a_different_command_tag', () => {
  // A status-shaped reply echoed under the events command is a well-formed
  // CommandResult envelope, but its tag cannot satisfy `ResultOf<'events'>`.
  const echoed: CommandResult = { command: 'status', value: statusReport };
  expect(() => validateResult({ type: 'events', after: '' }, echoed)).toThrow(/mismatched or echoed/);
  // The reverse: an events-shaped reply under status.
  expect(() => validateResult({ type: 'status' }, { command: 'events', value: [eventEnvelope] })).toThrow(/mismatched or echoed/);
});

test('result_of_rejects_a_result_whose_leaf_shape_mismatches_its_own_tag', () => {
  // The envelope claims `events` but the leaf is a StatusReport object, not an
  // array of EventEnvelope — the schema must reject it, not the tag check.
  const lying: CommandResult = { command: 'events', value: statusReport as unknown as EventEnvelope[] };
  expect(() => validateResult({ type: 'events', after: '' }, lying)).toThrow(/failed canonical CommandResult schema/);
  // Envelope-shaped payload under a command whose leaf is an object DTO.
  expect(() => validateResult({ type: 'project_policy_show', project_id: 'project' }, { command: 'project_policy_show', value: { command: 'status', value: {} } })).toThrow();
});

test('result_of_rejects_unknown_fields_in_the_leaf', () => {
  expect(() => validateResult({ type: 'status' }, { command: 'status', value: { ...statusReport, forged_authority: true } }))
    .toThrow(/failed canonical CommandResult schema|additionalProperties/);
  expect(() => validateResult({ type: 'status' }, { command: 'status', value: { ...statusReport, extra: 1 } })).toThrow();
});

test('result_of_rejects_noncanonical_and_out_of_range_wire_decimals', () => {
  expect(() => validateResult({ type: 'status' }, { command: 'status', value: { ...statusReport, projects: '01' } })).toThrow();
  expect(() => validateResult({ type: 'status' }, { command: 'status', value: { ...statusReport, projects: '18446744073709551616' } })).toThrow();
  expect(() => validateResult({ type: 'status' }, { command: 'status', value: { ...statusReport, last_seq: '-1' } })).toThrow();
  // A JSON number is never accepted where a decimal wire string is required.
  expect(() => validateResult({ type: 'status' }, { command: 'status', value: { ...statusReport, projects: 2 } })).toThrow();
  // The full u64 range is accepted exactly.
  const max = validateResult({ type: 'status' }, { command: 'status', value: { ...statusReport, last_seq: '18446744073709551615' } });
  expect(max.last_seq).toBe('18446744073709551615');
});

test('validate_command_result_rejects_an_unknown_command_tag', () => {
  expect(() => validateCommandResult({ command: 'forged_command', value: {} })).toThrow(/failed canonical CommandResult schema/);
  expect(() => validateCommandResult({ value: statusReport })).toThrow();
});
