// H1: real binary probe registry. Each harness capability state is reported
// honestly: binary missing = Unavailable; binary present but untested by our
// live smoke = Unqualified; only a recorded qualification upgrades to Qualified.

use std::fmt;
use std::path::Path;

use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};

use crate::{
    contracts::{CapabilityObservation, CapabilityState, HarnessDescriptor, HarnessKind},
    error::{CoreError, Result},
    store::{Store, events::now_ms},
};

const HARNESS_OBS_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS harness_observations (
  harness TEXT NOT NULL,
  name TEXT NOT NULL,
  state TEXT NOT NULL CHECK(state IN ('unknown','unavailable','unqualified','qualified')),
  body TEXT NOT NULL,
  observed_at INTEGER NOT NULL,
  PRIMARY KEY (harness, name)
);
CREATE INDEX IF NOT EXISTS harness_observations_harness ON harness_observations(harness);
"#;

impl fmt::Display for HarnessKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", match self {
            HarnessKind::Omp => "omp",
            HarnessKind::Codex => "codex",
            HarnessKind::Opencode => "opencode",
            HarnessKind::Claude => "claude",
            HarnessKind::Hermes => "hermes",
            HarnessKind::Alphacode => "alphacode",
        })
    }
}

/// Registry of native harness capabilities via real binary probes.
#[derive(Clone)]
pub struct HarnessRegistry {
    store: Store,
}

impl HarnessRegistry {
    pub fn new(store: Store) -> Self {
        Self { store }
    }

    async fn ensure_schema(&self) -> Result<()> {
        self.store
            .with_connection(|conn| {
                conn.execute_batch(HARNESS_OBS_SCHEMA)?;
                Ok(())
            })
            .await
    }

    /// Describe a harness by probing its binary (if any). Returns honest
    /// capability states: Unavailable if the binary is missing; Unqualified if
    /// present but untested; Qualified only if a prior live qualification
    /// observation exists.
    pub async fn describe(&self, kind: HarnessKind) -> Result<HarnessDescriptor> {
        self.ensure_schema().await?;

        let (binary, transport, version) = probe_binary(kind).await;
        let base_state = match (binary.is_empty(), version.is_empty()) {
            (true, _) => CapabilityState::Unavailable,
            (false, false) => {
                // Binary present but no live qualification evidence.
                // Unqualified = implementation exists, not yet qualified.
                // (The live smoke gates Qualified.)
                CapabilityState::Unqualified
            }
            (false, true) => CapabilityState::Unqualified,
        };

        // Check for a prior qualified observation.
        let qualified = self.has_qualified_observation(kind).await?;

        let cap_state = if qualified { CapabilityState::Qualified } else { base_state };

        let capabilities = vec![
            CapabilityObservation {
                name: "approval".into(),
                state: cap_state,
                implementation_version: version.clone(),
                evidence_digest: None,
                observed_at: now_ms(),
            },
            CapabilityObservation {
                name: "cancel".into(),
                state: cap_state,
                implementation_version: version.clone(),
                evidence_digest: None,
                observed_at: now_ms(),
            },
            CapabilityObservation {
                name: "resume".into(),
                state: cap_state,
                implementation_version: version.clone(),
                evidence_digest: None,
                observed_at: now_ms(),
            },
            CapabilityObservation {
                name: "child_observability".into(),
                state: cap_state,
                implementation_version: version.clone(),
                evidence_digest: None,
                observed_at: now_ms(),
            },
        ];

        Ok(HarnessDescriptor {
            harness: kind,
            version,
            transport,
            route_compatibility: harness_route_compatibility(kind),
            approval: cap_state,
            cancel: cap_state,
            resume: cap_state,
            child_observability: cap_state,
            capabilities,
        })
    }

