use aws_sdk_s3::Client as S3Client;
use aws_sdk_ssm::Client as SsmClient;
use lambda_runtime::{service_fn, Error, LambdaEvent};
use rand::Rng;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::env;
use tokio_postgres::Client;
use tracing::{error, info, warn};

const LOCAL_TRACKING: &str = "
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

const OPS_SCHEMA: &str = "
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

const OPS_DB: &str = "platform_ops";

#[derive(Deserialize)]
struct ProjectConfig {
    db_name: String,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum MigrationEvent {
    S3Event {
        detail: S3Detail,
    },
    Manual {
        operation: String,
        project: String,
        target: Option<String>,
        comment: Option<String>,
    },
}

#[derive(Deserialize)]
struct S3Detail {
    object: S3Object,
}

#[derive(Deserialize)]
struct S3Object {
    key: String,
}

#[derive(Serialize, Default)]
struct Response {
    operation: String,
    project: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    applied: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    rolled_back: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    baselined: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    duration_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    db: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    file: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    comment: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    status: Option<String>,
}

struct MigrationFile {
    key: String,
    filename: String,
}

fn get_project_map() -> HashMap<String, ProjectConfig> {
    serde_json::from_str(&env::var("PROJECT_MAP").expect("PROJECT_MAP not set"))
        .expect("Invalid PROJECT_MAP JSON")
}

fn require_project(project: &str) -> Result<ProjectConfig, Error> {
    let mut map = get_project_map();
    map.remove(project)
        .ok_or_else(|| format!("Project \"{project}\" is not registered").into())
}

/// AWS RDS CA bundle embedded at build time.
/// Source: https://truststore.pki.rds.amazonaws.com/global/global-bundle.pem
const RDS_CA_BUNDLE: &[u8] = include_bytes!("../../certs/rds-global-bundle.pem");

fn make_tls_connector() -> tokio_postgres_rustls::MakeRustlsConnect {
    let mut root_store = rustls::RootCertStore::empty();
    let certs: Vec<rustls_pki_types::CertificateDer<'_>> =
        rustls_pemfile::certs(&mut &RDS_CA_BUNDLE[..])
            .collect::<Result<Vec<_>, _>>()
            .expect("Failed to parse RDS CA bundle");
    for cert in certs {
        root_store.add(cert).expect("Failed to add RDS CA cert");
    }
    let config = rustls::ClientConfig::builder()
        .with_root_certificates(root_store)
        .with_no_client_auth();
    tokio_postgres_rustls::MakeRustlsConnect::new(config)
}

async fn connect_with(db_name: &str, user: &str, password: &str) -> Result<Client, Error> {
    let host = env::var("DB_HOST")?;
    let port = env::var("DB_PORT").unwrap_or_else(|_| "5432".into());
    let connstr = format!(
        "host={host} port={port} user={user} password={password} dbname={db_name} sslmode=require"
    );
    let tls = make_tls_connector();
    let (client, connection) = tokio_postgres::connect(&connstr, tls).await?;
    tokio::spawn(async move {
        if let Err(e) = connection.await {
            error!("DB connection error: {e}");
        }
    });
    Ok(client)
}

/// Connect using master credentials from env vars (for admin operations)
async fn connect_to(db_name: &str) -> Result<Client, Error> {
    let user = env::var("DB_USER")?;
    let password = env::var("DB_PASSWORD")?;
    connect_with(db_name, &user, &password).await
}

/// Connect using the project's app role credentials from SSM
async fn connect_as_app(project: &str, db_name: &str, ssm: &SsmClient) -> Result<Client, Error> {
    let prefix = format!("/platform/db/{project}");
    let user = ssm
        .get_parameter()
        .name(format!("{prefix}/username"))
        .send()
        .await?
        .parameter()
        .ok_or("SSM param not found")?
        .value()
        .ok_or("empty value")?
        .to_string();
    let password = ssm
        .get_parameter()
        .name(format!("{prefix}/password"))
        .with_decryption(true)
        .send()
        .await?
        .parameter()
        .ok_or("SSM param not found")?
        .value()
        .ok_or("empty value")?
        .to_string();
    connect_with(db_name, &user, &password).await
}

fn generate_password() -> String {
    const CHARSET: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    let mut rng = rand::thread_rng();
    (0..32)
        .map(|_| CHARSET[rng.gen_range(0..CHARSET.len())] as char)
        .collect()
}

/// Ensures the project database exists with an application role and SSM credentials.
/// Creates database, app role, grants, and publishes credentials to SSM if needed.
async fn ensure_database(project: &str, db_name: &str, ssm: &SsmClient) -> Result<(), Error> {
    let pg = connect_to("postgres").await?;

    // Create database if needed
    let rows = pg
        .query("SELECT 1 FROM pg_database WHERE datname = $1", &[&db_name])
        .await?;
    if rows.is_empty() {
        info!(db = db_name, "Creating database");
        pg.batch_execute(&format!("CREATE DATABASE \"{db_name}\""))
            .await?;
    }

    // Create app role if needed, publish credentials to SSM
    let role_name = format!("{project}_app");
    let role_rows = pg
        .query("SELECT 1 FROM pg_roles WHERE rolname = $1", &[&role_name])
        .await?;
    if role_rows.is_empty() {
        let password = generate_password();
        info!(project, role = role_name, "Creating application role");

        pg.batch_execute(&format!(
            "CREATE ROLE \"{role_name}\" LOGIN PASSWORD '{password}'"
        ))
        .await?;

        // Publish credentials to SSM
        let ssm_prefix = format!("/platform/db/{project}");

        ssm.put_parameter()
            .name(format!("{ssm_prefix}/username"))
            .r#type(aws_sdk_ssm::types::ParameterType::String)
            .value(&role_name)
            .overwrite(true)
            .send()
            .await?;

        ssm.put_parameter()
            .name(format!("{ssm_prefix}/password"))
            .r#type(aws_sdk_ssm::types::ParameterType::SecureString)
            .value(&password)
            .overwrite(true)
            .send()
            .await?;

        ssm.put_parameter()
            .name(format!("{ssm_prefix}/database"))
            .r#type(aws_sdk_ssm::types::ParameterType::String)
            .value(db_name)
            .overwrite(true)
            .send()
            .await?;

        info!(
            project,
            role = role_name,
            "App role created and credentials published to SSM"
        );
    }

    // Create reader role if needed, publish credentials to SSM
    let reader_name = format!("{project}_reader");
    let reader_rows = pg
        .query(
            "SELECT 1 FROM pg_roles WHERE rolname = $1",
            &[&reader_name],
        )
        .await?;
    if reader_rows.is_empty() {
        let password = generate_password();
        info!(project, role = reader_name, "Creating reader role");

        pg.batch_execute(&format!(
            "CREATE ROLE \"{reader_name}\" LOGIN PASSWORD '{password}'"
        ))
        .await?;

        let ssm_prefix = format!("/platform/db/{project}/reader");

        ssm.put_parameter()
            .name(format!("{ssm_prefix}/username"))
            .r#type(aws_sdk_ssm::types::ParameterType::String)
            .value(&reader_name)
            .overwrite(true)
            .send()
            .await?;

        ssm.put_parameter()
            .name(format!("{ssm_prefix}/password"))
            .r#type(aws_sdk_ssm::types::ParameterType::SecureString)
            .value(&password)
            .overwrite(true)
            .send()
            .await?;

        info!(
            project,
            role = reader_name,
            "Reader role created and credentials published to SSM"
        );
    }

    // Always ensure grants are in place (idempotent — covers fresh databases after drop)
    pg.batch_execute(&format!(
        "GRANT ALL PRIVILEGES ON DATABASE \"{db_name}\" TO \"{role_name}\";
         GRANT CONNECT ON DATABASE \"{db_name}\" TO \"{reader_name}\";"
    ))
    .await?;

    // Grant app role membership to admin so ALTER DEFAULT PRIVILEGES FOR ROLE works in PG16
    pg.batch_execute(&format!("GRANT \"{role_name}\" TO CURRENT_USER"))
        .await?;

    let db = connect_to(db_name).await?;
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

async fn ensure_database_bare(db_name: &str) -> Result<(), Error> {
    let pg = connect_to("postgres").await?;
    let rows = pg
        .query("SELECT 1 FROM pg_database WHERE datname = $1", &[&db_name])
        .await?;
    if rows.is_empty() {
        info!(db = db_name, "Creating database");
        pg.batch_execute(&format!("CREATE DATABASE \"{db_name}\""))
            .await?;
    }
    Ok(())
}

async fn get_ops_client() -> Result<Client, Error> {
    ensure_database_bare(OPS_DB).await?;
    let client = connect_to(OPS_DB).await?;
    client.batch_execute(OPS_SCHEMA).await?;
    Ok(client)
}

struct AuditEntry<'a> {
    project: &'a str,
    operation: &'a str,
    filename: Option<&'a str>,
    checksum: Option<&'a str>,
    status: &'a str,
    error_message: Option<&'a str>,
    duration_ms: Option<i32>,
    comment: Option<&'a str>,
}

async fn audit(ops: &Client, entry: AuditEntry<'_>) {
    if let Err(e) = ops
        .execute(
            "INSERT INTO migration_audit (project, operation, filename, checksum, status, comment, error_message, duration_ms)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
            &[&entry.project, &entry.operation, &entry.filename, &entry.checksum, &entry.status, &entry.comment, &entry.error_message, &entry.duration_ms],
        )
        .await
    {
        warn!("Audit write failed (non-fatal): {e}");
    }
}

fn lock_id(project: &str) -> i64 {
    let mut h: i64 = 0;
    for b in project.bytes() {
        h = h.wrapping_mul(31).wrapping_add(b as i64);
    }
    h.abs()
}

async fn acquire_lock(client: &Client, project: &str) -> Result<(), Error> {
    let id = lock_id(project);
    client
        .execute("SELECT pg_advisory_lock($1)", &[&id])
        .await?;
    info!(project, lock_id = id, "Lock acquired");
    Ok(())
}

async fn release_lock(client: &Client, project: &str) -> Result<(), Error> {
    let id = lock_id(project);
    client
        .execute("SELECT pg_advisory_unlock($1)", &[&id])
        .await?;
    Ok(())
}

async fn list_files(
    s3: &S3Client,
    bucket: &str,
    prefix: &str,
) -> Result<Vec<MigrationFile>, Error> {
    let resp = s3
        .list_objects_v2()
        .bucket(bucket)
        .prefix(prefix)
        .send()
        .await?;
    let prefix_depth = prefix.matches('/').count();
    let mut files: Vec<MigrationFile> = resp
        .contents()
        .iter()
        .filter_map(|obj| {
            let key = obj.key()?;
            // Only include files directly under the prefix, not in subdirectories
            if key.ends_with(".sql") && key.matches('/').count() == prefix_depth {
                Some(MigrationFile {
                    key: key.to_string(),
                    filename: key.rsplit('/').next().unwrap_or(key).to_string(),
                })
            } else {
                None
            }
        })
        .collect();
    files.sort_by(|a, b| a.filename.cmp(&b.filename));
    Ok(files)
}

async fn read_file(s3: &S3Client, bucket: &str, key: &str) -> Result<String, Error> {
    let resp = s3.get_object().bucket(bucket).key(key).send().await?;
    let bytes = resp.body.collect().await?.into_bytes();
    Ok(String::from_utf8(bytes.to_vec())?)
}

fn checksum(content: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(content.as_bytes());
    format!("{:x}", hasher.finalize())[..16].to_string()
}

fn bucket() -> String {
    env::var("MIGRATIONS_BUCKET").expect("MIGRATIONS_BUCKET not set")
}

// =============================================================================
// Operations
// =============================================================================

async fn migrate(
    s3: &S3Client,
    ssm: &SsmClient,
    project: &str,
    config: &ProjectConfig,
) -> Result<Response, Error> {
    ensure_database(project, &config.db_name, ssm).await?;
    let ops = get_ops_client().await?;
    let db = connect_as_app(project, &config.db_name, ssm).await?;
    let bucket = bucket();

    acquire_lock(&ops, project).await?;
    let result = async {
        db.batch_execute(LOCAL_TRACKING).await?;

        let files = list_files(s3, &bucket, &format!("migrations/{project}/")).await?;
        info!(project, count = files.len(), "Migration files found");

        let applied_rows = db
            .query("SELECT filename, checksum FROM schema_migrations", &[])
            .await?;
        let applied: HashMap<String, String> = applied_rows
            .iter()
            .map(|r| (r.get::<_, String>(0), r.get::<_, String>(1)))
            .collect();

        let mut count = 0i32;
        for file in &files {
            let sql = read_file(s3, &bucket, &file.key).await?;
            let h = checksum(&sql);

            if let Some(existing) = applied.get(&file.filename) {
                if existing != &h {
                    let msg = format!("Checksum mismatch for {}", file.filename);
                    audit(&ops, AuditEntry {
                        project,
                        operation: "migrate",
                        filename: Some(&file.filename),
                        checksum: Some(&h),
                        status: "error",
                        error_message: Some(&msg),
                        duration_ms: None,
                        comment: None,
                    }).await;
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
                    ).await?;
                    db.batch_execute("COMMIT").await?;
                    count += 1;
                    info!(project, file = file.filename, duration_ms = dur, "Applied");
                    audit(&ops, AuditEntry {
                        project,
                        operation: "migrate",
                        filename: Some(&file.filename),
                        checksum: Some(&h),
                        status: "success",
                        error_message: None,
                        duration_ms: Some(dur),
                        comment: None,
                    }).await;
                }
                Err(e) => {
                    db.batch_execute("ROLLBACK").await.ok();
                    let msg = e.to_string();
                    error!(project, file = file.filename, error = msg, "Failed");
                    audit(&ops, AuditEntry {
                        project,
                        operation: "migrate",
                        filename: Some(&file.filename),
                        checksum: Some(&h),
                        status: "error",
                        error_message: Some(&msg),
                        duration_ms: None,
                        comment: None,
                    }).await;
                    return Err(e.into());
                }
            }
        }

        info!(project, applied = count, total = files.len(), "Migrate complete");
        Ok(Response {
            operation: "migrate".into(),
            project: project.into(),
            applied: Some(count),
            ..Default::default()
        })
    }
    .await;

