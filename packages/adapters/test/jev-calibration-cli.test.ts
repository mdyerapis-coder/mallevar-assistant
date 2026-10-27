import { expect, test } from 'bun:test';
import { mkdtemp, mkdir, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import type { CalibratorArtifact } from '../src/jev/calibration';
import { assignSplits } from '../src/jev/domains/types';
import { sha256 } from '../src/jev/identity';

const root = resolve(dirname(fileURLToPath(import.meta.url)), '../../..');
const preload = join(root, 'packages/adapters/test/fixtures/jev-calibration-process.ts');
const entry = join(root, 'packages/adapters/qa/qualification/jev/calibrate.ts');
type Scenario = 'consistent' | 'between_domains' | 'transient_last_row';

async function runCalibration(scenario: Scenario): Promise<{ code: number; receipts: CalibratorArtifact[] }> {
  const directory = await mkdtemp(join(tmpdir(), 'ironbark-jev-continuity-'));
  try {
    const home = join(directory, 'home');
    await mkdir(home, { mode: 0o700 });
    const config = join(directory, 'bunfig.toml');
    await writeFile(config, '', { mode: 0o600 });
    for (const domain of ['task_intent', 'retrieval_relevance']) {
      const ids = Array.from({ length: 10 }, (_, index) => `${domain}-continuity-${index}`);
      const splits = assignSplits(ids);
      const rows = ids.map((id, index) => {
        const text = `Private diagnostic ${domain} case ${index}; expected KEEP, not a semantic qualification sample.`;
        return { id, domain, version: 'continuity-fixture-v1', class: ['positive', 'negative', 'ambiguous', 'hostile'][index % 4],
          text, expected: 'KEEP', meta: {}, content_digest: sha256(text), split: splits[id] };
      });
      await writeFile(join(directory, `${domain}.jsonl`), rows.map(row => JSON.stringify(row)).join('\n'), { mode: 0o600 });
    }
    const child = Bun.spawn([process.execPath, '--no-env-file', '--no-install', `--config=${config}`,
      `--preload=${preload}`, entry, '--live'], {
      cwd: directory, stdin: 'ignore', stdout: 'pipe', stderr: 'pipe',
      env: { PATH: process.env.PATH ?? '/usr/bin:/bin', HOME: home, TMPDIR: directory,
        IRONBARK_ROOT: directory, IRONBARK_TEST_JEV_SCENARIO: scenario },
    });
    const deadline = setTimeout(() => child.kill('SIGKILL'), 4000);
    try {
      const [code, output, diagnostic] = await Promise.all([
        child.exited, new Response(child.stdout).text(), new Response(child.stderr).text(),
      ]);
      if (!output.trim()) throw new Error(`calibration_fixture_failed:${code}:${diagnostic}`);
      const receipts: CalibratorArtifact[] = output.trim().split('\n').map(line => JSON.parse(line));
      return { code, receipts };
    } finally {
      clearTimeout(deadline);
      if (child.exitCode === null) { child.kill('SIGKILL'); await child.exited; }
    }
  } finally {
    await rm(directory, { recursive: true, force: true });
  }
}

test('consistent_real_cli_rows_keep_a_diagnostic_binding_without_review_authority', async () => {
  const { code, receipts } = await runCalibration('consistent');
  expect(code).toBe(0);
  expect(receipts).toHaveLength(1);
  expect(receipts[0].runtime_identity_digest).toMatch(/^[0-9a-f]{64}$/);
  expect(receipts[0].review_freeze_digest).toBeNull();
  expect(receipts[0].selection).toMatchObject({ attempted: 2, available: 2, accepted: 2, wrong: 0 });
});

test('a_later_domain_drift_invalidates_already_computed_artifacts', async () => {
  const { code, receipts } = await runCalibration('between_domains');
  expect(receipts).toHaveLength(2);
  expect(receipts.map(receipt => receipt.runtime_identity_digest)).toEqual([null, null]);
  expect(code).toBe(1);
});

test('last_row_intra_request_drift_cannot_be_hidden_by_the_final_matching_probe', async () => {
  const { code, receipts } = await runCalibration('transient_last_row');
  expect(receipts).toHaveLength(1);
  expect(receipts[0].runtime_identity_digest).toBeNull();
  expect(code).toBe(1);
});
