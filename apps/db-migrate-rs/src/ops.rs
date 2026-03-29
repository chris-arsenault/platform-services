use crate::storage::{CredentialStore, FileStore};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use tokio_postgres::Client;
use tracing::{error, info, warn};

pub const LOCAL_TRACKING: &str = "
CREATE TABLE IF NOT EXISTS schema_migrations (
  id SERIAL PRIMARY KEY,
  filename TEXT NOT NULL UNIQUE,
  checksum TEXT NOT NULL,
  noop BOOLEAN NOT NULL DEFAULT FALSE,
  comment TEXT,
  applied_at TIMESTAMPTZ DEFAULT NOW(),
  duration_ms INTEGER
);
";

pub const OPS_SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS migration_audit (
  id SERIAL PRIMARY KEY,
  project TEXT NOT NULL,
  operation TEXT NOT NULL,
  filename TEXT,
  checksum TEXT,
  status TEXT NOT NULL,
  comment TEXT,
  error_message TEXT,
  duration_ms INTEGER,
  created_at TIMESTAMPTZ DEFAULT NOW()
);
CREATE INDEX IF NOT EXISTS idx_audit_project ON migration_audit (project, created_at DESC);

CREATE TABLE IF NOT EXISTS seed_runs (
  id SERIAL PRIMARY KEY,
  project TEXT NOT NULL,
  filename TEXT NOT NULL,
  checksum TEXT NOT NULL,
  created_at TIMESTAMPTZ DEFAULT NOW()
);
CREATE INDEX IF NOT EXISTS idx_seed_project ON seed_runs (project, created_at DESC);
";

#[derive(Deserialize)]
pub struct ProjectConfig {
    pub db_name: String,
}

#[derive(Serialize, Default, Debug)]
pub struct Response {
    pub operation: String,
    pub project: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub applied: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rolled_back: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub baselined: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub db: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
}

pub struct AuditEntry<'a> {
    pub project: &'a str,
    pub operation: &'a str,
    pub filename: Option<&'a str>,
    pub checksum: Option<&'a str>,
    pub status: &'a str,
    pub error_message: Option<&'a str>,
    pub duration_ms: Option<i32>,
    pub comment: Option<&'a str>,
}

