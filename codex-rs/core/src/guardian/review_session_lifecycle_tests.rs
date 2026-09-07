use super::super::GuardianReviewSessionManager;
use super::super::tests::test_review_session;
use super::*;
use futures::FutureExt;

#[tokio::test]
async fn retired_guardian_failure_remains_visible_after_another_reviewer_succeeds() {
    let manager = GuardianReviewSessionManager::default();
    let (mut failed, _events, _submissions) = test_review_session().await;
    failed.io.session_loop_termination =
        futures::future::ready(Err("fixture retired reviewer cleanup failed".to_string()))
            .boxed()
            .shared();
    manager.state.lock().await.trunk = Some(manager.lifecycle.register(failed));
    manager.invalidate().await;
    let (healthy, _healthy_events, _healthy_submissions) = test_review_session().await;
    manager.state.lock().await.trunk = Some(manager.lifecycle.register(healthy));
    for _ in 0..2 {
        let error = manager
            .shutdown()
            .await
            .expect_err("retired cleanup failure cannot disappear");
        assert!(
            error
                .to_string()
                .contains("fixture retired reviewer cleanup failed")
        );
    }
}

#[tokio::test]
async fn shutdown_joins_cleanup_of_a_retired_guardian() {
    let manager = Arc::new(GuardianReviewSessionManager::default());
    let (mut review, _events, _submissions) = test_review_session().await;
    let (release, released) = tokio::sync::oneshot::channel::<()>();
    review.io.session_loop_termination =
        crate::session::session_loop_termination_from_handle(tokio::spawn(async move {
            released.await.expect("release fixture cleanup");
            Ok(())
        }));
    manager.state.lock().await.trunk = Some(manager.lifecycle.register(review));
    manager.invalidate().await;
    let mut shutdown = tokio::spawn({
        let manager = Arc::clone(&manager);
        async move { manager.shutdown().await }
    });
    assert!(
        tokio::time::timeout(Duration::from_millis(25), &mut shutdown)
            .await
            .is_err()
    );
    release.send(()).expect("release pending cleanup");
    shutdown
        .await
        .expect("join cleanup")
        .expect("confirmed cleanup");
}

#[tokio::test]
async fn shutdown_drains_guardian_registered_after_its_initial_snapshot() {
    let lifecycle = Arc::new(GuardianSessionLifecycle::default());
    let (review, _events, _submissions) = test_review_session().await;
    let entered = Arc::new(tokio::sync::Notify::new());
    let (release, released) = tokio::sync::oneshot::channel::<()>();
    let starting = tokio::spawn({
        let lifecycle = Arc::clone(&lifecycle);
        let entered = Arc::clone(&entered);
        async move {
            lifecycle
                .spawn(async move {
                    entered.notify_one();
                    released.await.expect("release startup");
                    Ok(review)
                })
                .await
        }
    });
    entered.notified().await;
    let stopping = tokio::spawn({
        let lifecycle = Arc::clone(&lifecycle);
        async move { lifecycle.shutdown(Duration::from_secs(5)).await }
    });
    tokio::time::timeout(Duration::from_secs(1), async {
        while !lifecycle.closed.load(Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("admission closes");
    release.send(()).expect("release admitted startup");
    let review = starting
        .await
        .expect("join startup")
        .expect("admitted startup");
    stopping
        .await
        .expect("join shutdown")
        .expect("late reviewer cleanup");
    assert_eq!(review.shutdown_result.get(), Some(&Ok(())));
}

#[tokio::test]
async fn canceled_guardian_constructor_remains_unconfirmed_on_later_shutdowns() {
    let lifecycle = Arc::new(GuardianSessionLifecycle::default());
    let entered = Arc::new(tokio::sync::Notify::new());
    let starting = tokio::spawn({
        let lifecycle = Arc::clone(&lifecycle);
        let entered = Arc::clone(&entered);
        async move {
            lifecycle
                .spawn(async move {
                    entered.notify_one();
                    std::future::pending::<anyhow::Result<GuardianReviewSession>>().await
                })
                .await
        }
    });
    entered.notified().await;
    starting.abort();
    assert!(matches!(starting.await, Err(error) if error.is_cancelled()));
    for _ in 0..2 {
        assert!(lifecycle.shutdown(Duration::from_secs(1)).await.is_err());
    }
}
