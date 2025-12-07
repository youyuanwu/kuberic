use k8s_openapi::apiextensions_apiserver::pkg::apis::apiextensions::v1::CustomResourceDefinition;
use kube::{
    Api, CustomResource, CustomResourceExt, ResourceExt,
    api::PatchParams,
    runtime::wait::{await_condition, conditions},
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use xedio_shared::XEDIO_TEST_NAMESPACE;

#[derive(CustomResource, Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[kube(group = "nullable.se", version = "v1", kind = "ConfigMapGenerator")]
#[kube(shortname = "cmg", namespaced)]
pub struct ConfigMapGeneratorSpec {
    pub content: String,
}

/// Client wrapper for ConfigMapGenerator operations
pub struct ConfigMapGeneratorClient {
    client: kube::Client,
    namespace: String,
}

impl ConfigMapGeneratorClient {
    pub fn new(client: kube::Client, namespace: String) -> Self {
        Self { client, namespace }
    }

    pub fn new_with_test_namespace(client: kube::Client) -> Self {
        Self {
            client,
            namespace: XEDIO_TEST_NAMESPACE.to_string(),
        }
    }

    pub async fn apply_crd(&self) -> Result<(), Box<dyn std::error::Error>> {
        let ssapply = PatchParams::apply("crd_apply_example").force();

        // Ensure the CRD is installed
        let crds: Api<CustomResourceDefinition> = Api::all(self.client.clone());

        let crd = ConfigMapGenerator::crd();
        let crd_name = crd.name_any();
        tracing::info!("Creating CRD: {}", serde_yaml::to_string(&crd)?);

        crds.patch(&crd_name, &ssapply, &kube::api::Patch::Apply(&crd))
            .await?;

        tracing::info!("Waiting for the api-server to accept the CRD");
        let establish = await_condition(crds, &crd_name, conditions::is_crd_established());
        tokio::time::timeout(std::time::Duration::from_secs(10), establish).await??;

        tracing::info!("CRD established successfully");
        Ok(())
    }

    pub async fn create_config_map_generator(
        &self,
        name: &str,
        content: &str,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let cmg_api: Api<ConfigMapGenerator> =
            Api::namespaced(self.client.clone(), &self.namespace);
        let cmg = ConfigMapGenerator {
            metadata: kube::api::ObjectMeta {
                name: Some(name.to_string()),
                ..Default::default()
            },
            spec: ConfigMapGeneratorSpec {
                content: content.to_string(),
            },
        };

        tracing::info!(
            "Creating ConfigMapGenerator: {}",
            serde_yaml::to_string(&cmg)?
        );
        cmg_api
            .create(&kube::api::PostParams::default(), &cmg)
            .await?;
        Ok(())
    }

    pub async fn delete_config_map_generator(
        &self,
        name: &str,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let cmg_api: Api<ConfigMapGenerator> =
            Api::namespaced(self.client.clone(), &self.namespace);
        tracing::info!("Deleting ConfigMapGenerator: {}", name);
        cmg_api
            .delete(name, &kube::api::DeleteParams::default())
            .await?;
        Ok(())
    }

    pub async fn patch_config_map_generator(
        &self,
        name: &str,
        new_content: &str,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let cmg_api: Api<ConfigMapGenerator> =
            Api::namespaced(self.client.clone(), &self.namespace);
        let patch_params = kube::api::PatchParams::apply("crd_patch_example").force();
        let patch = serde_json::json!({
            "spec": {
                "content": new_content
            }
        });

        tracing::info!("Patching ConfigMapGenerator: {}", name);
        cmg_api
            .patch(name, &patch_params, &kube::api::Patch::Merge(&patch))
            .await?;
        Ok(())
    }
}
