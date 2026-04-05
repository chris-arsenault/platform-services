use ci_ingest::db::{self, BuildReport};
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;
use tokio_postgres::NoTls;

async fn setup() -> (
    tokio_postgres::Client,
    testcontainers::ContainerAsync<Postgres>,
) {
    let container = Postgres::default().start().await.unwrap();
    let host = container.get_host().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let connstr =
        format!("host={host} port={port} user=postgres password=postgres dbname=postgres");
    let (client, connection) = tokio_postgres::connect(&connstr, NoTls).await.unwrap();
    tokio::spawn(async move {
        connection.await.ok();
    });
    db::init_schema(&client).await.unwrap();
    (client, container)
}

fn sample_report(run_id: &str) -> BuildReport {
    BuildReport {
        repo: Some("chris-arsenault/test-repo".into()),
        workflow: Some("Deploy".into()),
        status: Some("success".into()),
        branch: Some("main".into()),
        commit_sha: Some("abc123".into()),
        run_id: Some(run_id.into()),
        run_url: Some("https://github.com/test/actions/runs/1".into()),
        duration_seconds: Some(42),
        lint_passed: Some(true),
        test_passed: Some(true),
    }
}

#[tokio::test]
async fn test_validate_report_ok() {
    let report = sample_report("run-1");
    assert!(db::validate_report(&report).is_ok());
}

#[tokio::test]
async fn test_validate_report_missing_repo() {
    let mut report = sample_report("run-1");
    report.repo = None;
    assert!(db::validate_report(&report).is_err());
}

#[tokio::test]
async fn test_validate_report_empty_status() {
    let mut report = sample_report("run-1");
    report.status = Some("".into());
    assert!(db::validate_report(&report).is_err());
}

#[tokio::test]
async fn test_upsert_and_get_builds() {
    let (client, _container) = setup().await;

    db::upsert_build(&client, &sample_report("run-100"))
        .await
        .unwrap();
    db::upsert_build(&client, &sample_report("run-101"))
        .await
        .unwrap();

    let builds = db::get_builds(&client).await.unwrap();
    assert_eq!(builds.len(), 2);
    assert_eq!(builds[0].run_id, "run-101"); // most recent first
    assert_eq!(builds[1].run_id, "run-100");
}

#[tokio::test]
async fn test_upsert_updates_on_conflict() {
    let (client, _container) = setup().await;

    let mut report = sample_report("run-200");
    report.status = Some("running".into());
    report.lint_passed = None;
    db::upsert_build(&client, &report).await.unwrap();

    report.status = Some("success".into());
    report.lint_passed = Some(true);
    db::upsert_build(&client, &report).await.unwrap();

    let builds = db::get_builds(&client).await.unwrap();
    assert_eq!(builds.len(), 1);
    assert_eq!(builds[0].status, "success");
    assert_eq!(builds[0].lint_passed, Some(true));
}

#[tokio::test]
async fn test_summary_returns_latest_per_repo_workflow() {
    let (client, _container) = setup().await;

    let mut r1 = sample_report("run-300");
    r1.status = Some("failure".into());
    db::upsert_build(&client, &r1).await.unwrap();

    let mut r2 = sample_report("run-301");
    r2.status = Some("success".into());
    db::upsert_build(&client, &r2).await.unwrap();

    let summary = db::get_summary(&client).await.unwrap();
    assert_eq!(summary.len(), 1);
    assert_eq!(summary[0].status, "success");
}

#[tokio::test]
async fn test_summary_groups_by_repo_and_workflow() {
    let (client, _container) = setup().await;

    db::upsert_build(&client, &sample_report("run-400"))
        .await
        .unwrap();

    let mut different_workflow = sample_report("run-401");
    different_workflow.workflow = Some("Lint".into());
    db::upsert_build(&client, &different_workflow)
        .await
        .unwrap();

    let summary = db::get_summary(&client).await.unwrap();
    assert_eq!(summary.len(), 2);
}

#[tokio::test]
async fn test_empty_builds() {
    let (client, _container) = setup().await;
    let builds = db::get_builds(&client).await.unwrap();
    assert!(builds.is_empty());
}

#[tokio::test]
async fn test_empty_summary() {
    let (client, _container) = setup().await;
    let summary = db::get_summary(&client).await.unwrap();
    assert!(summary.is_empty());
}
