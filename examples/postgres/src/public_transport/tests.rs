use super::*;
use std::os::unix::fs::symlink;
use std::os::unix::net::UnixListener;

// Test-only SQL interoperability corpus. Production candidate stores only SHA256.
const CHUNKS_SQL: &str = concat!(
    "SELECT path, begin,\n",
    "  pg_read_binary_file(path, begin, len, true) AS chunk\n",
    "FROM unnest ($1::text[], $2::int8[], $3::int4[]) as x(path, begin, len)"
);
const LIST_SQL: &str = concat!(
    "WITH RECURSIVE files (path, filename, size, isdir) AS (\n",
    "  SELECT '' AS path, filename, size, isdir FROM\n",
    "  (SELECT pg_ls_dir('.', true, false) AS filename) AS fn,\n",
    "        pg_stat_file(fn.filename, true) AS this\n",
    "  UNION ALL\n",
    "  SELECT parent.path || parent.filename || '/' AS path,\n",
    "         fn, this.size, this.isdir\n",
    "  FROM files AS parent,\n",
    "       pg_ls_dir(parent.path || parent.filename, true, false) AS fn,\n",
    "       pg_stat_file(parent.path || parent.filename || '/' || fn, true) AS this\n",
    "       WHERE parent.isdir = 't'\n",
    ")\n",
    "SELECT path || filename, size, isdir,\n",
    "       pg_tablespace_location(pg_tablespace.oid) AS link_target\n",
    "FROM files\n",
    "LEFT OUTER JOIN pg_tablespace ON files.path = 'pg_tblspc/'\n",
    "                             AND oid::text = files.filename\n"
);

struct Fixture {
    parent: PathBuf,
    root: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let directory =
            std::mem::ManuallyDrop::new(crate::testing::TestDataDir::new("rewind-wire"));
        let parent = directory.path().to_path_buf();
        let root = parent.join("pgdata");
        for directory in ["global", "pg_wal", "base/1", "pg_tblspc", "pg_stat_tmp"] {
            fs::create_dir_all(root.join(directory)).unwrap();
        }
        for file in [
            "global/pg_control",
            "pg_wal/00000002.history",
            "pg_wal/0000000A.history",
            "base/1/123",
            "postgresql.conf",
        ] {
            fs::write(root.join(file), b"owned fixture bytes, not real PGDATA").unwrap();
        }
        fs::create_dir_all(parent.join("outside")).unwrap();
        fs::write(parent.join("outside/secret"), b"outside").unwrap();
        Self { parent, root }
    }

    fn protocol(&self) -> RewindProtocol {
        RewindProtocol::new(self.root.clone()).unwrap()
    }

    fn chunks(&self) -> RewindProtocol {
        let mut protocol = self.protocol();
        protocol
            .validate(b'P', &parse("fetch_chunks_stmt", CHUNKS_SQL, &[]))
            .unwrap();
        protocol.validate(b'S', &[]).unwrap();
        protocol
    }

    fn file(&self) -> RewindProtocol {
        let mut protocol = self.protocol();
        protocol
            .validate(
                b'P',
                &parse("", std::str::from_utf8(FILE_SQL).unwrap(), &[]),
            )
            .unwrap();
        protocol
    }
}

fn cstr(text: &str) -> Vec<u8> {
    let mut result = text.as_bytes().to_vec();
    result.push(0);
    result
}

fn parse(name: &str, query: &str, types: &[u32]) -> Vec<u8> {
    let mut result = cstr(name);
    result.extend(cstr(query));
    result.extend((types.len() as u16).to_be_bytes());
    for oid in types {
        result.extend(oid.to_be_bytes());
    }
    result
}

fn bind_raw(
    portal: &str,
    statement: &str,
    formats: &[u16],
    parameters: &[Option<&[u8]>],
    results: &[u16],
) -> Vec<u8> {
    let mut body = cstr(portal);
    body.extend(cstr(statement));
    body.extend((formats.len() as u16).to_be_bytes());
    for format in formats {
        body.extend(format.to_be_bytes());
    }
    body.extend((parameters.len() as u16).to_be_bytes());
    for parameter in parameters {
        if let Some(value) = parameter {
            body.extend((value.len() as i32).to_be_bytes());
            body.extend(*value);
        } else {
            body.extend((-1i32).to_be_bytes());
        }
    }
    body.extend((results.len() as u16).to_be_bytes());
    for format in results {
        body.extend(format.to_be_bytes());
    }
    body
}

