use crate::suite::config::Platform;
use serde::{Deserialize, Serialize};

/// A tester's judgment of one outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Answer {
    Held,
    Failed,
    /// Observed, but the tester can't tell whether it's right.
    Unsure,
    /// Never got to observe it.
    CouldntCheck,
}

/// What a tester's judgments add up to. Unsure is a claim, never a final result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum Claim {
    Pass,
    Fail,
    Blocked,
    Unsure,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum Final {
    Pass,
    Fail,
    Blocked,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Bug,
    Finding,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Judgment {
    /// Index into the spec's Outcomes list.
    pub outcome: usize,
    pub answer: Answer,
    /// What the tester saw; required for every answer, so a result always rests on an observation.
    pub observed: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence: Vec<String>,
}

/// A problem a tester found outside the spec's outcomes while probing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SideFind {
    pub summary: String,
    pub observed: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Posted {
    pub judgments: Vec<Judgment>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub side_finds: Vec<SideFind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proposed_severity: Option<Severity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
}

impl Posted {
    pub fn claim(&self) -> Claim {
        let any = |a: Answer| self.judgments.iter().any(|j| j.answer == a);
        if any(Answer::Failed) {
            Claim::Fail
        } else if any(Answer::CouldntCheck) {
            Claim::Blocked
        } else if any(Answer::Unsure) {
            Claim::Unsure
        } else {
            Claim::Pass
        }
    }
}

/// The coordinator's review of a claim. Every call carries a one-line reason.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "call", rename_all = "kebab-case")]
pub enum Call {
    /// The evidence is conclusive: file the claim. A FAIL needs a severity; a BLOCKED without one is a harness gap.
    Uphold { severity: Option<Severity> },
    /// Queue a fresh attempt told the claim, to reproduce it.
    Verify { claim: String },
    /// The tester erred: queue a fresh attempt without the claim.
    SendBack,
    /// Resolve an Unsure from the evidence.
    Judge {
        result: Final,
        severity: Option<Severity>,
    },
    /// Still unclear after verifying: BLOCKED, and the spec needs rewording.
    SpecUnclear,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProblemSource {
    Row,
    SideFind,
    /// A verification attempt passed: the original claim didn't reproduce.
    NotReproduced,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Problem {
    pub id: u32,
    pub spec: String,
    pub platform: Platform,
    pub attempt: u32,
    pub severity: Severity,
    pub source: ProblemSource,
    pub summary: String,
    /// Whether a verification attempt reproduced it before it was upheld.
    pub verified: bool,
    pub reason: String,
    pub status: Status,
}

/// Where triage left a problem. Open until the person and the coordinator decide.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "kebab-case")]
pub enum Status {
    #[default]
    Open,
    /// Not a bug, or not reproduced.
    Dismissed { reason: String },
    /// A fix landed, proven by a later pass of its row.
    Fixed { ticket: String, reason: String },
    /// Accepted for this release.
    Deferred { ticket: String, reason: String },
    /// Folded into the original: its platform and evidence join it, and the original's status applies.
    Duplicate { of: u32 },
}

/// A triage decision, as the coordinator records it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "kebab-case")]
pub enum Decision {
    Dismiss,
    Fix { ticket: String },
    Defer { ticket: String },
    Duplicate { of: u32 },
}
