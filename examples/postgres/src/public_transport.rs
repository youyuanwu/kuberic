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
    pub name: String,
    pub secret: String,
}

impl NativeCredential {
    pub fn conninfo(&self) -> Result<String> {
        if self.host.parse::<std::net::IpAddr>().is_err()
            || self.port == 0
            || self.name.is_empty()
            || self.name.len() > 63
            || !self
                .name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_')
            || self.secret.len() != 64
            || !self.secret.bytes().all(|b| b.is_ascii_hexdigit())
        {
            return Err(application_error("invalid signed native connection"));
        }
        Ok(format!(
            "host={} port={} user={} password={} application_name={} sslmode=disable connect_timeout=5",
            self.host, self.port, self.name, self.secret, self.name
        ))
    }
}

pub(super) struct NativeGateway {
    cancellation: CancellationToken,
    task: Option<JoinHandle<()>>,
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
        valid: Arc<dyn Fn() -> bool + Send + Sync>,
    ) -> Result<(Self, NativeCredential)> {
        let random = random_bytes()?;
        let credential = NativeCredential {
            host: host.into(),
            port: 0,
            name,
            secret: random.iter().map(|byte| format!("{byte:02x}")).collect(),
        };
        let listener = TcpListener::bind((host, 0))
            .await
            .map_err(application_error)?;
        let credential = NativeCredential {
            port: listener.local_addr().map_err(application_error)?.port(),
            ..credential
        };
        let cancellation = CancellationToken::new();
        let stopped = cancellation.clone();
        let expected = credential.clone();
        let task = tokio::spawn(async move {
            let mut connections = JoinSet::new();
            let mut tick = tokio::time::interval(Duration::from_millis(20));
            loop {
                tokio::select! {
                    biased;
                    _ = stopped.cancelled() => break,
                    _ = tick.tick() => if !valid() { break; },
                    _ = connections.join_next(), if !connections.is_empty() => {},
                    accepted = listener.accept() => {
                        let Ok((client, _)) = accepted else { break; };
                        if connections.len() >= 4 || !valid() {
                            continue;
                        }
                        let expected = expected.clone();
                        let socket = socket.clone();
                        let slot = slot.clone();
                        connections.spawn(async move {
                            if let Err(error) = connection(client, socket, expected, slot, build).await {
                                tracing::debug!(%error, "native gateway connection ended");
                            }
                        });
                    }
                }
            }
            // Dropping a listener alone does not fence already-open WAL senders.
            drop(listener);
            connections.abort_all();
            while connections.join_next().await.is_some() {}
        });
        Ok((
            Self {
                cancellation,
                task: Some(task),
            },
            credential,
        ))
    }

    pub async fn drain(mut self) -> Result<()> {
        self.cancellation.cancel();
        if let Some(task) = self.task.take() {
            task.await.map_err(application_error)?;
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
    build: bool,
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
        if fields.get("user") != Some(&expected.name.as_str())
            || fields.get("application_name") != Some(&expected.name.as_str())
            || fields.get("replication") != Some(&"true")
        {
            return Err(io::Error::other("native authorization binding differs"));
        }
        frame(&mut client, b'R', &3u32.to_be_bytes()).await?;
        if client.read_u8().await? != b'p' {
            return Err(io::Error::other("native credential required"));
        }
        let password = packet(&mut client, 128).await?;
        let wanted = format!("{}\0", expected.secret);
        if password.len() != wanted.len()
            || password
                .iter()
                .zip(wanted.bytes())
                .fold(0, |diff, (a, b)| diff | (a ^ b))
                != 0
        {
            return Err(io::Error::other("invalid native credential"));
        }
        // Backend access is Unix-only. The peer never receives backend login
        // credentials or a socket capable of changing its ACK application name.
        let mut backend = UnixStream::connect(socket).await?;
        let user = whoami::username().unwrap_or_else(|_| "postgres".into());
        let mut startup = 196608u32.to_be_bytes().to_vec();
        startup.extend_from_slice(
            format!(
                "user\0{user}\0replication\0true\0application_name\0{}\0\0",
                expected.name
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
                b'Q' if allowed_query(&body, &slot, build) => {}
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