fn bind(statement: &str, parameters: &[&str]) -> Vec<u8> {
    let parameters: Vec<_> = parameters.iter().map(|p| Some(p.as_bytes())).collect();
    bind_raw("", statement, &[], &parameters, &[1])
}

fn chunks(paths: &str, offsets: &str, lengths: &str) -> Vec<u8> {
    bind("fetch_chunks_stmt", &[paths, offsets, lengths])
}

fn execute(name: &str, rows: u32) -> Vec<u8> {
    let mut body = cstr(name);
    body.extend(rows.to_be_bytes());
    body
}

fn object(kind: u8, name: &str) -> Vec<u8> {
    let mut body = vec![kind];
    body.extend(cstr(name));
    body
}

fn complete(protocol: &mut RewindProtocol, body: &[u8]) {
    protocol.validate(b'B', body).unwrap();
    protocol.validate(b'D', &object(b'P', "")).unwrap();
    protocol.validate(b'E', &execute("", 0)).unwrap();
    protocol.validate(b'S', &[]).unwrap();
}

#[test]
fn actual_rel_16_15_sql_corpus_matches_recorded_digests() {
    assert_eq!(CHUNKS_SQL.len(), 146);
    assert_eq!(LIST_SQL.len(), 751);
    assert!(has_digest(CHUNKS_SQL.as_bytes(), CHUNKS_SQL_SHA256));
    assert!(has_digest(LIST_SQL.as_bytes(), LIST_SQL_SHA256));
    assert_eq!(
        SECURE_PATH,
        b"SELECT pg_catalog.set_config('search_path', '', false);"
    );
}

#[test]
fn whole_helper_sequence_and_repeated_real_calls() {
    let fixture = Fixture::new();
    let mut protocol = fixture.protocol();
    for query in [
        "SET statement_timeout = 0",
        "SET lock_timeout = 0",
        "SET idle_in_transaction_session_timeout = 0",
        "SET default_transaction_read_only = on",
        std::str::from_utf8(SECURE_PATH).unwrap(),
        "SHOW full_page_writes",
    ] {
        protocol.validate(b'Q', &cstr(query)).unwrap();
    }
    protocol
        .validate(b'P', &parse("fetch_chunks_stmt", CHUNKS_SQL, &[]))
        .unwrap();
    protocol.validate(b'H', &[]).unwrap();
    protocol.validate(b'S', &[]).unwrap();
    for _ in 0..3 {
        protocol
            .validate(
                b'P',
                &parse("", std::str::from_utf8(FILE_SQL).unwrap(), &[]),
            )
            .unwrap();
        complete(&mut protocol, &bind("", &["global/pg_control"]));
        protocol
            .validate(
                b'P',
                &parse("", std::str::from_utf8(FILE_SQL).unwrap(), &[]),
            )
            .unwrap();
        complete(&mut protocol, &bind("", &["pg_wal/00000002.history"]));
        protocol.validate(b'Q', &cstr(LIST_SQL)).unwrap();
        complete(
            &mut protocol,
            &chunks(
                "{\"base/1/123\",\"base/1/123\",\"postgresql.conf\"}",
                "{0,1048576,0}",
                "{1048576,8192,1048576}",
            ),
        );
        protocol
            .validate(b'Q', &cstr("SELECT pg_current_wal_insert_lsn()"))
            .unwrap();
    }
    protocol.validate(b'X', &[]).unwrap();
    assert!(
        protocol
            .validate(b'Q', &cstr("SHOW full_page_writes"))
            .is_err()
    );
}

#[test]
fn exact_inferred_or_correct_pg16_parameter_types_only() {
    let fixture = Fixture::new();
    for types in [&[][..], &[1009, 1016, 1007][..]] {
        let mut protocol = fixture.protocol();
        protocol
            .validate(b'P', &parse("fetch_chunks_stmt", CHUNKS_SQL, types))
            .unwrap();
    }
    for types in [&[][..], &[25][..]] {
        let mut protocol = fixture.protocol();
        protocol
            .validate(
                b'P',
                &parse("", std::str::from_utf8(FILE_SQL).unwrap(), types),
            )
            .unwrap();
    }
    for types in [
        &[0, 0, 0][..],
        &[1009, 1007, 1016][..],
        &[1009][..],
        &[1009, 1016, 1007, 25][..],
    ] {
        assert!(
            fixture
                .protocol()
                .validate(b'P', &parse("fetch_chunks_stmt", CHUNKS_SQL, types))
                .is_err()
        );
    }
    for types in [&[0][..], &[17][..], &[25, 25][..]] {
        assert!(
            fixture
                .protocol()
                .validate(
                    b'P',
                    &parse("", std::str::from_utf8(FILE_SQL).unwrap(), types)
                )
                .is_err()
        );
    }
}

