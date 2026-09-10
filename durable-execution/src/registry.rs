use std::{collections::BTreeMap, future::Future, pin::Pin, sync::Arc};

use thiserror::Error;

use crate::{
    ActivityCallError, ActivityFailure, ActivityName, ActivityObservation, ActivityOptions,
    AttemptId, CheckpointError, CheckpointStore, DurableActivity, DurableHost, ExactBytes,
    HostOutcome, LogicalActivityId, decode_activity_input, encode_activity_result,
};

type HandlerFuture = Pin<Box<dyn Future<Output = Result<ExactBytes, ActivityHandlerError>> + Send>>;
type ErasedHandler = dyn Fn(ActivityContext, ExactBytes) -> HandlerFuture + Send + Sync;

struct RegistryEntry {
    handler: Option<Arc<ErasedHandler>>,
    max_input_bytes: u64,
    max_result_bytes: u64,
}

/// Runtime-owned metadata for one physical invocation of a logical activity.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActivityContext {
    logical_id: LogicalActivityId,
    attempt_id: AttemptId,
    attempt_ordinal: u32,
    retry_not_before_unix_millis: Option<i64>,
}

impl ActivityContext {
    pub fn new(
        logical_id: LogicalActivityId,
        attempt_id: AttemptId,
        attempt_ordinal: u32,
        retry_not_before_unix_millis: Option<i64>,
    ) -> Self {
        Self {
            logical_id,
            attempt_id,
            attempt_ordinal,
            retry_not_before_unix_millis,
        }
    }

    pub const fn activity_instance_id(&self) -> &LogicalActivityId {
        &self.logical_id
    }

    pub const fn attempt_id(&self) -> AttemptId {
        self.attempt_id
    }

    pub const fn attempt_ordinal(&self) -> u32 {
        self.attempt_ordinal
    }

    pub const fn options(&self) -> &ActivityOptions {
        self.logical_id.options()
    }

    pub const fn action_deadline_unix_millis(&self) -> Option<i64> {
        self.options().action_deadline_unix_millis()
    }

    pub const fn retry_not_before_unix_millis(&self) -> Option<i64> {
        self.retry_not_before_unix_millis
    }

    pub const fn attempt_timeout_millis(&self) -> Option<u64> {
        self.options().attempt_timeout_millis()
    }
}

/// Handler failures are classified before retry policy is applied.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum ActivityHandlerError {
    #[error("activity returned a retryable application failure")]
    Retryable(ExactBytes),
    #[error("activity returned a terminal application failure")]
    Terminal(ExactBytes),
    #[error("activity invocation timed out")]
    TimedOut,
    #[error("activity is waiting until {0} unix milliseconds")]
    WaitUntil(i64),
    #[error("activity codec failed: {0}")]
    Codec(ActivityCallError),
}

/// Absolute wakeup candidates returned to the reconciler-hosted runner.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ActivityWakeups {
    pub action_deadline_unix_millis: Option<i64>,
    pub retry_not_before_unix_millis: Option<i64>,
    pub handler_wait_unix_millis: Option<i64>,
    pub attempt_timeout_unix_millis: Option<i64>,
}

impl ActivityWakeups {
    pub fn earliest(self) -> Option<i64> {
        [
            self.action_deadline_unix_millis,
            self.retry_not_before_unix_millis,
            self.handler_wait_unix_millis,
            self.attempt_timeout_unix_millis,
        ]
        .into_iter()
        .flatten()
        .min()
    }
}

