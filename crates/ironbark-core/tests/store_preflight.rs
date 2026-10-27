//! Offline inspection must not open or repair the original SQLite files.
use std::{collections::BTreeMap, fs, path::PathBuf};
use fs2::FileExt;
use ironbark_core::{
    paths::{private_directory, private_file, RuntimePaths},
    store::{migrations, preflight::{MigrationPreflight, MigrationPreflightReport}},
};
use rusqlite::Connection;
use tempfile::TempDir;

struct Fixture { _root: TempDir, paths: RuntimePaths }
impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let data_dir = root.path().join("data");
        let state_dir = root.path().join("state");
        let runtime_dir = root.path().join("runtime");
        let paths = RuntimePaths {
            database: data_dir.join("core.sqlite3"),
            nonce: runtime_dir.join("session.nonce"),
            socket: runtime_dir.join("core.sock"),
            lock: runtime_dir.join("core.lock"),
            data_dir, state_dir, runtime_dir,
        };
        Self { _root: root, paths }
    }
    fn database(&self) -> Connection {
        private_directory(&self.paths.data_dir).unwrap();
        let mut connection = Connection::open(&self.paths.database).unwrap();
        migrations::migrate(&mut connection).unwrap();
        connection.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);").unwrap();
        connection
    }
    fn original_bytes(&self) -> BTreeMap<String, Vec<u8>> {
        ["core.sqlite3", "core.sqlite3-wal", "core.sqlite3-shm"].into_iter()
            .filter_map(|name| {
                let path = self.paths.data_dir.join(name);
                match fs::read(path) {
                    Ok(bytes) => Some((name.to_owned(), bytes)),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                    Err(error) => panic!("original read failed: {error}"),
                }
            }).collect()
    }
}

fn assert_denied(result: ironbark_core::error::Result<MigrationPreflightReport>) {
    match result {
        Ok(MigrationPreflightReport::Existing { writable_allowed, blockers, .. }) => {
            assert!(!writable_allowed, "unsafe source was admitted to writable startup");
            assert!(!blockers.is_empty(), "a denied inspection needs an observed blocker");
        }
        Ok(MigrationPreflightReport::NewNamespace { .. }) => panic!("existing evidence was misclassified as an empty namespace"),
        Err(_) => {}, // Unsafe source parsing may fail closed without inventing schema metadata.
    }
}

#[test]
fn fresh_inspection_does_not_create_the_selected_namespace() {
    let fixture = Fixture::new();
    let report = MigrationPreflight::inspect(&fixture.paths, None).unwrap();
    assert!(matches!(report, MigrationPreflightReport::NewNamespace { .. }));
    assert!(!fixture.paths.data_dir.exists());
    assert!(!fixture.paths.state_dir.exists());
    assert!(!fixture.paths.runtime_dir.exists());
}

#[test]
fn current_schema_is_read_from_a_copy_and_every_original_companion_is_exact() {
    let fixture = Fixture::new();
    let _connection = fixture.database();
    let before = fixture.original_bytes();
    assert!(before.contains_key("core.sqlite3-shm"));
    let report = MigrationPreflight::inspect(&fixture.paths, None).unwrap();
    match report {
        MigrationPreflightReport::Existing { source_schema, writable_allowed, checked_backup_digest, blockers, .. } => {
            assert_eq!(source_schema, 1);
            assert!(!writable_allowed);
            assert!(checked_backup_digest.is_none());
            assert_eq!(blockers, ["RecoveryImageRequired"],
                "valid source must reach the missing-selected-image decision");
        }
        MigrationPreflightReport::NewNamespace { .. } => panic!("existing database disappeared"),
    }
    // Positive control for the same actual replay/schema path used by the
    // future-schema and WAL-only unknown-table producer refusals below.
    let image = fixture._root.path().join("valid-source-control");
    MigrationPreflight::create_recovery_image(&fixture.paths, &image).unwrap();
    assert!(image.join("snapshot.sqlite3").is_file());
    assert_eq!(fixture.original_bytes(), before);
}

#[test]
fn an_unregistered_table_present_only_in_wal_blocks_startup_without_repairing_shm() {
    let fixture = Fixture::new();
    let connection = fixture.database();
    connection.execute_batch("CREATE TABLE future_reference(id TEXT PRIMARY KEY, body TEXT NOT NULL); INSERT INTO future_reference VALUES ('opaque','{\"artifact_id\":\"future\"}');").unwrap();
    let before = fixture.original_bytes();
    assert!(before.get("core.sqlite3-wal").unwrap().len() > 32);
    assert_eq!(MigrationPreflight::inspect(&fixture.paths, None).unwrap_err().code, "UnknownBackupSchema");
    let destination = fixture._root.path().join("unknown-table-image");
    assert_eq!(MigrationPreflight::create_recovery_image(&fixture.paths, &destination).unwrap_err().code,
        "UnknownBackupSchema", "refusal must come from the WAL-replayed source schema, not missing backup");
    assert!(!destination.exists());
    assert_eq!(fixture.original_bytes(), before);
}

