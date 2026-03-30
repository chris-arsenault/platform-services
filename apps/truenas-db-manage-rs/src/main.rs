use aws_sdk_ssm::Client as SsmClient;
use lambda_runtime::{service_fn, Error, LambdaEvent};
use rand::Rng;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::env;
use tokio_postgres::{Client, NoTls};
use tracing::{error, info};

#[derive(Deserialize)]
struct ProjectConfig {
    db_name: String,
}

#[derive(Deserialize)]
struct Request {
    project: String,
}

#[derive(Serialize)]
struct Response {
    project: String,
    db_name: String,
    created: bool,
}

fn get_project_map() -> HashMap<String, ProjectConfig> {
    serde_json::from_str(&env::var("PROJECT_MAP").expect("PROJECT_MAP not set"))
        .expect("Invalid PROJECT_MAP JSON")
}

fn generate_password() -> String {
    const CHARSET: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    let mut rng = rand::thread_rng();
    (0..32)
        .map(|_| CHARSET[rng.gen_range(0..CHARSET.len())] as char)
        .collect()
}

async fn connect_admin(ssm: &SsmClient) -> Result<Client, Error> {
    let host = env::var("PG_HOST")?;
    let port = env::var("PG_PORT").unwrap_or_else(|_| "5432".into());

    let user = ssm
        .get_parameter()
        .name("/platform/truenas/pg-admin-user")
        .with_decryption(true)
        .send()
        .await
        .map_err(|e| format!("Failed to read admin user from SSM: {e}"))?
        .parameter()
        .and_then(|p| p.value().map(|v| v.to_string()))
        .ok_or("SSM param /platform/truenas/pg-admin-user has no value")?;

    let password = ssm
        .get_parameter()
        .name("/platform/truenas/pg-admin-password")
        .with_decryption(true)
        .send()
        .await
        .map_err(|e| format!("Failed to read admin password from SSM: {e}"))?
        .parameter()
        .and_then(|p| p.value().map(|v| v.to_string()))
        .ok_or("SSM param /platform/truenas/pg-admin-password has no value")?;

    let connstr =
        format!("host={host} port={port} user={user} password={password} dbname=postgres");
    // TrueNAS Postgres is on LAN via VPN, no TLS needed
    let (client, connection) = tokio_postgres::connect(&connstr, NoTls).await?;
    tokio::spawn(async move {
        if let Err(e) = connection.await {
            error!("DB connection error: {e}");
        }
    });
    Ok(client)
}

