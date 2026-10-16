use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio::task::{JoinHandle, JoinSet};

enum Command {
    Cut(bool, oneshot::Sender<()>),
    Stop,
}

struct Proxy {
    address: String,
    commands: mpsc::Sender<Command>,
    task: Option<JoinHandle<Result<()>>>,
}

impl Proxy {
    async fn new(backend: String, blocked: bool) -> Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = format!("http://{}", listener.local_addr()?);
        let (commands, mut receiver) = mpsc::channel(4);
        let task = tokio::spawn(async move {
            let mut streams = JoinSet::new();
            let mut blocked = blocked;
            loop {
                tokio::select! {
                    biased;
                    command = receiver.recv() => match command {
                        Some(Command::Cut(cut, ack)) => {
                            if cut == blocked {
                                let _ = ack.send(());
                                continue;
                            }
                            blocked = cut;
                            if cut {
                                streams.abort_all();
                                while let Some(joined) = streams.join_next().await {
                                    if let Err(error) = joined { ensure!(error.is_cancelled(), "forwarder panicked: {error}"); }
                                }
                            }
                            let _ = ack.send(());
                        }
                        Some(Command::Stop) | None => break,
                    },
                    accepted = listener.accept() => {
                        let (mut incoming, _) = accepted?;
                        if blocked { drop(incoming); continue; }
                        let backend = backend.clone();
                        streams.spawn(async move {
                            if let Ok(mut outgoing) = TcpStream::connect(backend).await {
                                // Resets and EOF are observable transport failures, not protocol replies.
                                let _ = tokio::io::copy_bidirectional(&mut incoming, &mut outgoing).await;
                            }
                        });
                    },
                    joined = streams.join_next(), if !streams.is_empty() => {
                        if let Some(Err(error)) = joined { return Err(error.into()); }
                    }
                }
            }
            streams.abort_all();
            while let Some(joined) = streams.join_next().await {
                if let Err(error) = joined {
                    ensure!(error.is_cancelled(), "forwarder panicked: {error}");
                }
            }
            Ok(())
        });
        Ok(Self {
            address,
            commands,
            task: Some(task),
        })
    }

    async fn cut(&self, blocked: bool) -> Result<()> {
        let (ack, received) = oneshot::channel();
        self.commands.send(Command::Cut(blocked, ack)).await?;
        tokio::time::timeout(Duration::from_secs(5), received).await??;
        Ok(())
    }

    async fn finish(mut self) -> Result<()> {
        self.commands.send(Command::Stop).await?;
        let mut task = self.task.take().unwrap();
        match tokio::time::timeout(Duration::from_secs(5), &mut task).await {
            Ok(result) => result??,
            Err(error) => {
                task.abort();
                let _ = task.await;
                return Err(error.into());
            }
        }
        Ok(())
    }
}

impl Drop for Proxy {
    fn drop(&mut self) {
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}

#[derive(Default)]
struct State {
    owners: BTreeMap<String, String>,
    isolated: BTreeSet<String>,
    proxies: BTreeMap<(String, String), Proxy>,
    closed: bool,
    isolate_on_dial: Option<String>,
}

#[derive(Clone, Default)]
pub(super) struct Network(Arc<Mutex<State>>);

fn endpoint_key(endpoint: &str) -> String {
    endpoint.trim_end_matches('/').to_owned()
}

impl Network {
    pub async fn isolate_on_next_dial(&self, uid: &str) {
        self.0.lock().await.isolate_on_dial = Some(uid.into());
    }

    pub async fn route(&self, source: &str, destination: &str, endpoint: &str) -> Result<String> {
        let mut state = self.0.lock().await;
        ensure!(!state.closed, "test transport closed");
        let endpoint = endpoint_key(endpoint);
        let old = state
            .owners
            .insert(endpoint.clone(), destination.to_owned());
        ensure!(
            old.as_deref().is_none_or(|owner| owner == destination),
            "endpoint reused by another live instance"
        );
        if state
            .isolate_on_dial
            .as_deref()
            .is_some_and(|uid| uid == source || uid == destination)
        {
            let uid = state.isolate_on_dial.take().unwrap();
            state.isolated.insert(uid);
            Self::update_gates(&state).await?;
        }
        let key = (source.to_owned(), endpoint.clone());
        if !state.proxies.contains_key(&key) {
            let uri: http::Uri = endpoint.parse()?;
            ensure!(uri.scheme_str() == Some("http") && uri.host() == Some("127.0.0.1"));
            let backend = uri
                .authority()
                .context("TCP authority missing")?
                .to_string();
            let blocked = state.isolated.contains(source)
                || state
                    .owners
                    .get(&endpoint)
                    .is_some_and(|uid| state.isolated.contains(uid));
            let proxy = Proxy::new(backend, blocked).await?;
            state.proxies.insert(key.clone(), proxy);
        }
        Ok(state.proxies[&key].address.clone())
    }

    pub async fn isolate(&self, uid: &str, isolated: bool) -> Result<()> {
        let mut state = self.0.lock().await;
        if isolated {
            state.isolated.insert(uid.into());
        } else {
            state.isolated.remove(uid);
        }
        Self::update_gates(&state).await
    }

