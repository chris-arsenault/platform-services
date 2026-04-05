use serde::{Deserialize, Serialize};
use tokio_postgres::Client;

/// Schema is managed by the platform migration service (db-migrate).
/// Only exposed for integration tests where there is no migration service.
#[cfg(any(test, feature = "test-support"))]
pub const SCHEMA: &str = include_str!("../../../db/migrations/001_create_ci_builds.sql");

#[cfg(any(test, feature = "test-support"))]
pub async fn init_schema(client: &Client) -> Result<(), tokio_postgres::Error> {
    client.batch_execute(SCHEMA).await
}

#[derive(Deserialize)]
pub struct BuildReport {
    pub repo: Option<String>,
    pub workflow: Option<String>,
    pub status: Option<String>,
    pub branch: Option<String>,
    pub commit_sha: Option<String>,
    pub run_id: Option<String>,
    pub run_url: Option<String>,
    pub duration_seconds: Option<i32>,
    pub lint_passed: Option<bool>,
    pub test_passed: Option<bool>,
}

#[derive(Serialize, Debug)]
pub struct BuildRow {
    pub repo: String,
    pub workflow: String,
    pub status: String,
    pub branch: String,
    pub commit_sha: String,
    pub run_id: String,
    pub run_url: Option<String>,
    pub duration_seconds: Option<i32>,
    pub lint_passed: Option<bool>,
    pub test_passed: Option<bool>,
    pub created_at: String,
}

#[derive(Serialize, Debug)]
pub struct SummaryRow {
    pub repo: String,
    pub workflow: String,
    pub status: String,
    pub branch: String,
    pub commit_sha: String,
    pub run_url: Option<String>,
    pub created_at: String,
}

pub fn validate_report(report: &BuildReport) -> Result<(), &'static str> {
    let fields = [
        &report.repo,
        &report.workflow,
        &report.status,
        &report.branch,
        &report.commit_sha,
        &report.run_id,
    ];
    for f in fields {
        if f.as_deref().unwrap_or("").is_empty() {
            return Err("Missing required fields");
        }
    }
    Ok(())
}

pub async fn upsert_build(
    client: &Client,
    report: &BuildReport,
) -> Result<(), tokio_postgres::Error> {
    client.execute(
        "INSERT INTO ci_builds (repo, workflow, status, branch, commit_sha, run_id, run_url, duration_seconds, lint_passed, test_passed)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
         ON CONFLICT (run_id) DO UPDATE SET
           status = EXCLUDED.status,
           duration_seconds = EXCLUDED.duration_seconds,
           lint_passed = EXCLUDED.lint_passed,
           test_passed = EXCLUDED.test_passed",
        &[
            &report.repo.as_deref().unwrap_or(""),
            &report.workflow.as_deref().unwrap_or(""),
            &report.status.as_deref().unwrap_or(""),
            &report.branch.as_deref().unwrap_or(""),
            &report.commit_sha.as_deref().unwrap_or(""),
            &report.run_id.as_deref().unwrap_or(""),
            &report.run_url.as_deref(),
            &report.duration_seconds,
            &report.lint_passed,
            &report.test_passed,
        ],
    ).await?;
    Ok(())
}

pub async fn get_builds(client: &Client) -> Result<Vec<BuildRow>, tokio_postgres::Error> {
    let rows = client
        .query(
            "SELECT repo, workflow, status, branch, commit_sha, run_id, run_url,
                duration_seconds, lint_passed, test_passed, created_at
         FROM ci_builds ORDER BY created_at DESC LIMIT 100",
            &[],
        )
        .await?;

    Ok(rows
        .iter()
        .map(|r| BuildRow {
            repo: r.get(0),
            workflow: r.get(1),
            status: r.get(2),
            branch: r.get(3),
            commit_sha: r.get(4),
            run_id: r.get(5),
            run_url: r.get(6),
            duration_seconds: r.get(7),
            lint_passed: r.get(8),
            test_passed: r.get(9),
            created_at: r.get::<_, chrono::DateTime<chrono::Utc>>(10).to_rfc3339(),
        })
        .collect())
}

pub async fn get_summary(client: &Client) -> Result<Vec<SummaryRow>, tokio_postgres::Error> {
    let rows = client
        .query(
            "SELECT DISTINCT ON (repo, workflow)
                repo, workflow, status, branch, commit_sha, run_url, created_at
         FROM ci_builds ORDER BY repo, workflow, created_at DESC",
            &[],
        )
        .await?;

    Ok(rows
        .iter()
        .map(|r| SummaryRow {
            repo: r.get(0),
            workflow: r.get(1),
            status: r.get(2),
            branch: r.get(3),
            commit_sha: r.get(4),
            run_url: r.get(5),
            created_at: r.get::<_, chrono::DateTime<chrono::Utc>>(6).to_rfc3339(),
        })
        .collect())
}
