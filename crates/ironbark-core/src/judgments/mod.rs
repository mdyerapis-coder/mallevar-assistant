//! J1/J2/J3: local JEV judgment domains, held-out qualification, advisory use.
pub mod advisory;
pub mod qualification;
pub use advisory::JudgmentAdvisor;
pub use qualification::{
    binom_upper95, gate, ClassMetrics, DomainQualifications, EvaluationReportInput, GateVerdict,
    HoldoutMetrics, LatencyMs, MIN_HOLDOUT, QualificationStore, SplitDigests,
};
