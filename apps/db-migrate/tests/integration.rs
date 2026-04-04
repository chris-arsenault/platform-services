use db_migrate::ops::{self, OPS_SCHEMA};
use db_migrate::storage::{FileStore, MemoryFileStore};
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;
use tokio_postgres::NoTls;

async fn setup() -> (
    tokio_postgres::Client,
    testcontainers::ContainerAsync<Postgres>,
) {
    let container = Postgres::default().start().await.unwrap();
    let host = container.get_host().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let connstr =
        format!("host={host} port={port} user=postgres password=postgres dbname=postgres");
    let (client, connection) = tokio_postgres::connect(&connstr, NoTls).await.unwrap();
    tokio::spawn(async move {
        connection.await.ok();
    });
    // Init ops schema on same db for simplicity
    client.batch_execute(OPS_SCHEMA).await.unwrap();
    (client, container)
}

fn make_store(project: &str, files: Vec<(&str, &str)>) -> MemoryFileStore {
    let mut store = MemoryFileStore::new();
    for (name, content) in files {
        store.add_file(&format!("migrations/{project}/{name}"), content);
    }
    store
}

fn make_store_with_rollbacks(
    project: &str,
    migrations: Vec<(&str, &str)>,
    rollbacks: Vec<(&str, &str)>,
) -> MemoryFileStore {
    let mut store = MemoryFileStore::new();
    for (name, content) in migrations {
        store.add_file(&format!("migrations/{project}/{name}"), content);
    }
    for (name, content) in rollbacks {
        store.add_file(&format!("migrations/{project}/rollback/{name}"), content);
    }
    store
}

fn make_store_with_seeds(
    project: &str,
    migrations: Vec<(&str, &str)>,
    seeds: Vec<(&str, &str)>,
) -> MemoryFileStore {
    let mut store = MemoryFileStore::new();
    for (name, content) in migrations {
        store.add_file(&format!("migrations/{project}/{name}"), content);
    }
    for (name, content) in seeds {
        store.add_file(&format!("migrations/{project}/seed/{name}"), content);
    }
    store
}

// =============================================================================
// Migrate tests
// =============================================================================

#[tokio::test]
async fn test_migrate_applies_in_order() {
    let (client, _c) = setup().await;
    let store = make_store(
        "testproj",
        vec![
            (
                "001_create.sql",
                "CREATE TABLE items (id SERIAL PRIMARY KEY, name TEXT);",
            ),
            (
                "002_add_col.sql",
                "ALTER TABLE items ADD COLUMN active BOOLEAN DEFAULT TRUE;",
            ),
        ],
    );

    let result = ops::migrate(&client, &client, &store, "testproj")
        .await
        .unwrap();
    assert_eq!(result.applied, Some(2));

    // Verify tables exist
    let rows = client
        .query(
            "SELECT column_name FROM information_schema.columns WHERE table_name = 'items' ORDER BY ordinal_position",
            &[],
        )
        .await
        .unwrap();
    let cols: Vec<String> = rows.iter().map(|r| r.get(0)).collect();
    assert_eq!(cols, vec!["id", "name", "active"]);
}

#[tokio::test]
async fn test_migrate_is_idempotent() {
    let (client, _c) = setup().await;
    let store = make_store(
        "testproj",
        vec![(
            "001_create.sql",
            "CREATE TABLE things (id SERIAL PRIMARY KEY);",
        )],
    );

    let r1 = ops::migrate(&client, &client, &store, "testproj")
        .await
        .unwrap();
    assert_eq!(r1.applied, Some(1));

    let r2 = ops::migrate(&client, &client, &store, "testproj")
        .await
        .unwrap();
    assert_eq!(r2.applied, Some(0));
}

#[tokio::test]
async fn test_migrate_detects_checksum_mismatch() {
    let (client, _c) = setup().await;
    let store1 = make_store(
        "testproj",
        vec![(
            "001_create.sql",
            "CREATE TABLE widgets (id SERIAL PRIMARY KEY);",
        )],
    );
    ops::migrate(&client, &client, &store1, "testproj")
        .await
        .unwrap();

    // Same filename, different content
    let store2 = make_store(
        "testproj",
        vec![(
            "001_create.sql",
            "CREATE TABLE widgets (id SERIAL PRIMARY KEY, name TEXT);",
        )],
    );
    let result = ops::migrate(&client, &client, &store2, "testproj").await;
    assert!(result.is_err());
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("Checksum mismatch"));
}

#[tokio::test]
async fn test_migrate_tracks_in_schema_migrations() {
    let (client, _c) = setup().await;
    let store = make_store(
        "testproj",
        vec![(
            "001_create.sql",
            "CREATE TABLE tracked (id SERIAL PRIMARY KEY);",
        )],
    );

    ops::migrate(&client, &client, &store, "testproj")
        .await
        .unwrap();

    let rows = client
        .query(
            "SELECT filename, checksum, noop FROM schema_migrations",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get::<_, String>(0), "001_create.sql");
    assert!(!rows[0].get::<_, bool>(2));
}

