use super::*;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::Path;

use super::rewind_protocol::{MAX_FRAME_BYTES, RewindProtocol};

pub(super) fn check_candidate_layout(root: PathBuf) -> io::Result<()> {
    RewindProtocol::new(root.clone())?;
    let pinned = std::fs::symlink_metadata(&root)?;
    let mut directories = vec![std::fs::read_dir(&root)?];
    let mut count = 0;
    while let Some(directory) = directories.last_mut() {
        let Some(entry) = directory.next() else {
            directories.pop();
            continue;
        };
        count += 1;
        let path = entry?.path();
        let metadata = std::fs::symlink_metadata(&path)?;
        if count > 1_000_000 || metadata.file_type().is_symlink() {
            return Err(io::Error::other("unsafe rewind candidate layout"));
        }
        if metadata.is_dir() {
            if directories.len() >= 64 {
                return Err(io::Error::other("rewind candidate tree too deep"));
            }
            directories.push(std::fs::read_dir(path)?);
        } else if !metadata.is_file() || metadata.nlink() != 1 {
            return Err(io::Error::other(
                "rewind candidate has linked/special files",
            ));
        }
    }
    RewindProtocol::new(root.clone())?;
    let current = std::fs::symlink_metadata(root)?;
    if current.dev() != pinned.dev() || current.ino() != pinned.ino() {
        return Err(io::Error::other("rewind candidate root changed"));
    }
    Ok(())
}

pub(super) struct RewindSource {
    root: PathBuf,
    device: u64,
    inode: u64,
    socket_device: u64,
    socket_inode: u64,
}

impl RewindSource {
    pub(super) fn new(root: PathBuf, socket: &Path) -> io::Result<Self> {
        RewindProtocol::new(root.clone())?;
        if socket.parent() != Some(root.join("pg_stat_tmp").as_path())
            || !socket
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| {
                    n.strip_prefix(".s.PGSQL.").is_some_and(|port| {
                        port.parse::<u16>().is_ok_and(|port| port != 0)
                            && port.bytes().all(|b| b.is_ascii_digit())
                    })
                })
        {
            return Err(io::Error::other("rewind socket is not source owned"));
        }
        let root_metadata = std::fs::symlink_metadata(&root)?;
        let socket_metadata = std::fs::symlink_metadata(socket)?;
        let result = Self {
            root,
            device: root_metadata.dev(),
            inode: root_metadata.ino(),
            socket_device: socket_metadata.dev(),
            socket_inode: socket_metadata.ino(),
        };
        result.check(socket)?;
        Ok(result)
    }

    fn check(&self, socket: &Path) -> io::Result<()> {
        let root = std::fs::symlink_metadata(&self.root)?;
        let endpoint = std::fs::symlink_metadata(socket)?;
        if !root.is_dir()
            || root.dev() != self.device
            || root.ino() != self.inode
            || !endpoint.file_type().is_socket()
            || endpoint.dev() != self.socket_device
            || endpoint.ino() != self.socket_inode
            || std::fs::canonicalize(socket.parent().unwrap())? != self.root.join("pg_stat_tmp")
        {
            return Err(io::Error::other("rewind source root/socket changed"));
        }
        Ok(())
    }
}

pub(super) async fn lease_valid(valid: &GatewayValidity, cancellation: &CancellationToken) -> bool {
    tokio::select! {
        biased;
        _ = cancellation.cancelled() => false,
        result = tokio::time::timeout(Duration::from_secs(5), valid()) => result.unwrap_or(false),
    }
}

async fn require_lease(
    valid: &GatewayValidity,
    cancellation: &CancellationToken,
) -> io::Result<()> {
    if lease_valid(valid, cancellation).await {
        Ok(())
    } else {
        Err(io::Error::other("rewind lease revoked"))
    }
}

fn startup(body: &[u8], expected: &NativeCredential) -> io::Result<()> {
    if body.len() < 5 || body[..4] != 196608u32.to_be_bytes() {
        return Err(io::Error::other("normal rewind startup required"));
    }
    let text = std::str::from_utf8(&body[4..])
        .map_err(|_| io::Error::other("invalid rewind startup encoding"))?;
    let mut fields = BTreeMap::new();
    let mut values = text.split('\0');
    loop {
        let key = values
            .next()
            .ok_or_else(|| io::Error::other("missing startup terminator"))?;
        if key.is_empty() {
            // Exactly one final key terminator: no ignored fields or trailing bytes.
            if values.next() != Some("") || values.next().is_some() {
                return Err(io::Error::other("trailing rewind startup bytes"));
            }
            break;
        }
        let value = values
            .next()
            .ok_or_else(|| io::Error::other("missing startup value"))?;
        if fields.len() >= 3 || fields.insert(key, value).is_some() {
            return Err(io::Error::other("unsupported rewind startup fields"));
        }
    }
    if fields.len() != 3
        || fields.get("user") != Some(&expected.user.as_str())
        || fields.get("database") != Some(&"postgres")
        || fields.get("application_name") != Some(&expected.name.as_str())
        || expected.mode != NativeConnectionMode::RewindRead
    {
        return Err(io::Error::other("rewind startup identity differs"));
    }
    Ok(())
}