#[test]
fn reject_generic_sql_mutations_even_if_readonly_looking() {
    let fixture = Fixture::new();
    for query in [
        "",
        "SELECT 1",
        "SELECT * FROM pg_authid",
        "SELECT pg_read_binary_file('global/pg_control')",
        "SELECT pg_current_wal_insert_lsn();",
        "SHOW full_page_writes; SELECT 1",
        "SHOW FULL_PAGE_WRITES",
        "SET statement_timeout=0",
        "SET statement_timeout = 1",
        "SET default_transaction_read_only = off",
        "SET ROLE postgres",
        "SET SESSION AUTHORIZATION postgres",
        "CREATE ROLE attacker SUPERUSER",
        "DROP TABLE important",
        "INSERT INTO t VALUES (1)",
        "COPY (SELECT 1) TO STDOUT",
        "IDENTIFY_SYSTEM",
        "BASE_BACKUP",
        "START_REPLICATION 0/0",
        "SELECT pg_catalog.set_config('search_path', 'public', false);",
        "SELECT pg_catalog.set_config('search_path', '', false); ",
    ] {
        assert!(
            fixture.protocol().validate(b'Q', &cstr(query)).is_err(),
            "{query}"
        );
        assert!(
            fixture
                .protocol()
                .validate(b'P', &parse("", query, &[]))
                .is_err(),
            "{query}"
        );
    }
    // This identity is valid only in the unnamed extended-query path, not Q.
    assert!(
        fixture
            .protocol()
            .validate(b'Q', &cstr(std::str::from_utf8(FILE_SQL).unwrap()))
            .is_err()
    );
    for query in [
        format!("{LIST_SQL} "),
        LIST_SQL.replace("true, false", "false, false"),
    ] {
        assert!(fixture.protocol().validate(b'Q', &cstr(&query)).is_err());
    }
    assert!(
        fixture
            .protocol()
            .validate(
                b'P',
                &parse("fetch_chunks_stmt", &format!("{CHUNKS_SQL} "), &[])
            )
            .is_err()
    );
}

#[test]
fn malformed_queries_counts_lengths_and_trailing_bytes_are_denied() {
    let fixture = Fixture::new();
    let mut frames = vec![
        (b'Q', b"SHOW full_page_writes".to_vec()),
        (b'Q', b"SHOW full_page_writes\0junk".to_vec()),
        (b'Q', b"SHOW full_page_writes\0\0".to_vec()),
        (b'Q', vec![b'x'; MAX_FRAME_BYTES - 4]),
        (b'P', vec![]),
        (b'B', vec![]),
        (b'D', vec![]),
        (b'E', vec![]),
        (b'C', vec![]),
    ];
    let valid_parse = parse("", std::str::from_utf8(FILE_SQL).unwrap(), &[25]);
    for length in 0..valid_parse.len() {
        frames.push((b'P', valid_parse[..length].to_vec()));
    }
    let mut extra = valid_parse.clone();
    extra.push(0);
    frames.push((b'P', extra));
    for (kind, body) in frames {
        assert!(
            fixture.protocol().validate(kind, &body).is_err(),
            "kind {kind}"
        );
    }
    let valid_bind = bind("", &["global/pg_control"]);
    for length in 0..valid_bind.len() {
        assert!(
            fixture
                .file()
                .validate(b'B', &valid_bind[..length])
                .is_err(),
            "{length}"
        );
    }
    let mut extra = valid_bind.clone();
    extra.push(0);
    assert!(fixture.file().validate(b'B', &extra).is_err());
    // Length fields include NULL (-1), invalid negative, absurd positive and truncation.
    for length in [-1i32, -2, i32::MAX, 1_000_000, 0] {
        let mut body = vec![0, 0, 0, 0, 0, 1];
        body.extend(length.to_be_bytes());
        body.extend([0, 1, 0, 1]);
        assert!(fixture.file().validate(b'B', &body).is_err());
    }
    let mut excessive_types = cstr("");
    excessive_types.extend(cstr(std::str::from_utf8(FILE_SQL).unwrap()));
    excessive_types.extend(u16::MAX.to_be_bytes());
    assert!(fixture.protocol().validate(b'P', &excessive_types).is_err());
}

