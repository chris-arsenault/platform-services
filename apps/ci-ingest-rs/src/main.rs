use lambda_http::{run, service_fn, Body, Error, Request, Response};
use serde::{Deserialize, Serialize};
use std::env;
use std::sync::OnceLock;
use tokio::sync::Mutex;
use tokio_postgres::Client;
use tracing::{error, info};

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS ci_builds (
  id SERIAL PRIMARY KEY,
  repo TEXT NOT NULL,
  workflow TEXT NOT NULL,
  status TEXT NOT NULL,
  branch TEXT NOT NULL,
  commit_sha TEXT NOT NULL,
  run_id TEXT NOT NULL UNIQUE,
  run_url TEXT,
  duration_seconds INTEGER,
  lint_passed BOOLEAN,
  test_passed BOOLEAN,
  created_at TIMESTAMPTZ DEFAULT NOW()
);
CREATE INDEX IF NOT EXISTS idx_ci_builds_repo ON ci_builds (repo, created_at DESC);
CREATE INDEX IF NOT EXISTS idx_ci_builds_status ON ci_builds (status, created_at DESC);
";

#[derive(Deserialize)]
struct BuildReport {
    repo: Option<String>,
    workflow: Option<String>,
    status: Option<String>,
    branch: Option<String>,
    commit_sha: Option<String>,
    run_id: Option<String>,
    run_url: Option<String>,
    duration_seconds: Option<i32>,
    lint_passed: Option<bool>,
    test_passed: Option<bool>,
}

#[derive(Serialize)]
struct JsonResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    ok: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

static DB: OnceLock<Mutex<Option<Client>>> = OnceLock::new();

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

async fn get_client() -> Result<Client, Error> {
    let host = env::var("DB_HOST")?;
    let port = env::var("DB_PORT").unwrap_or_else(|_| "5432".into());
    let user = env::var("DB_USER")?;
    let password = env::var("DB_PASSWORD")?;
    let db_name = env::var("DB_NAME")?;
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
    client.batch_execute(SCHEMA).await?;
    Ok(client)
}

async fn ensure_client() -> Result<tokio::sync::MutexGuard<'static, Option<Client>>, Error> {
    let mutex = DB.get_or_init(|| Mutex::new(None));
    let mut guard = mutex.lock().await;
    if guard.is_none() {
        *guard = Some(get_client().await?);
    }
    Ok(guard)
}

fn json_response(status: u16, body: impl Serialize) -> Result<Response<Body>, Error> {
    Ok(Response::builder()
        .status(status)
        .header("Content-Type", "application/json")
        .body(Body::Text(serde_json::to_string(&body)?))?)
}

async fn handler(req: Request) -> Result<Response<Body>, Error> {
    let method = req.method().as_str();
    let path = req.uri().path();

    info!(method, path, "Request");

    let guard = ensure_client().await?;
    let db = guard.as_ref().unwrap();

    match (method, path) {
        ("POST", "/api/ci/report") => {
            let body = std::str::from_utf8(req.body().as_ref()).unwrap_or("{}");
            let report: BuildReport = serde_json::from_str(body)?;

            // Validate required fields
            let repo = report.repo.as_deref().unwrap_or("");
            let workflow = report.workflow.as_deref().unwrap_or("");
            let status = report.status.as_deref().unwrap_or("");
            let branch = report.branch.as_deref().unwrap_or("");
            let commit_sha = report.commit_sha.as_deref().unwrap_or("");
            let run_id = report.run_id.as_deref().unwrap_or("");

            if repo.is_empty()
                || workflow.is_empty()
                || status.is_empty()
                || branch.is_empty()
                || commit_sha.is_empty()
                || run_id.is_empty()
            {
                return json_response(
                    400,
                    JsonResponse {
                        ok: None,
                        error: Some("Missing required fields".into()),
                    },
                );
            }

            // Token auth
            if let Ok(token) = env::var("INGEST_TOKEN") {
                let auth = req
                    .headers()
                    .get("authorization")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("");
                let provided = auth.strip_prefix("Bearer ").unwrap_or("");
                if provided != token {
                    return json_response(
                        401,
                        JsonResponse {
                            ok: None,
                            error: Some("Unauthorized".into()),
                        },
                    );
                }
            }

            db.execute(
                "INSERT INTO ci_builds (repo, workflow, status, branch, commit_sha, run_id, run_url, duration_seconds, lint_passed, test_passed)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
                 ON CONFLICT (run_id) DO UPDATE SET
                   status = EXCLUDED.status,
                   duration_seconds = EXCLUDED.duration_seconds,
                   lint_passed = EXCLUDED.lint_passed,
                   test_passed = EXCLUDED.test_passed",
                &[
                    &repo, &workflow, &status, &branch, &commit_sha, &run_id,
                    &report.run_url.as_deref(),
                    &report.duration_seconds,
                    &report.lint_passed,
                    &report.test_passed,
                ],
            ).await?;

            json_response(
                200,
                JsonResponse {
                    ok: Some(true),
                    error: None,
                },
            )
        }

        ("GET", "/api/ci/builds") => {
            let rows = db
                .query(
                    "SELECT repo, workflow, status, branch, commit_sha, run_id, run_url,
                        duration_seconds, lint_passed, test_passed, created_at
                 FROM ci_builds ORDER BY created_at DESC LIMIT 100",
                    &[],
                )
                .await?;

            let builds: Vec<serde_json::Value> = rows
                .iter()
                .map(|r| {
                    serde_json::json!({
                        "repo": r.get::<_, String>(0),
                        "workflow": r.get::<_, String>(1),
                        "status": r.get::<_, String>(2),
                        "branch": r.get::<_, String>(3),
                        "commit_sha": r.get::<_, String>(4),
                        "run_id": r.get::<_, String>(5),
                        "run_url": r.get::<_, Option<String>>(6),
                        "duration_seconds": r.get::<_, Option<i32>>(7),
                        "lint_passed": r.get::<_, Option<bool>>(8),
                        "test_passed": r.get::<_, Option<bool>>(9),
                        "created_at": r.get::<_, chrono::DateTime<chrono::Utc>>(10).to_rfc3339(),
                    })
                })
                .collect();

            json_response(200, builds)
        }

        ("GET", "/api/ci/summary") => {
            let rows = db
                .query(
                    "SELECT DISTINCT ON (repo, workflow)
                        repo, workflow, status, branch, commit_sha, run_url, created_at
                 FROM ci_builds ORDER BY repo, workflow, created_at DESC",
                    &[],
                )
                .await?;

            let summary: Vec<serde_json::Value> = rows
                .iter()
                .map(|r| {
                    serde_json::json!({
                        "repo": r.get::<_, String>(0),
                        "workflow": r.get::<_, String>(1),
                        "status": r.get::<_, String>(2),
                        "branch": r.get::<_, String>(3),
                        "commit_sha": r.get::<_, String>(4),
                        "run_url": r.get::<_, Option<String>>(5),
                        "created_at": r.get::<_, chrono::DateTime<chrono::Utc>>(6).to_rfc3339(),
                    })
                })
                .collect();

            json_response(200, summary)
        }

        ("GET", "/api/ci/health") => json_response(
            200,
            JsonResponse {
                ok: Some(true),
                error: None,
            },
        ),

        _ => json_response(
            404,
            JsonResponse {
                ok: None,
                error: Some("Not found".into()),
            },
        ),
    }
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

    run(service_fn(handler)).await
}
