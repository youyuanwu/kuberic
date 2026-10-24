use std::{collections::BTreeMap, io, path::PathBuf, sync::Arc, time::Duration};

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UnixStream};
use tokio::task::{JoinHandle, JoinSet};
use tokio_util::sync::CancellationToken;

use super::application_error;
use kuberic_runtime::Result;

pub(super) fn random_bytes() -> Result<[u8; 32]> {
    let mut random = [0; 32];
    let mut filled = 0;
    while filled < random.len() {
        filled +=
            rustix::rand::getrandom(&mut random[filled..], rustix::rand::GetRandomFlags::empty())
                .map_err(application_error)?;
    }
    Ok(random)
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NativeCredential {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub name: String,
}

impl NativeCredential {
    pub fn conninfo(&self) -> Result<String> {
        if self.host.parse::<std::net::IpAddr>().is_err()
            || self.port == 0
            || self.user.is_empty()
            || self.name.is_empty()
            || self.name.len() > 63
            || !self
                .name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        {
            return Err(application_error("invalid public native connection"));
        }
        Ok(format!(
            "host={} port={} user={} application_name={} sslmode=disable connect_timeout=5",
            self.host, self.port, self.user, self.name
        ))
    }
}

pub(super) struct NativeGateway {
    cancellation: CancellationToken,
    task: Option<JoinHandle<Result<()>>>,
    copy_allowed: Arc<std::sync::atomic::AtomicBool>,
    running: Arc<std::sync::atomic::AtomicBool>,
    #[cfg(all(test, feature = "testing"))]
    exit_gate: Arc<std::sync::Mutex<Option<Arc<GatewayExitGate>>>>,
}

struct GatewayRun(Arc<std::sync::atomic::AtomicBool>);
impl Drop for GatewayRun {
    fn drop(&mut self) {
        self.0.store(false, std::sync::atomic::Ordering::Release);
    }
}

pub(super) type GatewayValidity = Arc<
    dyn Fn() -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send>> + Send + Sync,
>;

#[cfg(all(test, feature = "testing"))]
pub(super) struct GatewayExitGate {
    pub entered: tokio::sync::Notify,
    pub release: tokio::sync::Notify,
}

impl Drop for NativeGateway {
    fn drop(&mut self) {
        self.cancellation.cancel();
    }
}

impl NativeGateway {
    pub async fn start(
        host: &str,
        socket: PathBuf,
        name: String,
        slot: String,
        build: bool,
        valid: GatewayValidity,
        cancellation: CancellationToken,
    ) -> Result<(Self, NativeCredential)> {
        let credential = NativeCredential {
            host: host.into(),
            port: 0,
            user: whoami::username().unwrap_or_else(|_| "postgres".into()),
            name,
        };
        let listener = TcpListener::bind((host, 0))
            .await
            .map_err(application_error)?;
        let credential = NativeCredential {
            port: listener.local_addr().map_err(application_error)?.port(),
            ..credential
        };
        let stopped = cancellation.clone();
        let expected = credential.clone();
        let copy_allowed = Arc::new(std::sync::atomic::AtomicBool::new(build));
        let command_mode = copy_allowed.clone();
        let running = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let run = running.clone();
        #[cfg(all(test, feature = "testing"))]
        let exit_gate = Arc::new(std::sync::Mutex::new(None::<Arc<GatewayExitGate>>));
        #[cfg(all(test, feature = "testing"))]
        let exiting = exit_gate.clone();
        let task = tokio::spawn(async move {
            let _run = GatewayRun(run.clone());
            let mut connections = JoinSet::new();
            let mut failures = Vec::new();
            let mut tick = tokio::time::interval(Duration::from_millis(20));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    biased;
                    _ = stopped.cancelled() => break,
                    joined = connections.join_next(), if !connections.is_empty() => {
                        if let Some(Err(error)) = joined {
                            failures.push(error.to_string());
                            break;
                        }
                    },
                    accepted = listener.accept() => {
                        let Ok((client, _)) = accepted else { break; };
                        if connections.len() >= 4 || !valid().await {
                            continue;
                        }
                        let expected = expected.clone();
                        let socket = socket.clone();
                        let slot = slot.clone();
                        let command_mode = command_mode.clone();
                        connections.spawn(async move {
                            if let Err(error) = connection(client, socket, expected, slot, command_mode).await {
                                tracing::debug!(%error, "native gateway connection ended");
                            }
                        });
                    },
                    _ = tick.tick() => if !valid().await { break; },
                }
            }
            // Dropping a listener alone does not fence already-open WAL senders.
            run.store(false, std::sync::atomic::Ordering::Release);
            drop(listener);
            connections.abort_all();
            while let Some(result) = connections.join_next().await {
                if let Err(error) = result
                    && !error.is_cancelled()
                {
                    failures.push(error.to_string());
                }
            }
            #[cfg(all(test, feature = "testing"))]
            {
                let gate = exiting.lock().unwrap().take();
                if let Some(gate) = gate {
                    gate.entered.notify_one();
                    gate.release.notified().await;
                }
            }
            if failures.is_empty() {
                Ok(())
            } else {
                Err(application_error(failures.join("; ")))
            }
        });
        Ok((
            Self {
                cancellation,
                task: Some(task),
                copy_allowed,
                running,
                #[cfg(all(test, feature = "testing"))]
                exit_gate,
            },
            credential,
        ))
    }

    pub(super) fn finish_copy(&self) {
        self.copy_allowed
            .store(false, std::sync::atomic::Ordering::Release);
    }

    pub(super) fn liveness(&self) -> Arc<std::sync::atomic::AtomicBool> {
        self.running.clone()
    }

    #[cfg(all(test, feature = "testing"))]
    pub fn fail_task(&self) {
        self.task.as_ref().unwrap().abort();
    }

    #[cfg(all(test, feature = "testing"))]
    pub fn task_abort_handle(&self) -> tokio::task::AbortHandle {
        self.task.as_ref().unwrap().abort_handle()
    }

    #[cfg(all(test, feature = "testing"))]
    pub fn hold_exit(&self) -> Arc<GatewayExitGate> {
        let gate = Arc::new(GatewayExitGate {
            entered: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
        });
        *self.exit_gate.lock().unwrap() = Some(gate.clone());
        gate
    }

    pub async fn drain(mut self) -> Result<()> {
        self.cancellation.cancel();
        if let Some(task) = self.task.take() {
            task.await.map_err(application_error)??;
        }
        Ok(())
    }
}