    /// Record a live qualification observation (e.g. after a passing smoke).
    pub async fn record_qualification(&self, kind: HarnessKind, version: String, evidence: String) -> Result<()> {
        self.ensure_schema().await?;
        let obs = serde_json::json!({
            "qualified": true,
            "evidence": evidence,
            "version": version,
        });
        self.store
            .with_transaction(move |tx| {
                tx.execute(
                    "INSERT INTO harness_observations(harness,name,state,body,observed_at) VALUES(?1,'approval','qualified',?2,?3)
                     ON CONFLICT(harness,name) DO UPDATE SET state='qualified',body=excluded.body,observed_at=excluded.observed_at",
                    params![kind.to_string(), serde_json::to_string(&obs)?, now_ms()],
                )?;
                // Same for other capability names (idempotent)
                for name in ["cancel", "resume", "child_observability"] {
                    tx.execute(
                        "INSERT INTO harness_observations(harness,name,state,body,observed_at) VALUES(?1,?2,'qualified',?3,?4)
                         ON CONFLICT(harness,name) DO UPDATE SET state='qualified',body=excluded.body,observed_at=excluded.observed_at",
                        params![kind.to_string(), name, serde_json::to_string(&obs)?, now_ms()],
                    )?;
                }
            })
            .await
    }

    async fn has_qualified_observation(&self, kind: HarnessKind) -> Result<bool> {
        self.store
            .with_connection({
                let kind = kind.to_string();
                move |conn| {
                    let cnt: i64 = conn.query_row(
                        "SELECT COUNT(*) FROM harness_observations WHERE harness=?1 AND state='qualified'",
                        [kind],
                        |r| r.get(0),
                    )?;
                    Ok(cnt > 0)
                }
            })
            .await
    }
}

/// Probe a harness binary for version. Returns (binary_path, transport, version).
/// If the binary is missing or version cannot be determined, version is empty.
async fn probe_binary(kind: HarnessKind) -> (String, String, String) {
    let (binary_name, transport) = match kind {
        HarnessKind::Omp => ("omp", "acp-stdio"),
        HarnessKind::Codex => ("codex", "app-server"),
        HarnessKind::Opencode => ("opencode", "acp-stdio"),
        HarnessKind::Claude => ("claude", "agent-sdk"),
        HarnessKind::Hermes => ("hermes", "acp-stdio"),
        HarnessKind::Alphacode => ("alphacode", "managed-socket"),
    };
    let transport = transport.to_owned();

    // Check presence via `which`.
    let binary_path = tokio::task::spawn_blocking(move || {
        let out = std::process::Command::new("which")
            .arg(binary_name)
            .output();
        match out {
            Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim().to_owned(),
            _ => String::new(),
        }
    })
    .await
    .unwrap_or_default();

    if binary_path.is_empty() {
        return (String::new(), transport, String::new());
    }

    // Probe version.
    let version = tokio::task::spawn_blocking(move || {
        let out = std::process::Command::new(binary_name)
            .arg("--version")
            .output();
        match out {
            Ok(o) if o.status.success() => {
                let s = String::from_utf8_lossy(&o.stdout).trim().to_owned();
                // Take first line only (some tools print build info).
                s.lines().next().unwrap_or(&s).to_owned()
            }
            _ => String::new(),
        }
    })
    .await
    .unwrap_or_default();

    (binary_path, transport, version)
}

/// The set of route connection ids this harness's adapter can present/accept.
fn harness_route_compatibility(kind: HarnessKind) -> Vec<String> {
    match kind {
        HarnessKind::Omp => vec!["acp:stdin/stdout".to_owned()],
        HarnessKind::Codex => vec!["app-server".to_owned()],
        HarnessKind::Opencode => vec!["acp:stdin/stdout".to_owned()],
        HarnessKind::Claude => vec!["agent-sdk".to_owned()],
        HarnessKind::Hermes => vec!["acp:stdin/stdout".to_owned()],
        HarnessKind::Alphacode => vec!["managed-socket".to_owned()],
    }
}