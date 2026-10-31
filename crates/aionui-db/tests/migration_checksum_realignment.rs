//! Databases created before a released migration file was rewritten must stay openable.
//!
//! `#233`/`#234` consolidated later migrations into `001_initial_schema.sql`. sqlx stores a
//! SHA-384 of every file in `_sqlx_migrations`, so a database created before that rewrite
//! reports "migration 1 was previously applied but has been modified" and refuses to start.
//! Startup must re-stamp the already-applied rows instead of stranding the user's data.

use aionui_db::init_database_staged;

#[tokio::test]
async fn realigns_checksums_of_migrations_rewritten_after_release() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("aionui-backend.db");

    let db = init_database_staged(&path).await.unwrap();
    // Simulate a database written by a build that shipped a different 001 body.
    sqlx::query("UPDATE _sqlx_migrations SET checksum = x'deadbeef' WHERE version = 1")
        .execute(db.pool())
        .await
        .unwrap();
    db.close().await;

    let reopened = init_database_staged(&path)
        .await
        .expect("a rewritten migration must not strand an already-migrated database");

    let checksum: Vec<u8> = sqlx::query_scalar("SELECT checksum FROM _sqlx_migrations WHERE version = 1")
        .fetch_one(reopened.pool())
        .await
        .unwrap();
    assert_ne!(checksum, b"\xde\xad\xbe\xef".to_vec());
    assert_eq!(checksum.len(), 48, "sqlx stores a SHA-384 digest");
    reopened.close().await;
}

#[tokio::test]
async fn keeps_failing_for_migrations_outside_the_allowlist() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("aionui-backend.db");

    let db = init_database_staged(&path).await.unwrap();
    // Version 2 was never rewritten after release: a mismatch there is real tampering and must
    // keep the strict failure instead of being silently re-stamped.
    sqlx::query("UPDATE _sqlx_migrations SET checksum = x'deadbeef' WHERE version = 2")
        .execute(db.pool())
        .await
        .unwrap();
    db.close().await;

    let err = init_database_staged(&path)
        .await
        .expect_err("tampering outside the allowlist must still fail");
    assert_eq!(err.stage(), "database.migration");
}

#[tokio::test]
async fn keeps_other_applied_rows_untouched() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("aionui-backend.db");

    let db = init_database_staged(&path).await.unwrap();
    let before: Vec<(i64, Vec<u8>)> =
        sqlx::query_as("SELECT version, checksum FROM _sqlx_migrations WHERE success = 1 ORDER BY version")
            .fetch_all(db.pool())
            .await
            .unwrap();
    sqlx::query("UPDATE _sqlx_migrations SET checksum = x'deadbeef' WHERE version = 1")
        .execute(db.pool())
        .await
        .unwrap();
    db.close().await;

    let reopened = init_database_staged(&path).await.unwrap();
    let after: Vec<(i64, Vec<u8>)> =
        sqlx::query_as("SELECT version, checksum FROM _sqlx_migrations WHERE success = 1 ORDER BY version")
            .fetch_all(reopened.pool())
            .await
            .unwrap();
    assert_eq!(before.len(), after.len());
    for (previous, current) in before.iter().zip(after.iter()) {
        assert_eq!(previous.0, current.0);
        if previous.0 != 1 {
            assert_eq!(previous.1, current.1, "migration {} must be untouched", previous.0);
        }
    }
    reopened.close().await;
}