#[tokio::test]
async fn test_migrate_rolls_back_on_sql_error() {
    let (client, _c) = setup().await;
    let store = make_store("testproj", vec![("001_bad.sql", "THIS IS NOT VALID SQL;")]);

    let result = ops::migrate(&client, &client, &store, "testproj").await;
    assert!(result.is_err());

    // Nothing should be tracked
    let rows = client
        .query("SELECT * FROM schema_migrations", &[])
        .await
        .unwrap();
    assert!(rows.is_empty());
}

#[tokio::test]
async fn test_migrate_no_files() {
    let (client, _c) = setup().await;
    let store = MemoryFileStore::new();

    let result = ops::migrate(&client, &client, &store, "testproj")
        .await
        .unwrap();
    assert_eq!(result.applied, Some(0));
}

// =============================================================================
// Noop tests
// =============================================================================

#[tokio::test]
async fn test_noop_records_without_executing() {
    let (client, _c) = setup().await;
    let store = make_store(
        "testproj",
        vec![(
            "001_create.sql",
            "CREATE TABLE noop_test (id SERIAL PRIMARY KEY);",
        )],
    );

    let result = ops::noop(
        &client,
        &client,
        &store,
        "testproj",
        "001_create.sql",
        "Baseline import",
    )
    .await
    .unwrap();
    assert_eq!(result.operation, "noop");

    // Table should NOT exist
    let rows = client
        .query(
            "SELECT 1 FROM information_schema.tables WHERE table_name = 'noop_test'",
            &[],
        )
        .await
        .unwrap();
    assert!(rows.is_empty());

    // But tracking record should exist with noop=true
    let tracked = client
        .query(
            "SELECT noop, comment FROM schema_migrations WHERE filename = '001_create.sql'",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(tracked.len(), 1);
    assert!(tracked[0].get::<_, bool>(0));
    assert_eq!(
        tracked[0].get::<_, Option<String>>(1),
        Some("Baseline import".into())
    );
}

#[tokio::test]
async fn test_noop_is_idempotent() {
    let (client, _c) = setup().await;
    let store = make_store(
        "testproj",
        vec![(
            "001_create.sql",
            "CREATE TABLE noop_idem (id SERIAL PRIMARY KEY);",
        )],
    );

    ops::noop(
        &client,
        &client,
        &store,
        "testproj",
        "001_create.sql",
        "First",
    )
    .await
    .unwrap();
    let r2 = ops::noop(
        &client,
        &client,
        &store,
        "testproj",
        "001_create.sql",
        "Second",
    )
    .await
    .unwrap();
    assert_eq!(r2.status.as_deref(), Some("already_recorded"));
}

#[tokio::test]
async fn test_noop_then_migrate_skips() {
    let (client, _c) = setup().await;

    // Manually create the table (simulating a restore)
    client
        .batch_execute("CREATE TABLE preexisting (id SERIAL PRIMARY KEY);")
        .await
        .unwrap();

    let store = make_store(
        "testproj",
        vec![(
            "001_create.sql",
            "CREATE TABLE preexisting (id SERIAL PRIMARY KEY);",
        )],
    );

    // Noop it
    ops::noop(
        &client,
        &client,
        &store,
        "testproj",
        "001_create.sql",
        "Already exists",
    )
    .await
    .unwrap();

    // Migrate should skip it
    let result = ops::migrate(&client, &client, &store, "testproj")
        .await
        .unwrap();
    assert_eq!(result.applied, Some(0));
}

// =============================================================================
// Rollback tests
// =============================================================================

#[tokio::test]
async fn test_rollback_reverses_migrations() {
    let (client, _c) = setup().await;
    let store = make_store_with_rollbacks(
        "testproj",
        vec![(
            "001_create.sql",
            "CREATE TABLE rollback_test (id SERIAL PRIMARY KEY);",
        )],
        vec![("001_create.sql", "DROP TABLE rollback_test;")],
    );

    ops::migrate(&client, &client, &store, "testproj")
        .await
        .unwrap();

    let result = ops::rollback(&client, &client, &store, "testproj", None)
        .await
        .unwrap();
    assert_eq!(result.rolled_back, Some(1));

    // Table should be gone
    let rows = client
        .query(
            "SELECT 1 FROM information_schema.tables WHERE table_name = 'rollback_test'",
            &[],
        )
        .await
        .unwrap();
    assert!(rows.is_empty());

    // Tracking should be empty
    let tracked = client
        .query("SELECT * FROM schema_migrations", &[])
        .await
        .unwrap();
    assert!(tracked.is_empty());
}

#[tokio::test]
async fn test_rollback_to_target() {
    let (client, _c) = setup().await;
    let store = make_store_with_rollbacks(
        "testproj",
        vec![
            (
                "001_create.sql",
                "CREATE TABLE rb_target (id SERIAL PRIMARY KEY);",
            ),
            (
                "002_add_col.sql",
                "ALTER TABLE rb_target ADD COLUMN name TEXT;",
            ),
        ],
        vec![
            ("001_create.sql", "DROP TABLE rb_target;"),
            ("002_add_col.sql", "ALTER TABLE rb_target DROP COLUMN name;"),
        ],
    );

    ops::migrate(&client, &client, &store, "testproj")
        .await
        .unwrap();

    // Roll back to 001 (only 002 should be rolled back)
    let result = ops::rollback(&client, &client, &store, "testproj", Some("001_create.sql"))
        .await
        .unwrap();
    assert_eq!(result.rolled_back, Some(1));

    // Table should still exist but without the name column
    let cols = client
        .query(
            "SELECT column_name FROM information_schema.columns WHERE table_name = 'rb_target' ORDER BY ordinal_position",
            &[],
        )
        .await
        .unwrap();
    let col_names: Vec<String> = cols.iter().map(|r| r.get(0)).collect();
    assert_eq!(col_names, vec!["id"]);
}

#[tokio::test]
async fn test_rollback_nothing() {
    let (client, _c) = setup().await;
    client.batch_execute(ops::LOCAL_TRACKING).await.unwrap();
    let store = MemoryFileStore::new();

    let result = ops::rollback(&client, &client, &store, "testproj", None)
        .await
        .unwrap();
    assert_eq!(result.rolled_back, Some(0));
}

// =============================================================================
// Seed tests
// =============================================================================

#[tokio::test]
async fn test_seed_runs_files() {
    let (client, _c) = setup().await;
    client
        .batch_execute("CREATE TABLE seed_test (id SERIAL PRIMARY KEY, name TEXT);")
        .await
        .unwrap();

    let store = make_store_with_seeds(
        "testproj",
        vec![],
        vec![(
            "001_data.sql",
            "INSERT INTO seed_test (name) VALUES ('alice'), ('bob');",
        )],
    );

    let result = ops::seed(&client, &client, &store, "testproj")
        .await
        .unwrap();
    assert_eq!(result.applied, Some(1));

    let rows = client
        .query("SELECT name FROM seed_test ORDER BY name", &[])
        .await
        .unwrap();
    let names: Vec<String> = rows.iter().map(|r| r.get(0)).collect();
    assert_eq!(names, vec!["alice", "bob"]);
}

#[tokio::test]
async fn test_seed_no_files() {
    let (client, _c) = setup().await;
    let store = MemoryFileStore::new();

    let result = ops::seed(&client, &client, &store, "testproj")
        .await
        .unwrap();
    assert_eq!(result.applied, Some(0));
}

// =============================================================================
// Audit tests
// =============================================================================

#[tokio::test]
async fn test_audit_records_operations() {
    let (client, _c) = setup().await;
    let store = make_store(
        "testproj",
        vec![(
            "001_create.sql",
            "CREATE TABLE audit_test (id SERIAL PRIMARY KEY);",
        )],
    );

    ops::migrate(&client, &client, &store, "testproj")
        .await
        .unwrap();

    let rows = client
        .query(
            "SELECT project, operation, filename, status FROM migration_audit ORDER BY id",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get::<_, String>(0), "testproj");
    assert_eq!(rows[0].get::<_, String>(1), "migrate");
    assert_eq!(
        rows[0].get::<_, Option<String>>(2),
        Some("001_create.sql".into())
    );
    assert_eq!(rows[0].get::<_, String>(3), "success");
}

// =============================================================================
// Checksum tests
// =============================================================================

#[tokio::test]
async fn test_checksum_deterministic() {
    let c1 = ops::checksum("SELECT 1;");
    let c2 = ops::checksum("SELECT 1;");
    assert_eq!(c1, c2);
    assert_eq!(c1.len(), 16);
}

#[tokio::test]
async fn test_checksum_differs() {
    let c1 = ops::checksum("SELECT 1;");
    let c2 = ops::checksum("SELECT 2;");
    assert_ne!(c1, c2);
}

// =============================================================================
// File store depth filtering tests
// =============================================================================

#[tokio::test]
async fn test_file_store_excludes_subdirectories() {
    let mut store = MemoryFileStore::new();
    store.add_file(
        "migrations/proj/001_create.sql",
        "CREATE TABLE t1 (id INT);",
    );
    store.add_file("migrations/proj/rollback/001_create.sql", "DROP TABLE t1;");
    store.add_file(
        "migrations/proj/seed/001_data.sql",
        "INSERT INTO t1 VALUES (1);",
    );

    let files = store.list_files("migrations/proj/").await.unwrap();
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].filename, "001_create.sql");
}
