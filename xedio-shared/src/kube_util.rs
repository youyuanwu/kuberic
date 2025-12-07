use k8s_openapi::{
    api::core::v1::Namespace,
    apiextensions_apiserver::pkg::apis::apiextensions::v1::CustomResourceDefinition,
};
use kube::{
    Api, ResourceExt,
    runtime::{conditions, wait::await_condition},
};

/// Create test namespace once for all tests.
pub async fn init_test_namespace() {
    static INIT: tokio::sync::OnceCell<()> = tokio::sync::OnceCell::const_new();
    INIT.get_or_init(|| async {
        let client = kube::Client::try_default().await.unwrap();
        let namespaces = kube::Api::all(client);
        let ns = Namespace {
            metadata: kube::api::ObjectMeta {
                name: Some(crate::XEDIO_TEST_NAMESPACE.to_string()),
                ..Default::default()
            },
            ..Default::default()
        };
        match namespaces
            .create(&kube::api::PostParams::default(), &ns)
            .await
        {
            Ok(_) => (),
            Err(kube::Error::Api(ae)) if ae.code == 409 => (), // already exists
            Err(e) => panic!("Failed to create test namespace: {}", e),
        }
    })
    .await;
}

pub async fn apply_crd(
    client: &kube::Client,
    def: CustomResourceDefinition,
) -> Result<(), crate::Error> {
    let ssapply = kube::api::PatchParams::apply("xedio-test").force();
    // Ensure the CRD is installed
    let crds: Api<CustomResourceDefinition> = Api::all(client.clone());

    let crd = def;
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