async fn ensure_database(
    pg: &Client,
    ssm: &SsmClient,
    project: &str,
    config: &ProjectConfig,
) -> Result<Response, Error> {
    let db_name = &config.db_name;
    let role_name = format!("{project}_app");
    let ssm_prefix = format!("/platform/truenas-db/{project}");
    let mut created = false;

    // Create database if needed
    let db_rows = pg
        .query("SELECT 1 FROM pg_database WHERE datname = $1", &[db_name])
        .await
        .map_err(|e| format!("Failed to query pg_database: {e}"))?;
    if db_rows.is_empty() {
        info!(project, db = db_name, "Creating database");
        pg.batch_execute(&format!("CREATE DATABASE \"{db_name}\""))
            .await
            .map_err(|e| format!("Failed to CREATE DATABASE {db_name}: {e}"))?;
        created = true;
    }

    // Create role if needed
    let role_rows = pg
        .query("SELECT 1 FROM pg_roles WHERE rolname = $1", &[&role_name])
        .await
        .map_err(|e| format!("Failed to query pg_roles: {e}"))?;
    if role_rows.is_empty() {
        let password = generate_password();
        info!(project, role = role_name, "Creating application role");

        pg.batch_execute(&format!(
            "CREATE ROLE \"{role_name}\" LOGIN PASSWORD '{password}'"
        ))
        .await
        .map_err(|e| format!("Failed to CREATE ROLE {role_name}: {e}"))?;

        // Publish credentials to SSM
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

        info!(project, role = role_name, "Credentials published to SSM");
        created = true;
    }

    // Always ensure grants (idempotent)
    pg.batch_execute(&format!(
        "GRANT ALL PRIVILEGES ON DATABASE \"{db_name}\" TO \"{role_name}\""
    ))
    .await
    .map_err(|e| format!("Failed to GRANT on database {db_name}: {e}"))?;

    // Connect to the project database to set schema grants
    let host = env::var("PG_HOST")?;
    let port = env::var("PG_PORT").unwrap_or_else(|_| "5432".into());
    let user = ssm
        .get_parameter()
        .name("/platform/truenas/pg-admin-user")
        .with_decryption(true)
        .send()
        .await
        .map_err(|e| format!("Failed to read admin user from SSM: {e}"))?
        .parameter()
        .and_then(|p| p.value().map(|v| v.to_string()))
        .ok_or("SSM param /platform/truenas/pg-admin-user has no value")?;
    let password = ssm
        .get_parameter()
        .name("/platform/truenas/pg-admin-password")
        .with_decryption(true)
        .send()
        .await
        .map_err(|e| format!("Failed to read admin password from SSM: {e}"))?
        .parameter()
        .and_then(|p| p.value().map(|v| v.to_string()))
        .ok_or("SSM param /platform/truenas/pg-admin-password has no value")?;
    let connstr =
        format!("host={host} port={port} user={user} password={password} dbname={db_name}");
    let (db, conn) = tokio_postgres::connect(&connstr, NoTls)
        .await
        .map_err(|e| format!("Failed to connect to database {db_name}: {e}"))?;
    tokio::spawn(async move {
        if let Err(e) = conn.await {
            error!("DB connection error: {e}");
        }
    });

    db.batch_execute(&format!(
        "GRANT ALL ON SCHEMA public TO \"{role_name}\";
         ALTER DEFAULT PRIVILEGES IN SCHEMA public GRANT ALL ON TABLES TO \"{role_name}\";
         ALTER DEFAULT PRIVILEGES IN SCHEMA public GRANT ALL ON SEQUENCES TO \"{role_name}\";
         GRANT ALL ON ALL TABLES IN SCHEMA public TO \"{role_name}\";
         GRANT ALL ON ALL SEQUENCES IN SCHEMA public TO \"{role_name}\";"
    ))
    .await
    .map_err(|e| format!("Failed to set schema grants for {role_name} on {db_name}: {e}"))?;

    info!(project, db = db_name, role = role_name, "Database ready");

    Ok(Response {
        project: project.to_string(),
        db_name: db_name.to_string(),
        created,
    })
}

async fn handler(event: LambdaEvent<serde_json::Value>) -> Result<serde_json::Value, Error> {
    let (payload, _ctx) = event.into_parts();
    info!(event = %payload, "TrueNAS DB manage invoked");

    let request: Request = serde_json::from_value(payload)?;
    let project_map = get_project_map();

    let config = project_map.get(&request.project).ok_or_else(|| {
        format!(
            "Project \"{}\" not registered. Registered: {:?}",
            request.project,
            project_map.keys().collect::<Vec<_>>()
        )
    })?;

    let aws_config = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
    let ssm = SsmClient::new(&aws_config);

    let pg = connect_admin(&ssm).await.map_err(|e| {
        let msg = format!("Failed to connect to TrueNAS Postgres: {e}");
        error!(error = msg, "Connection failed");
        msg
    })?;

    let response = ensure_database(&pg, &ssm, &request.project, config)
        .await
        .map_err(|e| {
            let msg = format!("ensure_database failed for {}: {e}", request.project);
            error!(error = msg, "Database setup failed");
            msg
        })?;
    Ok(serde_json::to_value(response)?)
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env().add_directive("info".parse()?),
        )
        .without_time()
        .init();

    lambda_runtime::run(service_fn(handler)).await
}
