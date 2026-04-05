use aws_sdk_dynamodb::Client as DdbClient;
use aws_sdk_ssm::Client as SsmClient;
use lambda_runtime::{service_fn, Error, LambdaEvent};
use serde::Deserialize;
use std::collections::HashMap;
use std::env;
use std::sync::OnceLock;
use tokio::sync::Mutex;
use tracing::{error, info};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CognitoEvent {
    caller_context: CallerContext,
    user_name: String,
    #[allow(dead_code)]
    #[serde(flatten)]
    rest: serde_json::Value,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CallerContext {
    client_id: String,
}

static CLIENT_MAP: OnceLock<Mutex<Option<HashMap<String, String>>>> = OnceLock::new();

async fn load_client_map(ssm: &SsmClient) -> Result<HashMap<String, String>, Error> {
    let param_name = env::var("CLIENT_MAP_PARAM")?;
    let result = ssm.get_parameter().name(&param_name).send().await?;
    let value = result
        .parameter()
        .and_then(|p| p.value())
        .ok_or("CLIENT_MAP_PARAM has no value")?;
    let map: HashMap<String, String> = serde_json::from_str(value)?;
    Ok(map)
}

async fn get_client_map(ssm: &SsmClient) -> Result<HashMap<String, String>, Error> {
    let mutex = CLIENT_MAP.get_or_init(|| Mutex::new(None));
    let mut guard = mutex.lock().await;
    if guard.is_none() {
        *guard = Some(load_client_map(ssm).await?);
    }
    Ok(guard.as_ref().unwrap().clone())
}

async fn handler(event: LambdaEvent<serde_json::Value>) -> Result<serde_json::Value, Error> {
    let (payload, _ctx) = event.into_parts();

    let cognito: CognitoEvent = serde_json::from_value(payload.clone())?;
    let client_id = &cognito.caller_context.client_id;
    let username = &cognito.user_name;

    info!(username, client_id, "Pre-authentication check");

    // Seeded admin user always passes
    if username == "chris" {
        return Ok(payload);
    }

    let aws_config = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
    let ssm = SsmClient::new(&aws_config);
    let ddb = DdbClient::new(&aws_config);

    let map = get_client_map(&ssm).await?;
    let app_key = map
        .get(client_id)
        .ok_or_else(|| format!("Unknown application: {client_id}"))?;

    let table_name = env::var("TABLE_NAME")?;
    let result = ddb
        .get_item()
        .table_name(&table_name)
        .key(
            "username",
            aws_sdk_dynamodb::types::AttributeValue::S(username.clone()),
        )
        .send()
        .await?;

    let item = result.item().ok_or("Access denied")?;
    let apps = item
        .get("apps")
        .and_then(|v| v.as_m().ok())
        .ok_or("Access denied")?;

    if !apps.contains_key(app_key) {
        error!(
            username,
            app = app_key,
            "Access denied — app not in user record"
        );
        return Err("Access denied".into());
    }

    info!(username, app = app_key, "Access granted");
    Ok(payload)
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