#[test]
fn bind_text_formats_and_binary_results_are_exact() {
    let fixture = Fixture::new();
    let single = [Some(b"global/pg_control".as_slice())];
    for formats in [&[][..], &[0][..]] {
        let mut protocol = fixture.file();
        complete(&mut protocol, &bind_raw("", "", formats, &single, &[1]));
    }
    let parameters = [
        Some(b"{\"base/1/123\"}".as_slice()),
        Some(b"{0}"),
        Some(b"{1}"),
    ];
    for formats in [&[][..], &[0][..], &[0, 0, 0][..]] {
        let mut protocol = fixture.chunks();
        complete(
            &mut protocol,
            &bind_raw("", "fetch_chunks_stmt", formats, &parameters, &[1]),
        );
    }
    for formats in [&[1][..], &[0, 1, 0][..], &[2][..], &[0, 0][..]] {
        assert!(
            fixture
                .chunks()
                .validate(
                    b'B',
                    &bind_raw("", "fetch_chunks_stmt", formats, &parameters, &[1])
                )
                .is_err()
        );
    }
    for results in [&[][..], &[0][..], &[1, 1, 1][..], &[2][..]] {
        assert!(
            fixture
                .file()
                .validate(b'B', &bind_raw("", "", &[], &single, results))
                .is_err()
        );
    }
    assert!(
        fixture
            .file()
            .validate(b'B', &bind_raw("", "", &[], &[None], &[1]))
            .is_err()
    );
    assert!(
        fixture
            .file()
            .validate(b'B', &bind_raw("", "", &[], &[], &[1]))
            .is_err()
    );
    assert!(
        fixture
            .file()
            .validate(b'B', &bind_raw("", "", &[], &[single[0], single[0]], &[1]))
            .is_err()
    );
    let binary_path = [Some(b"global/pg_control\0ignored".as_slice())];
    assert!(
        fixture
            .file()
            .validate(b'B', &bind_raw("", "", &[], &binary_path, &[1]))
            .is_err()
    );
}

#[test]
fn statement_and_portal_lifecycle_denies_stale_foreign_and_duplicate_objects() {
    let fixture = Fixture::new();
    for name in ["foreign", "fetch_chunks_stmt", ""] {
        assert!(
            fixture
                .protocol()
                .validate(b'B', &bind(name, &["global/pg_control"]))
                .is_err()
        );
    }
    assert!(
        fixture
            .protocol()
            .validate(b'P', &parse("foreign", CHUNKS_SQL, &[]))
            .is_err()
    );
    assert!(
        fixture
            .protocol()
            .validate(b'P', &parse("", CHUNKS_SQL, &[]))
            .is_err()
    );
    assert!(
        fixture
            .protocol()
            .validate(
                b'P',
                &parse(
                    "fetch_chunks_stmt",
                    std::str::from_utf8(FILE_SQL).unwrap(),
                    &[]
                )
            )
            .is_err()
    );
    assert!(
        fixture
            .chunks()
            .validate(b'P', &parse("fetch_chunks_stmt", CHUNKS_SQL, &[]))
            .is_err()
    );

    // Unnamed replacement is valid, including repeated PQexecParams.
    let mut protocol = fixture.file();
    protocol.validate(b'S', &[]).unwrap();
    protocol
        .validate(
            b'P',
            &parse("", std::str::from_utf8(FILE_SQL).unwrap(), &[25]),
        )
        .unwrap();
    complete(&mut protocol, &bind("", &["global/pg_control"]));
    protocol
        .validate(b'Q', &cstr("SHOW full_page_writes"))
        .unwrap();
    assert!(
        protocol
            .validate(b'B', &bind("", &["global/pg_control"]))
            .is_err()
    );

    let mut protocol = fixture.file();
    protocol
        .validate(b'B', &bind("", &["global/pg_control"]))
        .unwrap();
    assert!(
        protocol
            .validate(b'B', &bind("", &["global/pg_control"]))
            .is_err()
    );
    assert!(fixture.file().validate(b'D', &object(b'P', "")).is_err());
    assert!(
        fixture
            .protocol()
            .validate(b'D', &object(b'S', ""))
            .is_err()
    );
    assert!(
        fixture
            .chunks()
            .validate(b'D', &object(b'S', "foreign"))
            .is_err()
    );
    assert!(fixture.chunks().validate(b'D', &object(b'Q', "")).is_err());
    let mut protocol = fixture.chunks();
    protocol
        .validate(b'D', &object(b'S', "fetch_chunks_stmt"))
        .unwrap();
    protocol.validate(b'S', &[]).unwrap();
    let parameters = [
        Some(b"{\"base/1/123\"}".as_slice()),
        Some(b"{0}"),
        Some(b"{1}"),
    ];
    assert!(
        protocol
            .validate(
                b'B',
                &bind_raw("foreign", "fetch_chunks_stmt", &[], &parameters, &[1])
            )
            .is_err()
    );
}