#[test]
fn a_future_schema_is_never_opened_for_migration_or_downgrade() {
    let fixture = Fixture::new();
    let connection = fixture.database();
    connection.execute_batch("PRAGMA user_version=999;").unwrap();
    let before = fixture.original_bytes();
    assert_eq!(MigrationPreflight::inspect(&fixture.paths, None).unwrap_err().code, "UnsupportedMigrationSchema");
    let destination = fixture._root.path().join("future-schema-image");
    assert_eq!(MigrationPreflight::create_recovery_image(&fixture.paths, &destination).unwrap_err().code,
        "UnsupportedMigrationSchema", "future version must be rejected before any image is published");
    assert!(!destination.exists());
    assert_eq!(fixture.original_bytes(), before);
}

#[test]
fn corrupt_database_bytes_are_not_replaced_with_an_empty_store() {
    let fixture = Fixture::new();
    private_directory(&fixture.paths.data_dir).unwrap();
    fs::write(&fixture.paths.database, b"SQLite format 3\0opaque-corrupt-original").unwrap();
    let before = fixture.original_bytes();
    assert_denied(MigrationPreflight::inspect(&fixture.paths, None));
    assert_eq!(fixture.original_bytes(), before);
}

#[test]
fn a_busy_core_namespace_is_rejected_before_sqlite_inspection() {
    let fixture = Fixture::new();
    let _connection = fixture.database();
    let lock = private_file(&fixture.paths.data_dir.join("core.lock")).unwrap();
    lock.try_lock_exclusive().unwrap();
    let before = fixture.original_bytes();
    assert!(MigrationPreflight::inspect(&fixture.paths, None).is_err());
    assert_eq!(fixture.original_bytes(), before);
}

#[test]
fn a_journal_symlink_does_not_disclose_or_modify_its_target() {
    let fixture = Fixture::new();
    drop(fixture.database());
    let outside: PathBuf = fixture._root.path().join("unrelated-private-bytes");
    fs::write(&outside, b"unrelated target stays exact").unwrap();
    std::os::unix::fs::symlink(&outside, fixture.paths.data_dir.join("core.sqlite3-wal")).unwrap();
    let before_database = fs::read(&fixture.paths.database).unwrap();
    assert_denied(MigrationPreflight::inspect(&fixture.paths, None));
    assert_eq!(fs::read(&outside).unwrap(), b"unrelated target stays exact");
    assert_eq!(fs::read(&fixture.paths.database).unwrap(), before_database);
    assert!(fs::symlink_metadata(fixture.paths.data_dir.join("core.sqlite3-wal")).unwrap().file_type().is_symlink());
}

// These cases catch accepting a merely named image, overlooking WAL/private
// bytes, or replacing originals/destinations after a failed evidence copy.
#[test]
fn checked_image_binds_raw_private_wal_and_replayed_snapshot() {
    let fixture = Fixture::new();
    let connection = fixture.database();
    connection.execute("INSERT INTO projects VALUES ('private-id','raw-private-value','/private/root',1)", []).unwrap();
    let before = fixture.original_bytes();
    let destination = fixture._root.path().join("recovery");
    MigrationPreflight::create_recovery_image(&fixture.paths, &destination).unwrap();
    assert_eq!(fixture.original_bytes(), before);
    for (name, bytes) in &before {
        assert_eq!(fs::read(destination.join("originals").join(name)).unwrap(), *bytes);
    }
    let snapshot = Connection::open(destination.join("snapshot.sqlite3")).unwrap();
    assert_eq!(snapshot.query_row("SELECT name FROM projects", [], |r| r.get::<_, String>(0)).unwrap(), "raw-private-value");
    drop(snapshot);
    match MigrationPreflight::inspect(&fixture.paths, Some(&destination)).unwrap() {
        MigrationPreflightReport::Existing { writable_allowed, checked_backup_digest, blockers, .. } => {
            assert!(writable_allowed, "{blockers:?}");
            assert!(checked_backup_digest.is_some());
        }
        _ => panic!("existing source was lost"),
    }
    assert_eq!(fixture.original_bytes(), before);
}

