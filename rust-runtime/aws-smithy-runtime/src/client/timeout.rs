/*
 * Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
 * SPDX-License-Identifier: Apache-2.0
 */

use aws_smithy_async::future::timeout::Timeout;
use aws_smithy_async::rt::sleep::{AsyncSleep, Sleep};
use aws_smithy_client::SdkError;
use aws_smithy_runtime_api::client::orchestrator::{ConfigBagAccessors, HttpResponse};
use aws_smithy_runtime_api::config_bag::ConfigBag;
use aws_smithy_types::timeout::TimeoutConfig;
use pin_project_lite::pin_project;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

#[derive(Debug)]
struct MaybeTimeoutError {
    kind: TimeoutKind,
    duration: Duration,
}

impl MaybeTimeoutError {
    fn new(kind: TimeoutKind, duration: Duration) -> Self {
        Self { kind, duration }
    }
}

impl std::fmt::Display for MaybeTimeoutError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} occurred after {:?}",
            match self.kind {
                TimeoutKind::Operation => "operation timeout (all attempts including retries)",
                TimeoutKind::OperationAttempt => "operation attempt timeout (single attempt)",
            },
            self.duration
        )
    }
}

impl std::error::Error for MaybeTimeoutError {}

pin_project! {
    #[non_exhaustive]
    #[must_use = "futures do nothing unless you `.await` or poll them"]
    // This allow is needed because otherwise Clippy will get mad we didn't document the
    // generated MaybeTimeoutFutureProj
    #[allow(missing_docs)]
    #[project = MaybeTimeoutFutureProj]
    /// A timeout future that may or may not have a timeout depending on
    /// whether or not one was set. A `kind` can be set so that when a timeout occurs, there
    /// is additional context attached to the error.
    pub(super) enum MaybeTimeoutFuture<F> {
        /// A wrapper around an inner future that will output an [`SdkError`] if it runs longer than
        /// the given duration
        Timeout {
            #[pin]
            future: Timeout<F, Sleep>,
            timeout_kind: TimeoutKind,
            duration: Duration,
        },
        /// A thin wrapper around an inner future that will never time out
        NoTimeout {
            #[pin]
            future: F
        }
    }
}

impl<InnerFuture, T, E> Future for MaybeTimeoutFuture<InnerFuture>
where
    InnerFuture: Future<Output = Result<T, SdkError<E, HttpResponse>>>,
{
    type Output = Result<T, SdkError<E, HttpResponse>>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let (future, kind, duration) = match self.project() {
            MaybeTimeoutFutureProj::NoTimeout { future } => return future.poll(cx),
            MaybeTimeoutFutureProj::Timeout {
                future,
                timeout_kind,
                duration,
            } => (future, timeout_kind, duration),
        };
        match future.poll(cx) {
            Poll::Ready(Ok(response)) => Poll::Ready(response),
            Poll::Ready(Err(_timeout)) => Poll::Ready(Err(SdkError::timeout_error(
                MaybeTimeoutError::new(*kind, *duration),
            ))),
            Poll::Pending => Poll::Pending,
        }
    }
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(super) enum TimeoutKind {
    Operation,
    OperationAttempt,
}

#[derive(Clone, Debug)]
pub(super) struct MaybeTimeoutConfig {
    sleep_impl: Option<Arc<dyn AsyncSleep>>,
    timeout: Option<Duration>,
    timeout_kind: TimeoutKind,
}

pub(super) trait ProvideMaybeTimeoutConfig {
    fn maybe_timeout_config(&self, timeout_kind: TimeoutKind) -> MaybeTimeoutConfig;
}

