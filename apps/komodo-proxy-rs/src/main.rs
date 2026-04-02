use aws_sdk_ssm::Client as SsmClient;
use lambda_runtime::{service_fn, Error, LambdaEvent};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::env;
use tracing::{error, info};

#[derive(Deserialize)]
struct Request {
    actions: Vec<KomodoAction>,
}

#[derive(Deserialize)]
#[serde(tag = "type")]
enum KomodoAction {
    /// Set Komodo variables. Values starting with "ssm:" are resolved from SSM.
    SetVariables {
        variables: HashMap<String, String>,
        #[serde(default)]
        secret: bool,
    },

    /// Set a stack's environment field. Resolves SSM values, builds KEY=value lines,
    /// and updates the stack config via the Komodo API. Docker Compose reads these from .env.
    SetEnvironment {
        stack: String,
        variables: HashMap<String, String>,
    },

    /// List all Komodo servers.
    ListServers,

    /// Create a new stack with git-backed compose.
    CreateStack {
        name: String,
        server: String,
        repo: String,
        branch: String,
        file_paths: Vec<String>,
        #[serde(default = "default_git_provider")]
        git_provider: String,
    },

    /// Deploy an existing stack.
    DeployStack { stack: String },
}

fn default_git_provider() -> String {
    "github.com".into()
}

#[derive(Serialize)]
struct Response {
    results: Vec<ActionResult>,
}

