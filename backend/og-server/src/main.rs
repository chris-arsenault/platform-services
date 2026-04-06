/// Platform OG Server — generic Lambda that serves HTML with dynamic OpenGraph
/// meta tags. Configured via OG_CONFIG env var (JSON). Each project deploys
/// its own instance with project-specific route config and DB credentials.
use lambda_http::{Body, Error, Request, Response, run, service_fn};
use serde::Deserialize;
use std::collections::HashMap;
use std::env;
use std::sync::OnceLock;
use tokio::sync::Mutex;
use tokio_postgres::Client;
use tracing::{error, info, warn};

const RDS_CA_BUNDLE: &[u8] = include_bytes!("../../certs/rds-global-bundle.pem");

// =============================================================================
// Configuration (from OG_CONFIG env var)
// =============================================================================

#[derive(Deserialize)]
struct OgConfig {
    site_name: String,
    defaults: OgDefaults,
    #[serde(default)]
    routes: Vec<RouteConfig>,
}

#[derive(Deserialize)]
struct OgDefaults {
    title: String,
    description: String,
    #[serde(default)]
    image: String,
}

#[derive(Deserialize)]
struct RouteConfig {
    /// URL pattern with named params, e.g. "/recipes/:slug"
    pattern: String,
    /// SQL query. Use $1, $2 for positional params from URL.
    query: String,
    /// If set, fetch all rows and match by slugifying this column against the URL param.
    /// If not set, URL params are passed directly as query parameters.
    #[serde(default)]
    match_field: Option<String>,
    /// Template for og:title. Use {{column_name}} for row values.
    title: String,
    /// Template for og:description.
    description: String,
    /// Template for og:image. Relative paths are resolved against site_url.
    #[serde(default)]
    image: Option<String>,
    /// og:type value (default: "article")
    #[serde(default = "default_og_type")]
    og_type: String,
}

fn default_og_type() -> String {
    "article".into()
}

// =============================================================================
// Route matching
// =============================================================================

struct MatchedRoute<'a> {
    config: &'a RouteConfig,
    params: Vec<String>,
}

fn match_route<'a>(path: &str, routes: &'a [RouteConfig]) -> Option<MatchedRoute<'a>> {
    for route in routes {
        let pattern_parts: Vec<&str> = route.pattern.trim_matches('/').split('/').collect();
        let path_parts: Vec<&str> = path.trim_matches('/').split('/').collect();

        if pattern_parts.len() != path_parts.len() {
            continue;
        }

        let mut params = Vec::new();
        let mut matched = true;

        for (pattern, actual) in pattern_parts.iter().zip(path_parts.iter()) {
            if let Some(stripped) = pattern.strip_prefix(':') {
                let _ = stripped; // param name, not needed — positional
                params.push((*actual).to_string());
            } else if pattern != actual {
                matched = false;
                break;
            }
        }

        if matched {
            return Some(MatchedRoute {
                config: route,
                params,
            });
        }
    }
    None
}

// =============================================================================
// Slug matching
// =============================================================================

fn slugify(s: &str) -> String {
    s.to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect::<String>()
        .split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-")
}

// =============================================================================
// Template rendering
// =============================================================================

