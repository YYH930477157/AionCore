//! A database must stay openable by a build that differs only in line endings.
//!
//! sqlx stores a SHA-384 of every migration file, computed from the bytes the binary embedded at
//! compile time. The same source therefore records different checksums from a CRLF checkout (the
//! GitHub Windows runners, up to v0.2.2-woody.4) and an LF one (every build since). Startup must
//! accept exactly that difference and nothing else.

use std::path::Path;

use aionui_db::init_database_staged;
use sha2::{Digest, Sha384};
use sqlx::Connection;
use sqlx::sqlite::{SqliteConnectOptions, SqliteConnection};

const MCP_RECONCILED_VERSION: i64 = 7;
const TAMPERED: [u8; 4] = [0xde, 0xad, 0xbe, 0xef];

/// Every migration as `(version, LF checksum, CRLF checksum)`.
fn migrations() -> Vec<(i64, Vec<u8>, Vec<u8>)> {
    let directory = Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations");
    std::fs::read_dir(directory)
        .unwrap()
        .map(|entry| {
            let path = entry.unwrap().path();
            let name = path.file_name().unwrap().to_str().unwrap().to_owned();
            let version = name.split('_').next().unwrap().parse().unwrap();
            let lf = std::fs::read_to_string(&path).unwrap().replace("\r\n", "\n");
            let crlf = lf.replace('\n', "\r\n");
            (
                version,
                Sha384::digest(lf.as_bytes()).to_vec(),
                Sha384::digest(crlf.as_bytes()).to_vec(),
            )
        })
        .collect()
}

async fn stamp(conn: &mut SqliteConnection, version: i64, checksum: &[u8]) {
    sqlx::query("UPDATE _sqlx_migrations SET checksum = ? WHERE version = ?")
        .bind(checksum)
        .bind(version)
        .execute(&mut *conn)
        .await
        .unwrap();
}

async fn connect(path: &Path) -> SqliteConnection {
    SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(path))
        .await
        .unwrap()
}

/// Create a database, then record every migration as a CRLF build would have.
async fn crlf_database(path: &Path) -> SqliteConnection {
    init_database_staged(path).await.unwrap().close().await;
    let mut conn = connect(path).await;
    for (version, _, crlf) in migrations() {
        stamp(&mut conn, version, &crlf).await;
    }
    conn
}

async fn stored_checksums(conn: &mut SqliteConnection) -> Vec<(i64, Vec<u8>)> {
    sqlx::query_as("SELECT version, checksum FROM _sqlx_migrations ORDER BY version")
        .fetch_all(&mut *conn)
        .await
        .unwrap()
}

fn embedded_checksums() -> Vec<(i64, Vec<u8>)> {
    let mut checksums: Vec<_> = migrations().into_iter().map(|(version, lf, _)| (version, lf)).collect();
    checksums.sort();
    checksums
}

#[tokio::test]
async fn opens_a_database_written_by_a_crlf_build() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("aionui-backend.db");
    crlf_database(&path).await.close().await.unwrap();

    let reopened = init_database_staged(&path)
        .await
        .expect("line endings must not strand a database");
    reopened.close().await;

    let mut conn = connect(&path).await;
    assert_eq!(stored_checksums(&mut conn).await, embedded_checksums());
}

#[tokio::test]
async fn applies_pending_migrations_after_realigning() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("aionui-backend.db");
    let mut conn = crlf_database(&path).await;
    // The state an upgrade really meets: an older schema, recorded by a CRLF build.
    sqlx::query("DROP TABLE project_workspaces")
        .execute(&mut conn)
        .await
        .unwrap();
    sqlx::query("DELETE FROM _sqlx_migrations WHERE version = 45")
        .execute(&mut conn)
        .await
        .unwrap();
    conn.close().await.unwrap();

    init_database_staged(&path).await.unwrap().close().await;

    let mut conn = connect(&path).await;
    sqlx::query("SELECT project_id FROM project_workspaces")
        .fetch_all(&mut conn)
        .await
        .expect("the pending migration must run");
    assert_eq!(stored_checksums(&mut conn).await, embedded_checksums());
}

#[tokio::test]
async fn opens_a_database_with_mixed_line_endings() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("aionui-backend.db");
    let mut conn = crlf_database(&path).await;
    // An earlier allowlist re-stamped only version 1 before failing on version 2.
    let first = migrations().into_iter().find(|migration| migration.0 == 1).unwrap();
    stamp(&mut conn, 1, &first.1).await;
    conn.close().await.unwrap();

    init_database_staged(&path).await.unwrap().close().await;

    let mut conn = connect(&path).await;
    assert_eq!(stored_checksums(&mut conn).await, embedded_checksums());
}

#[tokio::test]
async fn a_modified_migration_fails_without_changing_any_checksum() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("aionui-backend.db");
    let mut conn = crlf_database(&path).await;
    stamp(&mut conn, 3, &TAMPERED).await;
    let before = stored_checksums(&mut conn).await;
    conn.close().await.unwrap();

    let err = init_database_staged(&path)
        .await
        .expect_err("a checksum no line ending explains is a modified migration");
    assert_eq!(err.stage(), "database.migration");

    let mut conn = connect(&path).await;
    assert_eq!(stored_checksums(&mut conn).await, before);
}

#[tokio::test]
async fn the_first_migration_gets_no_special_treatment() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("aionui-backend.db");
    init_database_staged(&path).await.unwrap().close().await;
    let mut conn = connect(&path).await;
    stamp(&mut conn, 1, &TAMPERED).await;
    conn.close().await.unwrap();

    let err = init_database_staged(&path)
        .await
        .expect_err("version 1 is as protected as every other migration");
    assert_eq!(err.stage(), "database.migration");
}

#[tokio::test]
async fn the_mcp_repair_still_runs_on_a_crlf_database() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("aionui-backend.db");
    let mut conn = crlf_database(&path).await;
    stamp(&mut conn, MCP_RECONCILED_VERSION, &TAMPERED).await;
    conn.close().await.unwrap();

    init_database_staged(&path)
        .await
        .expect("version 7 keeps its own schema-based repair")
        .close()
        .await;

    let mut conn = connect(&path).await;
    assert_eq!(stored_checksums(&mut conn).await, embedded_checksums());
}