#[test]
fn execute_has_exact_portal_count_and_single_consumption() {
    let fixture = Fixture::new();
    for frame in [execute("", 0), execute("foreign", 0), execute("", 1)] {
        assert!(fixture.file().validate(b'E', &frame).is_err());
    }
    for frame in [execute("foreign", 0), execute("", 1), execute("", u32::MAX)] {
        let mut protocol = fixture.file();
        protocol
            .validate(b'B', &bind("", &["global/pg_control"]))
            .unwrap();
        assert!(protocol.validate(b'E', &frame).is_err());
    }
    let valid = execute("", 0);
    for length in 0..valid.len() {
        let mut protocol = fixture.file();
        protocol
            .validate(b'B', &bind("", &["global/pg_control"]))
            .unwrap();
        assert!(protocol.validate(b'E', &valid[..length]).is_err());
    }
    let mut protocol = fixture.file();
    protocol
        .validate(b'B', &bind("", &["global/pg_control"]))
        .unwrap();
    protocol.validate(b'E', &valid).unwrap();
    assert!(protocol.validate(b'E', &valid).is_err());
    let mut protocol = fixture.file();
    complete(&mut protocol, &bind("", &["global/pg_control"]));
    assert!(protocol.validate(b'E', &valid).is_err());
}

#[test]
fn close_only_known_objects_and_reprepare_after_close() {
    let fixture = Fixture::new();
    let mut protocol = fixture.chunks();
    protocol
        .validate(b'C', &object(b'S', "fetch_chunks_stmt"))
        .unwrap();
    protocol.validate(b'S', &[]).unwrap();
    protocol
        .validate(b'P', &parse("fetch_chunks_stmt", CHUNKS_SQL, &[]))
        .unwrap();
    protocol.validate(b'S', &[]).unwrap();
    complete(&mut protocol, &chunks("{\"base/1/123\"}", "{0}", "{1}"));
    let mut protocol = fixture.chunks();
    protocol
        .validate(b'C', &object(b'S', "fetch_chunks_stmt"))
        .unwrap();
    protocol.validate(b'S', &[]).unwrap();
    assert!(
        protocol
            .validate(b'B', &chunks("{\"base/1/123\"}", "{0}", "{1}"))
            .is_err()
    );
    for target in [
        object(b'S', "foreign"),
        object(b'P', ""),
        object(b'P', "foreign"),
        object(b'X', ""),
    ] {
        assert!(fixture.chunks().validate(b'C', &target).is_err());
    }
    let mut protocol = fixture.file();
    protocol
        .validate(b'B', &bind("", &["global/pg_control"]))
        .unwrap();
    protocol.validate(b'C', &object(b'P', "")).unwrap();
    protocol.validate(b'S', &[]).unwrap();
    assert!(protocol.validate(b'E', &execute("", 0)).is_err());
}

#[test]
fn cross_mode_unknown_commands_and_failed_connections_stay_closed() {
    let fixture = Fixture::new();
    for kind in [0, b'd', b'c', b'f', b'F', b'p', b'R', b'W', b'Y', 255] {
        let mut protocol = fixture.protocol();
        assert!(protocol.validate(kind, &[]).is_err());
        assert!(
            protocol
                .validate(b'Q', &cstr("SHOW full_page_writes"))
                .is_err()
        );
    }
    for kind in *b"HS" {
        assert!(fixture.protocol().validate(kind, &[]).is_err());
    }
    for kind in *b"HSX" {
        assert!(fixture.file().validate(kind, &[0]).is_err());
    }
    assert!(
        fixture
            .file()
            .validate(b'Q', &cstr("SHOW full_page_writes"))
            .is_err()
    );
    let mut protocol = fixture.file();
    protocol
        .validate(b'B', &bind("", &["global/pg_control"]))
        .unwrap();
    assert!(protocol.validate(b'S', &[]).is_err());
}

