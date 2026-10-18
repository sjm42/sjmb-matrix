use crate::*;

const DRAIN_TIMEOUT: Duration = Duration::from_secs(10);

pub(crate) async fn shutdown_signal() -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::select! {
            result = tokio::signal::ctrl_c() => result?,
            _ = terminate.recv() => {},
        }
    }
    #[cfg(not(unix))]
    tokio::signal::ctrl_c().await?;
    Ok(())
}

/// All futures are owned by this scope: cancellation cannot detach a worker.
/// The receiver closes admission on drain, then finishes already queued work.
pub(crate) async fn supervise(
    sync: impl Future<Output = anyhow::Result<()>>,
    worker: impl Future<Output = ()>,
    shutdown: impl Future<Output = anyhow::Result<()>>,
    drain: watch::Sender<bool>,
    counters: Arc<Counters>,
) -> anyhow::Result<()> {
    tokio::pin!(worker);
    let mut report = interval(Duration::from_secs(30));
    let result = {
        tokio::pin!(sync, shutdown);
        loop {
            tokio::select! {
                result = &mut shutdown => break result,
                result = &mut sync => break result,
                () = &mut worker => {
                    counters.report();
                    bail!("Matrix message worker stopped unexpectedly");
                },
                _ = report.tick() => counters.report(),
            }
        }
    }; // Stop polling sync before requesting drain, including blocked callbacks.
    let _ = drain.send(true);
    let drained = timeout(DRAIN_TIMEOUT, &mut worker).await;
    counters.report();
    if drained.is_err() {
        error!("Matrix message drain exceeded 10 seconds; queued URLs may be lost");
    }
    result?;
    drained.context("Matrix message drain exceeded 10 seconds; queued URLs may be lost")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn shutdown_drains_the_worker_and_sync_failure_is_preserved() {
        for fail in [false, true] {
            let (drain, mut requested) = watch::channel(false);
            let finished = AtomicBool::new(false);
            let worker = async {
                requested.changed().await.unwrap();
                assert!(*requested.borrow());
                finished.store(true, Ordering::Relaxed);
            };
            let result = if fail {
                supervise(
                    async { bail!("sync failed") },
                    worker,
                    pending(),
                    drain,
                    Arc::default(),
                )
                .await
            } else {
                supervise(pending(), worker, async { Ok(()) }, drain, Arc::default()).await
            };
            assert!(finished.load(Ordering::Relaxed));
            assert_eq!(result.is_err(), fail);
        }
    }

    #[tokio::test]
    async fn worker_failure_is_observed() {
        let (drain, _requested) = watch::channel(false);
        let error = supervise(pending(), async {}, pending(), drain, Arc::default())
            .await
            .unwrap_err();
        assert!(error.to_string().contains("worker stopped"));
    }

    #[tokio::test]
    async fn a_stuck_worker_is_cancelled_at_the_drain_deadline() {
        struct MarkDropped<'a>(&'a AtomicBool);
        impl Drop for MarkDropped<'_> {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Relaxed);
            }
        }
        let dropped = AtomicBool::new(false);
        let marker = MarkDropped(&dropped);
        let (drain, _requested) = watch::channel(false);
        let error = supervise(
            pending(),
            async move {
                let _marker = marker;
                pending::<()>().await
            },
            async { Ok(()) },
            drain,
            Arc::default(),
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("drain exceeded"));
        assert!(dropped.load(Ordering::Relaxed));
    }
}
