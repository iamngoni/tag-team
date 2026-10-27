use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{
    BlobRef, EventKind, FailReason, NodeId, ResultSummary, SubtaskState, schedule::NoFitReason,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "input", rename_all = "snake_case")]
pub enum SubtaskInput {
    Placed { node: NodeId },
    NoFit { reasons: Vec<NoFitReason> },
    Prepared,
    RunnerStarted { session: String },
    Finished { result: ResultSummary },
    Collected { blob: BlobRef },
    Landed { result: ResultSummary },
    Fail { reason: FailReason },
    Cancel { reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("illegal subtask transition from {current:?} for {input:?}")]
pub struct IllegalTransition {
    pub current: SubtaskState,
    pub input: SubtaskInput,
}

/// The only state-transition function. Store the returned state and event in
/// the same SQLite transaction so watchers cannot observe one without the other.
pub fn step(
    current: &SubtaskState,
    input: SubtaskInput,
) -> Result<(SubtaskState, EventKind), Box<IllegalTransition>> {
    if current.is_terminal() {
        return Err(Box::new(IllegalTransition {
            current: current.clone(),
            input,
        }));
    }

    match (&current, &input) {
        (
            SubtaskState::Queued | SubtaskState::Placing | SubtaskState::Reassigning { .. },
            SubtaskInput::Placed { node },
        ) => Ok((SubtaskState::Preparing, EventKind::Placed { node: *node })),
        (SubtaskState::Placing, SubtaskInput::NoFit { reasons }) => Ok((
            SubtaskState::Queued,
            EventKind::Progress {
                note: format!("queued: {} reason(s) reported", reasons.len()),
                percent: None,
            },
        )),
        (SubtaskState::Preparing, SubtaskInput::Prepared) => Ok((
            SubtaskState::Running,
            EventKind::Progress {
                note: "workspace prepared".into(),
                percent: None,
            },
        )),
        (
            SubtaskState::Preparing | SubtaskState::Running,
            SubtaskInput::RunnerStarted { session },
        ) => Ok((
            SubtaskState::Running,
            EventKind::Progress {
                note: format!("runner started ({session})"),
                percent: None,
            },
        )),
        (SubtaskState::Running, SubtaskInput::Finished { result }) => {
            let next = if result.bundle.is_some() {
                SubtaskState::Collecting
            } else {
                SubtaskState::Done
            };
            let kind = if result.bundle.is_some() {
                EventKind::Progress {
                    note: "runner finished; collecting result".into(),
                    percent: Some(100),
                }
            } else {
                EventKind::Finished(result.clone())
            };
            Ok((next, kind))
        }
        (SubtaskState::Running | SubtaskState::Collecting, SubtaskInput::Collected { blob }) => {
            Ok((
                SubtaskState::Collecting,
                EventKind::Progress {
                    note: format!("result bundle received ({} bytes)", blob.size),
                    percent: Some(100),
                },
            ))
        }
        (SubtaskState::Collecting, SubtaskInput::Landed { result }) => {
            Ok((SubtaskState::Done, EventKind::Finished(result.clone())))
        }
        (_, SubtaskInput::Fail { reason }) => Ok((
            SubtaskState::Failed {
                reason: reason.clone(),
            },
            EventKind::Failed {
                reason: reason.clone(),
            },
        )),
        (_, SubtaskInput::Cancel { .. }) => Ok((SubtaskState::Cancelled, EventKind::Cancelled)),
        _ => Err(Box::new(IllegalTransition {
            current: current.clone(),
            input,
        })),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Check, DiffStat, Outcome, ResultSummary};

    fn summary(bundle: bool) -> ResultSummary {
        ResultSummary {
            outcome: Outcome::Completed,
            summary: "done".into(),
            bundle: bundle.then_some(BlobRef {
                sha256: [9; 32],
                size: 10,
            }),
            branch: Some("tagteam/test-abc123".into()),
            diffstat: DiffStat::default(),
            assumptions: vec![],
            checks: vec![Check {
                what: "test".into(),
                command: Some("cargo test".into()),
                passed: true,
                output_tail: String::new(),
            }],
        }
    }

    #[test]
    fn terminal_states_never_leave() {
        for state in [
            SubtaskState::Done,
            SubtaskState::Failed {
                reason: FailReason::Model("no".into()),
            },
            SubtaskState::Cancelled,
        ] {
            let result = step(
                &state,
                SubtaskInput::Cancel {
                    reason: "again".into(),
                },
            );
            assert!(result.is_err());
        }
    }

    #[test]
    fn cancel_is_reachable_from_every_non_terminal_state() {
        let states = [
            SubtaskState::Queued,
            SubtaskState::Placing,
            SubtaskState::Preparing,
            SubtaskState::Running,
            SubtaskState::WaitingOnMain {
                question: crate::QuestionId::new(),
            },
            SubtaskState::Reassigning {
                reason: crate::ReassignReason::NodeLost,
            },
            SubtaskState::Collecting,
        ];
        for state in states {
            let (next, event) = step(
                &state,
                SubtaskInput::Cancel {
                    reason: "owner".into(),
                },
            )
            .expect("cancel must work");
            assert_eq!(next, SubtaskState::Cancelled);
            assert_eq!(event, EventKind::Cancelled);
        }
    }

    #[test]
    fn a_finished_result_without_a_bundle_is_done() {
        let (next, event) = step(
            &SubtaskState::Running,
            SubtaskInput::Finished {
                result: summary(false),
            },
        )
        .expect("finish");
        assert_eq!(next, SubtaskState::Done);
        assert!(matches!(event, EventKind::Finished(_)));
    }

    #[test]
    fn a_finished_result_with_a_bundle_is_collected_then_landed() {
        let (collecting, first) = step(
            &SubtaskState::Running,
            SubtaskInput::Finished {
                result: summary(true),
            },
        )
        .expect("finish");
        assert_eq!(collecting, SubtaskState::Collecting);
        assert!(matches!(first, EventKind::Progress { .. }));

        let (done, final_event) = step(
            &collecting,
            SubtaskInput::Landed {
                result: summary(true),
            },
        )
        .expect("land");
        assert_eq!(done, SubtaskState::Done);
        assert!(matches!(final_event, EventKind::Finished(_)));
    }
}