#[derive(Serialize)]
struct ActionResult {
    action: String,
    success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    body: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

struct KomodoClient {
    http: reqwest::Client,
    base_url: String,
    api_key: String,
    api_secret: String,
}

impl KomodoClient {
    async fn post(
        &self,
        path: &str,
        body: &serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        let url = format!("{}{}", self.base_url, path);
        let resp = self
            .http
            .post(&url)
            .header("x-api-key", &self.api_key)
            .header("x-api-secret", &self.api_secret)
            .header("content-type", "application/json")
            .json(body)
            .send()
            .await
            .map_err(|e| e.to_string())?;

        let status = resp.status();
        let text = resp.text().await.map_err(|e| e.to_string())?;

        if !status.is_success() {
            return Err(format!("Komodo API {status}: {text}"));
        }

        Ok(serde_json::from_str(&text).unwrap_or(serde_json::Value::String(text)))
    }
}

async fn resolve_ssm_value(ssm: &SsmClient, value: &str) -> Result<String, String> {
    if let Some(param_name) = value.strip_prefix("ssm:") {
        let result = ssm
            .get_parameter()
            .name(param_name)
            .with_decryption(true)
            .send()
            .await
            .map_err(|e| format!("SSM read failed for {param_name}: {e}"))?;

        result
            .parameter()
            .and_then(|p| p.value())
            .map(|v| v.to_string())
            .ok_or_else(|| format!("SSM param {param_name} has no value"))
    } else {
        Ok(value.to_string())
    }
}

async fn handle_set_variables(
    ssm: &SsmClient,
    komodo: &KomodoClient,
    variables: &HashMap<String, String>,
    secret: bool,
) -> ActionResult {
    let mut resolved = Vec::new();

    for (name, value) in variables {
        match resolve_ssm_value(ssm, value).await {
            Ok(v) => resolved.push((name.clone(), v)),
            Err(e) => {
                return ActionResult {
                    action: "SetVariables".into(),
                    success: false,
                    body: None,
                    error: Some(e),
                };
            }
        }
    }

    for (name, value) in &resolved {
        let body = serde_json::json!({
            "name": name,
            "value": value,
            "is_secret": secret,
        });

        match komodo.post("/write/CreateVariable", &body).await {
            Ok(_) => {
                info!(name = name, "Variable set");
            }
            Err(e) => {
                let update_body = serde_json::json!({
                    "name": name,
                    "value": value,
                });
                if let Err(e2) = komodo
                    .post("/write/UpdateVariableValue", &update_body)
                    .await
                {
                    return ActionResult {
                        action: "SetVariables".into(),
                        success: false,
                        body: None,
                        error: Some(format!("Create: {e}, Update: {e2}")),
                    };
                }
                info!(name = name, "Variable updated");
            }
        }
    }

    ActionResult {
        action: "SetVariables".into(),
        success: true,
        body: Some(serde_json::json!({ "count": resolved.len() })),
        error: None,
    }
}

async fn handle_set_environment(
    ssm: &SsmClient,
    komodo: &KomodoClient,
    stack: &str,
    variables: &HashMap<String, String>,
) -> ActionResult {
    let mut env_lines = Vec::new();

    for (name, value) in variables {
        match resolve_ssm_value(ssm, value).await {
            Ok(v) => env_lines.push(format!("{name}={v}")),
            Err(e) => {
                return ActionResult {
                    action: "SetEnvironment".into(),
                    success: false,
                    body: None,
                    error: Some(e),
                };
            }
        }
    }

    let environment = env_lines.join("\n");
    let body = serde_json::json!({
        "id": stack,
        "config": {
            "environment": environment,
        },
    });

    match komodo.post("/write/UpdateStack", &body).await {
        Ok(_) => {
            info!(
                stack = stack,
                count = env_lines.len(),
                "Stack environment updated"
            );
            ActionResult {
                action: "SetEnvironment".into(),
                success: true,
                body: Some(serde_json::json!({ "count": env_lines.len() })),
                error: None,
            }
        }
        Err(e) => ActionResult {
            action: "SetEnvironment".into(),
            success: false,
            body: None,
            error: Some(e),
        },
    }
}

async fn handle_list_servers(komodo: &KomodoClient) -> ActionResult {
    match komodo
        .post("/read/ListServers", &serde_json::json!({}))
        .await
    {
        Ok(resp) => ActionResult {
            action: "ListServers".into(),
            success: true,
            body: Some(resp),
            error: None,
        },
        Err(e) => ActionResult {
            action: "ListServers".into(),
            success: false,
            body: None,
            error: Some(e),
        },
    }
}

async fn handle_create_stack(
    komodo: &KomodoClient,
    name: &str,
    server: &str,
    repo: &str,
    branch: &str,
    file_paths: &[String],
    git_provider: &str,
) -> ActionResult {
    let body = serde_json::json!({
        "name": name,
        "config": {
            "server": server,
            "repo": repo,
            "branch": branch,
            "file_paths": file_paths,
            "git_provider": git_provider,
        },
    });

    match komodo.post("/write/CreateStack", &body).await {
        Ok(resp) => ActionResult {
            action: "CreateStack".into(),
            success: true,
            body: Some(resp),
            error: None,
        },
        Err(e) => ActionResult {
            action: "CreateStack".into(),
            success: false,
            body: None,
            error: Some(e),
        },
    }
}

async fn handle_deploy_stack(komodo: &KomodoClient, stack: &str) -> ActionResult {
    let body = serde_json::json!({ "stack": stack });

    match komodo.post("/execute/DeployStack", &body).await {
        Ok(resp) => ActionResult {
            action: "DeployStack".into(),
            success: true,
            body: Some(resp),
            error: None,
        },
        Err(e) => ActionResult {
            action: "DeployStack".into(),
            success: false,
            body: None,
            error: Some(e),
        },
    }
}

async fn handler(event: LambdaEvent<serde_json::Value>) -> Result<serde_json::Value, Error> {
    let (payload, _ctx) = event.into_parts();
    info!(event = %payload, "Komodo proxy invoked");

    let request: Request = serde_json::from_value(payload)?;

    let aws_config = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
    let ssm = SsmClient::new(&aws_config);

    let api_key = resolve_ssm_value(&ssm, "ssm:/platform/komodo/api-key")
        .await
        .map_err(|e| -> Error { e.into() })?;
    let api_secret = resolve_ssm_value(&ssm, "ssm:/platform/komodo/api-secret")
        .await
        .map_err(|e| -> Error { e.into() })?;
    let base_url = env::var("KOMODO_URL").unwrap_or_else(|_| "http://192.168.66.3:30160".into());

    let komodo = KomodoClient {
        http: reqwest::Client::new(),
        base_url,
        api_key,
        api_secret,
    };

    let mut results = Vec::new();

    for action in &request.actions {
        let result = match action {
            KomodoAction::SetVariables { variables, secret } => {
                handle_set_variables(&ssm, &komodo, variables, *secret).await
            }
            KomodoAction::SetEnvironment { stack, variables } => {
                handle_set_environment(&ssm, &komodo, stack, variables).await
            }
            KomodoAction::ListServers => handle_list_servers(&komodo).await,
            KomodoAction::CreateStack {
                name,
                server,
                repo,
                branch,
                file_paths,
                git_provider,
            } => {
                handle_create_stack(
                    &komodo,
                    name,
                    server,
                    repo,
                    branch,
                    file_paths,
                    git_provider,
                )
                .await
            }
            KomodoAction::DeployStack { stack } => handle_deploy_stack(&komodo, stack).await,
        };

        let failed = !result.success;
        if failed {
            error!(
                action = result.action,
                error = result.error.as_deref().unwrap_or("unknown"),
                "Action failed, aborting remaining actions"
            );
        }
        results.push(result);

        if failed {
            break;
        }
    }

    Ok(serde_json::to_value(Response { results })?)
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
