use super::*;
use crate::access::initialize_application_role;
use crate::config::PgConfig;
use crate::durable::{PgDurableIdentity, PgDurableStore, StorageMode};
use crate::instance::PgInstanceManager;
use crate::testing::{ProcessProbe, TestDataDir, allocate_port, find_pg_bin};
use kuberic_runtime::protocol::types::{
    AgentGeneration, ReplicaId, ReplicaIdentity, ReplicaInstanceId, ResourceUid,
};
use sha2::{Digest, Sha256};
use std::mem::ManuallyDrop;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::net::UnixListener;

fn validity(alive: Arc<AtomicBool>) -> GatewayValidity {
    Arc::new(move || {
        let alive = alive.clone();
        Box::pin(async move { alive.load(Ordering::Acquire) })
    })
}

fn startup_body(credential: &NativeCredential, extra: &str) -> Vec<u8> {
    let mut body = 196608u32.to_be_bytes().to_vec();
    body.extend_from_slice(
        format!(
            "user\0{}\0database\0postgres\0application_name\0{}\0{extra}\0",
            credential.user, credential.name,
        )
        .as_bytes(),
    );
    body
}

async fn send_startup(client: &mut TcpStream, body: &[u8]) {
    client.write_u32((body.len() + 4) as u32).await.unwrap();
    client.write_all(body).await.unwrap();
}

async fn authenticate(credential: &NativeCredential) -> TcpStream {
    let mut client = TcpStream::connect((&*credential.host, credential.port))
        .await
        .unwrap();
    send_startup(&mut client, &startup_body(credential, "")).await;
    assert_eq!(client.read_u8().await.unwrap(), b'R');
    assert_eq!(packet(&mut client, 128).await.unwrap(), 3u32.to_be_bytes());
    let mut password = credential.secret.as_bytes().to_vec();
    password.push(0);
    frame(&mut client, b'p', &password).await.unwrap();
    loop {
        let kind = client.read_u8().await.unwrap();
        let body = packet(&mut client, 8192).await.unwrap();
        assert_ne!(kind, b'E', "backend startup error");
        if kind == b'Z' {
            assert_eq!(body, b"I");
            break;
        }
    }
    client
}

async fn closed(client: &mut TcpStream) {
    let result = tokio::time::timeout(Duration::from_secs(5), client.read_u8())
        .await
        .unwrap();
    assert!(
        result.is_err(),
        "normal connection survived denial/revocation"
    );
}

fn fake_source() -> (ManuallyDrop<TestDataDir>, PathBuf, UnixListener) {
    let root = ManuallyDrop::new(TestDataDir::new("rewind-normal-auth"));
    std::fs::create_dir(root.path().join("pg_stat_tmp")).unwrap();
    let socket = root.path().join("pg_stat_tmp/.s.PGSQL.5432");
    let listener = UnixListener::bind(&socket).unwrap();
    (root, socket, listener)
}

