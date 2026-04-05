use kube::{Api, CustomResource, CustomResourceExt};
use kubelicate_shared::XEDIO_TEST_NAMESPACE;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

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

    pub async fn apply_crd(&self) -> Result<(), kubelicate_shared::Error> {
        let crd = ConfigMapGenerator::crd();
        kubelicate_shared::kube_util::apply_crd(&self.client, crd).await
    }

    pub async fn create_config_map_generator(
        &self,
        name: &str,
        content: &str,
    ) -> Result<(), kubelicate_shared::Error> {
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
    ) -> Result<(), kubelicate_shared::Error> {
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
    ) -> Result<(), kubelicate_shared::Error> {
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
