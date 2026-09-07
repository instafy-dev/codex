//! Retains cleanup ownership for guardian sessions, including retired reviewers.

use super::GuardianReviewSession;
use std::future::Future;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::sync::RwLock;
use tokio::sync::RwLockReadGuard;

#[derive(Default)]
pub(super) struct GuardianSessionLifecycle {
    admission: RwLock<()>,
    closed: AtomicBool,
    unconfirmed_startup: AtomicBool,
    reviews: Mutex<Vec<Arc<GuardianReviewSession>>>,
}

struct StartupGuard<'a> {
    _admission: RwLockReadGuard<'a, ()>,
    unconfirmed: &'a AtomicBool,
    cleanup_required: bool,
}

impl Drop for StartupGuard<'_> {
    fn drop(&mut self) {
        if self.cleanup_required {
            self.unconfirmed.store(true, Ordering::Release);
        }
    }
}

impl GuardianSessionLifecycle {
    pub(super) async fn spawn(
        &self,
        future: impl Future<Output = anyhow::Result<GuardianReviewSession>>,
    ) -> anyhow::Result<Arc<GuardianReviewSession>> {
        anyhow::ensure!(
            !self.closed.load(Ordering::Acquire),
            "guardian manager is shutting down"
        );
        let admission = self.admission.read().await;
        anyhow::ensure!(
            !self.closed.load(Ordering::Acquire),
            "guardian manager is shutting down"
        );
        {
            let reviews = self
                .reviews
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            anyhow::ensure!(
                !self.unconfirmed_startup.load(Ordering::Acquire)
                    && !reviews
                        .iter()
                        .any(|review| matches!(review.shutdown_result.get(), Some(Err(_)))),
                "previous guardian cleanup could not be confirmed"
            );
        }
        let mut guard = StartupGuard {
            _admission: admission,
            unconfirmed: &self.unconfirmed_startup,
            cleanup_required: true,
        };
        let review = self.register(future.await?);
        guard.cleanup_required = false;
        Ok(review)
    }

    pub(super) fn register(&self, review: GuardianReviewSession) -> Arc<GuardianReviewSession> {
        let review = Arc::new(review);
        let mut reviews = self
            .reviews
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        reviews.retain(|review| !matches!(review.shutdown_result.get(), Some(Ok(()))));
        reviews.push(Arc::clone(&review));
        review
    }

    pub(super) async fn shutdown(&self, timeout: Duration) -> anyhow::Result<()> {
        self.closed.store(true, Ordering::Release);
        let deadline = tokio::time::Instant::now() + timeout;
        let initial = self
            .reviews
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        // Cancel existing reviews while joining constructors: a constructor can depend
        // on an existing review finishing. The writer then fences late registration.
        let (admission, initial_result) = tokio::join!(
            tokio::time::timeout_at(deadline, self.admission.write()),
            shutdown_reviews(&initial, deadline),
        );
        let additional = self
            .reviews
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .filter(|review| !initial.iter().any(|old| Arc::ptr_eq(old, review)))
            .cloned()
            .collect::<Vec<_>>();
        let additional_result = shutdown_reviews(&additional, deadline).await;
        anyhow::ensure!(
            admission.is_ok() && !self.unconfirmed_startup.load(Ordering::Acquire),
            "guardian startup cleanup could not be confirmed"
        );
        initial_result?;
        additional_result
    }
}

async fn shutdown_reviews(
    reviews: &[Arc<GuardianReviewSession>],
    deadline: tokio::time::Instant,
) -> anyhow::Result<()> {
    let results = futures::future::join_all(reviews.iter().map(|review| async move {
        tokio::time::timeout_at(deadline, review.shutdown())
            .await
            .map_err(|_| anyhow::anyhow!("guardian shutdown timed out"))?
    }))
    .await;
    for result in results {
        result?;
    }
    Ok(())
}

#[cfg(test)]
#[path = "review_session_lifecycle_tests.rs"]
mod tests;
