//! H6: concurrency, request and spend admission.
//!
//! Admission is the only place a `queued` run may advance to `ready`. It is
//! driven by durable rows, not by in-process counters: how many workers are
//! active, how many requests have been consumed and how much has been spent are
//! all read back from the store, so a restart resumes the same decisions. The
//! concurrency ceiling defaults to [`DEFAULT_MAX_WORKERS`] (2), so a third node
//! stays `queued` until a worker frees up. A node whose request or spend budget
//! is exhausted, or whose route cost is unknown while the policy forbids
//! unknown cost, is not admitted. A consumer of a failed dependency is durably
//! `failed` rather than left to wait forever; its siblings are unaffected.
//!
//! Every state change goes through `state::transition` (the single legal
//! authority over run state) with a compare-and-set on the from-state.

use std::collections::HashMap;

use rusqlite::params;

use crate::contracts::{ProjectId, ProjectPolicy, RunId, RunState, SpendPolicy};
use crate::error::{CoreError, Result};
use crate::store::{events::append_tx, Store};

use super::state::{state_str, transition, RunEvent};

/// The scheduler's default concurrency ceiling when a policy leaves it at zero.
pub const DEFAULT_MAX_WORKERS: u16 = 2;

/// Run states that occupy a worker slot: anything the core has admitted but not
/// yet resolved.
const ACTIVE_STATES: [&str; 5] = ["ready", "running", "awaiting_approval", "verifying", "reviewing"];

/// What is known about a candidate's route cost. `None` is a genuine unknown,
/// never assumed to be zero.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RouteCost {
    pub known_cost_microusd: Option<u64>,
}

/// The usage already consumed by a project, as observed from durable rows.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Usage {
    pub active_workers: u16,
    pub requests: u32,
    pub cost_microusd: u64,
}

/// The ceilings a project policy imposes on admission.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdmissionBudget {
    pub max_workers: u16,
    pub max_requests: u32,
    pub max_cost_microusd: Option<u64>,
    pub free_only: bool,
    pub allow_unknown_cost: bool,
}

impl AdmissionBudget {
    pub fn from_policy(policy: &ProjectPolicy) -> Self {
        Self {
            max_workers: if policy.max_workers == 0 { DEFAULT_MAX_WORKERS } else { policy.max_workers },
            max_requests: policy.max_requests,
            max_cost_microusd: policy.max_cost_microusd,
            free_only: policy.spend_policy == SpendPolicy::FreeOnly,
            allow_unknown_cost: policy.allow_unknown_cost,
        }
    }

    /// Decide whether one more node may be admitted. `Err.code` is the stable
    /// reason a caller maps to "stays queued" (`concurrency_limit`) or "cannot
    /// proceed" (a budget that is exhausted, or an unknown cost the policy
    /// forbids).
    pub fn check(&self, usage: Usage, cost: RouteCost) -> Result<()> {
        if usage.active_workers >= self.max_workers {
            return Err(CoreError::new(
                "concurrency_limit",
                "The concurrency ceiling is reached; the node stays queued until a worker frees up",
            ));
        }
        if usage.requests >= self.max_requests {
            return Err(CoreError::new("request_budget_exhausted", "The request budget is exhausted"));
        }
        match self.max_cost_microusd {
            Some(budget) => match cost.known_cost_microusd {
                None if !self.allow_unknown_cost => Err(CoreError::new(
                    "unknown_cost_not_allowed",
                    "The route cost is unknown and the policy does not admit unknown-cost routes",
                )),
                None => Ok(()),
                Some(cost) if usage.cost_microusd.saturating_add(cost) > budget => {
                    Err(CoreError::new("spend_budget_exhausted", "The spend budget cannot cover this route"))
                }
                Some(_) => Ok(()),
            },
            None if self.free_only => match cost.known_cost_microusd {
                None if !self.allow_unknown_cost => Err(CoreError::new(
                    "unknown_cost_not_allowed",
                    "The route cost is unknown under a free-only policy",
                )),
                Some(cost) if cost > 0 => Err(CoreError::new("paid_route_not_allowed", "A free-only policy admits only zero-cost routes")),
                _ => Ok(()),
            },
            None => Ok(()),
        }
    }
}

/// A run that was not admitted, with the reason it was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockedRun {
    pub run_id: RunId,
    pub reason: String,
}

/// The outcome of one admission sweep.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Admission {
    /// Runs moved `queued -> ready`.
    pub admitted: Vec<RunId>,
    /// Runs that remain `queued` (a dependency is not yet satisfied, or the
    /// concurrency ceiling is reached).
    pub queued: Vec<RunId>,
    /// Runs that could not proceed: their reason code is recorded durably.
    pub blocked: Vec<BlockedRun>,
}

/// Durable admission for one project. Holds no counters: every decision reads
/// the current rows so a restarted core admits exactly the same work.
#[derive(Clone)]
pub struct Budgets {
    store: Store,
    project: ProjectId,
}

impl Budgets {
    pub fn new(store: Store, project: ProjectId) -> Self {
        Self { store, project }
    }

    /// The observed usage a policy is checked against.
    pub async fn usage(&self) -> Result<Usage> {
        Ok(self.observe().await?.usage)
    }

