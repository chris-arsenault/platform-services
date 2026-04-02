use aws_sdk_ssm::Client as SsmClient;
use lambda_runtime::{service_fn, Error, LambdaEvent};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::env;
use tracing::{error, info};

#[derive(Deserialize)]
struct Request {
    /// Komodo API actions to execute in order.
    actions: Vec<KomodoAction>,
}

#[derive(Deserialize)]
#[serde(tag = "type")]
enum KomodoAction {
    /// Set Komodo variables. Values starting with "ssm:" are resolved from SSM.
    /// e.g. { "type": "SetVariables", "variables": { "DB_HOST": "ssm:/platform/rds/address" } }
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

    /// HTTP call to a service on the TrueNAS network (192.168.66.0/24 only).
    /// Header/body values starting with "ssm:" are resolved from SSM.
    /// Returns the response body for the caller to process.
    HttpCall {
        url: String,
        method: String,
        #[serde(default)]
        headers: HashMap<String, String>,
        #[serde(default)]
        form: HashMap<String, String>,
    },

    /// Forward a raw Komodo API request.
    /// e.g. { "type": "Api", "method": "POST", "path": "/execute/DeployStack", "body": {...} }
    Api {
        method: String,
        path: String,
        #[serde(default)]
        body: Option<serde_json::Value>,
    },
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
    async fn request(
        &self,
        method: &str,
        path: &str,
        body: Option<&serde_json::Value>,
    ) -> Result<serde_json::Value, String> {
        let url = format!("{}{}", self.base_url, path);
        let builder = match method.to_uppercase().as_str() {
            "GET" => self.http.get(&url),
            "POST" => self.http.post(&url),
            "PATCH" => self.http.patch(&url),
            "PUT" => self.http.put(&url),
            "DELETE" => self.http.delete(&url),
            _ => return Err(format!("Unsupported method: {method}")),
        };

        let builder = builder
            .header("x-api-key", &self.api_key)
            .header("x-api-secret", &self.api_secret);

        let builder = if let Some(b) = body {
            builder.header("content-type", "application/json").json(b)
        } else {
            builder
        };

        let resp = builder.send().await.map_err(|e| e.to_string())?;
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

    // Push each variable to Komodo via the API
    for (name, value) in &resolved {
        let body = serde_json::json!({
            "name": name,
            "value": value,
            "is_secret": secret,
        });

        match komodo
            .request("POST", "/write/CreateVariable", Some(&body))
            .await
        {
            Ok(_) => {
                info!(name = name, "Variable set");
            }
            Err(e) => {
                // Try update if create fails (variable already exists)
                let update_body = serde_json::json!({
                    "name": name,
                    "value": value,
                });
                if let Err(e2) = komodo
                    .request("POST", "/write/UpdateVariableValue", Some(&update_body))
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

    match komodo
        .request("POST", "/write/UpdateStack", Some(&body))
        .await
    {
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

async fn handle_http_call(
    ssm: &SsmClient,
    http: &reqwest::Client,
    url: &str,
    method: &str,
    headers: &HashMap<String, String>,
    form: &HashMap<String, String>,
) -> ActionResult {
    // Validate URL is within TrueNAS network
    let host = url
        .split("://")
        .nth(1)
        .and_then(|s| s.split('/').next())
        .and_then(|s| s.split(':').next())
        .unwrap_or("");
    if !host.starts_with("192.168.66.") {
        return ActionResult {
            action: "HttpCall".into(),
            success: false,
            body: None,
            error: Some(format!(
                "URL host {host} not in allowed range 192.168.66.0/24"
            )),
        };
    }

    // Resolve SSM values in headers
    let mut resolved_headers = Vec::new();
    for (k, v) in headers {
        match resolve_ssm_value(ssm, v).await {
            Ok(resolved) => resolved_headers.push((k.clone(), resolved)),
            Err(e) => {
                return ActionResult {
                    action: "HttpCall".into(),
                    success: false,
                    body: None,
                    error: Some(e),
                };
            }
        }
    }

    // Resolve SSM values in form fields
    let mut resolved_form = Vec::new();
    for (k, v) in form {
        match resolve_ssm_value(ssm, v).await {
            Ok(resolved) => resolved_form.push((k.clone(), resolved)),
            Err(e) => {
                return ActionResult {
                    action: "HttpCall".into(),
                    success: false,
                    body: None,
                    error: Some(e),
                };
            }
        }
    }

    let builder = match method.to_uppercase().as_str() {
        "GET" => http.get(url),
        "POST" => http.post(url),
        "PUT" => http.put(url),
        "PATCH" => http.patch(url),
        "DELETE" => http.delete(url),
        _ => {
            return ActionResult {
                action: "HttpCall".into(),
                success: false,
                body: None,
                error: Some(format!("Unsupported method: {method}")),
            };
        }
    };

    let mut builder = builder;
    for (k, v) in &resolved_headers {
        builder = builder.header(k, v);
    }
    if !resolved_form.is_empty() {
        builder = builder.form(&resolved_form);
    }

    match builder.send().await {
        Ok(resp) => {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            let body_json: serde_json::Value =
                serde_json::from_str(&text).unwrap_or(serde_json::Value::String(text.clone()));
            ActionResult {
                action: "HttpCall".into(),
                success: status.is_success(),
                body: Some(serde_json::json!({ "status": status.as_u16(), "body": body_json })),
                error: if status.is_success() {
                    None
                } else {
                    Some(format!("HTTP {status}: {text}"))
                },
            }
        }
        Err(e) => ActionResult {
            action: "HttpCall".into(),
            success: false,
            body: None,
            error: Some(e.to_string()),
        },
    }
}

async fn handle_api(
    komodo: &KomodoClient,
    method: &str,
    path: &str,
    body: Option<&serde_json::Value>,
) -> ActionResult {
    match komodo.request(method, path, body).await {
        Ok(resp) => ActionResult {
            action: format!("{method} {path}"),
            success: true,
            body: Some(resp),
            error: None,
        },
        Err(e) => ActionResult {
            action: format!("{method} {path}"),
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

    // Read Komodo connection details from SSM
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
            KomodoAction::HttpCall {
                url,
                method,
                headers,
                form,
            } => handle_http_call(&ssm, &komodo.http, url, method, headers, form).await,
            KomodoAction::Api { method, path, body } => {
                handle_api(&komodo, method, path, body.as_ref()).await
            }
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
