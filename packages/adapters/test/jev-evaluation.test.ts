import { expect, test } from 'bun:test';
import type { JudgmentState } from '../../contracts/src/generated';
import { EvaluationRunIdentity } from '../src/jev/evaluation';

const first = 'a'.repeat(64);
const changed = 'b'.repeat(64);
const states: JudgmentState[] = ['available', 'abstain', 'invalid', 'unavailable', 'unqualified'];

test('identity_continuity_requires_at_least_one_actual_observation', () => {
  const result = new EvaluationRunIdentity().finish();
  expect(result.continuous).toBe(false);
  expect(result.identity_digest).toBeNull();
});

test('a_consistent_reported_identity_does_not_require_a_successful_choice', () => {
  const run = new EvaluationRunIdentity();
  for (const state of states) run.observe(first, state);
  expect(run.finish()).toMatchObject({ identity_digest: first, continuous: true });
});

test.each(states)('a_changed_identity_on_%s_invalidates_the_entire_run', state => {
  const run = new EvaluationRunIdentity();
  run.observe(first, 'available');
  run.observe(changed, state);
  run.observe(first, 'available');
  const result = run.finish();
  expect(result.continuous).toBe(false);
  expect(result.identity_digest).toBeNull();
});

test.each(states)('missing_identity_on_%s_cannot_be_erased_by_a_later_success', state => {
  const run = new EvaluationRunIdentity();
  run.observe(first, 'available');
  run.observe(null, state);
  run.observe(first, 'available');
  expect(run.finish().continuous).toBe(false);
  expect(run.finish().identity_digest).toBeNull();
});

test('a_late_first_identity_does_not_repair_an_unobserved_initial_attempt', () => {
  const run = new EvaluationRunIdentity();
  run.observe(null, 'unavailable');
  run.observe(first, 'available');
  expect(run.finish().continuous).toBe(false);
  expect(run.finish().identity_digest).toBeNull();
});

test.each(['runtime_identity_changed', 'runtime_identity_unknown', 'completion_model_mismatch'])(
  'an_observed_%s_is_sticky_even_if_the_record_and_final_probe_match',
  reason => {
    const run = new EvaluationRunIdentity();
    run.observe(first, 'available');
    run.observe(first, 'unqualified', reason);
    run.observe(first, 'available');
    expect(run.finish().continuous).toBe(false);
    expect(run.finish().identity_digest).toBeNull();
  },
);
