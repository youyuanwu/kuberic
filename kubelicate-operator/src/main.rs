use std::sync::Arc;

use futures::StreamExt;
use k8s_openapi::api::core::v1::Pod;
use kube::runtime::controller::{Action, Controller};
use kube::runtime::watcher;
use kube::{Api, Client};
use tracing::info;

use kubelicate_operator::cluster_api::KubeClusterApi;
use kubelicate_operator::crd::KubelicateSet;
use kubelicate_operator::reconciler::{ReconcileAction, ReconcilerState};

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
struct OperatorError(String);

struct Context {
    api: KubeClusterApi,
    state: ReconcilerState,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();

    info!("Starting kubelicate-operator");

    let client = Client::try_default().await?;

    let sets: Api<KubelicateSet> = Api::all(client.clone());
    let pods: Api<Pod> = Api::all(client.clone());

    let ctx = Arc::new(Context {
        api: KubeClusterApi {
            client: client.clone(),
        },
        state: ReconcilerState::default(),
    });

    info!("Watching KubelicateSets");

    Controller::new(sets, watcher::Config::default())
        .owns(pods, watcher::Config::default())
        .run(
            |set: Arc<KubelicateSet>, ctx: Arc<Context>| async move {
                match kubelicate_operator::reconciler::reconcile_set(&set, &ctx.api, &ctx.state)
                    .await
                {
                    Ok(ReconcileAction::Requeue(d)) => Ok(Action::requeue(d)),
                    Err(e) => Err(OperatorError(e)),
                }
            },
            |_set: Arc<KubelicateSet>, error, _ctx: Arc<Context>| {
                tracing::warn!(?error, "controller error");
                Action::requeue(std::time::Duration::from_secs(10))
            },
            ctx,
        )
        .for_each(|res| async move {
            match res {
                Ok(o) => info!("reconciled {:?}", o),
                Err(e) => tracing::warn!("reconcile failed: {}", e),
            }
        })
        .await;

    Ok(())
}
