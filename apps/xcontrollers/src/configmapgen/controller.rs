use anyhow::Result;
use futures::StreamExt;
use k8s_openapi::api::core::v1::ConfigMap;
use kube::{
    Client, ResourceExt,
    api::{Api, ObjectMeta, Patch, PatchParams, Resource},
    runtime::{
        controller::{Action, Config, Controller},
        finalizer::{Event, finalizer},
        watcher,
    },
};
use tokio_util::sync::CancellationToken;

use std::{collections::BTreeMap, sync::Arc};
use thiserror::Error;
use tokio::time::Duration;

use super::crd::ConfigMapGenerator;

const FINALIZER_NAME: &str = "configmapgenerator.nullable.se/finalizer";

#[derive(Debug, Error)]
enum Error {
    #[error("Finalizer error: {0}")]
    FinalizerError(#[source] Box<kube::runtime::finalizer::Error<kube::Error>>),
    #[error("MissingObjectKey: {0}")]
    MissingObjectKey(&'static str),
}

/// Controller triggers this whenever our main object or our children changed
async fn reconcile(generator: Arc<ConfigMapGenerator>, ctx: Arc<Data>) -> Result<Action, Error> {
    let client = &ctx.client;
    let namespace = generator
        .metadata
        .namespace
        .as_ref()
        .ok_or_else(|| Error::MissingObjectKey(".metadata.namespace"))?;

    let cmg_api: Api<ConfigMapGenerator> = Api::namespaced(client.clone(), namespace);

    finalizer(&cmg_api, FINALIZER_NAME, generator, |event| async {
        match event {
            Event::Apply(generator) => {
                // Create or update the ConfigMap
                reconcile_apply(generator, ctx.clone()).await
            }
            Event::Cleanup(generator) => {
                // Cleanup before deletion
                reconcile_cleanup(generator, ctx.clone()).await
            }
        }
    })
    .await
    .map_err(|e| Error::FinalizerError(Box::new(e)))
}

/// Reconcile when the object is created or updated
async fn reconcile_apply(
    generator: Arc<ConfigMapGenerator>,
    ctx: Arc<Data>,
) -> Result<Action, kube::Error> {
    let client = &ctx.client;
    let namespace = generator.metadata.namespace.as_ref().ok_or_else(|| {
        kube::Error::Api(kube::error::ErrorResponse {
            status: "Failure".to_string(),
            message: "Missing .metadata.namespace".to_string(),
            reason: "MissingObjectKey".to_string(),
            code: 500,
        })
    })?;

    let mut contents = BTreeMap::new();
    contents.insert("content".to_string(), generator.spec.content.clone());
    let oref = generator.controller_owner_ref(&()).unwrap();
    let cm = ConfigMap {
        metadata: ObjectMeta {
            name: generator.metadata.name.clone(),
            owner_references: Some(vec![oref]),
            ..ObjectMeta::default()
        },
        data: Some(contents),
        ..Default::default()
    };
    let cm_api = Api::<ConfigMap>::namespaced(client.clone(), namespace);
    cm_api
        .patch(
            cm.metadata.name.as_ref().ok_or_else(|| {
                kube::Error::Api(kube::error::ErrorResponse {
                    status: "Failure".to_string(),
                    message: "Missing .metadata.name".to_string(),
                    reason: "MissingObjectKey".to_string(),
                    code: 500,
                })
            })?,
            &PatchParams::apply("configmapgenerator.kube-rt.nullable.se"),
            &Patch::Apply(&cm),
        )
        .await?;

    tracing::info!("ConfigMap created/updated for {}", generator.name_any());
    Ok(Action::requeue(Duration::from_secs(300)))
}

/// Cleanup when the object is being deleted
async fn reconcile_cleanup(
    generator: Arc<ConfigMapGenerator>,
    _ctx: Arc<Data>,
) -> Result<Action, kube::Error> {
    tracing::info!("Cleaning up ConfigMapGenerator: {}", generator.name_any());
    // Perform any cleanup logic here if needed
    // The owned ConfigMap will be automatically deleted by Kubernetes due to owner references
    Ok(Action::await_change())
}

/// The controller triggers this on reconcile errors
fn error_policy(_object: Arc<ConfigMapGenerator>, _error: &Error, _ctx: Arc<Data>) -> Action {
    Action::requeue(Duration::from_secs(1))
}

// Data we want access to in error/reconcile calls
struct Data {
    client: Client,
}

pub async fn run_controller(
    token: CancellationToken,
    client: Client,
    reload_trigger: futures::channel::mpsc::Receiver<()>,
) -> Result<(), Box<dyn std::error::Error>> {
    let cmgs = Api::<ConfigMapGenerator>::all(client.clone());
    let cms = Api::<ConfigMap>::all(client.clone());

    tracing::info!("starting configmapgen-controller");

    // limit the controller to running a maximum of two concurrent reconciliations
    let config = Config::default().concurrency(2);

    Controller::new(cmgs, watcher::Config::default())
        .owns(cms, watcher::Config::default())
        .with_config(config)
        .reconcile_all_on(reload_trigger.map(|_| ()))
        .graceful_shutdown_on(async move { token.cancelled().await })
        .run(reconcile, error_policy, Arc::new(Data { client }))
        .for_each(|res| async move {
            match res {
                Ok(o) => tracing::info!("reconciled {:?}", o),
                Err(e) => tracing::warn!("reconcile failed: {}", e),
            }
        })
        .await;
    tracing::info!("controller terminated");
    Ok(())
}
