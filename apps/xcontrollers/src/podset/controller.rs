use anyhow::Result;
use futures::StreamExt;
use k8s_openapi::api::core::v1::Pod;
use kube::{
    Client, ResourceExt,
    api::Api,
    runtime::{
        controller::{Action, Config, Controller},
        finalizer::{Event, finalizer},
        watcher,
    },
};
use tokio_util::sync::CancellationToken;

use std::sync::Arc;

use tokio::time::Duration;

use crate::shared::XError;

use super::crd::PodSet;

const FINALIZER_NAME: &str = "podset.nullable.se/finalizer";

/// Controller triggers this whenever our main object or our children changed
async fn reconcile(generator: Arc<PodSet>, ctx: Arc<Data>) -> Result<Action, XError> {
    let client = &ctx.client;
    let namespace = generator
        .metadata
        .namespace
        .as_ref()
        .ok_or_else(|| XError::MissingObjectKey(".metadata.namespace"))?;

    let podset_api: Api<PodSet> = Api::namespaced(client.clone(), namespace);

    finalizer(&podset_api, FINALIZER_NAME, generator, |event| async {
        match event {
            Event::Apply(generator) => {
                // Create or update the PodSet
                reconcile_apply(generator, ctx.clone()).await
            }
            Event::Cleanup(generator) => {
                // Cleanup before deletion
                reconcile_cleanup(generator, ctx.clone()).await
            }
        }
    })
    .await
    .map_err(|e| XError::FinalizerError(Box::new(e)))
}

/// Reconcile when the object is created or updated
async fn reconcile_apply(generator: Arc<PodSet>, ctx: Arc<Data>) -> Result<Action, kube::Error> {
    let client = &ctx.client;

    for i in 0..generator.spec.replicas {
        crate::podset::controller_intenals::create_or_update_pod(generator.as_ref(), i, client)
            .await?;
    }

    tracing::info!("PodSet created/updated for {}", generator.name_any());
    Ok(Action::requeue(Duration::from_secs(300)))
}

/// Cleanup when the object is being deleted
async fn reconcile_cleanup(generator: Arc<PodSet>, _ctx: Arc<Data>) -> Result<Action, kube::Error> {
    tracing::info!("Cleaning up PodSet: {}", generator.name_any());
    // Perform any cleanup logic here if needed
    // The owned resources will be automatically deleted by Kubernetes due to owner references
    Ok(Action::await_change())
}

/// The controller triggers this on reconcile errors
fn error_policy(_object: Arc<PodSet>, _error: &XError, _ctx: Arc<Data>) -> Action {
    Action::requeue(Duration::from_secs(1))
}

// Data we want access to in error/reconcile calls
struct Data {
    client: Client,
}

pub async fn run_controller(
    token: CancellationToken,
    client: Client,
) -> Result<(), Box<dyn std::error::Error>> {
    let podsets = Api::<PodSet>::all(client.clone());
    let pods = Api::<Pod>::all(client.clone());

    tracing::info!("starting podset-controller");

    // limit the controller to running a maximum of two concurrent reconciliations
    let config = Config::default().concurrency(2);

    Controller::new(podsets, watcher::Config::default())
        .owns(pods, watcher::Config::default().labels("app=podset"))
        .with_config(config)
        .graceful_shutdown_on(async move { token.cancelled().await })
        .run(reconcile, error_policy, Arc::new(Data { client }))
        .for_each(|res| async move {
            match res {
                Ok(o) => tracing::debug!("reconciled {:?}", o),
                Err(e) => tracing::warn!("reconcile failed: {}", e),
            }
        })
        .await;
    tracing::info!("controller terminated");
    Ok(())
}
