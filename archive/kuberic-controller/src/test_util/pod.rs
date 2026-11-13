use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use k8s_openapi::api::core::v1::{PersistentVolumeClaim, Pod, Secret};
use kube::ResourceExt;
use kuberic_runtime::host::ReplicaProcessConfig;
use kuberic_runtime::protocol::types::{PodUid, PvcUid, ReplicaId, ResourceUid};

pub(super) const RPC_DEADLINE: std::time::Duration = std::time::Duration::from_secs(5);

pub(super) fn resolve(
    pod: &Pod,
    pvcs: &[PersistentVolumeClaim],
    secrets: &[Secret],
    root: &Path,
    image: &str,
) -> Result<(ReplicaProcessConfig, BTreeMap<String, String>)> {
    let spec = pod.spec.as_ref().context("Pod spec missing")?;
    ensure!(
        spec.init_containers
            .as_deref()
            .unwrap_or_default()
            .is_empty()
            && spec
                .ephemeral_containers
                .as_deref()
                .unwrap_or_default()
                .is_empty(),
        "additional container lifecycles are unsupported"
    );
    ensure!(
        spec.containers.len() == 1,
        "only one application container is supported"
    );
    let container = &spec.containers[0];
    ensure!(
        container.image.as_deref() == Some(image),
        "unsupported application image"
    );
    ensure!(
        container.command.is_none() && container.args.is_none(),
        "custom container command is unsupported"
    );
    ensure!(container.env_from.is_none(), "envFrom is unsupported");
    let mut environment = BTreeMap::new();
    for env in container.env.as_deref().unwrap_or_default() {
        let value = match (&env.value, &env.value_from) {
            (Some(value), None) => value.clone(),
            (None, Some(source)) if source.field_ref.is_some() => {
                let field = &source.field_ref.as_ref().unwrap().field_path;
                match field.as_str() {
                    "metadata.uid" => pod.uid().context("Pod UID missing")?,
                    "metadata.name" => pod.name_any(),
                    "metadata.namespace" => pod.namespace().context("Pod namespace missing")?,
                    "status.podIP" => pod
                        .status
                        .as_ref()
                        .and_then(|status| status.pod_ip.clone())
                        .context("Pod IP missing")?,
                    _ => bail!("unsupported downward API field {field}"),
                }
            }
            (None, Some(source)) if source.secret_key_ref.is_some() => {
                let reference = source.secret_key_ref.as_ref().unwrap();
                let secret = secrets
                    .iter()
                    .find(|secret| {
                        secret.name_any() == reference.name && secret.namespace() == pod.namespace()
                    })
                    .with_context(|| format!("Secret {} missing", reference.name))?;
                let bytes = secret
                    .data
                    .as_ref()
                    .and_then(|data| data.get(&reference.key))
                    .with_context(|| format!("Secret key {} missing", reference.key))?;
                String::from_utf8(bytes.0.clone()).context("Secret value is not UTF-8")?
            }
            _ => bail!("unsupported environment source for {}", env.name),
        };
        ensure!(
            environment.insert(env.name.clone(), value).is_none(),
            "duplicate environment variable {}",
            env.name
        );
    }
    let value = |name: &str| {
        environment
            .get(name)
            .with_context(|| format!("missing {name}"))
    };
    let data_path = value("KUBERIC_DATA_ROOT")?;
    ensure!(
        container.volume_mounts.as_deref().unwrap_or_default().len() == 1
            && spec.volumes.as_deref().unwrap_or_default().len() == 1,
        "only the declared application PVC mount is supported"
    );
    let mount = container
        .volume_mounts
        .as_deref()
        .unwrap_or_default()
        .iter()
        .find(|mount| &mount.mount_path == data_path)
        .context("application data mount missing")?;
    ensure!(
        mount.read_only != Some(true) && mount.sub_path.is_none() && mount.sub_path_expr.is_none(),
        "unsupported data mount"
    );
    let claim = spec
        .volumes
        .as_deref()
        .unwrap_or_default()
        .iter()
        .find(|volume| volume.name == mount.name)
        .and_then(|volume| volume.persistent_volume_claim.as_ref())
        .context("application volume must reference a PVC")?;
    ensure!(
        claim.read_only != Some(true),
        "read-only PVC is unsupported"
    );
    let pvc = pvcs
        .iter()
        .find(|pvc| pvc.name_any() == claim.claim_name && pvc.namespace() == pod.namespace())
        .context("declared PVC missing")?;
    let pvc_uid = pvc.uid().context("PVC UID missing")?;
    ensure!(
        value("KUBERIC_PVC_UID")? == &pvc_uid,
        "PVC identity differs from Pod environment"
    );
    ensure!(
        value("KUBERIC_POD_UID")? == &pod.uid().context("Pod UID missing")?,
        "Pod identity differs from environment"
    );
    ensure!(
        value("KUBERIC_NAMESPACE")? == &pod.namespace().context("Pod namespace missing")?,
        "namespace differs from environment"
    );
    ensure!(
        !pvc_uid.is_empty()
            && pvc_uid
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
        "unsafe PVC UID"
    );
    let replica_id: i64 = value("KUBERIC_REPLICA_ID")?
        .parse()
        .context("invalid replica ID")?;
    ensure!(replica_id > 0, "replica ID must be positive");
    let config = ReplicaProcessConfig {
        resource_uid: ResourceUid::new(value("KUBERIC_RESOURCE_UID")?),
        replica_id: ReplicaId::new(replica_id),
        pod_uid: PodUid::new(value("KUBERIC_POD_UID")?),
        pvc_uid: PvcUid::new(pvc_uid.clone()),
        data_root: PathBuf::from(root).join(pvc_uid),
        replication_address: "127.0.0.1:0".parse()?,
        bearer_token: value("KUBERIC_AGENT_BEARER_TOKEN")?.clone(),
        rpc_deadline: RPC_DEADLINE,
        transport_window_capacity: 256,
    };
    environment.retain(|name, _| {
        !name.contains("TOKEN") && !name.contains("SECRET") && !name.contains("CREDENTIAL")
    });
    environment.insert(
        "KUBERIC_DATA_ROOT".into(),
        config.data_root.to_string_lossy().into_owned(),
    );
    Ok((config, environment))
}