#[test]
fn missing_selected_image_is_not_implicitly_created() {
    let fixture = Fixture::new();
    let _connection = fixture.database();
    let destination = fixture._root.path().join("absent-image");
    let before = fixture.original_bytes();
    assert_denied(MigrationPreflight::inspect(&fixture.paths, Some(&destination)));
    assert!(!destination.exists());
    assert_eq!(fixture.original_bytes(), before);
}

#[test]
fn existing_source_without_checked_recovery_is_not_admitted() {
    let fixture = Fixture::new();
    let _connection = fixture.database();
    assert_denied(MigrationPreflight::inspect(&fixture.paths, None));
}

#[test]
fn stale_image_denies_even_when_current_source_is_valid() {
    let fixture = Fixture::new();
    let connection = fixture.database();
    let destination = fixture._root.path().join("recovery");
    MigrationPreflight::create_recovery_image(&fixture.paths, &destination).unwrap();
    connection.execute("INSERT INTO projects VALUES ('later','later','/later',1)", []).unwrap();
    let before = fixture.original_bytes();
    assert_denied(MigrationPreflight::inspect(&fixture.paths, Some(&destination)));
    assert_eq!(fixture.original_bytes(), before);
}

#[test]
fn tampering_with_image_raw_bytes_denies_without_repair() {
    let fixture = Fixture::new();
    let _connection = fixture.database();
    let destination = fixture._root.path().join("recovery");
    MigrationPreflight::create_recovery_image(&fixture.paths, &destination).unwrap();
    fs::write(destination.join("originals/core.sqlite3"), b"tampered").unwrap();
    let before = fixture.original_bytes();
    assert_denied(MigrationPreflight::inspect(&fixture.paths, Some(&destination)));
    assert_eq!(fixture.original_bytes(), before);
    assert_eq!(fs::read(destination.join("originals/core.sqlite3")).unwrap(), b"tampered");
}

#[test]
fn image_creation_never_replaces_an_existing_destination() {
    let fixture = Fixture::new();
    let _connection = fixture.database();
    let destination = fixture._root.path().join("selected");
    fs::write(&destination, b"pre-existing destination").unwrap();
    let before = fixture.original_bytes();
    assert!(MigrationPreflight::create_recovery_image(&fixture.paths, &destination).is_err());
    assert_eq!(fs::read(destination).unwrap(), b"pre-existing destination");
    assert_eq!(fixture.original_bytes(), before);
}

#[test]
fn missing_typed_cas_root_blocks_copy_and_preserves_source() {
    let fixture = Fixture::new();
    let connection = fixture.database();
    connection.execute_batch("INSERT INTO projects VALUES ('p','private','/private',1); INSERT INTO artifacts VALUES ('a','p',NULL,'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa','aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa');").unwrap();
    let before = fixture.original_bytes();
    assert_denied(MigrationPreflight::inspect(&fixture.paths, None));
    let destination = fixture._root.path().join("incomplete");
    assert!(MigrationPreflight::create_recovery_image(&fixture.paths, &destination).is_err());
    assert!(!destination.exists());
    assert!(!fixture.paths.data_dir.join("objects").exists());
    assert_eq!(fixture.original_bytes(), before);
}

#[test]
fn empty_closure_does_not_create_a_source_cas_directory() {
    let fixture = Fixture::new();
    let _connection = fixture.database();
    MigrationPreflight::inspect(&fixture.paths, None).unwrap();
    MigrationPreflight::create_recovery_image(&fixture.paths, &fixture._root.path().join("image")).unwrap();
    assert!(!fixture.paths.data_dir.join("objects").exists());
}

#[test]
fn unsupported_schema_zero_is_not_initialized_in_place() {
    let fixture = Fixture::new();
    private_directory(&fixture.paths.data_dir).unwrap();
    let connection = Connection::open(&fixture.paths.database).unwrap();
    connection.execute_batch("CREATE TABLE historical_private(value TEXT); INSERT INTO historical_private VALUES ('preserve me');").unwrap();
    let before = fixture.original_bytes();
    assert_denied(MigrationPreflight::inspect(&fixture.paths, None));
    assert_eq!(fixture.original_bytes(), before);
}

