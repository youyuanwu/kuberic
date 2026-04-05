use k8s_openapi::api::core::v1::Pod;
use kube::{CustomResource, api::DeleteParams};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// XdataApp is a custom resource that defines a stateful xdata-app deployment
#[derive(CustomResource, Serialize, Deserialize, Debug, PartialEq, Clone, JsonSchema)]
#[kube(
    group = "nullable.se",
    version = "v1",
    kind = "PodSet",
    plural = "podsets",
    shortname = "pset",
    derive = "PartialEq",
    namespaced,
    // status = "TODO"
)]
#[serde(rename_all = "camelCase")]
pub struct PodSetSpec {
    /// Number of replicas for the PodSet
    pub replicas: i32,

    /// Container image to use
    pub image: String,

    /// Image pull policy
    pub image_pull_policy: String,

    // Command to run in the container
    pub command: Option<Vec<String>>,
}

pub struct PodSetClient {
    client: kube::Client,
    namespace: String,
}

impl PodSetClient {
    pub fn new(client: kube::Client, namespace: String) -> Self {
        PodSetClient { client, namespace }
    }

    pub async fn apply_crd(&self) -> Result<(), kubelicate_shared::Error> {
        use kube::CustomResourceExt;
        kubelicate_shared::kube_util::apply_crd(&self.client, PodSet::crd()).await?;
        Ok(())
    }

    pub async fn get_podset(&self, name: &str) -> Result<PodSet, kube::Error> {
        let podset_api = kube::Api::<PodSet>::namespaced(self.client.clone(), &self.namespace);
        podset_api.get(name).await
    }

    // Use patch to create the podset.
    pub async fn create_podset(&self, name: &str) -> Result<PodSet, kubelicate_shared::Error> {
        let podset = PodSet {
            metadata: kube::api::ObjectMeta {
                name: Some(name.to_string()),
                namespace: Some(self.namespace.clone()),
                ..Default::default()
            },
            spec: PodSetSpec {
                replicas: 3,
                image: "busybox:latest".to_string(),
                image_pull_policy: "IfNotPresent".to_string(),
                command: Some(vec!["sleep".to_string(), "infinity".to_string()]),
            },
        };
        let podset_api = kube::Api::<PodSet>::namespaced(self.client.clone(), &self.namespace);
        podset_api
            .patch(
                name,
                &kube::api::PatchParams::apply("create_podset.example"),
                &kube::api::Patch::Apply(podset.clone()),
            )
            .await?;

        // wait for the podset to be applied
        let establish = kube::runtime::wait::await_condition(
            podset_api.clone(),
            name,
            |podset: Option<&PodSet>| {
                podset
                    .map(|ps| ps.metadata.name == Some(name.to_string()))
                    .unwrap_or(false)
            },
        );
        tokio::time::timeout(std::time::Duration::from_secs(10), establish).await??;
        Ok(podset)
    }

    pub async fn delete_podset(&self, name: &str) -> Result<(), kube::Error> {
        let podset_api = kube::Api::<PodSet>::namespaced(self.client.clone(), &self.namespace);
        podset_api.delete(name, &Default::default()).await?;
        Ok(())
    }
}

impl PodSetClient {
    pub async fn list_pods(&self, podset_name: &str) -> Result<Vec<Pod>, kube::Error> {
        let pod_api = kube::Api::<Pod>::namespaced(self.client.clone(), &self.namespace);
        let pods = pod_api
            .list(&kube::api::ListParams::default().labels(&format!("podset={}", podset_name)))
            .await?;
        Ok(pods.items)
    }

    pub async fn delete_pod(&self, podset_name: &str, index: i32) -> Result<(), kube::Error> {
        let pod_name = format!("{}-pod-{}", podset_name, index);
        let pod_api = kube::Api::<Pod>::namespaced(self.client.clone(), &self.namespace);

        // Use foreground deletion to ensure the pod is actually removed
        let dp = DeleteParams {
            grace_period_seconds: Some(0),
            propagation_policy: Some(kube::api::PropagationPolicy::Foreground),
            ..Default::default()
        };

        pod_api.delete(&pod_name, &dp).await?;
        Ok(())
    }
}
