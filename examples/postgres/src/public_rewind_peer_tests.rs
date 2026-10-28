use super::*;
use crate::access::initialize_application_role;
use crate::instance::PgInstanceManager;
use crate::testing::{TestDataDir, allocate_port, find_pg_bin};

#[test]
fn m3_rewind_peer_mapping_is_exact_idempotent_and_preserves_foreign_rules() {
    let foreign = "# foreign mapping\nforeign_map another_user another_role\n";
    let once = replace_rewind_peer_map(foreign, "native_owner").unwrap();
    assert!(once.ends_with(foreign));
    assert_eq!(
        once,
        replace_rewind_peer_map(&once, "native_owner").unwrap()
    );
    assert!(once.contains("\nkuberic_native_rewind native_owner kuberic_rewind\n"));
    let changed = replace_rewind_peer_map(&once, "new_owner").unwrap();
    assert!(!changed.contains("native_owner"));
    assert!(changed.ends_with(foreign));
    for invalid in [
        "",
        "kuberic_rewind",
        "owner another_role",
        "/.*",
        "+group",
        "owner\nother",
        "owner\\escape",
    ] {
        assert!(replace_rewind_peer_map(foreign, invalid).is_err());
    }
    for malformed in [
        "kuberic_native_rewind foreign foreign\n",
        "\"kuberic_native_rewind\" foreign kuberic_rewind\n",
        "include 'foreign-ident'\n",
        "include_if_exists 'foreign-ident'\n",
        "include_dir 'foreign-ident.d'\n",
        ",kuberic_native_rewind foreign kuberic_rewind\n",
        ",include \"foreign-ident\"\n",
        ",include_if_exists \"foreign-ident\"\n",
        ",include_dir \"foreign-ident.d\"\n",
        "@foreign-map-name foreign kuberic_rewind\n",
        "kuberic_\\\nnative_rewind foreign kuberic_rewind\n",
        "# --- kuberic native rewind peer begin ---\n",
        "# --- kuberic native rewind peer end ---\n",
        "# --- kuberic native rewind peer begin ---\n# --- kuberic native rewind peer begin ---\n",
    ] {
        assert!(replace_rewind_peer_map(malformed, "native_owner").is_err());
    }
    let foreign_quoted = "\"foreign map\" another_user another_role\n\
                          foreign_regex /^other_(.*)$/ \\1\n";
    assert!(
        replace_rewind_peer_map(foreign_quoted, "native_owner")
            .unwrap()
            .ends_with(foreign_quoted)
    );
}

#[test]
fn m3_rewind_peer_hba_has_no_new_tcp_or_trust_reader_rule() {
    let fenced = replace_managed_hba(
        "# kuberic public-v1 native gateway\n",
        false,
        "kuberic_app",
        "kuberic",
    );
    let reader = "local postgres kuberic_rewind peer map=kuberic_native_rewind\n";
    assert_eq!(fenced.matches(reader).count(), 1);
    assert!(fenced.find(reader).unwrap() < fenced.find("local   all             all").unwrap());
    assert!(fenced.contains("host    all             all              0.0.0.0/0         reject"));
    assert!(fenced.contains("host    all             all              ::0/0             reject"));
    assert!(
        fenced
            .lines()
            .all(|line| { !line.contains("kuberic_rewind") || line == reader.trim_end() })
    );
    let legacy = replace_managed_hba("", false, "kuberic_app", "kuberic");
    assert!(!legacy.contains("map=kuberic_native_rewind"));
}

