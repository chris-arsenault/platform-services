import { Client } from "pg";

interface BuildReport {
  repo: string;
  workflow: string;
  status: string;
  branch: string;
  commit_sha: string;
  run_id: string;
  run_url: string;
  duration_seconds?: number;
  lint_passed?: boolean;
  test_passed?: boolean;
}

const REQUIRED_FIELDS = ["repo", "workflow", "status", "branch", "commit_sha", "run_id"];

const SCHEMA = `
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
`;

let client: Client | null = null;

async function getClient(): Promise<Client> {
  if (client) return client;
  client = new Client({
    host: process.env.DB_HOST,
    port: parseInt(process.env.DB_PORT || "5432"),
    user: process.env.DB_USER,
    password: process.env.DB_PASSWORD,
    database: process.env.DB_NAME,
    ssl: { rejectUnauthorized: false },
  });
  await client.connect();
  await client.query(SCHEMA);
  return client;
}

function json(statusCode: number, body: unknown) {
  return {
    statusCode,
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(body),
  };
}

export async function handler(event: {
  httpMethod?: string;
  path?: string;
  body?: string;
  requestContext?: { http?: { method?: string; path?: string } };
}) {
  const method = event.requestContext?.http?.method || event.httpMethod || "GET";
  const path = event.requestContext?.http?.path || event.path || "/";

  try {
    const db = await getClient();

    if (method === "POST" && path === "/api/ci/report") {
      const report: BuildReport = JSON.parse(event.body || "{}");

      for (const field of REQUIRED_FIELDS) {
        if (!report[field as keyof BuildReport]) {
          return json(400, { error: `Missing required field: ${field}` });
        }
      }

      const token = process.env.INGEST_TOKEN;
      if (token) {
        const auth = (event as Record<string, unknown>).headers as Record<string, string> | undefined;
        const provided = auth?.["authorization"]?.replace("Bearer ", "") || auth?.["Authorization"]?.replace("Bearer ", "");
        if (provided !== token) {
          return json(401, { error: "Unauthorized" });
        }
      }

      await db.query(
        `INSERT INTO ci_builds (repo, workflow, status, branch, commit_sha, run_id, run_url, duration_seconds, lint_passed, test_passed)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
         ON CONFLICT (run_id) DO UPDATE SET
           status = EXCLUDED.status,
           duration_seconds = EXCLUDED.duration_seconds,
           lint_passed = EXCLUDED.lint_passed,
           test_passed = EXCLUDED.test_passed`,
        [
          report.repo,
          report.workflow,
          report.status,
          report.branch,
          report.commit_sha,
          report.run_id,
          report.run_url,
          report.duration_seconds || null,
          report.lint_passed ?? null,
          report.test_passed ?? null,
        ]
      );

      return json(200, { ok: true });
    }

    if (method === "GET" && path === "/api/ci/builds") {
      const result = await db.query(
        `SELECT repo, workflow, status, branch, commit_sha, run_id, run_url,
                duration_seconds, lint_passed, test_passed, created_at
         FROM ci_builds
         ORDER BY created_at DESC
         LIMIT 100`
      );
      return json(200, result.rows);
    }

    if (method === "GET" && path === "/api/ci/summary") {
      const result = await db.query(
        `SELECT DISTINCT ON (repo, workflow)
                repo, workflow, status, branch, commit_sha, run_url, created_at
         FROM ci_builds
         ORDER BY repo, workflow, created_at DESC`
      );
      return json(200, result.rows);
    }

    if (method === "GET" && path === "/api/ci/health") {
      return json(200, { ok: true });
    }

    return json(404, { error: "Not found" });
  } catch (err) {
    console.error(err);
    return json(500, { error: "Internal server error" });
  }
}
