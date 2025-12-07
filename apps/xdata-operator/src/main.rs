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

use async_trait::async_trait;
use crd::{XdataApp, XdataAppStatus};
use k8s_openapi::api::apps::v1::StatefulSet;
use k8s_openapi::api::core::v1::{Service, ServiceAccount};
use k8s_openapi::api::rbac::v1::{Role, RoleBinding};
use kube::api::{DeleteParams, Patch, PatchParams};
use kube::runtime::controller::Action;
use kube::runtime::watcher::Config;
use kube::{Api, Client};
use kuberator::cache::{CachingStrategy, StaticApiProvider};
use kuberator::error::Result as KubeResult;
use kuberator::k8s::K8sRepository;
use kuberator::{Context, Finalize, Reconcile};
use xedio_shared::NAME_XEDIO;

// Type alias for our repository
type MyK8sRepo = K8sRepository<XdataApp, StaticApiProvider<XdataApp>>;

// Define the context that holds our operator logic
struct XdataAppContext {
    repo: Arc<MyK8sRepo>,
    client: Client,
}

#[async_trait]
impl Context<XdataApp, MyK8sRepo, StaticApiProvider<XdataApp>> for XdataAppContext {
    fn k8s_repository(&self) -> Arc<MyK8sRepo> {
        Arc::clone(&self.repo)
    }

    fn finalizer(&self) -> &'static str {
        "xdataapp.xedio.io/finalizer"
    }

    async fn handle_apply(&self, object: Arc<XdataApp>) -> KubeResult<Action> {
        let name = object.metadata.name.as_ref().unwrap();
        let namespace = object.metadata.namespace.as_ref().unwrap();

        tracing::info!("Reconciling XdataApp {}/{}", namespace, name);

        // Create or update ServiceAccount
        let service_account = statefulset::build_service_account(&object);
        let sa_api: Api<ServiceAccount> = Api::namespaced(self.client.clone(), namespace);
        sa_api
            .patch(
                &service_account.metadata.name.clone().unwrap(),
                &PatchParams::apply("xdata-op"),
                &Patch::Apply(&service_account),
            )
            .await
            .map_err(|e| {
                tracing::error!("Failed to create/update ServiceAccount: {}", e);
                kuberator::error::Error::from(e)
            })?;

        // Create or update Role
        let role = statefulset::build_role(&object);
        let role_api: Api<Role> = Api::namespaced(self.client.clone(), namespace);
        role_api
            .patch(
                &role.metadata.name.clone().unwrap(),
                &PatchParams::apply("xdata-op"),
                &Patch::Apply(&role),
            )
            .await
            .map_err(|e| {
                tracing::error!("Failed to create/update Role: {}", e);
                kuberator::error::Error::from(e)
            })?;

        // Create or update RoleBinding
        let role_binding = statefulset::build_role_binding(&object);
        let rb_api: Api<RoleBinding> = Api::namespaced(self.client.clone(), namespace);
        rb_api
            .patch(
                &role_binding.metadata.name.clone().unwrap(),
                &PatchParams::apply("xdata-op"),
                &Patch::Apply(&role_binding),
            )
            .await
            .map_err(|e| {
                tracing::error!("Failed to create/update RoleBinding: {}", e);
                kuberator::error::Error::from(e)
            })?;

        // Create or update headless Service
        let headless_service = statefulset::build_headless_service(&object);
        let svc_api: Api<Service> = Api::namespaced(self.client.clone(), namespace);
        svc_api
            .patch(
                &headless_service.metadata.name.clone().unwrap(),
                &PatchParams::apply("xdata-op"),
                &Patch::Apply(&headless_service),
            )
            .await
            .map_err(|e| {
                tracing::error!("Failed to create/update headless Service: {}", e);
                kuberator::error::Error::from(e)
            })?;

        // Create or update NodePort services
        let nodeport_services = statefulset::build_nodeport_services(&object);
        for service in nodeport_services {
            svc_api
                .patch(
                    &service.metadata.name.clone().unwrap(),
                    &PatchParams::apply("xdata-op"),
                    &Patch::Apply(&service),
                )
                .await
                .map_err(|e| {
                    tracing::error!("Failed to create/update NodePort Service: {}", e);
                    kuberator::error::Error::from(e)
                })?;
        }

        // Create or update StatefulSet
        let statefulset = statefulset::build_statefulset(&object);
        let sts_api: Api<StatefulSet> = Api::namespaced(self.client.clone(), namespace);
        let sts = sts_api
            .patch(
                name,
                &PatchParams::apply("xdata-op"),
                &Patch::Apply(&statefulset),
            )
            .await
            .map_err(|e| {
                tracing::error!("Failed to create/update StatefulSet: {}", e);
                kuberator::error::Error::from(e)
            })?;

        tracing::info!("Successfully reconciled XdataApp {}/{}", namespace, name);

        // Update status
        let status = XdataAppStatus {
            state: Some("Running".to_string()),
            ready_replicas: sts.status.and_then(|s| s.ready_replicas),
            observed_generation: None, // Will be set by update_status
            error: None,
        };

        self.k8s_repository()
            .update_status(&object, status)
            .await
            .map_err(|e| {
                tracing::warn!("Failed to update status: {}", e);
                e
            })?;

        Ok(Action::requeue(std::time::Duration::from_secs(300)))
    }

    async fn handle_cleanup(&self, object: Arc<XdataApp>) -> KubeResult<Action> {
        let name = object.metadata.name.as_ref().unwrap();
        let namespace = object.metadata.namespace.as_ref().unwrap();

        tracing::info!("Cleaning up XdataApp {}/{}", namespace, name);

        // Delete StatefulSet
        let sts_api: Api<StatefulSet> = Api::namespaced(self.client.clone(), namespace);
        match sts_api.delete(name, &DeleteParams::default()).await {
            Ok(_) => tracing::info!("Deleted StatefulSet {}", name),
            Err(kube::Error::Api(ae)) if ae.code == 404 => {
                tracing::debug!("StatefulSet {} not found, already deleted", name)
            }
            Err(e) => {
                tracing::error!("Failed to delete StatefulSet: {}", e);
                return Err(kuberator::error::Error::from(e));
            }
        }

        // Delete headless Service
        let svc_api: Api<Service> = Api::namespaced(self.client.clone(), namespace);
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
        let rb_api: Api<RoleBinding> = Api::namespaced(self.client.clone(), namespace);
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
        let role_api: Api<Role> = Api::namespaced(self.client.clone(), namespace);
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
        let sa_api: Api<ServiceAccount> = Api::namespaced(self.client.clone(), namespace);
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
}