pub type ConnectFn = dyn Fn(
        &str,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<Client, Box<dyn std::error::Error + Send + Sync>>,
                > + Send,
        >,
    > + Send
    + Sync;

pub fn checksum(content: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(content.as_bytes());
    format!("{:x}", hasher.finalize())[..16].to_string()
}

pub fn lock_id(project: &str) -> i64 {
    let mut h: i64 = 0;
    for b in project.bytes() {
        h = h.wrapping_mul(31).wrapping_add(b as i64);
    }
    h.abs()
}

pub async fn acquire_lock(client: &Client, project: &str) -> Result<(), tokio_postgres::Error> {
    let id = lock_id(project);
    client
        .execute("SELECT pg_advisory_lock($1)", &[&id])
        .await?;
    info!(project, lock_id = id, "Lock acquired");
    Ok(())
}

pub async fn release_lock(client: &Client, project: &str) -> Result<(), tokio_postgres::Error> {
    let id = lock_id(project);
    client
        .execute("SELECT pg_advisory_unlock($1)", &[&id])
        .await?;
    Ok(())
}

pub async fn audit(ops: &Client, entry: AuditEntry<'_>) {
    if let Err(e) = ops
        .execute(
            "INSERT INTO migration_audit (project, operation, filename, checksum, status, comment, error_message, duration_ms)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
            &[
                &entry.project,
                &entry.operation,
                &entry.filename,
                &entry.checksum,
                &entry.status,
                &entry.comment,
                &entry.error_message,
                &entry.duration_ms,
            ],
        )
        .await
    {
        warn!("Audit write failed (non-fatal): {e}");
    }
}

pub async fn ensure_database(
    master: &Client,
    project: &str,
    db_name: &str,
    creds: &dyn CredentialStore,
    connect_fn: &ConnectFn,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Create database if needed
    let rows = master
        .query("SELECT 1 FROM pg_database WHERE datname = $1", &[&db_name])
        .await?;
    if rows.is_empty() {
        info!(db = db_name, "Creating database");
        master
            .batch_execute(&format!("CREATE DATABASE \"{db_name}\""))
            .await?;
    }

    // Create app role if needed
    let role_name = format!("{project}_app");
    let role_rows = master
        .query("SELECT 1 FROM pg_roles WHERE rolname = $1", &[&role_name])
        .await?;
    if role_rows.is_empty() {
        let password = generate_password();
        info!(project, role = role_name, "Creating application role");

        master
            .batch_execute(&format!(
                "CREATE ROLE \"{role_name}\" LOGIN PASSWORD '{password}'"
            ))
            .await?;

        let ssm_prefix = format!("/platform/db/{project}");
        creds
            .put_param(&format!("{ssm_prefix}/username"), &role_name)
            .await?;
        creds
            .put_secret(&format!("{ssm_prefix}/password"), &password)
            .await?;
        creds
            .put_param(&format!("{ssm_prefix}/database"), db_name)
            .await?;

        info!(
            project,
            role = role_name,
            "App role created and credentials published"
        );
    }

    // Create reader role if needed
    let reader_name = format!("{project}_reader");
    let reader_rows = master
        .query(
            "SELECT 1 FROM pg_roles WHERE rolname = $1",
            &[&reader_name],
        )
        .await?;
    if reader_rows.is_empty() {
        let password = generate_password();
        info!(project, role = reader_name, "Creating reader role");

        master
            .batch_execute(&format!(
                "CREATE ROLE \"{reader_name}\" LOGIN PASSWORD '{password}'"
            ))
            .await?;

        let ssm_prefix = format!("/platform/db/{project}/reader");
        creds
            .put_param(&format!("{ssm_prefix}/username"), &reader_name)
            .await?;
        creds
            .put_secret(&format!("{ssm_prefix}/password"), &password)
            .await?;

        info!(
            project,
            role = reader_name,
            "Reader role created and credentials published"
        );
    }

    // Always ensure grants
    master
        .batch_execute(&format!(
            "GRANT ALL PRIVILEGES ON DATABASE \"{db_name}\" TO \"{role_name}\";
             GRANT CONNECT ON DATABASE \"{db_name}\" TO \"{reader_name}\";"
        ))
        .await?;

    // Grant app role membership to admin so ALTER DEFAULT PRIVILEGES FOR ROLE works in PG16
    master
        .batch_execute(&format!("GRANT \"{role_name}\" TO CURRENT_USER"))
        .await?;

    let db = connect_fn(db_name).await?;
    db.batch_execute(&format!(
        "GRANT ALL ON SCHEMA public TO \"{role_name}\";
         ALTER DEFAULT PRIVILEGES IN SCHEMA public GRANT ALL ON TABLES TO \"{role_name}\";
         ALTER DEFAULT PRIVILEGES IN SCHEMA public GRANT ALL ON SEQUENCES TO \"{role_name}\";
         GRANT ALL ON ALL TABLES IN SCHEMA public TO \"{role_name}\";
         GRANT ALL ON ALL SEQUENCES IN SCHEMA public TO \"{role_name}\";"
    ))
    .await?;

    db.batch_execute(&format!(
        "GRANT USAGE ON SCHEMA public TO \"{reader_name}\";
         GRANT SELECT ON ALL TABLES IN SCHEMA public TO \"{reader_name}\";
         GRANT SELECT ON ALL SEQUENCES IN SCHEMA public TO \"{reader_name}\";
         ALTER DEFAULT PRIVILEGES IN SCHEMA public GRANT SELECT ON TABLES TO \"{reader_name}\";
         ALTER DEFAULT PRIVILEGES IN SCHEMA public GRANT SELECT ON SEQUENCES TO \"{reader_name}\";
         ALTER DEFAULT PRIVILEGES FOR ROLE \"{role_name}\" IN SCHEMA public GRANT SELECT ON TABLES TO \"{reader_name}\";
         ALTER DEFAULT PRIVILEGES FOR ROLE \"{role_name}\" IN SCHEMA public GRANT SELECT ON SEQUENCES TO \"{reader_name}\";"
    ))
    .await?;

    Ok(())
}

fn generate_password() -> String {
    use rand::Rng;
    const CHARSET: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    let mut rng = rand::thread_rng();
    (0..32)
        .map(|_| CHARSET[rng.gen_range(0..CHARSET.len())] as char)
        .collect()
}

/// Run pending migrations. Core logic extracted for testability.
pub async fn migrate(
    db: &Client,
    ops: &Client,
    files: &dyn FileStore,
    project: &str,
) -> Result<Response, Box<dyn std::error::Error + Send + Sync>> {
    db.batch_execute(LOCAL_TRACKING).await?;

    let migration_files = files.list_files(&format!("migrations/{project}/")).await?;
    info!(
        project,
        count = migration_files.len(),
        "Migration files found"
    );

    let applied_rows = db
        .query("SELECT filename, checksum FROM schema_migrations", &[])
        .await?;
    let applied: HashMap<String, String> = applied_rows
        .iter()
        .map(|r| (r.get::<_, String>(0), r.get::<_, String>(1)))
        .collect();

    let mut count = 0i32;
    for file in &migration_files {
        let sql = files.read_file(&file.key).await?;
        let h = checksum(&sql);

        if let Some(existing) = applied.get(&file.filename) {
            if existing != &h {
                let msg = format!("Checksum mismatch for {}", file.filename);
                audit(
                    ops,
                    AuditEntry {
                        project,
                        operation: "migrate",
                        filename: Some(&file.filename),
                        checksum: Some(&h),
                        status: "error",
                        error_message: Some(&msg),
                        duration_ms: None,
                        comment: None,
                    },
                )
                .await;
                return Err(msg.into());
            }
            continue;
        }

        info!(project, file = file.filename, "Applying");
        let start = std::time::Instant::now();

        db.batch_execute("BEGIN").await?;
        match db.batch_execute(&sql).await {
            Ok(()) => {
                let dur = start.elapsed().as_millis() as i32;
                db.execute(
                    "INSERT INTO schema_migrations (filename, checksum, duration_ms) VALUES ($1, $2, $3)",
                    &[&file.filename, &h, &dur],
                )
                .await?;
                db.batch_execute("COMMIT").await?;
                count += 1;
                info!(project, file = file.filename, duration_ms = dur, "Applied");
                audit(
                    ops,
                    AuditEntry {
                        project,
                        operation: "migrate",
                        filename: Some(&file.filename),
                        checksum: Some(&h),
                        status: "success",
                        error_message: None,
                        duration_ms: Some(dur),
                        comment: None,
                    },
                )
                .await;
            }
            Err(e) => {
                db.batch_execute("ROLLBACK").await.ok();
                let msg = e.to_string();
                error!(project, file = file.filename, error = msg, "Failed");
                audit(
                    ops,
                    AuditEntry {
                        project,
                        operation: "migrate",
                        filename: Some(&file.filename),
                        checksum: Some(&h),
                        status: "error",
                        error_message: Some(&msg),
                        duration_ms: None,
                        comment: None,
                    },
                )
                .await;
                return Err(e.into());
            }
        }
    }

    info!(
        project,
        applied = count,
        total = migration_files.len(),
        "Migrate complete"
    );
    Ok(Response {
        operation: "migrate".into(),
        project: project.into(),
        applied: Some(count),
        ..Default::default()
    })
}

/// Record a migration as applied without executing it.
pub async fn noop(
    db: &Client,
    ops: &Client,
    files: &dyn FileStore,
    project: &str,
    target: &str,
    comment: &str,
) -> Result<Response, Box<dyn std::error::Error + Send + Sync>> {
    db.batch_execute(LOCAL_TRACKING).await?;

    let existing = db
        .query(
            "SELECT 1 FROM schema_migrations WHERE filename = $1",
            &[&target],
        )
        .await?;
    if !existing.is_empty() {
        info!(project, file = target, "Already recorded");
        return Ok(Response {
            operation: "noop".into(),
            project: project.into(),
            file: Some(target.into()),
            status: Some("already_recorded".into()),
            ..Default::default()
        });
    }

    let migration_files = files.list_files(&format!("migrations/{project}/")).await?;
    let file = migration_files
        .iter()
        .find(|f| f.filename == target)
        .ok_or_else(|| format!("Migration file not found: migrations/{project}/{target}"))?;

    let sql = files.read_file(&file.key).await?;
    let h = checksum(&sql);

    info!(project, file = target, comment, "Recording noop");
    db.execute(
        "INSERT INTO schema_migrations (filename, checksum, noop, comment, duration_ms) VALUES ($1, $2, TRUE, $3, 0)",
        &[&target, &h, &comment],
    )
    .await?;
    audit(
        ops,
        AuditEntry {
            project,
            operation: "noop",
            filename: Some(target),
            checksum: Some(&h),
            status: "success",
            error_message: None,
            duration_ms: Some(0),
            comment: Some(comment),
        },
    )
    .await;

    Ok(Response {
        operation: "noop".into(),
        project: project.into(),
        file: Some(target.into()),
        comment: Some(comment.into()),
        ..Default::default()
    })
}

/// Roll back migrations.
pub async fn rollback(
    db: &Client,
    ops: &Client,
    files: &dyn FileStore,
    project: &str,
    target: Option<&str>,
) -> Result<Response, Box<dyn std::error::Error + Send + Sync>> {
    let applied_rows = db
        .query(
            "SELECT filename FROM schema_migrations ORDER BY filename DESC",
            &[],
        )
        .await?;
    if applied_rows.is_empty() {
        info!(project, "Nothing to roll back");
        return Ok(Response {
            operation: "rollback".into(),
            project: project.into(),
            rolled_back: Some(0),
            ..Default::default()
        });
    }

    let rollback_files = files
        .list_files(&format!("migrations/{project}/rollback/"))
        .await?;
    let rollback_map: HashMap<String, String> = rollback_files
        .into_iter()
        .map(|f| (f.filename, f.key))
        .collect();

    let mut count = 0i32;
    for row in &applied_rows {
        let filename: String = row.get(0);
        if let Some(t) = target {
            if filename.as_str() <= t {
                break;
            }
        }

        let rollback_key = rollback_map
            .get(&filename)
            .ok_or_else(|| format!("No rollback file for {filename}"))?;

        let sql = files.read_file(rollback_key).await?;
        info!(project, file = filename, "Rolling back");
        let start = std::time::Instant::now();

        db.batch_execute("BEGIN").await?;
        match db.batch_execute(&sql).await {
            Ok(()) => {
                db.execute(
                    "DELETE FROM schema_migrations WHERE filename = $1",
                    &[&filename],
                )
                .await?;
                db.batch_execute("COMMIT").await?;
                count += 1;
                let dur = start.elapsed().as_millis() as i32;
                info!(project, file = filename, duration_ms = dur, "Rolled back");
                audit(
                    ops,
                    AuditEntry {
                        project,
                        operation: "rollback",
                        filename: Some(&filename),
                        checksum: None,
                        status: "success",
                        error_message: None,
                        duration_ms: Some(dur),
                        comment: None,
                    },
                )
                .await;
            }
            Err(e) => {
                db.batch_execute("ROLLBACK").await.ok();
                let msg = e.to_string();
                audit(
                    ops,
                    AuditEntry {
                        project,
                        operation: "rollback",
                        filename: Some(&filename),
                        checksum: None,
                        status: "error",
                        error_message: Some(&msg),
                        duration_ms: None,
                        comment: None,
                    },
                )
                .await;
                return Err(e.into());
            }
        }
    }

    Ok(Response {
        operation: "rollback".into(),
        project: project.into(),
        rolled_back: Some(count),
        ..Default::default()
    })
}

/// Run seed files.
pub async fn seed(
    db: &Client,
    ops: &Client,
    files: &dyn FileStore,
    project: &str,
) -> Result<Response, Box<dyn std::error::Error + Send + Sync>> {
    let seed_files = files
        .list_files(&format!("migrations/{project}/seed/"))
        .await?;
    if seed_files.is_empty() {
        info!(project, "No seed files");
        return Ok(Response {
            operation: "seed".into(),
            project: project.into(),
            applied: Some(0),
            ..Default::default()
        });
    }

    let mut count = 0i32;
    for file in &seed_files {
        let sql = files.read_file(&file.key).await?;
        let h = checksum(&sql);
        info!(project, file = file.filename, "Seeding");
        let start = std::time::Instant::now();

        match db.batch_execute(&sql).await {
            Ok(()) => {
                count += 1;
                let dur = start.elapsed().as_millis() as i32;
                info!(project, file = file.filename, duration_ms = dur, "Seeded");
                ops.execute(
                    "INSERT INTO seed_runs (project, filename, checksum) VALUES ($1, $2, $3)",
                    &[&project, &file.filename, &h],
                )
                .await
                .ok();
                audit(
                    ops,
                    AuditEntry {
                        project,
                        operation: "seed",
                        filename: Some(&file.filename),
                        checksum: Some(&h),
                        status: "success",
                        error_message: None,
                        duration_ms: Some(dur),
                        comment: None,
                    },
                )
                .await;
            }
            Err(e) => {
                let msg = e.to_string();
                audit(
                    ops,
                    AuditEntry {
                        project,
                        operation: "seed",
                        filename: Some(&file.filename),
                        checksum: Some(&h),
                        status: "error",
                        error_message: Some(&msg),
                        duration_ms: None,
                        comment: None,
                    },
                )
                .await;
                return Err(e.into());
            }
        }
    }

    Ok(Response {
        operation: "seed".into(),
        project: project.into(),
        applied: Some(count),
        ..Default::default()
    })
}