#[tokio::test]
async fn m3_rewind_normal_authentication_and_startup_never_open_backend_on_denial() {
    let (root, socket, backend) = fake_source();
    let (gateway, credential) = NativeGateway::start_rewind(
        "127.0.0.1",
        root.path().into(),
        socket,
        "strict_reader".into(),
        validity(Arc::new(AtomicBool::new(true))),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert!(credential.helper_conninfo().is_err());
    assert!(credential.conninfo().is_err());
    let conninfo = credential.rewind_conninfo().unwrap();
    assert!(conninfo.contains("dbname=postgres"));
    assert!(!conninfo.contains(&credential.secret));
    for extra in [
        "replication\0true\0",
        "replication\0false\0",
        "options\0-c role=postgres\0",
        "client_encoding\0UTF8\0",
        "user\0postgres\0",
        "database\0other\0",
    ] {
        let mut client = TcpStream::connect((&*credential.host, credential.port))
            .await
            .unwrap();
        send_startup(&mut client, &startup_body(&credential, extra)).await;
        closed(&mut client).await;
    }
    let correct = startup_body(&credential, "");
    for bad in [
        [correct.as_slice(), b"trailing"].concat(),
        [correct.as_slice(), b"\0"].concat(),
        correct[..correct.len() - 1].to_vec(),
        80877102u32.to_be_bytes().to_vec(),
        80877104u32.to_be_bytes().to_vec(),
    ] {
        let mut client = TcpStream::connect((&*credential.host, credential.port))
            .await
            .unwrap();
        send_startup(&mut client, &bad).await;
        closed(&mut client).await;
    }
    for field in ["user", "database", "application_name"] {
        let mut bad = credential.clone();
        match field {
            "user" => bad.user = "postgres".into(),
            "application_name" => bad.name = "crossed".into(),
            _ => {}
        }
        let mut body = startup_body(&bad, "");
        if field == "database" {
            body = String::from_utf8(body)
                .unwrap()
                .replace("postgres", "template1")
                .into_bytes();
        }
        let mut client = TcpStream::connect((&*credential.host, credential.port))
            .await
            .unwrap();
        send_startup(&mut client, &body).await;
        closed(&mut client).await;
    }
    for malformed in [false, true] {
        let mut client = TcpStream::connect((&*credential.host, credential.port))
            .await
            .unwrap();
        send_startup(&mut client, &correct).await;
        assert_eq!(client.read_u8().await.unwrap(), b'R');
        assert_eq!(packet(&mut client, 128).await.unwrap(), 3u32.to_be_bytes());
        let mut password = credential.secret.as_bytes().to_vec();
        password[0] = if password[0] == b'a' { b'b' } else { b'a' };
        if !malformed {
            password.push(0);
        }
        frame(&mut client, b'p', &password).await.unwrap();
        closed(&mut client).await;
    }
    let mut oversized = TcpStream::connect((&*credential.host, credential.port))
        .await
        .unwrap();
    oversized.write_u32(8193).await.unwrap();
    closed(&mut oversized).await;
    assert!(
        tokio::time::timeout(Duration::from_millis(100), backend.accept())
            .await
            .is_err()
    );
    gateway.drain().await.unwrap();
    eprintln!("preserved startup-negative root {}", root.path().display());
}

#[tokio::test]
async fn m3_rewind_normal_backend_errors_close_and_lost_gateway_join_is_failure() {
    let (root, socket, backend) = fake_source();
    let (gateway, credential) = NativeGateway::start_rewind(
        "127.0.0.1",
        root.path().into(),
        socket,
        "backend_error".into(),
        validity(Arc::new(AtomicBool::new(true))),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    let server = tokio::spawn(async move {
        let (mut client, _) = backend.accept().await.unwrap();
        let body = packet(&mut client, 8192).await.unwrap();
        let text = std::str::from_utf8(&body[4..]).unwrap();
        assert!(text.contains("user\0kuberic_rewind\0database\0postgres\0"));
        assert!(!text.contains("replication"));
        frame(&mut client, b'R', &0u32.to_be_bytes()).await.unwrap();
        frame(&mut client, b'Z', b"I").await.unwrap();
        assert_eq!(client.read_u8().await.unwrap(), b'Q');
        assert_eq!(
            packet(&mut client, 8192).await.unwrap(),
            b"SHOW full_page_writes\0"
        );
        frame(&mut client, b'E', b"SERROR\0C42501\0Mdenied\0\0")
            .await
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_secs(5), client.read_u8())
                .await
                .unwrap()
                .is_err()
        );
    });
    let mut client = authenticate(&credential).await;
    frame(&mut client, b'Q', b"SHOW full_page_writes\0")
        .await
        .unwrap();
    assert_eq!(client.read_u8().await.unwrap(), b'E');
    packet(&mut client, 8192).await.unwrap();
    closed(&mut client).await;
    server.await.unwrap();
    gateway.fail_task();
    assert!(
        gateway.drain().await.is_err(),
        "lost gateway join became success"
    );
}

