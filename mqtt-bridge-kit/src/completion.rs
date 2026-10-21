//! Exact completion of a command that commits its own inventory mapping.
use std::{future::Future, sync::Arc};

/// Single-use permission to retain exactly one active command's final reply
/// through its own generation-table replacement. It cannot authorize old work.
#[derive(Debug, Clone)]
pub struct CommandCompletion(Arc<()>);
impl CommandCompletion {
    pub(crate) fn new() -> Self {
        Self(Arc::new(()))
    }
    pub(crate) fn same(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}
tokio::task_local! {static CURRENT:Option<CommandCompletion>;}
pub(crate) fn current() -> Option<CommandCompletion> {
    CURRENT.try_with(Clone::clone).ok().flatten()
}
pub(crate) async fn under_completion<F: Future>(completion: Option<CommandCompletion>, future: F) -> F::Output {
    // WAIT: device-op
    CURRENT.scope(completion, future).await
}
#[cfg(test)]
mod runtime_tests;

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn completion_scope_is_not_inherited_by_helpers_or_later_calls() {
        assert!(current().is_none());
        let token = CommandCompletion::new();
        under_completion(Some(token.clone()), async {
            assert!(current().is_some_and(|current| current.same(&token)));
            assert!(tokio::spawn(async { current() }).await.unwrap().is_none(), "spawned helper inherited committing capability");
        })
        .await;
        assert!(current().is_none(), "completed operation leaked its capability");
        under_completion(None, async {
            assert!(current().is_none());
        })
        .await;
    }
}