#[test]
fn reject_path_traversal_absolute_encoding_and_array_escapes() {
    let fixture = Fixture::new();
    for path in [
        "",
        "/etc/passwd",
        "../outside/secret",
        "base/../../outside/secret",
        "base/./1/123",
        "./base/1/123",
        "base//1/123",
        "base/1/123/",
        "base\\1\\123",
        "base/1/%2e%2e",
        "base/1/%2Fetc%2Fpasswd",
        "base/1/123;SELECT",
        "base/1/white space",
        "base/1/\"escaped\"",
        "base/1/'quote'",
        "base/1/123\n",
        "base/1/é",
    ] {
        let paths = format!("{{\"{path}\"}}");
        assert!(
            fixture
                .chunks()
                .validate(b'B', &chunks(&paths, "{0}", "{1}"))
                .is_err(),
            "{path:?}"
        );
    }
    for paths in [
        "{}",
        "{base/1/123}",
        "{\"base/1/123\",}",
        "{\"base/1/123\",NULL}",
        "{NULL}",
        "{{\"base/1/123\"}}",
        "[1:1]={\"base/1/123\"}",
        "{\"base/1/123\\\"\", \"outside\"}",
        "{\"base\\\\1\\\\123\"}",
        "{\"base/1/123\"} trailing",
        "{\"base/1/123\"}\0",
    ] {
        assert!(
            fixture
                .chunks()
                .validate(b'B', &chunks(paths, "{0}", "{1}"))
                .is_err(),
            "{paths:?}"
        );
    }
    let long = format!("{{\"{}\"}}", "a".repeat(MAX_PATH_BYTES + 1));
    assert!(
        fixture
            .chunks()
            .validate(b'B', &chunks(&long, "{0}", "{1}"))
            .is_err()
    );
    let mut invalid_utf8 = b"{\"base/1/".to_vec();
    invalid_utf8.extend([255, b'"', b'}']);
    let params = [Some(invalid_utf8.as_slice()), Some(b"{0}"), Some(b"{1}")];
    assert!(
        fixture
            .chunks()
            .validate(b'B', &bind_raw("", "fetch_chunks_stmt", &[], &params, &[1]))
            .is_err()
    );
}

#[test]
fn chunks_cardinality_and_checked_numeric_range_boundaries() {
    let fixture = Fixture::new();
    for size in [1, MAX_CHUNKS] {
        let paths = format!("{{{}}}", vec!["\"base/1/123\""; size].join(","));
        let offsets = format!("{{{}}}", vec!["0"; size].join(","));
        let lengths = format!("{{{}}}", vec!["1048576"; size].join(","));
        complete(&mut fixture.chunks(), &chunks(&paths, &offsets, &lengths));
    }
    let paths = format!("{{{}}}", vec!["\"base/1/123\""; MAX_CHUNKS + 1].join(","));
    let offsets = format!("{{{}}}", vec!["0"; MAX_CHUNKS + 1].join(","));
    let lengths = format!("{{{}}}", vec!["1"; MAX_CHUNKS + 1].join(","));
    assert!(
        fixture
            .chunks()
            .validate(b'B', &chunks(&paths, &offsets, &lengths))
            .is_err()
    );
    assert!(
        fixture
            .chunks()
            .validate(b'B', &chunks("{\"base/1/123\"}", &offsets, "{1}"))
            .is_err()
    );
    assert!(
        fixture
            .chunks()
            .validate(b'B', &chunks("{\"base/1/123\"}", "{0}", &lengths))
            .is_err()
    );
    for (offsets, lengths) in [
        ("{}", "{1}"),
        ("{0}", "{}"),
        ("{0,1}", "{1}"),
        ("{0}", "{1,1}"),
        ("{-1}", "{1}"),
        ("{+1}", "{1}"),
        ("{00}", "{1}"),
        ("{ 0}", "{1}"),
        ("{NULL}", "{1}"),
        ("{0}", "{0}"),
        ("{0}", "{-1}"),
        ("{0}", "{1048577}"),
        ("{9223372036854775807}", "{1}"),
        ("{9223372036854775808}", "{1}"),
        ("{18446744073709551615}", "{1}"),
        ("{\"0\"}", "{1}"),
        ("{{0}}", "{1}"),
        ("[0:0]={0}", "{1}"),
        ("{0}", "{2147483648}"),
    ] {
        assert!(
            fixture
                .chunks()
                .validate(b'B', &chunks("{\"base/1/123\"}", offsets, lengths))
                .is_err(),
            "{offsets} {lengths}"
        );
    }
    complete(
        &mut fixture.chunks(),
        &chunks("{\"base/1/123\"}", "{9223372036854775806}", "{1}"),
    );
}

