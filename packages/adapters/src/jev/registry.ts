// DomainRegistry: single source of truth for frozen domain definitions.
import type { DomainDefinition, JudgmentDomain } from "./domains/types";
import { intentDomain } from "./domains/intent";
import { relevanceDomain } from "./domains/relevance";
import { handoffDomain } from "./domains/handoff";
import { evidenceDomain } from "./domains/evidence";
import { routesDomain } from "./domains/routes";

const BY_DOMAIN: Record<JudgmentDomain, DomainDefinition> = {
  task_intent: intentDomain,
  retrieval_relevance: relevanceDomain,
  handoff_completeness: handoffDomain,
  evidence_needed: evidenceDomain,
  allowed_route_choice: routesDomain,
};

export const DomainRegistry = {
  /** Returns the frozen DomainDefinition for the given domain. */
  get(domain: JudgmentDomain): DomainDefinition {
    const d = BY_DOMAIN[domain];
    if (!d) throw new Error(`unknown judgment domain: ${domain}`);
    return d;
  },

  /** All known domains. */
  all(): DomainDefinition[] {
    return Object.values(BY_DOMAIN);
  },
};