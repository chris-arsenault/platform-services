mod db;

use aws_sdk_ssm::Client as SsmClient;
use db::{BuildReport, BuildRow, SummaryRow};
use lambda_http::{run, service_fn, Body, Error, Request, Response};
use serde::Serialize;
use std::env;
use std::sync::OnceLock;
use tokio::sync::Mutex;
use tokio_postgres::Client;
use tracing::{error, info};

const RDS_CA_BUNDLE: &[u8] = include_bytes!("../../certs/rds-global-bundle.pem");

#[derive(Serialize)]
struct JsonResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    ok: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

static DB: OnceLock<Mutex<Option<Client>>> = OnceLock::new();

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
    let db_name = env::var("DB_NAME")?;
    let ssm_prefix = env::var("DB_SSM_PREFIX")?;

    let aws_config = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
    let ssm = SsmClient::new(&aws_config);

    let user = ssm
        .get_parameter()
        .name(format!("{ssm_prefix}/username"))
        .send()
        .await?
        .parameter()
        .ok_or("SSM param not found")?
        .value()
        .ok_or("empty value")?
        .to_string();

    let password = ssm
        .get_parameter()
        .name(format!("{ssm_prefix}/password"))
        .with_decryption(true)
        .send()
        .await?
        .parameter()
        .ok_or("empty value")?
        .value()
        .ok_or("empty value")?
        .to_string();

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
    let client = guard.as_ref().unwrap();

    match (method, path) {
        ("POST", "/api/ci/report") => {
            let body = std::str::from_utf8(req.body().as_ref()).unwrap_or("{}");
            let report: BuildReport = serde_json::from_str(body)?;

            if let Err(msg) = db::validate_report(&report) {
                return json_response(
                    400,
                    JsonResponse {
                        ok: None,
                        error: Some(msg.into()),
                    },
                );
            }

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

            db::upsert_build(client, &report).await?;
            json_response(
                200,
                JsonResponse {
                    ok: Some(true),
                    error: None,
                },
            )
        }

        ("GET", "/api/ci/builds") => {
            let builds: Vec<BuildRow> = db::get_builds(client).await?;
            json_response(200, builds)
        }

        ("GET", "/api/ci/summary") => {
            let summary: Vec<SummaryRow> = db::get_summary(client).await?;
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
