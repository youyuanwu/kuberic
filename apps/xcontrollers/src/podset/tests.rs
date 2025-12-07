use kube::client;
use tokio_util::sync::CancellationToken;
use xedio_shared::XEDIO_TEST_NAMESPACE;

#[tokio::test]
#[test_log::test]
async fn test_crd() {
    xedio_shared::kube_util::init_test_namespace().await;

    let k8s_client = client::Client::try_default().await.unwrap();
    let client =
        super::crd::PodSetClient::new(k8s_client.clone(), XEDIO_TEST_NAMESPACE.to_string());

    // apply crd
    client.apply_crd().await.unwrap();

    // start the controller
    let token = CancellationToken::new();
    let h_ctrl = tokio::spawn({
        let token = token.clone();
        async move {
            super::controller::run_controller(token, k8s_client)
                .await
                .unwrap();
        }
    });

    tracing::info!("creating podset");
    let podset_name = "mypodset";
    // create a podset
    client.create_podset(podset_name).await.unwrap();

    // wait for the controller to process the podset
    tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;

    // check if the podset was processed
    let podset = client.get_podset(podset_name).await.unwrap(); // This should return the podset with the correct status
    assert_eq!(podset.spec.replicas, 3); // Assuming the controller sets the replicas to 3  

    // check if the pods were created
    let pods = client.list_pods(podset_name).await.unwrap();
    assert_eq!(pods.len(), 3); // Assuming the controller creates 3
    // check pods are running
    for pod in pods {
        assert_eq!(pod.status.unwrap().phase.unwrap(), "Running");
    }

    tracing::info!("deleting first pod");
    // delete the first pod
    client.delete_pod(podset_name, 0).await.unwrap();
    // wait for the controller to recreate the pod
    tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;

    // check if the pod was recreated
    let pods = client.list_pods(podset_name).await.unwrap();
    assert_eq!(pods.len(), 3); // Assuming the controller recreates the pod

    tracing::info!("deleting podset");
    // clean up
    client.delete_podset(podset_name).await.unwrap();

    // wait for deletion
    tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;

    token.cancel();
    h_ctrl.await.unwrap();
}