#[test]
fn single_file_api_is_only_control_and_real_uppercase_history() {
    let fixture = Fixture::new();
    for path in [
        "global/pg_control",
        "pg_wal/00000002.history",
        "pg_wal/0000000A.history",
    ] {
        complete(&mut fixture.file(), &bind("", &[path]));
    }
    for path in [
        "postgresql.conf",
        "base/1/123",
        "pg_wal/00000001.history",
        "pg_wal/00000000.history",
        "pg_wal/0000000a.history",
        "pg_wal/000000002.history",
        "pg_wal/../../outside/secret",
        "global//pg_control",
        "/global/pg_control",
        "pg_wal/00000003.history",
    ] {
        assert!(
            fixture.file().validate(b'B', &bind("", &[path])).is_err(),
            "{path}"
        );
    }
}

#[test]
fn every_file_bind_and_execute_checks_actual_owned_filesystem() {
    let fixture = Fixture::new();
    symlink(fixture.parent.join("outside"), fixture.root.join("escape")).unwrap();
    symlink(fixture.root.join("base"), fixture.root.join("inside-link")).unwrap();
    symlink(
        fixture.parent.join("outside/secret"),
        fixture.root.join("leaf-link"),
    )
    .unwrap();
    symlink("does-not-exist", fixture.root.join("dangling")).unwrap();
    for path in [
        "escape/secret",
        "inside-link/1/123",
        "leaf-link",
        "dangling",
        "base",
        "base/1/123/child",
    ] {
        let paths = format!("{{\"{path}\"}}");
        assert!(
            fixture
                .chunks()
                .validate(b'B', &chunks(&paths, "{0}", "{1}"))
                .is_err(),
            "{path}"
        );
    }
    let fixture = Fixture::new();
    let mut protocol = fixture.chunks();
    protocol
        .validate(b'B', &chunks("{\"base/1/123\"}", "{0}", "{1}"))
        .unwrap();
    fs::rename(
        fixture.root.join("base/1/123"),
        fixture.root.join("base/1/123.saved"),
    )
    .unwrap();
    symlink(
        fixture.parent.join("outside/secret"),
        fixture.root.join("base/1/123"),
    )
    .unwrap();
    assert!(protocol.validate(b'E', &execute("", 0)).is_err());

    let fixture = Fixture::new();
    let mut protocol = fixture.file();
    protocol
        .validate(b'B', &bind("", &["global/pg_control"]))
        .unwrap();
    fs::rename(
        fixture.root.join("global"),
        fixture.root.join("global.saved"),
    )
    .unwrap();
    symlink(
        fixture.root.join("global.saved"),
        fixture.root.join("global"),
    )
    .unwrap();
    assert!(protocol.validate(b'E', &execute("", 0)).is_err());
}

#[test]
fn chunk_missing_ok_is_not_single_file_missing_ok() {
    let fixture = Fixture::new();
    complete(
        &mut fixture.chunks(),
        &chunks("{\"base/1/removed\"}", "{0}", "{1}"),
    );
    complete(
        &mut fixture.chunks(),
        &chunks("{\"base/removed-dir/123\"}", "{0}", "{1}"),
    );
    let mut protocol = fixture.chunks();
    protocol
        .validate(b'B', &chunks("{\"base/1/123\"}", "{0}", "{1}"))
        .unwrap();
    fs::rename(
        fixture.root.join("base/1/123"),
        fixture.root.join("base/1/123.removed"),
    )
    .unwrap();
    protocol.validate(b'E', &execute("", 0)).unwrap();
    protocol.validate(b'S', &[]).unwrap();
    let mut protocol = fixture.chunks();
    protocol
        .validate(b'B', &chunks("{\"base/1/123.removed\"}", "{0}", "{1}"))
        .unwrap();
    fs::rename(
        fixture.root.join("base/1"),
        fixture.root.join("base/1.removed"),
    )
    .unwrap();
    protocol.validate(b'E', &execute("", 0)).unwrap();
    protocol.validate(b'S', &[]).unwrap();
    let mut protocol = fixture.file();
    protocol
        .validate(b'B', &bind("", &["global/pg_control"]))
        .unwrap();
    fs::rename(
        fixture.root.join("global/pg_control"),
        fixture.root.join("global/pg_control.removed"),
    )
    .unwrap();
    assert!(protocol.validate(b'E', &execute("", 0)).is_err());
    assert!(
        fixture
            .file()
            .validate(b'B', &bind("", &["global/pg_control"]))
            .is_err()
    );
}

