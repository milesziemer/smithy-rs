/*
 * Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
 * SPDX-License-Identifier: Apache-2.0
 */

use self::auth::orchestrate_auth;
use crate::client::orchestrator::endpoints::orchestrate_endpoint;
use crate::client::orchestrator::http::read_body;
use crate::client::orchestrator::phase::Phase;
use crate::client::timeout::{MaybeTimeout, ProvideMaybeTimeoutConfig, TimeoutKind};
use aws_smithy_http::result::SdkError;
use aws_smithy_runtime_api::client::interceptors::context::{Error, Input, Output};
use aws_smithy_runtime_api::client::interceptors::{InterceptorContext, Interceptors};
use aws_smithy_runtime_api::client::orchestrator::{BoxError, ConfigBagAccessors, HttpResponse};
use aws_smithy_runtime_api::client::retries::ShouldAttempt;
use aws_smithy_runtime_api::client::runtime_plugin::RuntimePlugins;
use aws_smithy_runtime_api::config_bag::ConfigBag;
use tracing::{debug_span, Instrument};

mod auth;
/// Defines types that implement a trait for endpoint resolution
pub mod endpoints;
mod http;
pub(self) mod phase;

pub async fn invoke(
    input: Input,
    runtime_plugins: &RuntimePlugins,
) -> Result<Output, SdkError<Error, HttpResponse>> {
    invoke_pre_config(input, runtime_plugins)
        .instrument(debug_span!("invoke"))
        .await
}

async fn invoke_pre_config(
    input: Input,
    runtime_plugins: &RuntimePlugins,
) -> Result<Output, SdkError<Error, HttpResponse>> {
    let mut cfg = ConfigBag::base();
    let cfg = &mut cfg;

    let mut interceptors = Interceptors::new();

    let context = Phase::construction(InterceptorContext::new(input))
        // Client configuration
        .include(|_| runtime_plugins.apply_client_configuration(cfg, &mut interceptors))?
        .include(|ctx| interceptors.client_read_before_execution(ctx, cfg))?
        // Operation configuration
        .include(|_| runtime_plugins.apply_operation_configuration(cfg, &mut interceptors))?
        .include(|ctx| interceptors.operation_read_before_execution(ctx, cfg))?
        .finish();

    let operation_timeout_config = cfg.maybe_timeout_config(TimeoutKind::Operation);
    invoke_post_config(cfg, context, interceptors)
        .maybe_timeout_with_config(operation_timeout_config)
        .await
}

async fn invoke_post_config(
    cfg: &mut ConfigBag,
    context: InterceptorContext,
    interceptors: Interceptors,
) -> Result<Output, SdkError<Error, HttpResponse>> {
    let context = Phase::construction(context)
        // Before serialization
        .include(|ctx| interceptors.read_before_serialization(ctx, cfg))?
        .include_mut(|ctx| interceptors.modify_before_serialization(ctx, cfg))?
        // Serialization
        .include_mut(|ctx| {
            let request_serializer = cfg.request_serializer();
            let request = request_serializer
                .serialize_input(ctx.take_input().expect("input set at this point"))?;
            ctx.set_request(request);
            Result::<(), BoxError>::Ok(())
        })?
        // After serialization
        .include(|ctx| interceptors.read_after_serialization(ctx, cfg))?
        // Before retry loop
        .include_mut(|ctx| interceptors.modify_before_retry_loop(ctx, cfg))?
        .finish();

    {
        let retry_strategy = cfg.retry_strategy();
        match retry_strategy.should_attempt_initial_request(cfg) {
            // Yes, let's make a request
            Ok(ShouldAttempt::Yes) => {}
            // No, this request shouldn't be sent
            Ok(ShouldAttempt::No) => {
                return Err(Phase::dispatch(context).fail(
                    "The retry strategy indicates that an initial request shouldn't be made, but it didn't specify why.",
                ))
            }
            // No, we shouldn't make a request because...
            Err(err) => return Err(Phase::dispatch(context).fail(err)),
            Ok(ShouldAttempt::YesAfterDelay(_)) => {
                unreachable!("Delaying the initial request is currently unsupported. If this feature is important to you, please file an issue in GitHub.")
            }
        }
    }

    let mut context = context;
    let handling_phase = loop {
        let attempt_timeout_config = cfg.maybe_timeout_config(TimeoutKind::OperationAttempt);
        let dispatch_phase = Phase::dispatch(context);
        context = make_an_attempt(dispatch_phase, cfg, &interceptors)
            .instrument(debug_span!("make_an_attempt"))
            .maybe_timeout_with_config(attempt_timeout_config)
            .await?
            .include(|ctx| interceptors.read_after_attempt(ctx, cfg))?
            .include_mut(|ctx| interceptors.modify_before_attempt_completion(ctx, cfg))?
            .finish();

        let retry_strategy = cfg.retry_strategy();
        match retry_strategy.should_attempt_retry(&context, cfg) {
            // Yes, let's retry the request
            Ok(ShouldAttempt::Yes) => continue,
            // No, this request shouldn't be retried
            Ok(ShouldAttempt::No) => {}
            Ok(ShouldAttempt::YesAfterDelay(_delay)) => {
                todo!("implement retries with an explicit delay.")
            }
            // I couldn't determine if the request should be retried because an error occurred.
            Err(err) => {
                return Err(Phase::response_handling(context).fail(err));
            }
        }

        let handling_phase = Phase::response_handling(context)
            .include_mut(|ctx| interceptors.modify_before_completion(ctx, cfg))?;
        cfg.trace_probe().dispatch_events();

        break handling_phase.include(|ctx| interceptors.read_after_execution(ctx, cfg))?;
    };

    handling_phase.finalize()
}

