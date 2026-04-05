use kubelicate_shared::proto::hello_world;
use kubelicate_shared::proto::hello_world::{HelloRequest, greeter_client::GreeterClient};
use kubelicate_shared::storage_client::StorageClientExt;

/// Test calling the gRPC service via NodePort
/// This test assumes the service is deployed in k8s and accessible via NodePort
/// Run with: cargo test -- --ignored
#[tokio::test]
#[test_log::test]
async fn test_call_nodeport() {
    crate::test_utils::ensure_test_env_deployed().await;

    // Replace with your actual node IP
    // You can find it with: kubectl get nodes -o wide
    let node_ip = std::env::var("K8S_NODE_IP").unwrap_or_else(|_| "localhost".to_string());
    let node_port = "30081";
    let endpoint = format!("http://{}:{}", node_ip, node_port);

    println!("Connecting to: {}", endpoint);

    // Create a gRPC channel
    let endpoint = tonic::transport::Endpoint::from_shared(endpoint)
        .expect("Invalid endpoint")
        .connect_timeout(std::time::Duration::from_secs(5));
    let channel = endpoint
        .connect()
        .await
        .expect("Failed to connect to gRPC server");

    // Create a gRPC client
    let mut greeter_client = GreeterClient::new(channel.clone());

    // Create a request
    let request = tonic::Request::new(HelloRequest {
        name: "TestClient".to_string(),
    });

    // Call the service
    let response = greeter_client
        .say_hello(request)
        .await
        .expect("Failed to call say_hello")
        .into_inner();

    println!("Response: {:?}", response);
    // Verify the response
    assert_eq!(response.message, "Hello TestClient!");

    // Get the Pod name
    let pod_name_request = tonic::Request::new(hello_world::PodNameRequest {});
    let pod_name_response = greeter_client
        .get_pod_name(pod_name_request)
        .await
        .expect("Failed to call get_pod_name")
        .into_inner();

    println!("Pod Name Response: {:?}", pod_name_response);
    assert_eq!(pod_name_response.pod_name, "xdata-app-0"); // Adjust based on your pod naming

    // create a storage client
    let storage_client = hello_world::storage_client::StorageClient::new(channel.clone());

    // Create a file handle for easier operations
    let mut file = storage_client.file("test.txt".to_string());

    // If file exists, delete it first
    if file.exists().await {
        let delete_response = file.delete().await.expect("Failed to delete existing file");
        println!("Deleted existing file: {:?}", delete_response);
    }
    // Write a file using the FileHandle wrapper
    let write_file_response = file
        .write("Hello, Xedio!".to_string())
        .await
        .expect("Failed to call write_file");
    println!("Write File Response: {:?}", write_file_response);
    assert!(write_file_response.success);

    // Read the file using the FileHandle wrapper
    let read_file_response = file.read().await.expect("Failed to call read_file");
    println!("Read File Response: {:?}", read_file_response);
    assert_eq!(read_file_response.content, "Hello, Xedio!");

    // List files again
    let list_files_request = tonic::Request::new(hello_world::ListFilesRequest {});
    let list_files_response = file
        .client_mut()
        .list_files(list_files_request)
        .await
        .expect("Failed to call list_files")
        .into_inner();
    println!("List Files Response: {:?}", list_files_response);
    assert_eq!(list_files_response.filenames, vec!["test.txt".to_string()]);

    // Delete the file using the FileHandle wrapper
    let delete_file_response = file.delete().await.expect("Failed to call delete_file");
    println!("Delete File Response: {:?}", delete_file_response);
    assert!(delete_file_response.success);

    // Check if it is leader
    let mut client = hello_world::leader_election_client::LeaderElectionClient::new(channel);
    let is_leader_request = tonic::Request::new(hello_world::IsLeaderRequest {});
    let is_leader_response = client
        .is_leader(is_leader_request)
        .await
        .expect("Failed to call is_leader")
        .into_inner();
    println!("Is Leader Response: {:?}", is_leader_response);
}
