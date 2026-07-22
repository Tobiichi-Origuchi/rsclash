use async_trait::async_trait;
use thiserror::Error;

#[async_trait]
pub trait ChangeAction: Send {
    fn name(&self) -> &str;
    async fn prepare(self: Box<Self>) -> std::result::Result<Box<dyn PreparedChange>, String>;
}

#[async_trait]
pub trait PreparedChange: Send {
    fn name(&self) -> &str;
    async fn commit(&mut self) -> std::result::Result<(), String>;
    async fn compensate(&mut self) -> std::result::Result<(), String>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChangeReceipt {
    pub committed_actions: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompensationFailure {
    pub action: String,
    pub message: String,
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum SideEffectError {
    #[error("failed to prepare {action}: {message}")]
    Prepare { action: String, message: String },
    #[error("failed to commit {action}: {message}")]
    Commit { action: String, message: String },
    #[error("failed to commit {action}: {message}; one or more compensations also failed")]
    CommitAndCompensate {
        action: String,
        message: String,
        compensation_failures: Vec<CompensationFailure>,
    },
}

#[derive(Default)]
pub struct SideEffectTransaction {
    actions: Vec<Box<dyn ChangeAction>>,
}

impl SideEffectTransaction {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn push(mut self, action: impl ChangeAction + 'static) -> Self {
        self.actions.push(Box::new(action));
        self
    }

    pub async fn execute(self) -> Result<ChangeReceipt, SideEffectError> {
        let mut prepared = Vec::with_capacity(self.actions.len());
        for action in self.actions {
            let name = action.name().to_string();
            let change = action
                .prepare()
                .await
                .map_err(|message| SideEffectError::Prepare {
                    action: name,
                    message,
                })?;
            prepared.push(change);
        }

        let mut committed = Vec::with_capacity(prepared.len());
        for index in 0..prepared.len() {
            let name = prepared[index].name().to_string();
            committed.push(index);
            if let Err(message) = prepared[index].commit().await {
                let compensation_failures = compensate(&mut prepared, &committed).await;
                if compensation_failures.is_empty() {
                    return Err(SideEffectError::Commit {
                        action: name,
                        message,
                    });
                }
                return Err(SideEffectError::CommitAndCompensate {
                    action: name,
                    message,
                    compensation_failures,
                });
            }
        }

        Ok(ChangeReceipt {
            committed_actions: committed
                .into_iter()
                .map(|index| prepared[index].name().to_string())
                .collect(),
        })
    }
}

async fn compensate(
    prepared: &mut [Box<dyn PreparedChange>],
    committed: &[usize],
) -> Vec<CompensationFailure> {
    let mut failures = Vec::new();
    for index in committed.iter().rev().copied() {
        if let Err(message) = prepared[index].compensate().await {
            failures.push(CompensationFailure {
                action: prepared[index].name().to_string(),
                message,
            });
        }
    }
    failures
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;

    use super::{ChangeAction, PreparedChange, SideEffectError, SideEffectTransaction};

    struct FakeAction {
        name: &'static str,
        events: Arc<Mutex<Vec<String>>>,
        fail_prepare: bool,
        fail_commit: bool,
        fail_compensate: bool,
    }

    impl FakeAction {
        fn new(name: &'static str, events: &Arc<Mutex<Vec<String>>>) -> Self {
            Self {
                name,
                events: Arc::clone(events),
                fail_prepare: false,
                fail_commit: false,
                fail_compensate: false,
            }
        }

        fn record(&self, phase: &str) {
            self.events
                .lock()
                .expect("event lock should open")
                .push(format!("{phase}:{}", self.name));
        }
    }

    #[async_trait]
    impl ChangeAction for FakeAction {
        fn name(&self) -> &str {
            self.name
        }

        async fn prepare(self: Box<Self>) -> std::result::Result<Box<dyn PreparedChange>, String> {
            self.record("prepare");
            if self.fail_prepare {
                Err("injected prepare failure".to_string())
            } else {
                Ok(self)
            }
        }
    }

    #[async_trait]
    impl PreparedChange for FakeAction {
        fn name(&self) -> &str {
            self.name
        }

        async fn commit(&mut self) -> std::result::Result<(), String> {
            self.record("commit");
            if self.fail_commit {
                Err("injected commit failure".to_string())
            } else {
                Ok(())
            }
        }

        async fn compensate(&mut self) -> std::result::Result<(), String> {
            self.record("compensate");
            if self.fail_compensate {
                Err("injected compensation failure".to_string())
            } else {
                Ok(())
            }
        }
    }

    #[tokio::test]
    async fn prepares_every_action_before_committing_any_action() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let receipt = SideEffectTransaction::new()
            .push(FakeAction::new("config", &events))
            .push(FakeAction::new("proxy", &events))
            .execute()
            .await
            .expect("transaction should succeed");

        assert_eq!(receipt.committed_actions, vec!["config", "proxy"]);
        assert_eq!(
            *events.lock().expect("event lock should open"),
            vec![
                "prepare:config",
                "prepare:proxy",
                "commit:config",
                "commit:proxy",
            ]
        );
    }

    #[tokio::test]
    async fn preparation_failure_prevents_every_commit() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let mut second = FakeAction::new("proxy", &events);
        second.fail_prepare = true;
        let error = SideEffectTransaction::new()
            .push(FakeAction::new("config", &events))
            .push(second)
            .execute()
            .await
            .expect_err("preparation should fail");

        assert!(matches!(error, SideEffectError::Prepare { .. }));
        assert_eq!(
            *events.lock().expect("event lock should open"),
            vec!["prepare:config", "prepare:proxy"]
        );
    }

    #[tokio::test]
    async fn commit_failure_compensates_attempted_actions_in_reverse_order() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let mut second = FakeAction::new("proxy", &events);
        second.fail_commit = true;
        let error = SideEffectTransaction::new()
            .push(FakeAction::new("config", &events))
            .push(second)
            .push(FakeAction::new("tun", &events))
            .execute()
            .await
            .expect_err("commit should fail");

        assert!(matches!(error, SideEffectError::Commit { .. }));
        assert_eq!(
            *events.lock().expect("event lock should open"),
            vec![
                "prepare:config",
                "prepare:proxy",
                "prepare:tun",
                "commit:config",
                "commit:proxy",
                "compensate:proxy",
                "compensate:config",
            ]
        );
    }

    #[tokio::test]
    async fn reports_compensation_failures_with_the_commit_error() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let mut first = FakeAction::new("config", &events);
        first.fail_compensate = true;
        let mut second = FakeAction::new("proxy", &events);
        second.fail_commit = true;
        let error = SideEffectTransaction::new()
            .push(first)
            .push(second)
            .execute()
            .await
            .expect_err("commit and compensation should fail");

        let SideEffectError::CommitAndCompensate {
            compensation_failures,
            ..
        } = error
        else {
            panic!("combined failure should be returned");
        };
        assert_eq!(compensation_failures.len(), 1);
        assert_eq!(compensation_failures[0].action, "config");
    }
}