async fn packet<R: AsyncRead + Unpin>(reader: &mut R, maximum: usize) -> io::Result<Vec<u8>> {
    let len = reader.read_u32().await? as usize;
    if !(4..=maximum).contains(&len) {
        return Err(io::Error::other("invalid PostgreSQL message length"));
    }
    let mut body = vec![0; len - 4];
    reader.read_exact(&mut body).await?;
    Ok(body)
}

async fn frame<W: AsyncWrite + Unpin>(writer: &mut W, kind: u8, body: &[u8]) -> io::Result<()> {
    writer.write_u8(kind).await?;
    writer.write_u32((body.len() + 4) as u32).await?;
    writer.write_all(body).await?;
    writer.flush().await
}

async fn connection(
    mut client: TcpStream,
    socket: PathBuf,
    expected: NativeCredential,
    slot: String,
    copy_allowed: Arc<std::sync::atomic::AtomicBool>,
) -> io::Result<()> {
    let mut backend = tokio::time::timeout(Duration::from_secs(5), async {
        let mut startup = packet(&mut client, 8192).await?;
        if startup == 80877103u32.to_be_bytes() {
            client.write_all(b"N").await?;
            startup = packet(&mut client, 8192).await?;
        }
        if startup.len() < 5 || startup[..4] != 196608u32.to_be_bytes() {
            return Err(io::Error::other("physical replication startup required"));
        }
        let text = std::str::from_utf8(&startup[4..])
            .map_err(|_| io::Error::other("invalid startup encoding"))?;
        let mut fields = BTreeMap::new();
        let mut values = text.split('\0');
        while let Some(key) = values.next() {
            if key.is_empty() {
                break;
            }
            let value = values
                .next()
                .ok_or_else(|| io::Error::other("missing startup value"))?;
            if fields.insert(key, value).is_some() {
                return Err(io::Error::other("duplicate startup value"));
            }
        }
        if fields.get("user") != Some(&expected.user.as_str())
            || fields.get("application_name") != Some(&expected.name.as_str())
            || fields.get("replication") != Some(&"true")
        {
            return Err(io::Error::other("native lifecycle binding differs"));
        }
        // Backend access is Unix-only. The peer never receives backend login
        // credentials or a socket capable of changing its ACK application name.
        let mut backend = UnixStream::connect(socket).await?;
        let mut startup = 196608u32.to_be_bytes().to_vec();
        startup.extend_from_slice(
            format!(
                "user\0{}\0replication\0true\0application_name\0{}\0\0",
                expected.user, expected.name
            )
            .as_bytes(),
        );
        backend.write_u32((startup.len() + 4) as u32).await?;
        backend.write_all(&startup).await?;
        Ok::<_, io::Error>(backend)
    })
    .await
    .map_err(|_| io::Error::other("native startup timed out"))??;
    let (mut client_read, mut client_write) = client.split();
    let (mut backend_read, mut backend_write) = backend.split();
    let incoming = async {
        loop {
            let kind = client_read.read_u8().await?;
            let body = packet(&mut client_read, 65536).await?;
            match kind {
                b'Q' if allowed_query(
                    &body,
                    &slot,
                    copy_allowed.load(std::sync::atomic::Ordering::Acquire),
                ) => {}
                b'd' if matches!(body.first(), Some(b'r' | b'h')) && body.len() <= 64 => {}
                b'c' | b'X' if body.is_empty() => {}
                _ => return Err(io::Error::other("unsupported native replication command")),
            }
            frame(&mut backend_write, kind, &body).await?;
        }
        #[allow(unreachable_code)]
        Ok::<(), io::Error>(())
    };
    tokio::select! {
        result = incoming => result,
        result = tokio::io::copy(&mut backend_read, &mut client_write) => result.map(|_| ()),
    }
}