// Making an HTTP request can fail for several reasons, but we still need to
// call lifecycle events when that happens. Therefore, we define this
// `make_an_attempt` function to make error handling simpler.
async fn make_an_attempt(
    dispatch_phase: Phase,
    cfg: &mut ConfigBag,
    interceptors: &Interceptors,
) -> Result<Phase, SdkError<Error, HttpResponse>> {
    let dispatch_phase = dispatch_phase
        .include(|ctx| interceptors.read_before_attempt(ctx, cfg))?
        .include_mut(|ctx| orchestrate_endpoint(ctx, cfg))?
        .include_mut(|ctx| interceptors.modify_before_signing(ctx, cfg))?
        .include(|ctx| interceptors.read_before_signing(ctx, cfg))?;

    let dispatch_phase = orchestrate_auth(dispatch_phase, cfg).await?;

    let mut context = dispatch_phase
        .include(|ctx| interceptors.read_after_signing(ctx, cfg))?
        .include_mut(|ctx| interceptors.modify_before_transmit(ctx, cfg))?
        .include(|ctx| interceptors.read_before_transmit(ctx, cfg))?
        .finish();

    // The connection consumes the request but we need to keep a copy of it
    // within the interceptor context, so we clone it here.
    let call_result = {
        let request = context.take_request().expect("request has been set");
        let connection = cfg.connection();
        connection.call(request).await
    };

    let mut context = Phase::dispatch(context)
        .include_mut(move |ctx| {
            ctx.set_response(call_result?);
            Result::<(), BoxError>::Ok(())
        })?
        .include(|ctx| interceptors.read_after_transmit(ctx, cfg))?
        .include_mut(|ctx| interceptors.modify_before_deserialization(ctx, cfg))?
        .include(|ctx| interceptors.read_before_deserialization(ctx, cfg))?
        .finish();

    let output_or_error = {
        let response = context.response_mut().expect("response has been set");
        let response_deserializer = cfg.response_deserializer();
        match response_deserializer.deserialize_streaming(response) {
            Some(output_or_error) => Ok(output_or_error),
            None => read_body(response)
                .instrument(debug_span!("read_body"))
                .await
                .map(|_| response_deserializer.deserialize_nonstreaming(response)),
        }
    };

    Phase::response_handling(context)
        .include_mut(move |ctx| {
            ctx.set_output_or_error(output_or_error?);
            Result::<(), BoxError>::Ok(())
        })?
        .include(|ctx| interceptors.read_after_deserialization(ctx, cfg))
}
