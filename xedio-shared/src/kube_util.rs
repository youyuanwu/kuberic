use k8s_openapi::api::core::v1::Namespace;

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