#[test]
fn listing_refuses_any_symlink_and_external_tablespace_before_forwarding() {
    for (path, target) in [
        ("custom", "outside"),
        ("pg_tblspc/123", "outside"),
        ("pg_wal/wal-link", "outside"),
        ("pg_stat_tmp/socket-link", "outside"),
    ] {
        let fixture = Fixture::new();
        symlink(fixture.parent.join(target), fixture.root.join(path)).unwrap();
        assert!(
            fixture.protocol().validate(b'Q', &cstr(LIST_SQL)).is_err(),
            "{path}"
        );
    }
    let fixture = Fixture::new();
    symlink(
        fixture.root.join("base"),
        fixture.root.join("internal-link"),
    )
    .unwrap();
    assert!(fixture.protocol().validate(b'Q', &cstr(LIST_SQL)).is_err());
    let fixture = Fixture::new();
    fs::create_dir(fixture.root.join("pg_tblspc/123")).unwrap();
    assert!(fixture.protocol().validate(b'Q', &cstr(LIST_SQL)).is_err());
    assert!(
        fixture
            .chunks()
            .validate(b'B', &chunks("{\"pg_tblspc/123/file\"}", "{0}", "{1}"))
            .is_err()
    );
}

#[test]
fn live_pg_stat_tmp_unix_socket_is_listable_never_readable() {
    let fixture = Fixture::new();
    let socket = fixture.root.join("pg_stat_tmp/s");
    let _listener = UnixListener::bind(&socket).unwrap();
    fixture.protocol().validate(b'Q', &cstr(LIST_SQL)).unwrap();
    assert!(
        fixture
            .chunks()
            .validate(b'B', &chunks("{\"pg_stat_tmp/s\"}", "{0}", "{1}"))
            .is_err()
    );
}

#[test]
fn root_identity_and_canonical_constructor_fail_closed() {
    let fixture = Fixture::new();
    for path in [
        PathBuf::from("relative"),
        fixture.root.join("missing"),
        fixture.root.join("global/pg_control"),
        fixture.root.join("."),
        fixture.root.join("global/.."),
    ] {
        assert!(RewindProtocol::new(path).is_err());
    }
    let linked = fixture.parent.join("linked-root");
    symlink(&fixture.root, &linked).unwrap();
    assert!(RewindProtocol::new(linked.clone()).is_err());
    assert!(RewindProtocol::new(linked.join("global")).is_err());
    let mut protocol = fixture.protocol();
    fs::rename(&fixture.root, fixture.parent.join("retained-original")).unwrap();
    fs::create_dir(&fixture.root).unwrap();
    assert!(
        protocol
            .validate(b'Q', &cstr("SHOW full_page_writes"))
            .is_err()
    );
    let fixture = Fixture::new();
    let mut protocol = fixture.file();
    protocol
        .validate(b'B', &bind("", &["global/pg_control"]))
        .unwrap();
    fs::rename(&fixture.root, fixture.parent.join("retained-original")).unwrap();
    fs::create_dir(&fixture.root).unwrap();
    fs::create_dir(fixture.root.join("global")).unwrap();
    fs::write(fixture.root.join("global/pg_control"), b"foreign").unwrap();
    assert!(protocol.validate(b'E', &execute("", 0)).is_err());
    let fixture = Fixture::new();
    let mut protocol = fixture.protocol();
    let retained = fixture.parent.join("retained-original");
    fs::rename(&fixture.root, &retained).unwrap();
    symlink(&retained, &fixture.root).unwrap();
    assert!(
        protocol
            .validate(b'Q', &cstr("SHOW full_page_writes"))
            .is_err()
    );
}

#[test]
fn bounded_path_depth_frame_and_deterministic_malformed_byte_corpus_do_not_panic() {
    let fixture = Fixture::new();
    let too_deep = format!("{{\"{}\"}}", vec!["a"; MAX_DEPTH + 1].join("/"));
    assert!(
        fixture
            .chunks()
            .validate(b'B', &chunks(&too_deep, "{0}", "{1}"))
            .is_err()
    );
    // Inputs exercise parser entrypoints independently, not after an earlier poison.
    let mut seed = 0x1234_5678u32;
    for kind in *b"QPBDECHSX" {
        for length in 1..96 {
            let body: Vec<_> = (0..length)
                .map(|_| {
                    seed ^= seed << 13;
                    seed ^= seed >> 17;
                    seed ^= seed << 5;
                    (seed & 255) as u8
                })
                .collect();
            let mut protocol = if kind == b'B' {
                fixture.chunks()
            } else {
                fixture.protocol()
            };
            assert!(
                protocol.validate(kind, &body).is_err(),
                "kind={kind} length={length}"
            );
        }
    }
}