#[test]
fn typed_cas_image_is_exact_and_tampered_member_blocks_admission() {
    use sha2::{Digest as _, Sha256};
    let fixture = Fixture::new();
    let connection = fixture.database();
    let payload = b"private raw CAS original; do not sanitize";
    let digest = format!("{:x}", Sha256::digest(payload));
    connection.execute("INSERT INTO projects VALUES ('p','private','/private',1)", []).unwrap();
    connection.execute("INSERT INTO artifacts VALUES ('a','p',NULL,?1,?1)", [&digest]).unwrap();
    let objects = fixture.paths.data_dir.join("objects");
    private_directory(&objects).unwrap();
    fs::write(objects.join(&digest), payload).unwrap();
    let destination = fixture._root.path().join("recovery");
    let before = fixture.original_bytes();
    let manifest = MigrationPreflight::create_recovery_image(&fixture.paths, &destination).unwrap();
    assert_eq!(manifest.objects.len(), 1);
    assert_eq!(manifest.references.len(), 1);
    assert_eq!(fs::read(destination.join("objects").join(&digest)).unwrap(), payload);
    assert!(MigrationPreflight::inspect(&fixture.paths, Some(&destination)).unwrap().writable_allowed());
    fs::write(destination.join("objects").join(&digest), b"tampered").unwrap();
    assert_denied(MigrationPreflight::inspect(&fixture.paths, Some(&destination)));
    assert_eq!(fs::read(objects.join(&digest)).unwrap(), payload);
    assert_eq!(fixture.original_bytes(), before);
}

#[test]
fn insufficient_capacity_refuses_sparse_copy_before_reading_or_repairing_it() {
    use std::io::Read;
    let fixture = Fixture::new();
    drop(fixture.database());
    // Logical size alone exceeds measured free capacity. Sparse extension uses
    // no host-disk filling, mount privilege, injected capacity or production hook.
    let free = fs2::available_space(fixture._root.path()).unwrap();
    let file = fs::OpenOptions::new().write(true).open(&fixture.paths.database).unwrap();
    file.set_len(free.checked_add(1).unwrap()).unwrap();
    drop(file);
    let prefix = |path: &std::path::Path| {
        let mut bytes = Vec::new();
        fs::File::open(path).unwrap().take(4096).read_to_end(&mut bytes).unwrap();
        bytes
    };
    let before = prefix(&fixture.paths.database);
    let size = fs::metadata(&fixture.paths.database).unwrap().len();
    let error = MigrationPreflight::inspect(&fixture.paths, None).unwrap_err();
    assert_eq!(error.code, "InsufficientSpace");
    let destination = fixture._root.path().join("no-space-image");
    assert_eq!(MigrationPreflight::create_recovery_image(&fixture.paths, &destination).unwrap_err().code, "InsufficientSpace");
    assert!(!destination.exists());
    assert_eq!(fs::metadata(&fixture.paths.database).unwrap().len(), size);
    assert_eq!(prefix(&fixture.paths.database), before);
}

#[test]
fn destination_parent_symlink_never_copies_into_its_target() {
    let fixture = Fixture::new();
    let _connection = fixture.database();
    let target = fixture._root.path().join("outside");
    private_directory(&target).unwrap();
    let alias = fixture._root.path().join("alias");
    std::os::unix::fs::symlink(&target, &alias).unwrap();
    let before = fixture.original_bytes();
    assert!(MigrationPreflight::create_recovery_image(&fixture.paths, &alias.join("image")).is_err());
    assert!(!target.join("image").exists());
    assert_eq!(fixture.original_bytes(), before);
}

#[test]
fn image_from_identical_bytes_in_another_namespace_is_not_current_evidence() {
    let first = Fixture::new();
    drop(first.database());
    let image = first._root.path().join("image");
    MigrationPreflight::create_recovery_image(&first.paths, &image).unwrap();
    let second = Fixture::new();
    private_directory(&second.paths.data_dir).unwrap();
    fs::copy(&first.paths.database, &second.paths.database).unwrap();
    let before = second.original_bytes();
    assert_denied(MigrationPreflight::inspect(&second.paths, Some(&image)));
    assert_eq!(second.original_bytes(), before);
}

#[test]
fn corrupt_wal_frame_is_not_silently_ignored_as_an_older_valid_database() {
    let fixture = Fixture::new();
    let connection = fixture.database();
    connection.execute("INSERT INTO projects VALUES ('wal-only','private','/private',1)", []).unwrap();
    let wal = fixture.paths.data_dir.join("core.sqlite3-wal");
    let mut bytes = fs::read(&wal).unwrap();
    assert!(bytes.len() > 56);
    bytes[56] ^= 1; // first frame page payload; stored checksum is now invalid
    fs::write(&wal, bytes).unwrap();
    let before = fixture.original_bytes();
    assert_denied(MigrationPreflight::inspect(&fixture.paths, None));
    let destination = fixture._root.path().join("invalid-wal-image");
    assert!(MigrationPreflight::create_recovery_image(&fixture.paths, &destination).is_err());
    assert!(!destination.exists());
    assert_eq!(fixture.original_bytes(), before);
}
