//! `Ask first`, done the Whity way: the call becomes a proposal the person
//! approves or declines, and it runs only on approval.
//!
//! The PHP host has one worker, so the bridge never holds a call open
//! waiting for a person. It answers `ask_first` with a proposal id at once;
//! the plugin shows "waiting for your approval" and asks for the outcome by
//! id. The app lists waiting proposals, the person answers, and on approval
//! the bridge runs the call with the arguments it captured and keeps the
//! result for the plugin to collect. A proposal nobody answers expires.

use crate::driver::{DriverError, Registry};
use serde::Serialize;
use serde_json::Value;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// How long a proposal waits for an answer.
pub const WAIT: Duration = Duration::from_secs(10 * 60);
/// How long an answered proposal stays collectable.
const KEEP: Duration = Duration::from_secs(60 * 60);

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", tag = "state")]
pub enum Outcome {
    Waiting,
    Done {
        result: Value,
    },
    /// Approved, and the driver refused or failed.
    Failed {
        error: DriverError,
    },
    Declined,
    Expired,
}

/// What the app shows in its approvals list and what the plugin polls.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Proposal {
    pub id: String,
    pub plugin: String,
    pub capability: String,
    pub args: Value,
    /// Seconds since the Unix epoch, for the screen.
    pub created_at: u64,
    pub outcome: Outcome,
    #[serde(skip)]
    created: Instant,
    #[serde(skip)]
    answered: Option<Instant>,
}

/// Fired when a proposal opens or is answered, so the app can update its
/// window without polling.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase", tag = "kind")]
pub enum ProposalEvent {
    Opened { proposal: Proposal },
    Answered { proposal: Proposal },
}

pub type ProposalListener = Arc<dyn Fn(ProposalEvent) + Send + Sync>;

pub struct Proposals {
    inner: Mutex<Vec<Proposal>>,
    registry: Arc<Registry>,
    listener: ProposalListener,
}

impl Proposals {
    pub fn new(registry: Arc<Registry>, listener: ProposalListener) -> Self {
        Self {
            inner: Mutex::new(Vec::new()),
            registry,
            listener,
        }
    }

    /// Open a proposal and return its id.
    pub fn open(&self, plugin: &str, capability: &str, args: Value) -> String {
        let id = crate::bridge::random_token();
        let proposal = Proposal {
            id: id.clone(),
            plugin: plugin.to_string(),
            capability: capability.to_string(),
            args,
            created_at: now_secs(),
            outcome: Outcome::Waiting,
            created: Instant::now(),
            answered: None,
        };
        {
            let mut inner = self.inner.lock().unwrap();
            self.sweep(&mut inner);
            inner.push(proposal.clone());
        }
        (self.listener)(ProposalEvent::Opened { proposal });
        id
    }

    /// Every proposal the app may show: waiting ones and recent answers.
    pub fn list(&self) -> Vec<Proposal> {
        let mut inner = self.inner.lock().unwrap();
        self.sweep(&mut inner);
        inner.clone()
    }

    /// The outcome for a plugin's own proposal; `None` when the id is not
    /// that plugin's (or does not exist), which the bridge reports as
    /// unauthorized rather than confirming the id exists.
    pub fn outcome_for(&self, plugin: &str, id: &str) -> Option<Outcome> {
        let mut inner = self.inner.lock().unwrap();
        self.sweep(&mut inner);
        inner
            .iter()
            .find(|p| p.id == id && p.plugin == plugin)
            .map(|p| p.outcome.clone())
    }

    /// The person's answer. Approval runs the call now, on this thread.
    pub fn answer(&self, id: &str, approve: bool) -> Result<Proposal, String> {
        let (capability, args) = {
            let mut inner = self.inner.lock().unwrap();
            self.sweep(&mut inner);
            let proposal = inner
                .iter_mut()
                .find(|p| p.id == id)
                .ok_or("no such proposal")?;
            if proposal.outcome != Outcome::Waiting {
                return Err("this proposal was already answered".into());
            }
            if !approve {
                proposal.outcome = Outcome::Declined;
                proposal.answered = Some(Instant::now());
                let done = proposal.clone();
                drop(inner);
                (self.listener)(ProposalEvent::Answered {
                    proposal: done.clone(),
                });
                return Ok(done);
            }
            (proposal.capability.clone(), proposal.args.clone())
        };
        // Run outside the lock: a driver call can take a while.
        let outcome = match self.registry.dispatch(&capability, args) {
            Ok(result) => Outcome::Done { result },
            Err(error) => Outcome::Failed { error },
        };
        let done = {
            let mut inner = self.inner.lock().unwrap();
            let proposal = inner
                .iter_mut()
                .find(|p| p.id == id)
                .ok_or("the proposal went away")?;
            proposal.outcome = outcome;
            proposal.answered = Some(Instant::now());
            proposal.clone()
        };
        (self.listener)(ProposalEvent::Answered {
            proposal: done.clone(),
        });
        Ok(done)
    }

