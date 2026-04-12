use std::path::PathBuf;

/// Find the pg_bin directory from the system.
/// Checks common locations for PostgreSQL 16.
pub fn find_pg_bin() -> PathBuf {
    let candidates = [
        "/usr/lib/postgresql/16/bin",
        "/usr/lib/postgresql/17/bin",
        "/usr/lib/postgresql/15/bin",
        "/usr/pgsql-16/bin",
        "/usr/local/pgsql/bin",
    ];

    for path in &candidates {
        let p = PathBuf::from(path);
        if p.join("initdb").exists() {
            return p;
        }
    }

    // Fallback: try PATH
    if let Some(bindir) = std::process::Command::new("pg_config")
        .arg("--bindir")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
    {
        return PathBuf::from(bindir);
    }

    panic!("PostgreSQL binaries not found. Install postgresql or set --pg-bin.");
}

/// Create a temporary data directory for testing.
pub fn temp_data_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("kuberic-pg-test-{name}-{}", std::process::id()));
    // Clean up any leftover from previous run
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp data dir");
    dir
}

/// Clean up a test data directory.
pub fn cleanup_data_dir(dir: &std::path::Path) {
    let _ = std::fs::remove_dir_all(dir);
}
