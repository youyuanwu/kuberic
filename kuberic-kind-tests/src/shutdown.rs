use std::{
    collections::{BTreeMap, BTreeSet},
    net::SocketAddrV4,
    time::Duration,
};

use anyhow::{ensure, Context, Result};
use fube_node_runner::Config;
use kuberic_controller::FrozenCohort;
use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream},
};

const PORT: u16 = 19002;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Ready {
    deployment: String,
    pod_uid: String,
    node_id: String,
}

pub(crate) struct Shutdown {
    listener: TcpListener,
    ready: BTreeMap<String, TcpStream>,
    released: bool,
}

impl Shutdown {
    pub async fn bind(ip: &str) -> Result<Self> {
        Ok(Self {
            listener: TcpListener::bind((ip, PORT)).await?,
            ready: BTreeMap::new(),
            released: false,
        })
    }

    pub async fn observe(
        &mut self,
        cohort: &FrozenCohort,
        deleting: &BTreeSet<String>,
    ) -> Result<()> {
        if self.released {
            return Ok(());
        }
        while let Ok(accepted) =
            tokio::time::timeout(Duration::from_millis(1), self.listener.accept()).await
        {
            let (mut stream, source) = accepted?;
            let ready: Ready = tokio::time::timeout(Duration::from_secs(1), async {
                let mut line = String::new();
                BufReader::new((&mut stream).take(1024))
                    .read_line(&mut line)
                    .await?;
                Ok::<_, anyhow::Error>(serde_json::from_str(&line)?)
            })
            .await
            .context("shutdown-ready message deadline")??;
            ensure!(
                ready.deployment == cohort.allocation.deployment
                    && cohort.bindings.iter().zip(&cohort.allocation.nodes).any(
                        |(binding, node)| binding.pod_uid == ready.pod_uid
                            && node.node_id == ready.node_id
                            && source.ip().to_string() == binding.ip
                    )
                    && deleting.contains(&ready.pod_uid)
                    && !self.ready.contains_key(&ready.pod_uid),
                "shutdown barrier received foreign, duplicate or premature node authority"
            );
            self.ready.insert(ready.pod_uid, stream);
        }
        if self.ready.len() == cohort.bindings.len() {
            ensure!(
                deleting.len() == cohort.bindings.len(),
                "shutdown requires every original Pod deletion"
            );
            // Kubelet termination is independently scheduled. Release the original
            // runners together so one cannot mistake another's normal exit for loss.
            for stream in self.ready.values_mut() {
                stream.write_all(b"stop\n").await?;
            }
            self.released = true;
            println!(
                "{}",
                serde_json::json!({"event": "coordinated_node_shutdown"})
            );
        }
        Ok(())
    }
}

pub(crate) async fn await_originals(config: &Config) -> Result<()> {
    let endpoint = SocketAddrV4::new(*config.fm.address.ip(), PORT);
    tokio::time::timeout(Duration::from_secs(10), async {
        let mut stream = TcpStream::connect(endpoint).await?;
        let ready = Ready {
            deployment: config.deployment.clone(),
            pod_uid: config
                .run_directory
                .file_name()
                .and_then(|value| value.to_str())
                .context("missing original Pod UID")?
                .into(),
            node_id: config.node.identity.id.clone(),
        };
        let mut request = serde_json::to_vec(&ready)?;
        request.push(b'\n');
        stream.write_all(&request).await?;
        let mut response = String::new();
        BufReader::new(stream.take(16))
            .read_line(&mut response)
            .await?;
        ensure!(
            response == "stop\n",
            "shutdown coordinator did not release the original runners"
        );
        Ok(())
    })
    .await
    .context("waiting for all original kubelet termination signals")?
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn cohort() -> FrozenCohort {
        let set: kuberic_controller::KubericSet = serde_json::from_str(include_str!(
            "../../kuberic-controller/examples/kubericset.json"
        ))
        .unwrap();
        serde_json::from_value(json!({
            "allocation": {
                "resourceUid": "set", "resourceName": "sample", "namespace": "test",
                "generation": 1, "deployment": "deployment", "partition": "partition",
                "fm": {"id": "fm", "instance": 1, "address": "127.0.0.1:19000", "generation": 1},
                "spec": set.spec,
                "nodes": (0..3).map(|index| json!({
                    "nodeId": format!("node-{index}"), "replicaId": index,
                    "podName": format!("pod-{index}"), "configMapName": format!("map-{index}")
                })).collect::<Vec<_>>()
            },
            "bindings": (0..3).map(|index| json!({
                "podUid": format!("uid-{index}"), "configMapUid": format!("map-{index}"),
                "ip": "127.0.0.1", "controlPort": 19001
            })).collect::<Vec<_>>()
        }))
        .unwrap()
    }

    async fn coordinator() -> Shutdown {
        Shutdown {
            listener: TcpListener::bind("127.0.0.1:0").await.unwrap(),
            ready: BTreeMap::new(),
            released: false,
        }
    }

    async fn request(shutdown: &Shutdown, deployment: &str, index: usize) -> TcpStream {
        let mut stream = TcpStream::connect(shutdown.listener.local_addr().unwrap())
            .await
            .unwrap();
        let mut request = serde_json::to_vec(&Ready {
            deployment: deployment.into(),
            pod_uid: format!("uid-{index}"),
            node_id: format!("node-{index}"),
        })
        .unwrap();
        request.push(b'\n');
        stream.write_all(&request).await.unwrap();
        stream
    }

    #[tokio::test]
    async fn original_runners_wait_for_every_kubelet_termination_request() {
        let cohort = cohort();
        let mut shutdown = coordinator().await;
        let deleting = cohort
            .bindings
            .iter()
            .map(|binding| binding.pod_uid.clone())
            .collect();
        let mut streams = vec![
            request(&shutdown, "deployment", 0).await,
            request(&shutdown, "deployment", 1).await,
        ];
        shutdown.observe(&cohort, &deleting).await.unwrap();
        assert!(!shutdown.released);
        for stream in &streams {
            assert_eq!(
                stream.try_read(&mut [0]).unwrap_err().kind(),
                std::io::ErrorKind::WouldBlock
            );
        }
        streams.push(request(&shutdown, "deployment", 2).await);
        shutdown.observe(&cohort, &deleting).await.unwrap();
        assert!(shutdown.released);
        for stream in streams {
            let mut response = String::new();
            tokio::time::timeout(
                Duration::from_secs(1),
                BufReader::new(stream).read_line(&mut response),
            )
            .await
            .unwrap()
            .unwrap();
            assert_eq!(response, "stop\n");
        }
    }

    #[tokio::test]
    async fn foreign_or_premature_shutdown_requests_cannot_release_originals() {
        let cohort = cohort();
        let deleting = cohort
            .bindings
            .iter()
            .map(|binding| binding.pod_uid.clone())
            .collect();
        for (deployment, deleting) in [("foreign", deleting), ("deployment", BTreeSet::new())] {
            let mut shutdown = coordinator().await;
            let _stream = request(&shutdown, deployment, 0).await;
            assert!(shutdown.observe(&cohort, &deleting).await.is_err());
            assert!(!shutdown.released);
        }
    }
}