fn allowed_query(body: &[u8], slot: &str, build: bool) -> bool {
    let Some(body) = body.strip_suffix(&[0]) else {
        return false;
    };
    let Ok(query) = std::str::from_utf8(body) else {
        return false;
    };
    if query.contains([';', '\0', '\n', '\r']) {
        return false;
    }
    query == "IDENTIFY_SYSTEM"
        || query
            .strip_prefix("TIMELINE_HISTORY ")
            .is_some_and(|id| !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit()))
        || (query.starts_with("START_REPLICATION ")
            && !query.contains("LOGICAL")
            && (build || query.starts_with(&format!("START_REPLICATION SLOT \"{slot}\" "))))
        || (build
            && (matches!(query, "SHOW data_directory_mode" | "SHOW wal_segment_size")
                || query.starts_with("BASE_BACKUP ")
                || query.starts_with("CREATE_REPLICATION_SLOT ")
                    && (query.ends_with(" TEMPORARY PHYSICAL")
                        || query.ends_with(" TEMPORARY PHYSICAL ( RESERVE_WAL)"))))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peer_gateway_accepts_only_the_authorized_physical_slot() {
        let slot = "kuberic_0123456789abcdef0123456789abcdef";
        assert!(allowed_query(
            format!("START_REPLICATION SLOT \"{slot}\" 0/0 TIMELINE 1\0").as_bytes(),
            slot,
            false,
        ));
        assert!(!allowed_query(
            b"START_REPLICATION SLOT \"kuberic_ffffffffffffffffffffffffffffffff\" 0/0 TIMELINE 1\0",
            slot,
            false,
        ));
        assert!(allowed_query(
            b"START_REPLICATION SLOT \"pg_basebackup_1\" 0/0 TIMELINE 1\0",
            slot,
            true,
        ));
    }
}
