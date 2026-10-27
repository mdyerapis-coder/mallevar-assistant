//! H6: disclosure-filtered context handoffs.
//!
//! When one node's result feeds another (a reviewer reading an implementer's
//! output, a second harness picking up where a first left off), the handoff is a
//! master [`ContextPackage`]: a brief, explicit constraints, and the excerpts a
//! downstream route is *allowed* to see. Nothing else crosses the boundary.
//!
//! Two rules are non-negotiable here:
//! - a `LocalOnly` excerpt is never handed to a route, and an
//!   `AllowedConnections` excerpt only to a route named in its policy;
//! - the handoff carries no hidden reasoning and no native session resume. Even
//!   when the upstream result records a native session, it is deliberately not
//!   forwarded: a fabricated or stale session reference would let a worker
//!   pretend to resume work it never owned. What crosses is named explicitly -
//!   the artifact ids and the checks the upstream run produced.

use sha2::{Digest as _, Sha256};

use crate::contracts::{ContextPackage, Digest, DisclosurePolicy, ProjectPolicy, ResultManifest, RunId};
use crate::error::Result;

/// Everything a handoff needs: the upstream result, the excerpts that could be
/// disclosed, the policy in force and the route that will consume the handoff.
/// `route_connection_id` is `None` for an in-core handoff (nothing leaves the
/// machine).
pub struct HandoffRequest<'a> {
    pub upstream_run: RunId,
    pub upstream: &'a ResultManifest,
    pub excerpts: &'a [crate::contracts::CitedExcerpt],
    pub policy: &'a ProjectPolicy,
    pub route_connection_id: Option<&'a str>,
}

/// Build the disclosure-filtered handoff for one downstream node.
pub fn build_handoff(request: &HandoffRequest<'_>) -> Result<ContextPackage> {
    let excerpts: Vec<crate::contracts::CitedExcerpt> = request
        .excerpts
        .iter()
        .filter(|excerpt| excerpt.project_id == request.policy.project_id)
        .filter(|excerpt| disclosable(&excerpt.disclosure, request.route_connection_id))
        .cloned()
        .collect();

    let passed: Vec<String> = request
        .upstream
        .checks
        .iter()
        .map(|check| format!("check {}: {}", check.name, if check.passed { "pass" } else { "fail" }))
        .collect();
    let mut constraints = vec![
        "handoff carries no hidden reasoning and no native session resume; only named artifacts and checks cross".to_string(),
    ];
    constraints.extend(passed);

    let brief = format!(
        "Handoff from run {}: {} artifact(s), {} check(s); {} excerpt(s) disclosed to {}.",
        request.upstream_run,
        request.upstream.artifacts.len(),
        request.upstream.checks.len(),
        excerpts.len(),
        request.route_connection_id.unwrap_or("in-core"),
    );

    let artifacts = request.upstream.artifacts.clone();
    let unresolved_questions = request.upstream.unresolved.clone();
    // Never fabricated: the upstream native session is not forwarded, and no
    // synthetic session is invented here.
    let originating_sessions = Vec::new();

    let digest = package_digest(&brief, &constraints, &excerpts, &artifacts, &unresolved_questions)?;
    Ok(ContextPackage {
        digest,
        brief,
        constraints,
        excerpts,
        artifacts,
        unresolved_questions,
        originating_sessions,
    })
}

/// A disclosure policy decides who may see an excerpt. `LocalOnly` never leaves
/// the core; `AllowedConnections` only reaches a named connection;
/// `ExplicitPublic` may go to any route.
fn disclosable(disclosure: &DisclosurePolicy, route_connection_id: Option<&str>) -> bool {
    match disclosure {
        DisclosurePolicy::LocalOnly => route_connection_id.is_none(),
        DisclosurePolicy::AllowedConnections { connection_ids } => route_connection_id
            .is_some_and(|route| connection_ids.iter().any(|allowed| allowed == route)),
        DisclosurePolicy::ExplicitPublic => true,
    }
}

fn package_digest(
    brief: &str,
    constraints: &[String],
    excerpts: &[crate::contracts::CitedExcerpt],
    artifacts: &[crate::contracts::ArtifactId],
    unresolved: &[String],
) -> Result<Digest> {
    let value = serde_json::json!({
        "brief": brief,
        "constraints": constraints,
        "excerpts": excerpts,
        "artifacts": artifacts.iter().map(|id| id.to_string()).collect::<Vec<_>>(),
        "unresolved_questions": unresolved,
    });
    let bytes = serde_json::to_vec(&value)?;
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    Digest::parse(&format!("{:x}", hasher.finalize()))
}