    /// Admit as many of `candidates` as the policy allows, in the order given.
    pub async fn admit(&self, policy: &ProjectPolicy, candidates: &[RunId]) -> Result<Admission> {
        let budget = AdmissionBudget::from_policy(policy);
        let observed = self.observe().await?;
        let mut usage = observed.usage;
        let mut admission = Admission::default();
        for candidate in candidates {
            let Some(row) = observed.runs.get(&candidate.to_string()) else {
                return Err(CoreError::new("run_missing", "A candidate run does not exist in this project"));
            };
            if row.state != "queued" {
                // Already decided (or running): nothing to admit, nothing to block.
                admission.queued.push(*candidate);
                continue;
            }

            // A dependency that failed or was cancelled can never be satisfied.
            let mut blocked_by_dependency = false;
            let mut dependency_pending = false;
            for dependency in &row.dependencies {
                match observed.node_states.get(dependency) {
                    Some(state) if state.as_str() == "succeeded" => {}
                    Some(state) if state.as_str() == "failed" || state.as_str() == "cancelled" => {
                        blocked_by_dependency = true;
                        break;
                    }
                    _ => dependency_pending = true,
                }
            }
            if blocked_by_dependency {
                self.commit(*candidate, RunState::Queued, RunState::Failed, "dependency_failed").await?;
                admission.blocked.push(BlockedRun { run_id: *candidate, reason: "dependency_failed".into() });
                continue;
            }
            if dependency_pending {
                admission.queued.push(*candidate);
                continue;
            }

            let cost = RouteCost { known_cost_microusd: row.route_cost_microusd };
            if let Err(error) = budget.check(usage, cost) {
                if error.code == "concurrency_limit" {
                    admission.queued.push(*candidate);
                } else {
                    admission.blocked.push(BlockedRun { run_id: *candidate, reason: error.code });
                }
                continue;
            }
            self.commit(*candidate, RunState::Queued, RunState::Ready, "admitted").await?;
            admission.admitted.push(*candidate);
            usage.active_workers = usage.active_workers.saturating_add(1);
            usage.requests = usage.requests.saturating_add(1);
            usage.cost_microusd = usage.cost_microusd.saturating_add(row.route_cost_microusd.unwrap_or(0));
        }
        Ok(admission)
    }

    /// Read every run of the project once, deriving usage from the rows.
    async fn observe(&self) -> Result<Observed> {
        let project = self.project.to_string();
        let rows = self
            .store
            .with_connection(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT r.id, r.node_id, r.state, n.body, r.body FROM runs r JOIN nodes n ON n.id=r.node_id JOIN jobs j ON j.id=n.job_id WHERE j.project_id=?1",
                )?;
                let mapped = stmt.query_map([project], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                    ))
                })?;
                let mut rows = Vec::new();
                for row in mapped {
                    rows.push(row?);
                }
                Ok(rows)
            })
            .await?;

        let mut observed = Observed::default();
        for (run_id, node_id, state, node_body, run_body) in rows {
            if ACTIVE_STATES.contains(&state.as_str()) {
                observed.usage.active_workers = observed.usage.active_workers.saturating_add(1);
            }
            if state != "queued" {
                observed.usage.requests = observed.usage.requests.saturating_add(1);
            }
            let node: serde_json::Value = serde_json::from_str(&node_body).map_err(|_| CoreError::new("storage_corrupt", "Node body is corrupt"))?;
            let dependencies = node
                .get("dependencies")
                .and_then(serde_json::Value::as_array)
                .map(|items| items.iter().filter_map(|item| item.as_str().map(str::to_string)).collect())
                .unwrap_or_default();
            let body: serde_json::Value = serde_json::from_str(&run_body).map_err(|_| CoreError::new("storage_corrupt", "Run body is corrupt"))?;
            let route_cost_microusd = body.get("route_cost_microusd").and_then(serde_json::Value::as_u64);
            observed.usage.cost_microusd = observed.usage.cost_microusd.saturating_add(route_cost_microusd.unwrap_or(0));
            observed.node_states.insert(node_id, state.clone());
            observed.runs.insert(run_id, RunRow { state, dependencies, route_cost_microusd });
        }
        Ok(observed)
    }

    /// Commit one validated transition, compare-and-set on the from-state, so a
    /// concurrent writer is detected instead of clobbered. The target state's
    /// event is resolved through the transition table first, so an illegal pair
    /// can never be written.
    async fn commit(&self, run_id: RunId, from: RunState, to: RunState, reason: &str) -> Result<()> {
        let event = match to {
            RunState::Ready => RunEvent::Ready,
            RunState::Failed => RunEvent::Failed,
            other => {
                return Err(CoreError::new("illegal_transition", format!("Admission never commits to {other:?}")));
            }
        };
        transition(from, event)?;
        let project = self.project;
        let reason = reason.to_string();
        self.store
            .with_transaction(move |tx| {
                let changed = tx.execute(
                    "UPDATE runs SET state=?2 WHERE id=?1 AND state=?3",
                    params![run_id.to_string(), state_str(to), state_str(from)],
                )?;
                if changed != 1 {
                    return Err(CoreError::new("state_conflict", "Run state changed concurrently; admit again"));
                }
                append_tx(
                    tx,
                    None,
                    Some(project),
                    Some(run_id),
                    &crate::contracts::CoreEvent::StateChanged {
                        entity_id: run_id.to_string(),
                        state: format!("{} -> {} ({})", state_str(from), state_str(to), reason),
                    },
                )?;
                Ok(())
            })
            .await
    }
}

#[derive(Debug, Clone)]
struct RunRow {
    state: String,
    dependencies: Vec<String>,
    route_cost_microusd: Option<u64>,
}

#[derive(Debug, Default)]
struct Observed {
    usage: Usage,
    runs: HashMap<String, RunRow>,
    node_states: HashMap<String, String>,
}