pub(super) async fn connection(
    mut client: TcpStream,
    socket: PathBuf,
    expected: NativeCredential,
    source: Arc<RewindSource>,
    valid: GatewayValidity,
    cancellation: CancellationToken,
) -> io::Result<()> {
    let mut protocol = RewindProtocol::new(source.root.clone())?;
    source.check(&socket)?;
    let mut backend = tokio::time::timeout(Duration::from_secs(5), async {
        let mut body = packet(&mut client, 8192).await?;
        if body == 80877103u32.to_be_bytes() {
            client.write_all(b"N").await?;
            body = packet(&mut client, 8192).await?;
        }
        startup(&body, &expected)?;
        require_lease(&valid, &cancellation).await?;
        frame(&mut client, b'R', &3u32.to_be_bytes()).await?;
        if client.read_u8().await? != b'p' {
            return Err(io::Error::other("rewind authentication required"));
        }
        let password = packet(&mut client, 128).await?;
        if password.len() != expected.secret.len() + 1
            || password.last() != Some(&0)
            || password[..password.len() - 1]
                .iter()
                .zip(expected.secret.bytes())
                .fold(0u8, |different, (actual, expected)| {
                    different | (*actual ^ expected)
                })
                != 0
        {
            return Err(io::Error::other("rewind authentication rejected"));
        }
        require_lease(&valid, &cancellation).await?;
        source.check(&socket)?;
        let mut backend = UnixStream::connect(&socket).await?;
        require_lease(&valid, &cancellation).await?;
        source.check(&socket)?;
        let mut body = 196608u32.to_be_bytes().to_vec();
        body.extend_from_slice(
            format!(
                "user\0kuberic_rewind\0database\0postgres\0application_name\0{}\0\0",
                expected.name,
            )
            .as_bytes(),
        );
        backend.write_u32((body.len() + 4) as u32).await?;
        backend.write_all(&body).await?;
        let kind = backend.read_u8().await?;
        let body = packet(&mut backend, MAX_FRAME_BYTES - 1).await?;
        if kind != b'R' || body != 0u32.to_be_bytes() {
            return Err(io::Error::other(
                "rewind peer backend authentication failed",
            ));
        }
        require_lease(&valid, &cancellation).await?;
        frame(&mut client, kind, &body).await?;
        Ok::<_, io::Error>(backend)
    })
    .await
    .map_err(|_| io::Error::other("rewind startup timed out"))??;
    let (mut client_read, mut client_write) = client.split();
    let (mut backend_read, mut backend_write) = backend.split();
    let incoming = async {
        loop {
            let kind = client_read.read_u8().await?;
            let body = packet(&mut client_read, MAX_FRAME_BYTES - 1).await?;
            source.check(&socket)?;
            #[cfg(test)]
            if matches!(kind, b'Q' | b'P') {
                eprintln!(
                    "rewind component received {}: {}",
                    char::from(kind),
                    String::from_utf8_lossy(&body).replace('\0', "<NUL>")
                );
            }
            protocol.validate(kind, &body)?;
            require_lease(&valid, &cancellation).await?;
            #[cfg(test)]
            if matches!(kind, b'B' | b'D' | b'E' | b'S') {
                eprintln!(
                    "rewind component forwarding {} body_bytes={}",
                    char::from(kind),
                    body.len()
                );
            }
            frame(&mut backend_write, kind, &body).await?;
            if kind == b'X' {
                return Ok::<(), io::Error>(());
            }
        }
    };
    let outgoing = async {
        loop {
            let kind = backend_read.read_u8().await?;
            let body = packet(&mut backend_read, MAX_FRAME_BYTES - 1).await?;
            require_lease(&valid, &cancellation).await?;
            frame(&mut client_write, kind, &body).await?;
            if kind == b'E' {
                return Err(io::Error::other("rewind backend rejected command"));
            }
        }
    };
    tokio::select! {
        result = incoming => result,
        result = outgoing => result,
        _ = cancellation.cancelled() => Err(io::Error::other("rewind cancelled")),
    }
}
