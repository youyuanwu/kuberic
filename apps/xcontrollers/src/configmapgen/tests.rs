use tokio_util::sync::CancellationToken;

use super::crd::ConfigMapGeneratorClient;

#[tokio::test]
#[test_log::test]
async fn test_crd() {
    xedio_shared::kube_util::init_test_namespace().await;
    let kube_cli = kube::Client::try_default().await.unwrap();
    let crd_cli = ConfigMapGeneratorClient::new_with_test_namespace(kube_cli.clone()); // create the client
    // create the crd
    crd_cli.apply_crd().await.unwrap();

    let (mut reload_tx, reload_rx) = futures::channel::mpsc::channel(0);

    // start the controller
    let token = CancellationToken::new();
    let h_ctrl = tokio::spawn({
        let token = token.clone();
        async move {
            super::controller::run_controller(token, kube_cli, reload_rx)
                .await
                .unwrap();
        }
    });

    // create the crd
    crd_cli
        .create_config_map_generator("mytest1", "myval1")
        .await
        .unwrap();

    // wait for the controller to process the crd
    tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;

    // delete the crd
    crd_cli
        .delete_config_map_generator("mytest1")
        .await
        .unwrap();

    // wait for the controller to process the crd
    tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;

    reload_tx.try_send(()).unwrap();

    token.cancel();
    h_ctrl.await.unwrap();
}
