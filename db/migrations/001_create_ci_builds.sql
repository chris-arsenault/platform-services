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