    async fn update_gates(state: &State) -> Result<()> {
        for ((source, endpoint), proxy) in &state.proxies {
            let blocked = state.isolated.contains(source)
                || state
                    .owners
                    .get(endpoint)
                    .is_some_and(|owner| state.isolated.contains(owner));
            proxy.cut(blocked).await?;
        }
        Ok(())
    }

    pub async fn assert_drained(&self) -> Result<()> {
        let state = self.0.lock().await;
        ensure!(
            state.closed && state.proxies.is_empty(),
            "transport owners not drained"
        );
        Ok(())
    }

    pub async fn is_isolated(&self, uid: &str) -> bool {
        self.0.lock().await.isolated.contains(uid)
    }

    pub async fn physical_route(&self, source: &str, endpoint: &str) -> Option<String> {
        self.0
            .lock()
            .await
            .proxies
            .get(&(source.to_owned(), endpoint_key(endpoint)))
            .map(|proxy| proxy.address.clone())
    }

    pub async fn forget(&self, uid: &str) -> Result<()> {
        let retired = {
            let mut state = self.0.lock().await;
            let endpoints = state
                .owners
                .iter()
                .filter(|(_, owner)| owner.as_str() == uid)
                .map(|(endpoint, _)| endpoint.clone())
                .collect::<BTreeSet<_>>();
            state.owners.retain(|_, owner| owner != uid);
            state.isolated.remove(uid);
            let keys = state
                .proxies
                .keys()
                .filter(|(source, endpoint)| source == uid || endpoints.contains(endpoint))
                .cloned()
                .collect::<Vec<_>>();
            keys.into_iter()
                .filter_map(|key| state.proxies.remove(&key))
                .collect::<Vec<_>>()
        };
        for proxy in retired {
            proxy.finish().await?;
        }
        Ok(())
    }

    pub async fn finish(&self) -> Result<()> {
        let proxies = {
            let mut state = self.0.lock().await;
            state.closed = true;
            std::mem::take(&mut state.proxies)
        };
        let mut errors = Vec::new();
        for (_, proxy) in proxies {
            if let Err(error) = proxy.finish().await {
                errors.push(error.to_string());
            }
        }
        ensure!(
            errors.is_empty(),
            "transport cleanup: {}",
            errors.join("; ")
        );
        Ok(())
    }
}

#[tokio::test]
async fn cuts_established_duplex_streams_by_source_and_destination_and_drains() -> Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let backend = TcpListener::bind("127.0.0.1:0").await?;
    let destination = format!("http://{}", backend.local_addr()?);
    let network = Network::default();
    let address = network.route("source", "destination", &destination).await?;
    let survivor_backend = TcpListener::bind("127.0.0.1:0").await?;
    let survivor_endpoint = format!("http://{}", survivor_backend.local_addr()?);
    let survivor_address = network
        .route("survivor-a", "survivor-b", &survivor_endpoint)
        .await?;
    let survivor_uri: http::Uri = survivor_address.parse()?;
    let mut survivor_client =
        TcpStream::connect(survivor_uri.authority().unwrap().as_str()).await?;
    let (mut survivor_server, _) =
        tokio::time::timeout(Duration::from_secs(5), survivor_backend.accept()).await??;
    for cut in ["source", "destination"] {
        let uri: http::Uri = address.parse()?;
        let mut client = TcpStream::connect(uri.authority().unwrap().as_str()).await?;
        let (mut server, _) =
            tokio::time::timeout(Duration::from_secs(5), backend.accept()).await??;
        client.write_all(b"a").await?;
        ensure!(server.read_u8().await? == b'a');
        server.write_all(b"b").await?;
        ensure!(client.read_u8().await? == b'b');
        network.isolate(cut, true).await?;
        network.isolate(cut, true).await?;
        survivor_client.write_all(b"s").await?;
        ensure!(survivor_server.read_u8().await? == b's');
        ensure!(
            tokio::time::timeout(Duration::from_secs(5), client.read_u8())
                .await?
                .is_err()
        );
        ensure!(
            tokio::time::timeout(Duration::from_secs(5), server.read_u8())
                .await?
                .is_err()
        );
        let mut fresh = TcpStream::connect(uri.authority().unwrap().as_str()).await?;
        ensure!(
            tokio::time::timeout(Duration::from_secs(5), fresh.read_u8())
                .await?
                .is_err()
        );
        network.isolate(cut, false).await?;
        network.isolate(cut, false).await?;
        survivor_server.write_all(b"h").await?;
        ensure!(survivor_client.read_u8().await? == b'h');
    }
    let uri: http::Uri = address.parse()?;
    let mut client = TcpStream::connect(uri.authority().unwrap().as_str()).await?;
    let (mut server, _) = tokio::time::timeout(Duration::from_secs(5), backend.accept()).await??;
    client.write_all(b"r").await?;
    ensure!(server.read_u8().await? == b'r');
    network.finish().await?;
    ensure!(
        tokio::time::timeout(Duration::from_secs(5), client.read_u8())
            .await?
            .is_err()
    );
    ensure!(
        tokio::time::timeout(Duration::from_secs(5), server.read_u8())
            .await?
            .is_err()
    );
    ensure!(
        network
            .route("source", "destination", &destination)
            .await
            .is_err()
    );
    Ok(())
}
