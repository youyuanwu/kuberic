//! XdataApp Operator
//!
//! This operator manages xdata-app StatefulSet deployments.
//! It watches for XdataApp custom resources and creates/manages the corresponding
//! StatefulSet, Services, ServiceAccount, and RBAC resources.
//!
//! To run this operator:
//! ```bash
//! cargo run --bin xdata-op
//! ```
//!
//! Note: This requires a working Kubernetes cluster and kubectl configuration.

mod crd;
mod statefulset;

use std::sync::Arc;

use crd::{XdataApp, XdataAppStatus};
use futures::StreamExt;
use k8s_openapi::api::apps::v1::StatefulSet;
use k8s_openapi::api::core::v1::{Service, ServiceAccount};
use k8s_openapi::api::rbac::v1::{Role, RoleBinding};
use kube::api::{DeleteParams, Patch, PatchParams};
use kube::runtime::controller::{Action, Config, Controller};
use kube::runtime::finalizer::{Event, finalizer};
use kube::runtime::watcher;
use kube::{Api, Client};
use xedio_shared::NAME_XEDIO;

const FINALIZER_NAME: &str = "xdataapp.xedio.io/finalizer";

#[derive(Debug, thiserror::Error)]
enum Error {
    #[error("Kube error: {0}")]
    Kube(#[from] kube::Error),

    #[error("Finalizer error: {0}")]
    Finalizer(#[source] Box<kube::runtime::finalizer::Error<kube::Error>>),

    #[error("Missing object key: {0}")]
    MissingObjectKey(&'static str),
}

struct Data {
    client: Client,
}

async fn reconcile(xapp: Arc<XdataApp>, ctx: Arc<Data>) -> Result<Action, Error> {
    let client = &ctx.client;
    let namespace = xapp
        .metadata
        .namespace
        .as_ref()
        .ok_or(Error::MissingObjectKey(".metadata.namespace"))?;

    let xapp_api: Api<XdataApp> = Api::namespaced(client.clone(), namespace);

    finalizer(&xapp_api, FINALIZER_NAME, xapp, |event| async {
        match event {
            Event::Apply(xapp) => reconcile_apply(xapp, ctx.clone()).await,
            Event::Cleanup(xapp) => reconcile_cleanup(xapp, ctx.clone()).await,
        }
    })
    .await
    .map_err(|e| Error::Finalizer(Box::new(e)))
}

async fn reconcile_apply(object: Arc<XdataApp>, ctx: Arc<Data>) -> Result<Action, kube::Error> {
    let client = &ctx.client;
    let name = object.metadata.name.as_ref().ok_or_else(|| {
        kube::Error::Api(
            kube::core::Status::failure("Missing .metadata.name", "MissingObjectKey").boxed(),
        )
    })?;
    let namespace = object.metadata.namespace.as_ref().ok_or_else(|| {
        kube::Error::Api(
            kube::core::Status::failure("Missing .metadata.namespace", "MissingObjectKey").boxed(),
        )
    })?;

    tracing::info!("Reconciling XdataApp {}/{}", namespace, name);

    // Create or update ServiceAccount
    let service_account = statefulset::build_service_account(&object);
    let sa_api: Api<ServiceAccount> = Api::namespaced(client.clone(), namespace);
    sa_api
        .patch(
            &service_account.metadata.name.clone().unwrap(),
            &PatchParams::apply("xdata-op"),
            &Patch::Apply(&service_account),
        )
        .await?;

    // Create or update Role
    let role = statefulset::build_role(&object);
    let role_api: Api<Role> = Api::namespaced(client.clone(), namespace);
    role_api
        .patch(
            &role.metadata.name.clone().unwrap(),
            &PatchParams::apply("xdata-op"),
            &Patch::Apply(&role),
        )
        .await?;

    // Create or update RoleBinding
    let role_binding = statefulset::build_role_binding(&object);
    let rb_api: Api<RoleBinding> = Api::namespaced(client.clone(), namespace);
    rb_api
        .patch(
            &role_binding.metadata.name.clone().unwrap(),
            &PatchParams::apply("xdata-op"),
            &Patch::Apply(&role_binding),
        )
        .await?;

    // Create or update headless Service
    let headless_service = statefulset::build_headless_service(&object);
    let svc_api: Api<Service> = Api::namespaced(client.clone(), namespace);
    svc_api
        .patch(
            &headless_service.metadata.name.clone().unwrap(),
            &PatchParams::apply("xdata-op"),
            &Patch::Apply(&headless_service),
        )
        .await?;

    // Create or update NodePort services
    let nodeport_services = statefulset::build_nodeport_services(&object);
    for service in nodeport_services {
        svc_api
            .patch(
                &service.metadata.name.clone().unwrap(),
                &PatchParams::apply("xdata-op"),
                &Patch::Apply(&service),
            )
            .await?;
    }

    // Create or update StatefulSet
    let statefulset = statefulset::build_statefulset(&object);
    let sts_api: Api<StatefulSet> = Api::namespaced(client.clone(), namespace);
    let sts = sts_api
        .patch(
            name,
            &PatchParams::apply("xdata-op"),
            &Patch::Apply(&statefulset),
        )
        .await?;

    tracing::info!("Successfully reconciled XdataApp {}/{}", namespace, name);

    // Update status
    let status = XdataAppStatus {
        state: Some("Running".to_string()),
        ready_replicas: sts.status.and_then(|s| s.ready_replicas),
        observed_generation: object.metadata.generation,
        error: None,
    };

    let status_patch = serde_json::json!({ "status": status });
    let xapp_api: Api<XdataApp> = Api::namespaced(client.clone(), namespace);
    xapp_api
        .patch_status(
            name,
            &PatchParams::apply("xdata-op"),
            &Patch::Merge(&status_patch),
        )
        .await
        .map_err(|e| {
            tracing::warn!("Failed to update status: {}", e);
            e
        })?;

    Ok(Action::requeue(std::time::Duration::from_secs(300)))
}

async fn reconcile_cleanup(object: Arc<XdataApp>, ctx: Arc<Data>) -> Result<Action, kube::Error> {
    let client = &ctx.client;
    let name = object.metadata.name.as_ref().unwrap();
    let namespace = object.metadata.namespace.as_ref().unwrap();

    tracing::info!("Cleaning up XdataApp {}/{}", namespace, name);

    // Delete StatefulSet
    let sts_api: Api<StatefulSet> = Api::namespaced(client.clone(), namespace);
    match sts_api.delete(name, &DeleteParams::default()).await {
        Ok(_) => tracing::info!("Deleted StatefulSet {}", name),
        Err(kube::Error::Api(ae)) if ae.code == 404 => {
            tracing::debug!("StatefulSet {} not found, already deleted", name)
        }
        Err(e) => {
            tracing::error!("Failed to delete StatefulSet: {}", e);
            return Err(e);
        }
    }

    // Delete headless Service
    let svc_api: Api<Service> = Api::namespaced(client.clone(), namespace);
    match svc_api.delete("xdata-app", &DeleteParams::default()).await {
        Ok(_) => tracing::info!("Deleted headless Service"),
        Err(kube::Error::Api(ae)) if ae.code == 404 => {
            tracing::debug!("Headless Service not found, already deleted")
        }
        Err(e) => {
            tracing::error!("Failed to delete headless Service: {}", e);
        }
    }

    // Delete NodePort services
    for i in 0..object.spec.replicas {
        let pod_svc_name = format!("{}-{}", name, i);
        match svc_api
            .delete(&pod_svc_name, &DeleteParams::default())
            .await
        {
            Ok(_) => tracing::info!("Deleted NodePort Service {}", pod_svc_name),
            Err(kube::Error::Api(ae)) if ae.code == 404 => {
                tracing::debug!(
                    "NodePort Service {} not found, already deleted",
                    pod_svc_name
                )
            }
            Err(e) => {
                tracing::error!("Failed to delete NodePort Service {}: {}", pod_svc_name, e);
            }
        }
    }

    // Delete RoleBinding
    let rb_api: Api<RoleBinding> = Api::namespaced(client.clone(), namespace);
    let rb_name = "xdata-app-leader-election";
    match rb_api.delete(rb_name, &DeleteParams::default()).await {
        Ok(_) => tracing::info!("Deleted RoleBinding {}", rb_name),
        Err(kube::Error::Api(ae)) if ae.code == 404 => {
            tracing::debug!("RoleBinding {} not found, already deleted", rb_name)
        }
        Err(e) => {
            tracing::error!("Failed to delete RoleBinding: {}", e);
        }
    }

    // Delete Role
    let role_api: Api<Role> = Api::namespaced(client.clone(), namespace);
    match role_api.delete(rb_name, &DeleteParams::default()).await {
        Ok(_) => tracing::info!("Deleted Role {}", rb_name),
        Err(kube::Error::Api(ae)) if ae.code == 404 => {
            tracing::debug!("Role {} not found, already deleted", rb_name)
        }
        Err(e) => {
            tracing::error!("Failed to delete Role: {}", e);
        }
    }

    // Delete ServiceAccount
    let sa_api: Api<ServiceAccount> = Api::namespaced(client.clone(), namespace);
    match sa_api.delete("xdata-app", &DeleteParams::default()).await {
        Ok(_) => tracing::info!("Deleted ServiceAccount"),
        Err(kube::Error::Api(ae)) if ae.code == 404 => {
            tracing::debug!("ServiceAccount not found, already deleted")
        }
        Err(e) => {
            tracing::error!("Failed to delete ServiceAccount: {}", e);
        }
    }

    tracing::info!("Cleanup complete for XdataApp {}/{}", namespace, name);

    Ok(Action::await_change())
}

fn error_policy(_object: Arc<XdataApp>, _error: &Error, _ctx: Arc<Data>) -> Action {
    Action::requeue(std::time::Duration::from_secs(5))
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();

    tracing::info!("Starting XdataApp Operator");

    let client = Client::try_default().await?;

    let xapps: Api<XdataApp> = Api::namespaced(client.clone(), NAME_XEDIO);
    let statefulsets: Api<StatefulSet> = Api::namespaced(client.clone(), NAME_XEDIO);

    let config = Config::default().concurrency(2);

    tracing::info!("Watching XdataApps in '{}' namespace", NAME_XEDIO);

    Controller::new(xapps, watcher::Config::default())
        .owns(statefulsets, watcher::Config::default())
        .with_config(config)
        .graceful_shutdown_on(xedio_shared::utils::wait_for_shutdown_signal())
        .run(reconcile, error_policy, Arc::new(Data { client }))
        .for_each(|res| async move {
            match res {
                Ok(o) => tracing::info!("reconciled {:?}", o),
                Err(e) => tracing::warn!("reconcile failed: {}", e),
            }
        })
        .await;

    tracing::info!("operator terminated");
    Ok(())
}