fn tree_digest(root: &Path) -> Vec<u8> {
    fn visit(root: &Path, path: &Path, digest: &mut Sha256) {
        let mut entries = std::fs::read_dir(path)
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect::<Vec<_>>();
        entries.sort();
        for path in entries {
            let metadata = std::fs::symlink_metadata(&path).unwrap();
            assert!(!metadata.file_type().is_symlink());
            let relative = path.strip_prefix(root).unwrap().to_str().unwrap();
            digest.update((relative.len() as u64).to_be_bytes());
            digest.update(relative.as_bytes());
            digest.update([u8::from(metadata.is_dir())]);
            if metadata.is_dir() {
                visit(root, &path, digest);
            } else {
                assert!(metadata.is_file());
                digest.update(metadata.len().to_be_bytes());
                digest.update(std::fs::read(path).unwrap());
            }
        }
    }
    let mut digest = Sha256::new();
    visit(root, root, &mut digest);
    digest.finalize().to_vec()
}

fn clone_stopped(from: &Path, to: &Path) {
    std::fs::create_dir(to).unwrap();
    std::fs::set_permissions(to, std::os::unix::fs::PermissionsExt::from_mode(0o700)).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let source = entry.path();
        let target = to.join(entry.file_name());
        let metadata = std::fs::symlink_metadata(&source).unwrap();
        if metadata.is_dir() {
            clone_stopped(&source, &target);
        } else {
            assert!(
                metadata.is_file(),
                "fixture stopped copy must not contain links/sockets"
            );
            std::fs::copy(source, target).unwrap();
        }
    }
}

struct RealFixture {
    _root: ManuallyDrop<TestDataDir>,
    store: Arc<PgDurableStore>,
    retained: PgInstanceManager,
    source: PgInstanceManager,
    candidate: PgInstanceManager,
    nonce: [u8; 16],
    gateway: NativeGateway,
    credential: NativeCredential,
    alive: Arc<AtomicBool>,
    original_digest: Vec<u8>,
}

