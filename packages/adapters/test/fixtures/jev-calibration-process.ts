// Private subprocess fixture: only the external model transport and tiny domain
import { validateExpected } from '../../src/jev/domains/types';

import { DomainRegistry } from '../../src/jev/registry';
import type { DomainDefinition } from '../../src/jev/domains/types';

const scenario = process.env.IRONBARK_TEST_JEV_SCENARIO;
if (!['consistent', 'between_domains', 'transient_last_row'].includes(scenario ?? ''))
  throw new Error('unknown_private_jev_scenario');
const domainNames = scenario === 'between_domains'
  ? ['task_intent', 'retrieval_relevance'] as const
  : ['task_intent'] as const;
const domains: DomainDefinition[] = domainNames.map(domain => ({
  domain, version: 'continuity-fixture-v1', prompt: 'Choose KEEP for this diagnostic fixture.',
  candidates: ['KEEP'],
  validateExpected,
  requiredClasses: ['positive', 'negative', 'ambiguous', 'hostile'], fixtures: [`${domain}.jsonl`],
}));
DomainRegistry.all = () => domains;

let completions = 0;
let readsAfterFirstDomain = 0;
let changedProbePending = false;
const reply = (value: unknown) => Response.json(value);
const transport = async (input: string | URL | Request, init?: RequestInit): Promise<Response> => {
  const url = typeof input === 'string' ? input : input instanceof URL ? input.href : input.url;
  if (url === 'http://127.0.0.1:8420/api/status' && init?.method === 'GET') {
    return reply({ llm: { online: true, mode: 'local', port: 18420,
      base_url: 'http://127.0.0.1:18420/v1', active_model_id: 'fixture.gguf',
      active_model_path: '/private/fixture.gguf', device: 'fixture' } });
  }
  if (url === 'http://127.0.0.1:18420/health' && init?.method === 'GET') return reply({ status: 'ok' });
  if (url === 'http://127.0.0.1:18420/props' && init?.method === 'GET') {
    let generation = 1;
    if (scenario === 'between_domains' && completions >= 2 && ++readsAfterFirstDomain > 1) generation = 2;
    if (changedProbePending) { generation = 2; changedProbePending = false; }
    return reply({ is_sleeping: false, model_alias: `fixture-${generation}`, model_path: '/private/fixture.gguf',
      model_ftype: 1, build_info: `fixture-build-${generation}`, chat_template: 'fixture-template',
      default_generation_settings: { n_ctx: 1024, params: { lora: [] } }, total_slots: 1 });
  }
  if (url === 'http://127.0.0.1:18420/completion' && init?.method === 'POST') {
    completions++;
    if (scenario === 'transient_last_row' && completions === 2) changedProbePending = true;
    return reply({ content: 'C0', stop: true, truncated: false, stop_type: 'eos',
      model: scenario === 'between_domains' && completions > 2 ? 'fixture-2' : 'fixture-1',
      completion_probabilities: [{ token: 'C0', logprob: -0.1 }] });
  }
  // No fallback: fixture requests can never reach an operator facade/model.
  throw new Error('private_jev_transport_denied');
};
globalThis.fetch = Object.assign(transport, { preconnect: () => { throw new Error('private_jev_network_denied'); } });