impl ActivityHandlerError {
    pub const fn is_retryable(&self) -> bool {
        matches!(self, Self::Retryable(_))
    }
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum ActivityRegistryError {
    #[error("activity {0} is not registered")]
    NotRegistered(ActivityName),
    #[error("activity {0} is registered more than once")]
    DuplicateRegistration(ActivityName),
    #[error(transparent)]
    Contract(#[from] ActivityCallError),
}

/// Builder for one immutable in-process activity registry.
#[derive(Default)]
pub struct ActivityRegistryBuilder {
    handlers: BTreeMap<ActivityName, RegistryEntry>,
    error: Option<ActivityRegistryError>,
}

impl ActivityRegistryBuilder {
    /// Register a typed contract whose concrete handler is hosted by an
    /// embedding runtime.
    pub fn register_contract<A>(self, name: &str) -> Self
    where
        A: DurableActivity + Send + Sync + 'static,
    {
        self.register_entry::<A>(name, None)
    }

    pub fn register<A, H, F>(self, name: &str, handler: H) -> Self
    where
        A: DurableActivity + Send + Sync + 'static,
        A::Input: Send + 'static,
        A::Output: Send + 'static,
        H: Fn(ActivityContext, A::Input) -> F + Send + Sync + 'static,
        F: Future<Output = Result<A::Output, ActivityHandlerError>> + Send + 'static,
    {
        self.register_inner::<A, H, F>(name, handler)
    }

    fn register_inner<A, H, F>(self, name: &str, handler: H) -> Self
    where
        A: DurableActivity + Send + Sync + 'static,
        A::Input: Send + 'static,
        A::Output: Send + 'static,
        H: Fn(ActivityContext, A::Input) -> F + Send + Sync + 'static,
        F: Future<Output = Result<A::Output, ActivityHandlerError>> + Send + 'static,
    {
        let erased = move |context: ActivityContext, input: ExactBytes| {
            let decoded = decode_activity_input::<A>(&input);
            match decoded {
                Ok(input) => {
                    let future = handler(context, input);
                    Box::pin(async move {
                        let output = future.await?;
                        encode_activity_result::<A>(&output).map_err(ActivityHandlerError::Codec)
                    }) as HandlerFuture
                }
                Err(error) => Box::pin(async move { Err(ActivityHandlerError::Codec(error)) })
                    as HandlerFuture,
            }
        };
        self.register_entry::<A>(name, Some(Arc::new(erased)))
    }

    fn register_entry<A>(mut self, name: &str, handler: Option<Arc<ErasedHandler>>) -> Self
    where
        A: DurableActivity,
    {
        if self.error.is_some() {
            return self;
        }
        if name != A::NAME {
            self.error = Some(ActivityRegistryError::Contract(
                ActivityCallError::NameMismatch {
                    registered: name.to_owned(),
                    contract: A::NAME.to_owned(),
                },
            ));
            return self;
        }
        let activity_name = match ActivityName::new(name, A::VERSION) {
            Ok(name) => name,
            Err(_) => {
                self.error = Some(ActivityRegistryError::Contract(
                    ActivityCallError::EmptyName,
                ));
                return self;
            }
        };
        if self.handlers.contains_key(&activity_name) {
            self.error = Some(ActivityRegistryError::DuplicateRegistration(activity_name));
            return self;
        }
        self.handlers.insert(
            activity_name,
            RegistryEntry {
                handler,
                max_input_bytes: A::MAX_INPUT_BYTES,
                max_result_bytes: A::MAX_RESULT_BYTES,
            },
        );
        self
    }

    pub fn build(self) -> Result<ActivityRegistry, ActivityRegistryError> {
        if let Some(error) = self.error {
            return Err(error);
        }
        Ok(ActivityRegistry {
            handlers: self.handlers,
        })
    }
}

/// Immutable registry used by the reconciler-hosted runner.
pub struct ActivityRegistry {
    handlers: BTreeMap<ActivityName, RegistryEntry>,
}

impl ActivityRegistry {
    pub fn builder() -> ActivityRegistryBuilder {
        ActivityRegistryBuilder::default()
    }

    pub fn contains(&self, name: &ActivityName) -> bool {
        self.handlers.contains_key(name)
    }

    pub async fn invoke(
        &self,
        context: ActivityContext,
        input: ExactBytes,
    ) -> Result<ExactBytes, ActivityRegistryError> {
        let name = context.activity_instance_id().name().clone();
        let entry = self
            .handlers
            .get(&name)
            .ok_or(ActivityRegistryError::NotRegistered(name))?;
        validate_registered_contract(entry, &context, &input)
            .map_err(ActivityRegistryError::Contract)?;
        let handler = entry.handler.as_ref().ok_or_else(|| {
            ActivityRegistryError::Contract(ActivityCallError::Handler(
                "activity handler is hosted by the embedding runtime".to_string(),
            ))
        })?;
        handler(context, input).await.map_err(|error| match error {
            ActivityHandlerError::Codec(error) => ActivityRegistryError::Contract(error),
            other => ActivityRegistryError::Contract(ActivityCallError::Handler(other.to_string())),
        })
    }

    pub async fn invoke_classified(
        &self,
        context: ActivityContext,
        input: ExactBytes,
    ) -> Result<ExactBytes, ActivityHandlerError> {
        let name = context.activity_instance_id().name().clone();
        let Some(entry) = self.handlers.get(&name) else {
            return Err(ActivityHandlerError::Codec(
                ActivityCallError::UnregisteredActivity(name.to_string()),
            ));
        };
        validate_registered_contract(entry, &context, &input)
            .map_err(ActivityHandlerError::Codec)?;
        let Some(handler) = entry.handler.as_ref() else {
            return Err(ActivityHandlerError::Codec(ActivityCallError::Handler(
                "activity handler is hosted by the embedding runtime".to_string(),
            )));
        };
        handler(context, input).await
    }

    /// Validate an invocation against an immutable typed contract registry.
    pub fn validate_invocation(
        &self,
        context: &ActivityContext,
        input: &ExactBytes,
    ) -> Result<(), ActivityRegistryError> {
        let name = context.activity_instance_id().name().clone();
        let entry = self
            .handlers
            .get(&name)
            .ok_or(ActivityRegistryError::NotRegistered(name))?;
        validate_registered_contract(entry, context, input).map_err(ActivityRegistryError::Contract)
    }

    /// Invoke an embedding-runtime handler through this registry's typed
    /// contract. This keeps identity, bounds, and input decoding owned by the
    /// framework while allowing the handler to borrow reconciliation state.
    pub async fn invoke_embedded<A, H, F, R>(
        &self,
        context: ActivityContext,
        input: ExactBytes,
        handler: H,
    ) -> Result<R, ActivityRegistryError>
    where
        A: DurableActivity,
        H: FnOnce(ActivityContext, A::Input) -> F,
        F: Future<Output = R>,
    {
        let name = context.activity_instance_id().name().clone();
        let entry = self
            .handlers
            .get(&name)
            .ok_or_else(|| ActivityRegistryError::NotRegistered(name.clone()))?;
        if name.name() != A::NAME || name.version() != A::VERSION {
            return Err(ActivityRegistryError::Contract(
                ActivityCallError::NameMismatch {
                    registered: name.to_string(),
                    contract: format!("{}@{}", A::NAME, A::VERSION),
                },
            ));
        }
        validate_registered_contract(entry, &context, &input)
            .map_err(ActivityRegistryError::Contract)?;
        let decoded =
            decode_activity_input::<A>(&input).map_err(ActivityRegistryError::Contract)?;
        Ok(handler(context, decoded).await)
    }
}

fn validate_registered_contract(
    entry: &RegistryEntry,
    context: &ActivityContext,
    input: &ExactBytes,
) -> Result<(), ActivityCallError> {
    let input_len = u64::try_from(input.as_slice().len()).unwrap_or(u64::MAX);
    let recorded_input_bound = context.activity_instance_id().spec().max_input_bytes();
    if recorded_input_bound != entry.max_input_bytes {
        return Err(ActivityCallError::Handler(format!(
            "registered input bound {} differs from recorded {}",
            entry.max_input_bytes, recorded_input_bound
        )));
    }
    if input_len > entry.max_input_bytes {
        return Err(ActivityCallError::InputTooLarge {
            actual_bytes: input_len,
            max_bytes: entry.max_input_bytes,
        });
    }
    let recorded_result_bound = context.activity_instance_id().max_result_bytes();
    if recorded_result_bound != entry.max_result_bytes {
        return Err(ActivityCallError::Handler(format!(
            "registered result bound {} differs from recorded {}",
            entry.max_result_bytes, recorded_result_bound
        )));
    }
    Ok(())
}

/// One bounded ordinary-activity runner step.
pub struct ActivityRunner<S> {
    host: DurableHost<S>,
    registry: ActivityRegistry,
}

impl<S: CheckpointStore> ActivityRunner<S> {
    pub fn new(host: DurableHost<S>, registry: ActivityRegistry) -> Self {
        Self { host, registry }
    }

    pub const fn host(&self) -> &DurableHost<S> {
        &self.host
    }

    pub async fn run_once<W: crate::Workflow>(
        &mut self,
        workflow: &W,
        execution: crate::ExecutionSpec,
        now_unix_millis: i64,
    ) -> HostOutcome {
        let outcome = self
            .host
            .turn_and_expose_at(workflow, execution.clone(), now_unix_millis)
            .await;
        if let HostOutcome::Quarantined {
            activity,
            prepared_command: None,
            ..
        } = &outcome
        {
            let retry = self
                .host
                .schedule_retry(&execution, activity, now_unix_millis)
                .await;
            if matches!(
                retry,
                HostOutcome::CheckpointRejected(
                    CheckpointError::ActivityAttemptLimitExceeded { .. }
                )
            ) {
                return self
                    .host
                    .observe_failure(
                        &execution,
                        activity,
                        ActivityFailure::Application(ExactBytes::new(
                            b"lost_result_retry_exhausted",
                        )),
                    )
                    .await;
            }
            return retry;
        }
        let HostOutcome::DispatchPermitted { permit, .. } = outcome else {
            return outcome;
        };
        let activity = permit.activity().clone();
        let attempt_id = permit.attempt_id();
        let stored = match self.host.store().load(execution.execution_id()).await {
            Ok(Some(stored)) => stored,
            Ok(None) => {
                return HostOutcome::CheckpointRejected(
                    CheckpointError::RetryRequiresExposedActivity {
                        sequence: activity.sequence(),
                    },
                );
            }
            Err(error) => {
                return HostOutcome::StoreFailed {
                    operation: crate::StoreOperation::Load,
                    error,
                };
            }
        };
        let payload = match stored
            .checkpoint()
            .decode_and_validate(&execution, self.host.checkpoint_limits())
        {
            Ok(payload) => payload,
            Err(error) => return HostOutcome::CheckpointRejected(error),
        };
        let attempt = payload
            .active_activities()
            .and_then(|activities| activities.last())
            .map(|record| record.attempt())
            .expect("accepted exposure stores its final activity");
        let context = ActivityContext::new(
            activity.clone(),
            attempt_id,
            attempt.ordinal(),
            attempt.retry_not_before_unix_millis(),
        );
        let input = activity.input().clone();
        if activity
            .options()
            .action_deadline_unix_millis()
            .is_some_and(|deadline| now_unix_millis >= deadline)
        {
            return self
                .host
                .observe_failure(
                    &execution,
                    &activity,
                    ActivityFailure::ActionDeadlineExceeded,
                )
                .await;
        }
        let invocation = self.registry.invoke_classified(context, input);
        let action_timeout = activity
            .options()
            .action_deadline_unix_millis()
            .and_then(|deadline| deadline.checked_sub(now_unix_millis))
            .and_then(|remaining| u64::try_from(remaining).ok());
        let attempt_timeout = activity.options().attempt_timeout_millis();
        let (effective_timeout, action_deadline_wins) = match (action_timeout, attempt_timeout) {
            (Some(action), Some(attempt)) if action <= attempt => (Some(action), true),
            (Some(_), Some(attempt)) => (Some(attempt), false),
            (Some(action), None) => (Some(action), true),
            (None, Some(attempt)) => (Some(attempt), false),
            (None, None) => (None, false),
        };
        let handler_result = match effective_timeout {
            Some(timeout_millis) => match tokio::time::timeout(
                std::time::Duration::from_millis(timeout_millis),
                invocation,
            )
            .await
            {
                Ok(result) => result,
                Err(_) if action_deadline_wins => {
                    return self
                        .host
                        .observe_failure(
                            &execution,
                            &activity,
                            ActivityFailure::ActionDeadlineExceeded,
                        )
                        .await;
                }
                Err(_) => Err(ActivityHandlerError::TimedOut),
            },
            None => invocation.await,
        };
        match handler_result {
            Ok(result) => {
                self.host
                    .observe(&execution, ActivityObservation::new(activity, result))
                    .await
            }
            Err(ActivityHandlerError::Retryable(failure)) => {
                if attempt.ordinal() < activity.options().max_attempts() {
                    self.host
                        .schedule_retry(&execution, &activity, now_unix_millis)
                        .await
                } else {
                    self.host
                        .observe_failure(
                            &execution,
                            &activity,
                            ActivityFailure::Application(failure),
                        )
                        .await
                }
            }
            Err(ActivityHandlerError::Terminal(failure)) => {
                self.host
                    .observe_failure(&execution, &activity, ActivityFailure::Application(failure))
                    .await
            }
            Err(ActivityHandlerError::TimedOut) => {
                self.host
                    .observe_failure(&execution, &activity, ActivityFailure::TimedOut)
                    .await
            }
            Err(ActivityHandlerError::WaitUntil(wake_at_unix_millis)) => {
                let wake_at_unix_millis = ActivityWakeups {
                    action_deadline_unix_millis: activity.options().action_deadline_unix_millis(),
                    handler_wait_unix_millis: Some(wake_at_unix_millis),
                    ..ActivityWakeups::default()
                }
                .earliest()
                .expect("handler wait is present");
                self.host
                    .defer_wait(&execution, &activity, wake_at_unix_millis)
                    .await
            }
            Err(ActivityHandlerError::Codec(_)) => HostOutcome::CheckpointRejected(
                CheckpointError::PreparedActivityRejected(crate::PreparedActivityError::Validation),
            ),
        }
    }
}
