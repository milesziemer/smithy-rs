/*
 * Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
 * SPDX-License-Identifier: Apache-2.0
 */

//! Contains the [`Either`] enum.

use pin_project_lite::pin_project;
use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};
use tower::{Layer, Service};

use crate::operation::Operation;

use super::Plugin;

pin_project! {
    /// Combine two different [`Future`]/[`Service`]/[`Layer`]/[`Plugin`] types into a single type.
    ///
    /// # Notes on [`Future`]
    ///
    /// The [`Future::Output`] must be identical.
    ///
    /// # Notes on [`Service`]
    ///
    /// The [`Service::Response`] and [`Service::Error`] must be identical.
    #[derive(Clone, Debug)]
    #[project = EitherProj]
    pub enum Either<L, R> {
        /// One type of backing [`Service`].
        Left { #[pin] value: L },
        /// The other type of backing [`Service`].
        Right { #[pin] value: R },
    }
}

impl<L, R> Future for Either<L, R>
where
    L: Future,
    R: Future<Output = L::Output>,
{
    type Output = L::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match self.project() {
            EitherProj::Left { value } => value.poll(cx),
            EitherProj::Right { value } => value.poll(cx),
        }
    }
}

impl<L, R, Request> Service<Request> for Either<L, R>
where
    L: Service<Request>,
    R: Service<Request, Response = L::Response, Error = L::Error>,
{
    type Response = L::Response;
    type Error = L::Error;
    type Future = Either<L::Future, R::Future>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        use self::Either::*;

        match self {
            Left { value } => value.poll_ready(cx),
            Right { value } => value.poll_ready(cx),
        }
    }

    fn call(&mut self, request: Request) -> Self::Future {
        use self::Either::*;

        match self {
            Left { value } => Either::Left {
                value: value.call(request),
            },
            Right { value } => Either::Right {
                value: value.call(request),
            },
        }
    }
}

impl<S, L, R> Layer<S> for Either<L, R>
where
    L: Layer<S>,
    R: Layer<S>,
{
    type Service = Either<L::Service, R::Service>;

    fn layer(&self, inner: S) -> Self::Service {
        match self {
            Either::Left { value } => Either::Left {
                value: value.layer(inner),
            },
            Either::Right { value } => Either::Right {
                value: value.layer(inner),
            },
        }
    }
}

impl<P, Op, S, L, Le, Ri> Plugin<P, Op, S, L> for Either<Le, Ri>
where
    Le: Plugin<P, Op, S, L>,
    Ri: Plugin<P, Op, S, L>,
{
    type Service = Either<Le::Service, Ri::Service>;
    type Layer = Either<Le::Layer, Ri::Layer>;

    fn map(&self, input: Operation<S, L>) -> Operation<Self::Service, Self::Layer> {
        match self {
            Either::Left { value } => {
                let Operation { inner, layer } = value.map(input);
                Operation {
                    inner: Either::Left { value: inner },
                    layer: Either::Left { value: layer },
                }
            }
            Either::Right { value } => {
                let Operation { inner, layer } = value.map(input);
                Operation {
                    inner: Either::Right { value: inner },
                    layer: Either::Right { value: layer },
                }
            }
        }
    }
}