// Define the reconciler
struct XdataAppReconciler {
    context: Arc<XdataAppContext>,
    crd_api: Api<XdataApp>,
}

#[async_trait]
impl Reconcile<XdataApp, XdataAppContext, MyK8sRepo, StaticApiProvider<XdataApp>>
    for XdataAppReconciler
{
    fn destruct(self) -> (Api<XdataApp>, Config, Arc<XdataAppContext>) {
        (self.crd_api, Config::default(), self.context)
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize logging
    tracing_subscriber::fmt::init();

    tracing::info!("Starting XdataApp Operator");

    // Create Kubernetes client
    let client = Client::try_default().await?;

    // Create API provider with Strict caching strategy
    let api_provider =
        StaticApiProvider::new(client.clone(), vec![NAME_XEDIO], CachingStrategy::Strict);

    // Create repository and context
    let k8s_repo = K8sRepository::new(api_provider);
    let context = XdataAppContext {
        repo: Arc::new(k8s_repo),
        client: client.clone(),
    };

    // Create reconciler
    let reconciler = XdataAppReconciler {
        context: Arc::new(context),
        crd_api: Api::namespaced(client, NAME_XEDIO),
    };

    tracing::info!("Watching XdataApps in '{}' namespace", NAME_XEDIO);

    // Start the reconciler with graceful shutdown
    reconciler
        .start(Some(xedio_shared::utils::wait_for_shutdown_signal()))
        .await;

    Ok(())
}