    release_lock(&ops, project).await.ok();
    result
}

async fn rollback(
    s3: &S3Client,
    ssm: &SsmClient,
    project: &str,
    config: &ProjectConfig,
    target: Option<&str>,
) -> Result<Response, Error> {
    let ops = get_ops_client().await?;
    let db = connect_as_app(project, &config.db_name, ssm).await?;
    let bucket = bucket();

    acquire_lock(&ops, project).await?;
    let result = async {
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

        let rollback_files =
            list_files(s3, &bucket, &format!("migrations/{project}/rollback/")).await?;
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

            let sql = read_file(s3, &bucket, rollback_key).await?;
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
                        &ops,
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
                        &ops,
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
    .await;

    release_lock(&ops, project).await.ok();
    result
}

async fn seed(
    s3: &S3Client,
    ssm: &SsmClient,
    project: &str,
    config: &ProjectConfig,
) -> Result<Response, Error> {
    let bucket = bucket();
    let seed_files = list_files(s3, &bucket, &format!("migrations/{project}/seed/")).await?;
    if seed_files.is_empty() {
        info!(project, "No seed files");
        return Ok(Response {
            operation: "seed".into(),
            project: project.into(),
            applied: Some(0),
            ..Default::default()
        });
    }

    let ops = get_ops_client().await?;
    let db = connect_as_app(project, &config.db_name, ssm).await?;

    acquire_lock(&ops, project).await?;
    let result =
        async {
            let mut count = 0i32;
            for file in &seed_files {
                let sql = read_file(s3, &bucket, &file.key).await?;
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
                    ).await.ok();
                        audit(
                            &ops,
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
                            &ops,
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
        .await;

    release_lock(&ops, project).await.ok();
    result
}

async fn drop_db(project: &str, config: &ProjectConfig) -> Result<Response, Error> {
    let ops = get_ops_client().await?;

    acquire_lock(&ops, project).await?;
    let result = async {
        warn!(project, db = config.db_name, "Dropping database");
        let pg = connect_to("postgres").await?;
        pg.execute(
            "SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE datname = $1 AND pid <> pg_backend_pid()",
            &[&config.db_name],
        ).await?;
        pg.batch_execute(&format!("DROP DATABASE IF EXISTS \"{}\"", config.db_name)).await?;
        info!(project, db = config.db_name, "Database dropped");
        audit(&ops, AuditEntry {
                        project,
                        operation: "drop",
                        filename: None,
                        checksum: None,
                        status: "success",
                        error_message: None,
                        duration_ms: None,
                        comment: None,
                    }).await;
        Ok(Response {
            operation: "drop".into(),
            project: project.into(),
            db: Some(config.db_name.clone()),
            ..Default::default()
        })
    }
    .await;

    release_lock(&ops, project).await.ok();
    result
}

async fn noop(
    s3: &S3Client,
    ssm: &SsmClient,
    project: &str,
    config: &ProjectConfig,
    target: &str,
    comment: &str,
) -> Result<Response, Error> {
    ensure_database(project, &config.db_name, ssm).await?;
    let ops = get_ops_client().await?;
    let db = connect_as_app(project, &config.db_name, ssm).await?;
    let bucket = bucket();

    acquire_lock(&ops, project).await?;
    let result = async {
        db.batch_execute(LOCAL_TRACKING).await?;

        let existing = db.query("SELECT 1 FROM schema_migrations WHERE filename = $1", &[&target]).await?;
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

        let files = list_files(s3, &bucket, &format!("migrations/{project}/")).await?;
        let file = files.iter().find(|f| f.filename == target)
            .ok_or_else(|| format!("Migration file not found in S3: migrations/{project}/{target}"))?;

        let sql = read_file(s3, &bucket, &file.key).await?;
        let h = checksum(&sql);

        info!(project, file = target, comment, "Recording noop");
        db.execute(
            "INSERT INTO schema_migrations (filename, checksum, noop, comment, duration_ms) VALUES ($1, $2, TRUE, $3, 0)",
            &[&target, &h, &comment],
        ).await?;
        audit(&ops, AuditEntry {
                        project,
                        operation: "noop",
                        filename: Some(target),
                        checksum: Some(&h),
                        status: "success",
                        error_message: None,
                        duration_ms: Some(0),
                        comment: Some(comment),
                    }).await;

        Ok(Response {
            operation: "noop".into(),
            project: project.into(),
            file: Some(target.into()),
            comment: Some(comment.into()),
            ..Default::default()
        })
    }
    .await;

    release_lock(&ops, project).await.ok();
    result
}

async fn restore(
    s3: &S3Client,
    ssm: &SsmClient,
    project: &str,
    config: &ProjectConfig,
    target: &str,
    comment: &str,
) -> Result<Response, Error> {
    ensure_database(project, &config.db_name, ssm).await?;
    let ops = get_ops_client().await?;
    let bucket = bucket();

    acquire_lock(&ops, project).await?;
    let result = async {
        // Drop and recreate for clean restore
        info!(project, db = config.db_name, "Dropping for clean restore");
        let pg = connect_to("postgres").await?;
        pg.execute(
            "SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE datname = $1 AND pid <> pg_backend_pid()",
            &[&config.db_name],
        ).await?;
        pg.batch_execute(&format!("DROP DATABASE IF EXISTS \"{}\"", config.db_name)).await?;
        pg.batch_execute(&format!("CREATE DATABASE \"{}\"", config.db_name)).await?;
        drop(pg);

        let db = connect_to(&config.db_name).await?;

        // Restore dump (strip psql metacommands, fix search_path)
        info!(project, key = target, "Restoring from S3");
        let raw = read_file(s3, &bucket, target).await?;
        let sql: String = raw
            .lines()
            .filter(|line| !line.starts_with('\\'))
            .map(|line| {
                if line.contains("set_config('search_path'") {
                    "SELECT pg_catalog.set_config('search_path', 'public', false);"
                } else {
                    line
                }
            })
            .collect::<Vec<&str>>()
            .join("\n");

        let start = std::time::Instant::now();
        db.batch_execute(&sql).await?;
        let dur = start.elapsed().as_millis() as u64;
        info!(project, key = target, duration_ms = dur, "SQL restored");

        // Re-grant app role permissions on the fresh database
        let role_name = format!("{project}_app");
        let reader_name = format!("{project}_reader");
        db.batch_execute("SET search_path TO public").await?;
        db.batch_execute(&format!(
            "GRANT ALL ON SCHEMA public TO \"{role_name}\";
             ALTER DEFAULT PRIVILEGES IN SCHEMA public GRANT ALL ON TABLES TO \"{role_name}\";
             ALTER DEFAULT PRIVILEGES IN SCHEMA public GRANT ALL ON SEQUENCES TO \"{role_name}\";
             GRANT ALL ON ALL TABLES IN SCHEMA public TO \"{role_name}\";
             GRANT ALL ON ALL SEQUENCES IN SCHEMA public TO \"{role_name}\";"
        )).await?;

        // Re-grant reader role permissions on the fresh database
        db.batch_execute(&format!(
            "GRANT \"{role_name}\" TO CURRENT_USER;
             GRANT USAGE ON SCHEMA public TO \"{reader_name}\";
             GRANT SELECT ON ALL TABLES IN SCHEMA public TO \"{reader_name}\";
             GRANT SELECT ON ALL SEQUENCES IN SCHEMA public TO \"{reader_name}\";
             ALTER DEFAULT PRIVILEGES IN SCHEMA public GRANT SELECT ON TABLES TO \"{reader_name}\";
             ALTER DEFAULT PRIVILEGES IN SCHEMA public GRANT SELECT ON SEQUENCES TO \"{reader_name}\";
             ALTER DEFAULT PRIVILEGES FOR ROLE \"{role_name}\" IN SCHEMA public GRANT SELECT ON TABLES TO \"{reader_name}\";
             ALTER DEFAULT PRIVILEGES FOR ROLE \"{role_name}\" IN SCHEMA public GRANT SELECT ON SEQUENCES TO \"{reader_name}\";"
        )).await?;

        db.batch_execute(LOCAL_TRACKING).await?;

        let migration_files = list_files(s3, &bucket, &format!("migrations/{project}/")).await?;
        let mut baselined = 0i32;
        for file in &migration_files {
            let existing = db.query("SELECT 1 FROM schema_migrations WHERE filename = $1", &[&file.filename]).await?;
            if !existing.is_empty() {
                continue;
            }
            let content = read_file(s3, &bucket, &file.key).await?;
            let h = checksum(&content);
            db.execute(
                "INSERT INTO schema_migrations (filename, checksum, noop, comment, duration_ms) VALUES ($1, $2, TRUE, $3, 0)",
                &[&file.filename, &h, &comment],
            ).await?;
            baselined += 1;
            info!(project, file = file.filename, "Baselined");
        }

        audit(&ops, AuditEntry {
                        project,
                        operation: "restore",
                        filename: Some(target),
                        checksum: None,
                        status: "success",
                        error_message: None,
                        duration_ms: Some(dur as i32),
                        comment: Some(comment),
                    }).await;
        info!(project, key = target, duration_ms = dur, baselined, "Restore complete");

        Ok(Response {
            operation: "restore".into(),
            project: project.into(),
            key: Some(target.into()),
            duration_ms: Some(dur),
            baselined: Some(baselined),
            ..Default::default()
        })
    }
    .await;

    release_lock(&ops, project).await.ok();
    result
}

// =============================================================================
// Entry point
// =============================================================================

async fn handler(event: LambdaEvent<serde_json::Value>) -> Result<serde_json::Value, Error> {
    let (payload, _ctx) = event.into_parts();
    info!(event = %payload, "Event received");

    let aws_config = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
    let s3 = S3Client::new(&aws_config);
    let ssm = SsmClient::new(&aws_config);

    let evt: MigrationEvent = serde_json::from_value(payload)?;

    let response = match evt {
        MigrationEvent::S3Event { detail } => {
            let key = &detail.object.key;
            let parts: Vec<&str> = key.split('/').collect();
            if parts.len() < 3 || parts[0] != "migrations" {
                warn!(key, "Ignoring non-migration upload");
                return Ok(serde_json::json!({"status": "ignored"}));
            }
            let project = parts[1];
            if parts[2] == "rollback" || parts[2] == "seed" {
                info!(key, project, "Ignoring rollback/seed upload");
                return Ok(serde_json::json!({"status": "ignored"}));
            }
            let cfg = require_project(project)?;
            migrate(&s3, &ssm, project, &cfg).await?
        }
        MigrationEvent::Manual {
            operation,
            project,
            target,
            comment,
        } => {
            let cfg = require_project(&project)?;
            match operation.as_str() {
                "migrate" => migrate(&s3, &ssm, &project, &cfg).await?,
                "rollback" => rollback(&s3, &ssm, &project, &cfg, target.as_deref()).await?,
                "seed" => seed(&s3, &ssm, &project, &cfg).await?,
                "drop" => drop_db(&project, &cfg).await?,
                "noop" => {
                    let t = target.as_deref().ok_or("noop requires target")?;
                    let c = comment.as_deref().ok_or("noop requires comment")?;
                    noop(&s3, &ssm, &project, &cfg, t, c).await?
                }
                "restore" => {
                    let t = target.as_deref().ok_or("restore requires target")?;
                    let c = comment.as_deref().ok_or("restore requires comment")?;
                    restore(&s3, &ssm, &project, &cfg, t, c).await?
                }
                _ => return Err(format!("Unknown operation: {operation}").into()),
            }
        }
    };

    Ok(serde_json::to_value(response)?)
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("Failed to install rustls crypto provider");

    tracing_subscriber::fmt()
        .json()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env().add_directive("info".parse()?),
        )
        .without_time()
        .init();

    lambda_runtime::run(service_fn(handler)).await
}
