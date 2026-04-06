/// Platform-wide CORS preflight handler.
/// Responds to all OPTIONS requests with permissive CORS headers.
/// Deployed as a single Lambda behind ALB priority 1.
use lambda_http::{Body, Error, Request, Response, run, service_fn};

async fn handler(_req: Request) -> Result<Response<Body>, Error> {
    Ok(Response::builder()
        .status(204)
        .header("Access-Control-Allow-Origin", "*")
        .header(
            "Access-Control-Allow-Methods",
            "GET, POST, PUT, DELETE, OPTIONS, HEAD",
        )
        .header(
            "Access-Control-Allow-Headers",
            "Authorization, Content-Type",
        )
        .header("Access-Control-Max-Age", "86400")
        .body(Body::Empty)?)
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("info".parse().unwrap()),
        )
        .without_time()
        .init();

    run(service_fn(handler)).await
}