    /// Expire the unanswered past `WAIT`, drop the answered past `KEEP`.
    fn sweep(&self, inner: &mut Vec<Proposal>) {
        let now = Instant::now();
        let mut expired = Vec::new();
        for proposal in inner.iter_mut() {
            if proposal.outcome == Outcome::Waiting && now.duration_since(proposal.created) > WAIT {
                proposal.outcome = Outcome::Expired;
                proposal.answered = Some(now);
                expired.push(proposal.clone());
            }
        }
        inner.retain(|p| p.answered.is_none_or(|t| now.duration_since(t) <= KEEP));
        for proposal in expired {
            (self.listener)(ProposalEvent::Answered { proposal });
        }
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::driver::tests::EchoDriver;
    use serde_json::json;

    fn proposals() -> (Proposals, Arc<Mutex<Vec<String>>>) {
        let mut registry = Registry::new();
        registry.add(Box::new(EchoDriver));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = seen.clone();
        let listener: ProposalListener = Arc::new(move |event: ProposalEvent| {
            let label = match &event {
                ProposalEvent::Opened { .. } => "opened".to_string(),
                ProposalEvent::Answered { proposal } => format!(
                    "answered:{}",
                    serde_json::to_value(&proposal.outcome).unwrap()["state"]
                ),
            };
            sink.lock().unwrap().push(label);
        });
        (Proposals::new(Arc::new(registry), listener), seen)
    }

    #[test]
    fn approval_runs_the_call_and_the_plugin_collects_the_result() {
        let (proposals, seen) = proposals();
        let id = proposals.open("Probe", "echo.say", json!({"n": 1}));
        assert_eq!(proposals.outcome_for("Probe", &id), Some(Outcome::Waiting));
        assert_eq!(proposals.outcome_for("Other", &id), None);
        let done = proposals.answer(&id, true).unwrap();
        assert_eq!(
            done.outcome,
            Outcome::Done {
                result: json!({"said": {"n": 1}})
            }
        );
        assert_eq!(
            proposals.outcome_for("Probe", &id),
            Some(done.outcome.clone())
        );
        assert!(proposals.answer(&id, true).is_err(), "answered once");
        assert_eq!(*seen.lock().unwrap(), vec!["opened", "answered:\"done\""]);
    }

    #[test]
    fn a_decline_runs_nothing_and_a_failing_driver_is_reported() {
        let (proposals, _) = proposals();
        let id = proposals.open("Probe", "echo.say", json!(null));
        assert_eq!(
            proposals.answer(&id, false).unwrap().outcome,
            Outcome::Declined
        );
        let id = proposals.open("Probe", "echo.fail", json!(null));
        assert_eq!(
            proposals.answer(&id, true).unwrap().outcome,
            Outcome::Failed {
                error: DriverError::Failed("as asked".into())
            }
        );
        assert!(proposals.answer("nope", true).is_err());
    }

    #[test]
    fn the_list_shows_waiting_and_answered_proposals() {
        let (proposals, _) = proposals();
        let a = proposals.open("Probe", "echo.say", json!(1));
        let b = proposals.open("Probe", "echo.say", json!(2));
        proposals.answer(&b, false).unwrap();
        let list = proposals.list();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].id, a);
        assert_eq!(list[0].outcome, Outcome::Waiting);
        assert_eq!(list[1].outcome, Outcome::Declined);
        let json = serde_json::to_value(&list[0]).unwrap();
        assert_eq!(json["outcome"], json!({"state": "waiting"}));
        assert!(json.get("created").is_none());
    }
}
