use kube::{
    Api, Client,
    api::{Patch, PatchParams},
};

use crate::podset::crd::PodSet;

pub async fn create_or_update_pod(
    podset: &PodSet,
    replica_index: i32,
    client: &Client,
) -> Result<(), kube::Error> {
    use kube::Resource;

    let podset_name = podset.metadata.name.clone().unwrap();
    let pod_name = format!("{}-pod-{}", podset_name, replica_index);

    let oref = podset.controller_owner_ref(&()).unwrap();

    // Create labels for filtering pods
    let mut labels = std::collections::BTreeMap::new();
    labels.insert("app".to_string(), "podset".to_string());
    labels.insert("podset".to_string(), podset_name.clone());
    labels.insert("replica-index".to_string(), replica_index.to_string());

    let pod = k8s_openapi::api::core::v1::Pod {
        metadata: k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta {
            name: Some(pod_name.clone()),
            namespace: podset.metadata.namespace.clone(),
            owner_references: Some(vec![oref]),
            labels: Some(labels),
            ..Default::default()
        },
        spec: Some(k8s_openapi::api::core::v1::PodSpec {
            containers: vec![k8s_openapi::api::core::v1::Container {
                name: pod_name,
                image: Some(podset.spec.image.clone()),
                command: podset.spec.command.clone(),

                ..Default::default()
            }],
            ..Default::default()
        }),
        ..Default::default()
    };
    // create the pod using patch api
    let pod_api: Api<k8s_openapi::api::core::v1::Pod> =
        Api::namespaced(client.clone(), podset.metadata.namespace.as_ref().unwrap());
    pod_api
        .patch(
            pod.metadata.name.as_ref().unwrap(),
            &PatchParams::apply("podsetcontroller.kube-rt.nullable.se"),
            &Patch::Apply(&pod),
        )
        .await?;
    Ok(())
}