fn render_template(template: &str, row: &HashMap<String, String>) -> String {
    let mut result = template.to_string();
    for (key, value) in row {
        result = result.replace(&format!("{{{{{key}}}}}"), value);
    }
    result
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

// =============================================================================
// OG tag resolution
// =============================================================================

struct OgTags {
    title: String,
    description: String,
    image: String,
    url: String,
    og_type: String,
}

async fn resolve_og(client: &Client, config: &OgConfig, site_url: &str, path: &str) -> OgTags {
    if let Some(matched) = match_route(path, &config.routes)
        && let Some(row) = query_og(client, matched.config, &matched.params).await
    {
        let image_raw = matched
            .config
            .image
            .as_deref()
            .map(|t| render_template(t, &row))
            .unwrap_or_default();
        let image = if image_raw.is_empty() {
            resolve_image(&config.defaults.image, site_url)
        } else {
            resolve_image(&image_raw, site_url)
        };

        return OgTags {
            title: render_template(&matched.config.title, &row),
            description: render_template(&matched.config.description, &row),
            image,
            url: format!("{site_url}{path}"),
            og_type: matched.config.og_type.clone(),
        };
    }

    // Default OG tags
    OgTags {
        title: config.defaults.title.clone(),
        description: config.defaults.description.clone(),
        image: resolve_image(&config.defaults.image, site_url),
        url: format!("{site_url}{path}"),
        og_type: "website".into(),
    }
}

/// OG resolution for static sites with no database. Matches routes by path and
/// uses each route's `title`/`description`/`image` fields as literal values
/// (no template substitution). Falls through to defaults for unmatched paths.
///
/// Activated when the `DB_HOST` env var is unset — callers in the handler
/// check this before invoking.
fn resolve_og_no_db(config: &OgConfig, site_url: &str, path: &str) -> OgTags {
    if let Some(matched) = match_route(path, &config.routes) {
        let image_raw = matched.config.image.clone().unwrap_or_default();
        let image = if image_raw.is_empty() {
            resolve_image(&config.defaults.image, site_url)
        } else {
            resolve_image(&image_raw, site_url)
        };

        return OgTags {
            title: matched.config.title.clone(),
            description: matched.config.description.clone(),
            image,
            url: format!("{site_url}{path}"),
            og_type: matched.config.og_type.clone(),
        };
    }

    // Unmatched: use defaults
    OgTags {
        title: config.defaults.title.clone(),
        description: config.defaults.description.clone(),
        image: resolve_image(&config.defaults.image, site_url),
        url: format!("{site_url}{path}"),
        og_type: "website".into(),
    }
}

fn resolve_image(image: &str, site_url: &str) -> String {
    if image.starts_with("http://") || image.starts_with("https://") {
        image.to_string()
    } else {
        format!("{site_url}{image}")
    }
}

async fn query_og(
    client: &Client,
    route: &RouteConfig,
    params: &[String],
) -> Option<HashMap<String, String>> {
    if let Some(ref match_field) = route.match_field {
        // Slug matching: fetch all rows, match by slugifying the match_field
        let slug = params.first()?;
        let rows = client.query(&route.query, &[]).await.ok()?;

        for row in &rows {
            let field_val: String = row.try_get(match_field.as_str()).ok()?;
            if slugify(&field_val) == *slug {
                let mut map = HashMap::new();
                for (i, col) in row.columns().iter().enumerate() {
                    let val: Option<String> = row.try_get(i).ok();
                    map.insert(col.name().to_string(), val.unwrap_or_default());
                }
                return Some(map);
            }
        }
        None
    } else {
        // Direct param matching: pass URL params as $1, $2, etc.
        let param_refs: Vec<&(dyn tokio_postgres::types::ToSql + Sync)> = params
            .iter()
            .map(|s| s as &(dyn tokio_postgres::types::ToSql + Sync))
            .collect();
        let row = client.query_opt(&route.query, &param_refs).await.ok()??;
        let mut map = HashMap::new();
        for (i, col) in row.columns().iter().enumerate() {
            let val: Option<String> = row.try_get(i).ok();
            map.insert(col.name().to_string(), val.unwrap_or_default());
        }
        Some(map)
    }
}

// =============================================================================
// HTML rendering
// =============================================================================

fn render_html(og: &OgTags, site_name: &str, entry_js: &str, entry_css: &str) -> String {
    let title = html_escape(&og.title);
    let desc = html_escape(&og.description);
    let image = html_escape(&og.image);
    let url = html_escape(&og.url);
    let site = html_escape(site_name);

    [
        "<!DOCTYPE html><html lang=\"en\"><head>",
        "<meta charset=\"utf-8\" />",
        "<link rel=\"icon\" type=\"image/svg+xml\" href=\"/favicon.svg\" />",
        "<link rel=\"icon\" type=\"image/png\" sizes=\"32x32\" href=\"/favicon-32x32.png\" />",
        "<link rel=\"icon\" type=\"image/png\" sizes=\"16x16\" href=\"/favicon-16x16.png\" />",
        "<link rel=\"apple-touch-icon\" href=\"/apple-touch-icon.png\" />",
        "<meta name=\"viewport\" content=\"width=device-width, initial-scale=1.0, viewport-fit=cover\" />",
        "<meta name=\"mobile-web-app-capable\" content=\"yes\" />",
        "<meta name=\"apple-mobile-web-app-capable\" content=\"yes\" />",
        "<meta name=\"apple-mobile-web-app-status-bar-style\" content=\"black-translucent\" />",
        "<link rel=\"manifest\" href=\"/manifest.webmanifest\" />",
        &format!("<title>{title} | {site}</title>"),
        &format!("<meta name=\"description\" content=\"{desc}\" />"),
        &format!("<meta property=\"og:title\" content=\"{title}\" />"),
        &format!("<meta property=\"og:description\" content=\"{desc}\" />"),
        &format!("<meta property=\"og:image\" content=\"{image}\" />"),
        &format!("<meta property=\"og:url\" content=\"{url}\" />"),
        &format!("<meta property=\"og:type\" content=\"{}\" />", html_escape(&og.og_type)),
        &format!("<meta property=\"og:site_name\" content=\"{site}\" />"),
        "<meta name=\"twitter:card\" content=\"summary_large_image\" />",
        &format!("<meta name=\"twitter:title\" content=\"{title}\" />"),
        &format!("<meta name=\"twitter:description\" content=\"{desc}\" />"),
        &format!("<meta name=\"twitter:image\" content=\"{image}\" />"),
        &format!("<link rel=\"stylesheet\" crossorigin href=\"{entry_css}\" />"),
        "</head><body><div id=\"root\"></div>",
        "<script src=\"/config.js\"></script>",
        &format!("<script type=\"module\" crossorigin src=\"{entry_js}\"></script>"),
        "</body></html>",
    ]
    .join("\n")
}

// =============================================================================
// Database connection
// =============================================================================

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
    let user = env::var("DB_USERNAME")?;
    let pass = env::var("DB_PASSWORD")?;

    let connstr = format!(
        "host={host} port={port} user={user} password={pass} dbname={db_name} sslmode=require"
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

// =============================================================================
// Lambda handler
// =============================================================================

struct AppConfig {
    og: OgConfig,
    entry_js: String,
    entry_css: String,
    site_url: String,
}

static CONFIG: OnceLock<AppConfig> = OnceLock::new();

fn get_config() -> &'static AppConfig {
    CONFIG.get_or_init(|| {
        let og_json = env::var("OG_CONFIG").expect("OG_CONFIG env var required");
        let og: OgConfig = serde_json::from_str(&og_json).expect("OG_CONFIG is not valid JSON");
        let entry_js = env::var("ENTRY_JS").unwrap_or_else(|_| "/assets/index.js".into());
        let entry_css = env::var("ENTRY_CSS").unwrap_or_else(|_| "/assets/index.css".into());
        let site_url = env::var("SITE_URL").expect("SITE_URL env var required");

        info!(
            site_name = %og.site_name,
            routes = og.routes.len(),
            "og-server configured"
        );

        AppConfig {
            og,
            entry_js,
            entry_css,
            site_url,
        }
    })
}

async fn handler(req: Request) -> Result<Response<Body>, Error> {
    let path = req.uri().path().to_string();
    info!(path = %path, "og-server request");

    let config = get_config();

    // Two modes:
    //   - DB_HOST set   → dynamic mode: query DB, render templates
    //   - DB_HOST unset → static mode: route literals only, no DB connection
    // When DB is configured but the connection fails, fall back to defaults only
    // (no route matching) so projects like tastebase don't leak unrendered
    // `{{placeholder}}` strings into OG tags during outages.
    let db_configured = env::var("DB_HOST").is_ok();

    let (og, db_failed) = if db_configured {
        match ensure_client().await {
            Ok(guard) => {
                let client = guard.as_ref().expect("client initialized on success");
                let og = resolve_og(client, &config.og, &config.site_url, &path).await;
                (og, false)
            }
            Err(e) => {
                warn!(error = %e, "DB connection failed, using default OG tags");
                let og = OgTags {
                    title: config.og.defaults.title.clone(),
                    description: config.og.defaults.description.clone(),
                    image: resolve_image(&config.og.defaults.image, &config.site_url),
                    url: format!("{}{}", config.site_url, path),
                    og_type: "website".into(),
                };
                (og, true)
            }
        }
    } else {
        let og = resolve_og_no_db(&config.og, &config.site_url, &path);
        (og, false)
    };

    // Matched routes get longer cache (content changes less often than defaults).
    // On DB failure we keep the short cache to recover quickly once DB is back.
    let cache_control = if !db_failed && match_route(&path, &config.og.routes).is_some() {
        "public, s-maxage=86400, max-age=0"
    } else {
        "public, s-maxage=3600, max-age=0"
    };

    let html = render_html(
        &og,
        &config.og.site_name,
        &config.entry_js,
        &config.entry_css,
    );

    Ok(Response::builder()
        .status(200)
        .header("content-type", "text/html; charset=utf-8")
        .header("cache-control", cache_control)
        .body(Body::Text(html))?)
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

    // Eagerly validate config at startup
    let _ = get_config();

    run(service_fn(handler)).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> OgConfig {
        OgConfig {
            site_name: "Test Site".into(),
            defaults: OgDefaults {
                title: "Default Title".into(),
                description: "Default Description".into(),
                image: "/default.png".into(),
            },
            routes: vec![
                RouteConfig {
                    pattern: "/about".into(),
                    query: String::new(),
                    match_field: None,
                    title: "About Us".into(),
                    description: "Learn more about the project".into(),
                    image: Some("/about.png".into()),
                    og_type: "website".into(),
                },
                RouteConfig {
                    pattern: "/".into(),
                    query: String::new(),
                    match_field: None,
                    title: "Home".into(),
                    description: "Welcome".into(),
                    image: None,
                    og_type: "website".into(),
                },
            ],
        }
    }

    #[test]
    fn resolve_og_no_db_uses_literal_fields_for_matched_route() {
        let config = test_config();
        let og = resolve_og_no_db(&config, "https://example.com", "/about");
        assert_eq!(og.title, "About Us");
        assert_eq!(og.description, "Learn more about the project");
        assert_eq!(og.image, "https://example.com/about.png");
        assert_eq!(og.url, "https://example.com/about");
        assert_eq!(og.og_type, "website");
    }

    #[test]
    fn resolve_og_no_db_falls_back_to_default_image_when_route_has_none() {
        let config = test_config();
        let og = resolve_og_no_db(&config, "https://example.com", "/");
        assert_eq!(og.title, "Home");
        assert_eq!(og.description, "Welcome");
        assert_eq!(og.image, "https://example.com/default.png");
    }

    #[test]
    fn resolve_og_no_db_uses_defaults_for_unmatched_path() {
        let config = test_config();
        let og = resolve_og_no_db(&config, "https://example.com", "/nonexistent");
        assert_eq!(og.title, "Default Title");
        assert_eq!(og.description, "Default Description");
        assert_eq!(og.image, "https://example.com/default.png");
        assert_eq!(og.og_type, "website");
        assert_eq!(og.url, "https://example.com/nonexistent");
    }

    #[test]
    fn resolve_og_no_db_does_not_substitute_templates() {
        // Literal braces in title should pass through unchanged in no-db mode.
        let config = OgConfig {
            site_name: "Test".into(),
            defaults: OgDefaults {
                title: "Default".into(),
                description: "Default".into(),
                image: "/d.png".into(),
            },
            routes: vec![RouteConfig {
                pattern: "/page".into(),
                query: String::new(),
                match_field: None,
                title: "Literal {{unused}} Title".into(),
                description: "Literal description".into(),
                image: None,
                og_type: "website".into(),
            }],
        };
        let og = resolve_og_no_db(&config, "https://example.com", "/page");
        assert_eq!(og.title, "Literal {{unused}} Title");
    }
}