impl RealFixture {
    async fn new() -> Self {
        let root = ManuallyDrop::new(TestDataDir::new("rewind-real-component"));
        eprintln!(
            "DATA-PLANE/HELPER COMPONENT fixture preserved at {}",
            root.path().display()
        );
        let bins = find_pg_bin();
        let version = std::process::Command::new(bins.join("pg_rewind"))
            .arg("--version")
            .output()
            .unwrap();
        assert!(version.status.success());
        assert!(String::from_utf8_lossy(&version.stdout).contains("16.15"));
        eprintln!("{}", String::from_utf8_lossy(&version.stdout));
        let store = Arc::new(
            PgDurableStore::open(
                root.path().join("metadata"),
                PgDurableIdentity {
                    resource_uid: ResourceUid::new("rewind-component"),
                    replica: ReplicaIdentity {
                        replica_id: ReplicaId::new(1),
                        instance_id: ReplicaInstanceId::new("retained"),
                        agent_generation: AgentGeneration::new("rewind-component"),
                    },
                },
                StorageMode::Fresh,
            )
            .await
            .unwrap(),
        );
        let retained =
            PgInstanceManager::new(root.path().join("r"), bins.clone(), allocate_port().await);
        retained.bind_generation_store(&store).await.unwrap();
        retained.init_db().await.unwrap();
        {
            use std::io::Write;
            let mut config = std::fs::OpenOptions::new()
                .append(true)
                .open(retained.data_dir().join("postgresql.conf"))
                .unwrap();
            writeln!(config, "\nwal_keep_size = '128MB'").unwrap();
        }
        PgConfig::public_replication_fence(retained.data_dir())
            .await
            .unwrap();
        let (faults, _observed) = tokio::sync::mpsc::channel(16);
        retained.start_native(faults.clone()).await.unwrap();
        initialize_application_role(&retained).await.unwrap();
        let (client, driver) = retained.connect().await.unwrap();
        client
            .batch_execute(
                "CREATE TABLE rewind_component(id integer PRIMARY KEY, value text NOT NULL);
             INSERT INTO rewind_component VALUES (1, 'fixture_prefix');
             CHECKPOINT",
            )
            .await
            .unwrap();
        drop(client);
        driver.await.unwrap();
        let alive = Arc::new(AtomicBool::new(true));
        let (physical, credential) = NativeGateway::start(
            "127.0.0.1",
            retained
                .socket_dir()
                .join(format!(".s.PGSQL.{}", retained.port())),
            "component_copy".into(),
            "component_slot".into(),
            true,
            validity(alive.clone()),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        let source = PgInstanceManager::new(
            root.path().join("source"),
            bins.clone(),
            allocate_port().await,
        );
        source.public_base_backup(&credential).await.unwrap();
        physical.drain().await.unwrap();
        std::fs::write(source.data_dir().join("standby.signal"), b"").unwrap();
        PgConfig::new(source.port(), source.data_dir())
            .disconnect_receiver(source.data_dir())
            .await
            .unwrap();
        source.start_native(faults).await.unwrap();
        source.promote().await.unwrap();
        let (old, old_driver) = retained.connect().await.unwrap();
        old.batch_execute(
            "INSERT INTO rewind_component VALUES (2, 'old_unacknowledged_fixture_tail'); CHECKPOINT",
        ).await.unwrap();
        drop(old);
        old_driver.await.unwrap();
        let (new, new_driver) = source.connect().await.unwrap();
        new.batch_execute(
            "INSERT INTO rewind_component VALUES (3, 'new_source_timeline'); CHECKPOINT",
        )
        .await
        .unwrap();
        drop(new);
        new_driver.await.unwrap();
        retained.stop().await.unwrap();
        let retained_identity = retained.clean_primary_shutdown_identity().await.unwrap();
        let source_identity = source.control_identity().await.unwrap();
        assert_eq!(retained_identity.0, source_identity.0);
        assert!(
            source_identity.1 > retained_identity.1,
            "fixture did not actually fork timelines"
        );
        let nonce: [u8; 16] = random_bytes().unwrap()[..16].try_into().unwrap();
        let hex: String = nonce.iter().map(|b| format!("{b:02x}")).collect();
        let path = root.path().join(format!("r-{hex}"));
        retained
            .clone_stopped_repair_candidate(
                &path,
                &nonce,
                validity(alive.clone()),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        store.ensure_helpers_complete().await.unwrap();
        let original_digest = tree_digest(retained.data_dir());
        eprintln!(
            "retained original tree digest {}",
            original_digest
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        );
        assert_eq!(original_digest, tree_digest(&path));
        let candidate = PgInstanceManager::new(path, bins, allocate_port().await);
        candidate.bind_generation_store(&store).await.unwrap();
        candidate.prepare_native_build().await.unwrap();
        let (gateway, credential) = NativeGateway::start_rewind(
            "127.0.0.1",
            source.data_dir().into(),
            source
                .socket_dir()
                .join(format!(".s.PGSQL.{}", source.port())),
            "component_rewind".into(),
            validity(alive.clone()),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        Self {
            _root: root,
            store,
            retained,
            source,
            candidate,
            nonce,
            gateway,
            credential,
            alive,
            original_digest,
        }
    }
}

#[test_log::test(tokio::test)]
async fn m3_rewind_normal_real_pg16_online_helper_preserves_original_and_recovers_candidate_data() {
    let fixture = RealFixture::new().await;
    let mut reader = tokio_postgres::Config::new();
    reader
        .host_path(fixture.source.socket_dir())
        .port(fixture.source.port())
        .user("kuberic_rewind")
        .dbname("postgres");
    let (reader, reader_driver) = reader.connect(tokio_postgres::NoTls).await.unwrap();
    let reader_driver = tokio::spawn(reader_driver);
    let identity = reader
        .query_one(
            "SELECT current_user::text, session_user::text, rolsuper, rolreplication
         FROM pg_roles WHERE rolname=current_user",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(identity.get::<_, String>(0), "kuberic_rewind");
    assert_eq!(identity.get::<_, String>(1), "kuberic_rewind");
    assert!(!identity.get::<_, bool>(2));
    assert!(!identity.get::<_, bool>(3));
    drop(reader);
    reader_driver.await.unwrap().unwrap();
    fixture
        .candidate
        .rewind_owned_candidate(
            &fixture.retained,
            &fixture.nonce,
            &fixture.credential,
            validity(fixture.alive.clone()),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let selected = fixture.store.revalidate().await.unwrap();
    let helper = selected.helper_executions.last().unwrap();
    assert_eq!(
        serde_json::to_value(&helper.launch.intent).unwrap()["candidateNonce"],
        serde_json::json!(fixture.nonce)
    );
    let receipt = helper.completed().unwrap();
    assert!(receipt.dispatched && !receipt.cancelled);
    assert_eq!(receipt.launcher_status, 0);
    assert_eq!(
        fixture.original_digest,
        tree_digest(fixture.retained.data_dir())
    );
    assert!(!fixture.retained.is_running().await);
    let (faults, _observed) = tokio::sync::mpsc::channel(8);
    fixture.candidate.start_native(faults).await.unwrap();
    let (candidate, driver) = fixture.candidate.connect().await.unwrap();
    let rows = candidate
        .query("SELECT id, value FROM rewind_component ORDER BY id", &[])
        .await
        .unwrap();
    assert_eq!(
        rows.iter()
            .map(|r| (r.get::<_, i32>(0), r.get::<_, String>(1)))
            .collect::<Vec<_>>(),
        vec![
            (1, "fixture_prefix".into()),
            (3, "new_source_timeline".into())
        ]
    );
    candidate
        .execute(
            "INSERT INTO rewind_component VALUES (4, 'candidate_only_new_write')",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(
        candidate
            .query_one("SELECT count(*) FROM rewind_component", &[])
            .await
            .unwrap()
            .get::<_, i64>(0),
        3
    );
    drop(candidate);
    driver.await.unwrap();
    fixture.candidate.stop().await.unwrap();
    assert_eq!(
        fixture.original_digest,
        tree_digest(fixture.retained.data_dir())
    );
    let mut held = authenticate(&fixture.credential).await;
    let (source, source_driver) = fixture.source.connect().await.unwrap();
    assert_eq!(source.query_one(
        "SELECT usename::text FROM pg_stat_activity WHERE application_name='component_rewind'", &[],
    ).await.unwrap().get::<_, String>(0), "kuberic_rewind");
    for query in [
        b"SELECT pg_read_binary_file('/etc/passwd')\0".as_slice(),
        b"SET ROLE postgres\0",
        b"CREATE TABLE forbidden(i int)\0",
        b"IDENTIFY_SYSTEM\0",
    ] {
        let mut denied = authenticate(&fixture.credential).await;
        frame(&mut denied, b'Q', query).await.unwrap();
        closed(&mut denied).await;
    }
    let mut malformed_bind = authenticate(&fixture.credential).await;
    frame(
        &mut malformed_bind,
        b'P',
        b"\0SELECT pg_read_binary_file($1)\0\0\0",
    )
    .await
    .unwrap();
    frame(&mut malformed_bind, b'S', b"").await.unwrap();
    loop {
        let kind = malformed_bind.read_u8().await.unwrap();
        packet(&mut malformed_bind, 8192).await.unwrap();
        if kind == b'Z' {
            break;
        }
        assert_eq!(kind, b'1');
    }
    frame(&mut malformed_bind, b'B', b"\0\0\0\0\0\x02")
        .await
        .unwrap();
    closed(&mut malformed_bind).await;
    let mut oversized = authenticate(&fixture.credential).await;
    oversized.write_u8(b'Q').await.unwrap();
    oversized
        .write_u32(rewind_protocol::MAX_FRAME_BYTES as u32)
        .await
        .unwrap();
    closed(&mut oversized).await;
    fixture.alive.store(false, Ordering::Release);
    closed(&mut held).await;
    fixture.gateway.drain().await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if source.query_one(
                "SELECT count(*) FROM pg_stat_activity WHERE application_name='component_rewind'", &[],
            ).await.unwrap().get::<_, i64>(0) == 0 { break; }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }).await.unwrap();
    drop(source);
    source_driver.await.unwrap();
    fixture.source.stop().await.unwrap();
    eprintln!(
        "component only: no selected native session, installation, quorum ACK, readiness or handoff"
    );
}

fn rewind_child(parent: u32) -> Option<(u32, Vec<u8>)> {
    let tasks = std::fs::read_dir(format!("/proc/{parent}/task")).ok()?;
    for task in tasks.flatten() {
        let Ok(children) = std::fs::read_to_string(task.path().join("children")) else {
            continue;
        };
        for pid in children
            .split_whitespace()
            .filter_map(|s| s.parse::<u32>().ok())
        {
            let Ok(command) = std::fs::read(format!("/proc/{pid}/cmdline")) else {
                continue;
            };
            if command
                .split(|b| *b == 0)
                .next()
                .is_some_and(|program| program.ends_with(b"/pg_rewind"))
            {
                return Some((pid, command));
            }
            if let Some(found) = rewind_child(pid) {
                return Some(found);
            }
        }
    }
    None
}

#[test_log::test(tokio::test)]
async fn m3_rewind_normal_real_helper_cancellation_reaps_child_and_candidate_checks_fail_closed() {
    let fixture = RealFixture::new().await;
    let original_candidate = tree_digest(fixture.candidate.data_dir());
    let mut crossed_nonce = fixture.nonce;
    crossed_nonce[0] ^= 1;
    assert!(
        fixture
            .candidate
            .rewind_owned_candidate(
                &fixture.retained,
                &crossed_nonce,
                &fixture.credential,
                validity(fixture.alive.clone()),
                CancellationToken::new(),
            )
            .await
            .is_err()
    );
    assert!(
        fixture
            .retained
            .rewind_owned_candidate(
                &fixture.retained,
                &fixture.nonce,
                &fixture.credential,
                validity(fixture.alive.clone()),
                CancellationToken::new(),
            )
            .await
            .is_err()
    );
    let mut physical = fixture.credential.clone();
    physical.mode = NativeConnectionMode::Physical;
    assert!(
        fixture
            .candidate
            .rewind_owned_candidate(
                &fixture.retained,
                &fixture.nonce,
                &physical,
                validity(fixture.alive.clone()),
                CancellationToken::new(),
            )
            .await
            .is_err()
    );
    assert_eq!(
        original_candidate,
        tree_digest(fixture.candidate.data_dir())
    );
    let mut wrong_password = fixture.credential.clone();
    let wrong = if wrong_password.secret.starts_with('a') {
        "b"
    } else {
        "a"
    };
    wrong_password.secret.replace_range(..1, wrong);
    let error = fixture
        .candidate
        .rewind_owned_candidate(
            &fixture.retained,
            &fixture.nonce,
            &wrong_password,
            validity(fixture.alive.clone()),
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("pg_rewind failed"));
    let selected = fixture.store.revalidate().await.unwrap();
    let receipt = selected
        .helper_executions
        .last()
        .unwrap()
        .completed()
        .unwrap();
    assert!(receipt.dispatched && !receipt.cancelled);
    assert_ne!(receipt.launcher_status, 0);
    assert_eq!(
        original_candidate,
        tree_digest(fixture.candidate.data_dir())
    );
    assert_eq!(
        fixture.original_digest,
        tree_digest(fixture.retained.data_dir())
    );
    let linked_nonce = [0x71; 16];
    let linked_path = fixture
        .retained
        .data_dir()
        .with_file_name(format!("r-{}", "71".repeat(16)));
    clone_stopped(fixture.retained.data_dir(), &linked_path);
    std::fs::hard_link(
        linked_path.join("global/pg_control"),
        fixture._root.path().join("linked-candidate-control"),
    )
    .unwrap();
    let linked = PgInstanceManager::new(linked_path, find_pg_bin(), allocate_port().await);
    let error = linked
        .rewind_owned_candidate(
            &fixture.retained,
            &linked_nonce,
            &fixture.credential,
            validity(fixture.alive.clone()),
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("linked/special"));

    // Hold only the data-plane admission callback. This is not a native lease;
    // the real helper is deliberately stalled before backend admission.
    let waiting: GatewayValidity = Arc::new(|| Box::pin(std::future::pending()));
    let (held_gateway, held_credential) = NativeGateway::start_rewind(
        "127.0.0.1",
        fixture.source.data_dir().into(),
        fixture
            .source
            .socket_dir()
            .join(format!(".s.PGSQL.{}", fixture.source.port())),
        "component_cancel".into(),
        waiting,
        CancellationToken::new(),
    )
    .await
    .unwrap();
    let cancel = CancellationToken::new();
    let result = {
        let operation = fixture.candidate.rewind_owned_candidate(
            &fixture.retained,
            &fixture.nonce,
            &held_credential,
            validity(fixture.alive.clone()),
            cancel.clone(),
        );
        tokio::pin!(operation);
        let child = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                tokio::select! {
                    biased;
                    result = &mut operation => panic!("helper unexpectedly finished before cancellation: {result:?}"),
                    _ = tokio::time::sleep(Duration::from_millis(10)) => {
                        if let Some(child) = rewind_child(std::process::id()) { break child; }
                    }
                }
            }
        }).await.unwrap();
        assert!(
            !child
                .1
                .windows(held_credential.secret.len())
                .any(|w| w == held_credential.secret.as_bytes())
        );
        let environment = std::fs::read(format!("/proc/{}/environ", child.0)).unwrap();
        let password = format!("PGPASSWORD={}", held_credential.secret);
        assert!(
            environment
                .split(|b| *b == 0)
                .any(|e| e == password.as_bytes())
        );
        let probe = ProcessProbe::process(child.0);
        cancel.cancel();
        let result = operation.await;
        assert!(
            result
                .as_ref()
                .unwrap_err()
                .to_string()
                .contains("cancelled")
        );
        probe.assert_gone().await;
        probe.assert_reaped();
        eprintln!(
            "actual pg_rewind child {} cancelled and reaped; secret env-only",
            child.0
        );
        result
    };
    assert!(result.is_err());
    let selected = fixture.store.revalidate().await.unwrap();
    let receipt = selected
        .helper_executions
        .last()
        .unwrap()
        .completed()
        .unwrap();
    assert!(receipt.dispatched && receipt.cancelled);
    held_gateway.drain().await.unwrap();
    assert_eq!(
        fixture.original_digest,
        tree_digest(fixture.retained.data_dir())
    );
    fixture.gateway.drain().await.unwrap();
    fixture.source.stop().await.unwrap();

    // Once a candidate owns a live postmaster it cannot be a rewind target.
    // This candidate never becomes the active/original retained directory.
    fixture.candidate.prepare_native_build().await.unwrap();
    let (faults, _observed) = tokio::sync::mpsc::channel(8);
    fixture.candidate.start_native(faults).await.unwrap();
    let error = fixture
        .candidate
        .rewind_owned_candidate(
            &fixture.retained,
            &fixture.nonce,
            &fixture.credential,
            validity(fixture.alive),
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("stopped owned"));
    fixture.candidate.stop().await.unwrap();
    assert_eq!(
        fixture.original_digest,
        tree_digest(fixture.retained.data_dir())
    );
}