#[tokio::test]
async fn m3_rewind_peer_backend_is_real_nonsuperuser_unix_only_and_readable() {
    let data = TestDataDir::new("rewind-peer");
    let instance = PgInstanceManager::new(data.path().into(), find_pg_bin(), allocate_port().await);
    instance.init_db().await.unwrap();
    PgConfig::public_replication_fence(instance.data_dir())
        .await
        .unwrap();
    let (faults, _observed) = tokio::sync::mpsc::channel(8);
    instance.start_native(faults).await.unwrap();
    initialize_application_role(&instance).await.unwrap();
    let mut configuration = tokio_postgres::Config::new();
    configuration
        .host_path(instance.socket_dir())
        .port(instance.port())
        .user("kuberic_rewind")
        .dbname("postgres");
    let (client, connection) = configuration.connect(tokio_postgres::NoTls).await.unwrap();
    let driver = tokio::spawn(connection);
    let role = client
        .query_one(
            "SELECT current_user::text, session_user::text, \
             rolsuper, rolreplication, rolcreatedb, rolcreaterole \
             FROM pg_roles WHERE rolname = current_user",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(role.get::<_, String>(0), "kuberic_rewind");
    assert_eq!(role.get::<_, String>(1), "kuberic_rewind");
    for column in 2..6 {
        assert!(!role.get::<_, bool>(column));
    }
    assert_eq!(
        client
            .query_one(
                "SELECT octet_length(pg_read_binary_file('global/pg_control'))",
                &[],
            )
            .await
            .unwrap()
            .get::<_, i32>(0),
        8192
    );
    assert!(
        client
            .batch_execute("CREATE ROLE forbidden_rewind_role")
            .await
            .is_err()
    );
    drop(client);
    driver.await.unwrap().unwrap();
    let mut remote = tokio_postgres::Config::new();
    remote
        .host("127.0.0.1")
        .port(instance.port())
        .user("kuberic_rewind")
        .dbname("postgres");
    assert!(remote.connect(tokio_postgres::NoTls).await.is_err());
    instance.stop().await.unwrap();
}

#[tokio::test]
async fn m3_rewind_peer_mapping_refuses_symlink_before_hba_or_target_mutation() {
    use std::os::unix::fs::symlink;
    let root = TestDataDir::new("rewind-peer-symlink");
    let data = root.path().join("pgdata");
    tokio::fs::create_dir(&data).await.unwrap();
    let foreign = root.path().join("foreign-ident");
    tokio::fs::write(&foreign, b"foreign identity mapping\n")
        .await
        .unwrap();
    tokio::fs::write(data.join("pg_hba.conf"), b"original hba\n")
        .await
        .unwrap();
    symlink(&foreign, data.join("pg_ident.conf")).unwrap();
    assert!(PgConfig::public_replication_fence(&data).await.is_err());
    assert_eq!(
        tokio::fs::read(&foreign).await.unwrap(),
        b"foreign identity mapping\n"
    );
    assert_eq!(
        tokio::fs::read(data.join("pg_hba.conf")).await.unwrap(),
        b"original hba\n"
    );
}

#[tokio::test]
async fn m3_rewind_peer_staging_aliases_cannot_mutate_foreign_ident_or_hba() {
    use std::os::unix::fs::symlink;
    for hardlink in [false, true] {
        let root = TestDataDir::new("rewind-peer-stage");
        let data = root.path().join("pgdata");
        tokio::fs::create_dir(&data).await.unwrap();
        let foreign = root.path().join("foreign-ident");
        tokio::fs::write(&foreign, b"foreign identity mapping\n")
            .await
            .unwrap();
        let ident = data.join("pg_ident.conf");
        tokio::fs::write(&ident, b"foreign_map other_user other_role\n")
            .await
            .unwrap();
        tokio::fs::write(data.join("pg_hba.conf"), b"original hba\n")
            .await
            .unwrap();
        let pending = ident.with_extension("conf.pending");
        if hardlink {
            std::fs::hard_link(&foreign, &pending).unwrap();
        } else {
            symlink(&foreign, &pending).unwrap();
        }
        assert!(PgConfig::public_replication_fence(&data).await.is_err());
        assert_eq!(
            tokio::fs::read(&foreign).await.unwrap(),
            b"foreign identity mapping\n"
        );
        assert_eq!(
            tokio::fs::read(&ident).await.unwrap(),
            b"foreign_map other_user other_role\n"
        );
        assert_eq!(
            tokio::fs::read(data.join("pg_hba.conf")).await.unwrap(),
            b"original hba\n"
        );
        assert_eq!(
            tokio::fs::symlink_metadata(&pending)
                .await
                .unwrap()
                .file_type()
                .is_symlink(),
            !hardlink
        );
    }
}

#[tokio::test]
async fn m3_rewind_peer_real_ident_parser_aliases_are_refused_without_reload_or_mutation() {
    let data = TestDataDir::new("rewind-peer-grammar");
    let instance = PgInstanceManager::new(data.path().into(), find_pg_bin(), allocate_port().await);
    instance.init_db().await.unwrap();
    PgConfig::public_replication_fence(instance.data_dir())
        .await
        .unwrap();
    let (faults, _observed) = tokio::sync::mpsc::channel(8);
    instance.start_native(faults).await.unwrap();
    initialize_application_role(&instance).await.unwrap();
    let (client, _driver) = instance.connect().await.unwrap();
    let ident = instance.data_dir().join("pg_ident.conf");
    let hba = instance.data_dir().join("pg_hba.conf");
    let original_ident = tokio::fs::read_to_string(&ident).await.unwrap();
    let original_hba = tokio::fs::read(&hba).await.unwrap();
    let included = instance.data_dir().join("foreign-ident");
    tokio::fs::write(
        &included,
        b"\"kuberic_native_rewind\" foreign_os_user kuberic_rewind\n",
    )
    .await
    .unwrap();
    let tokens = instance.data_dir().join("foreign-map-name");
    tokio::fs::write(&tokens, b"kuberic_native_rewind\n")
        .await
        .unwrap();
    for alias in [
        "\"kuberic_native_rewind\" foreign_os_user kuberic_rewind\n",
        "include \"foreign-ident\"\n",
        ",kuberic_native_rewind foreign_os_user kuberic_rewind\n",
        ",include \"foreign-ident\"\n",
        "@foreign-map-name foreign_os_user kuberic_rewind\n",
    ] {
        let input = format!("{original_ident}{alias}");
        tokio::fs::write(&ident, &input).await.unwrap();
        // The view parses current file contents without loading these rules
        // into the running server's authentication state.
        let rows = client
            .query(
                "SELECT map_name, sys_name, pg_username, error \
                 FROM pg_ident_file_mappings WHERE sys_name = 'foreign_os_user'",
                &[],
            )
            .await
            .unwrap();
        assert!(!rows.is_empty());
        for row in rows {
            assert_eq!(row.get::<_, String>(0), "kuberic_native_rewind");
            assert_eq!(row.get::<_, String>(1), "foreign_os_user");
            assert_eq!(row.get::<_, String>(2), "kuberic_rewind");
            assert!(row.get::<_, Option<String>>(3).is_none());
        }
        assert!(
            PgConfig::public_replication_fence(instance.data_dir())
                .await
                .is_err()
        );
        assert_eq!(tokio::fs::read(&ident).await.unwrap(), input.as_bytes());
        assert_eq!(tokio::fs::read(&hba).await.unwrap(), original_hba);
        assert_eq!(
            tokio::fs::read(&included).await.unwrap(),
            b"\"kuberic_native_rewind\" foreign_os_user kuberic_rewind\n"
        );
        assert_eq!(
            tokio::fs::read(&tokens).await.unwrap(),
            b"kuberic_native_rewind\n"
        );
    }
    drop(client);
    instance.stop().await.unwrap();
}