impl ProvideMaybeTimeoutConfig for ConfigBag {
    fn maybe_timeout_config(&self, timeout_kind: TimeoutKind) -> MaybeTimeoutConfig {
        if let Some(timeout_config) = self.get::<TimeoutConfig>() {
            let sleep_impl = self.sleep_impl();
            let timeout = match (sleep_impl.as_ref(), timeout_kind) {
                (None, _) => None,
                (Some(_), TimeoutKind::Operation) => timeout_config.operation_timeout(),
                (Some(_), TimeoutKind::OperationAttempt) => {
                    timeout_config.operation_attempt_timeout()
                }
            };
            MaybeTimeoutConfig {
                sleep_impl,
                timeout,
                timeout_kind,
            }
        } else {
            MaybeTimeoutConfig {
                sleep_impl: None,
                timeout: None,
                timeout_kind,
            }
        }
    }
}

/// Trait to conveniently wrap a future with an optional timeout.
pub(super) trait MaybeTimeout<T>: Sized {
    /// Wraps a future in a timeout if one is set.
    fn maybe_timeout_with_config(
        self,
        timeout_config: MaybeTimeoutConfig,
    ) -> MaybeTimeoutFuture<Self>;

    /// Wraps a future in a timeout if one is set.
    fn maybe_timeout(self, cfg: &ConfigBag, kind: TimeoutKind) -> MaybeTimeoutFuture<Self>;
}

impl<T> MaybeTimeout<T> for T
where
    T: Future,
{
    fn maybe_timeout_with_config(
        self,
        timeout_config: MaybeTimeoutConfig,
    ) -> MaybeTimeoutFuture<Self> {
        match timeout_config {
            MaybeTimeoutConfig {
                sleep_impl: Some(sleep_impl),
                timeout: Some(timeout),
                timeout_kind,
            } => MaybeTimeoutFuture::Timeout {
                future: Timeout::new(self, sleep_impl.sleep(timeout)),
                timeout_kind,
                duration: timeout,
            },
            _ => MaybeTimeoutFuture::NoTimeout { future: self },
        }
    }

    fn maybe_timeout(self, cfg: &ConfigBag, kind: TimeoutKind) -> MaybeTimeoutFuture<Self> {
        self.maybe_timeout_with_config(cfg.maybe_timeout_config(kind))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aws_smithy_async::assert_elapsed;
    use aws_smithy_async::future::never::Never;
    use aws_smithy_async::rt::sleep::TokioSleep;

    #[tokio::test]
    async fn test_no_timeout() {
        let sleep_impl: Arc<dyn AsyncSleep> = Arc::new(TokioSleep::new());
        let sleep_future = sleep_impl.sleep(Duration::from_millis(250));
        let underlying_future = async {
            sleep_future.await;
            Result::<_, SdkError<(), HttpResponse>>::Ok(())
        };

        let now = tokio::time::Instant::now();
        tokio::time::pause();

        let mut cfg = ConfigBag::base();
        cfg.put(TimeoutConfig::builder().build());
        cfg.set_sleep_impl(Some(sleep_impl));

        underlying_future
            .maybe_timeout(&cfg, TimeoutKind::Operation)
            .await
            .expect("success");

        assert_elapsed!(now, Duration::from_secs_f32(0.25));
    }

    #[tokio::test]
    async fn test_operation_timeout() {
        let sleep_impl: Arc<dyn AsyncSleep> = Arc::new(TokioSleep::new());
        let never = Never::new();
        let underlying_future = async {
            never.await;
            Result::<_, SdkError<(), HttpResponse>>::Ok(())
        };

        let now = tokio::time::Instant::now();
        tokio::time::pause();

        let mut cfg = ConfigBag::base();
        cfg.put(
            TimeoutConfig::builder()
                .operation_timeout(Duration::from_millis(250))
                .build(),
        );
        cfg.set_sleep_impl(Some(sleep_impl));

        let result = underlying_future
            .maybe_timeout(&cfg, TimeoutKind::Operation)
            .await;
        let err = result.expect_err("should have timed out");

        assert_eq!(format!("{:?}", err), "TimeoutError(TimeoutError { source: MaybeTimeoutError { kind: Operation, duration: 250ms } })");
        assert_elapsed!(now, Duration::from_secs_f32(0.25));
    }
}
