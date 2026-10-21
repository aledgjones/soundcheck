use crate::suite::config::Platform;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// A tester's judgment of one outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "UPPERCASE")]
pub enum Claim {
    Pass,
    Fail,
    Blocked,
    Unsure,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "UPPERCASE")]
pub enum Final {
    Pass,
    Fail,
    Blocked,
}

/// What a problem means for the release.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ProblemKind {
    /// Blocks the release until triaged: every upheld FAIL, including a test the app stopped.
    Failure,
    /// Doesn't block but needs a decision: outside the check, or not reproduced.
    Finding,
}

/// Why an upheld BLOCKED couldn't be checked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Cause {
    /// The app stopped it (a crash, a broken screen): the row is filed FAIL, and its problem is a failure.
    App,
    /// The environment or harness did (a missing label, a tool that can't reach a control): BLOCKED, with a note.
    Harness,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SideFind {
    pub summary: String,
    pub observed: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Posted {
    pub judgments: Vec<Judgment>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub side_finds: Vec<SideFind>,
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "call", rename_all = "kebab-case")]
pub enum Call {
    /// The evidence is conclusive: file the claim. A FAIL becomes a failure, which needs a plain-words title; a
    /// BLOCKED needs its cause, and a title too when the app caused it.
    Uphold {
        title: Option<String>,
        cause: Option<Cause>,
    },
    /// Queue a fresh attempt told the claim, to reproduce it.
    Verify { claim: String },
    /// The tester erred: queue a fresh attempt without the claim.
    SendBack,
    /// Resolve an Unsure from the evidence.
    Judge {
        result: Final,
        /// Needed when the result is FAIL: the problem's title.
        title: Option<String>,
    },
    /// Still unclear after verifying: BLOCKED, and the spec needs rewording.
    SpecUnclear,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum ProblemSource {
    Row,
    SideFind,
    /// A verification attempt passed: the original claim didn't reproduce.
    NotReproduced,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Problem {
    pub id: u32,
    pub spec: String,
    pub platform: Platform,
    pub attempt: u32,
    pub kind: ProblemKind,
    pub source: ProblemSource,
    /// In plain words, written by the coordinator.
    pub title: String,
    /// What was expected and what was seen, from the tester's judgments.
    pub detail: String,
    /// Whether a verification attempt reproduced it before it was upheld.
    pub verified: bool,
    pub reason: String,
    pub status: Status,
}

/// Where triage left a problem. Open until the person and the coordinator decide.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "decision", rename_all = "kebab-case")]
pub enum Decision {
    Dismiss,
    Fix { ticket: String },
    Defer { ticket: String },
    Duplicate { of: u32 },
}

/// The build installed on a platform, as the project's build-identity hook reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Build {
    pub commit: String,
    /// Changes when native code changes; a new one reruns every row on the platform.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

impl Build {
    pub fn label(&self) -> String {
        let short: String = self.commit.chars().take(7).collect();
        match &self.version {
            Some(v) => format!("{v} ({short})"),
            None => short,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Rerun {
    pub spec: String,
    pub reason: String,
}
