import { Client } from "pg";
import { S3Client, ListObjectsV2Command, GetObjectCommand } from "@aws-sdk/client-s3";
import { createHash } from "crypto";

const s3 = new S3Client({});

const OPS_DB = "platform_ops";

interface ProjectConfig {
  db_name: string;
}

interface S3Event {
  detail: { bucket: { name: string }; object: { key: string } };
}

interface ManualEvent {
  operation: "migrate" | "rollback" | "seed" | "drop" | "noop" | "restore";
  project: string;
  target?: string;  // rollback: filename to roll back to. noop: migration filename. restore: S3 key.
  comment?: string; // required for noop and restore
}

type MigrationEvent = S3Event | ManualEvent;

const LOCAL_TRACKING = `
CREATE TABLE IF NOT EXISTS schema_migrations (
  id SERIAL PRIMARY KEY,
  filename TEXT NOT NULL UNIQUE,
  checksum TEXT NOT NULL,
  noop BOOLEAN NOT NULL DEFAULT FALSE,
  comment TEXT,
  applied_at TIMESTAMPTZ DEFAULT NOW(),
  duration_ms INTEGER
);
`;

const OPS_SCHEMA = `
CREATE TABLE IF NOT EXISTS migration_audit (
  id SERIAL PRIMARY KEY,
  project TEXT NOT NULL,
  operation TEXT NOT NULL,
  filename TEXT,
  checksum TEXT,
  status TEXT NOT NULL,
  comment TEXT,
  error_message TEXT,
  duration_ms INTEGER,
  created_at TIMESTAMPTZ DEFAULT NOW()
);
CREATE INDEX IF NOT EXISTS idx_audit_project ON migration_audit (project, created_at DESC);

CREATE TABLE IF NOT EXISTS seed_runs (
  id SERIAL PRIMARY KEY,
  project TEXT NOT NULL,
  filename TEXT NOT NULL,
  checksum TEXT NOT NULL,
  created_at TIMESTAMPTZ DEFAULT NOW()
);
CREATE INDEX IF NOT EXISTS idx_seed_project ON seed_runs (project, created_at DESC);
`;

function log(level: string, msg: string, data?: Record<string, unknown>) {
  console.log(JSON.stringify({ level, msg, ts: new Date().toISOString(), ...data }));
}

function getProjectMap(): Record<string, ProjectConfig> {
  return JSON.parse(process.env.PROJECT_MAP!);
}

function getBucket(): string {
  return process.env.MIGRATIONS_BUCKET!;
}

function requireProject(project: string): ProjectConfig {
  const map = getProjectMap();
  const config = map[project];
  if (!config) {
    log("error", "Unknown project", { project, registered: Object.keys(map) });
    throw new Error(`Project "${project}" is not registered`);
  }
  return config;
}

async function connectTo(dbName: string): Promise<Client> {
  const client = new Client({
    host: process.env.DB_HOST,
    port: parseInt(process.env.DB_PORT || "5432"),
    user: process.env.DB_USER,
    password: process.env.DB_PASSWORD,
    database: dbName,
    ssl: { rejectUnauthorized: false },
  });
  await client.connect();
  return client;
}

async function ensureDatabase(dbName: string): Promise<void> {
  const pg = await connectTo("postgres");
  try {
    const r = await pg.query("SELECT 1 FROM pg_database WHERE datname = $1", [dbName]);
    if (r.rowCount === 0) {
      log("info", "Creating database", { db: dbName });
      await pg.query(`CREATE DATABASE "${dbName}"`);
    }
  } finally {
    await pg.end();
  }
}

async function getOpsClient(): Promise<Client> {
  await ensureDatabase(OPS_DB);
  const client = await connectTo(OPS_DB);
  await client.query(OPS_SCHEMA);
  return client;
}

async function audit(
  ops: Client,
  project: string,
  operation: string,
  filename: string | null,
  checksum: string | null,
  status: string,
  errorMessage: string | null,
  durationMs: number | null,
  comment: string | null = null
) {
  try {
    await ops.query(
      `INSERT INTO migration_audit (project, operation, filename, checksum, status, comment, error_message, duration_ms)
       VALUES ($1, $2, $3, $4, $5, $6, $7, $8)`,
      [project, operation, filename, checksum, status, comment, errorMessage, durationMs]
    );
  } catch (err) {
    log("warn", "Audit write failed (non-fatal)", { error: (err as Error).message });
  }
}

async function acquireLock(client: Client, project: string): Promise<void> {
  const lockId = Math.abs(hashCode(project));
  // Blocking lock — waits for any concurrent migration to finish rather than failing
  await client.query("SELECT pg_advisory_lock($1)", [lockId]);
  log("debug", "Lock acquired", { project, lockId });
}

async function releaseLock(client: Client, project: string): Promise<void> {
  const lockId = Math.abs(hashCode(project));
  await client.query("SELECT pg_advisory_unlock($1)", [lockId]);
}

function hashCode(s: string): number {
  let h = 0;
  for (let i = 0; i < s.length; i++) {
    h = (Math.imul(31, h) + s.charCodeAt(i)) | 0;
  }
  return h;
}

async function listFiles(prefix: string): Promise<{ key: string; filename: string }[]> {
  const command = new ListObjectsV2Command({ Bucket: getBucket(), Prefix: prefix });
  const response = await s3.send(command);
  return (response.Contents || [])
    .filter((obj) => obj.Key && obj.Key.endsWith(".sql"))
    .map((obj) => ({ key: obj.Key!, filename: obj.Key!.split("/").pop()! }))
    .sort((a, b) => a.filename.localeCompare(b.filename));
}

async function readFile(key: string): Promise<string> {
  const response = await s3.send(new GetObjectCommand({ Bucket: getBucket(), Key: key }));
  return await response.Body!.transformToString();
}

function checksum(content: string): string {
  return createHash("sha256").update(content).digest("hex").slice(0, 16);
}

// =============================================================================
// Operations
// =============================================================================

async function migrate(project: string, config: ProjectConfig) {
  await ensureDatabase(config.db_name);
  const ops = await getOpsClient();
  const db = await connectTo(config.db_name);

  try {
    await acquireLock(ops, project);
    await db.query(LOCAL_TRACKING);

    const files = await listFiles(`migrations/${project}/`);
    log("info", "Migration files", { project, count: files.length, files: files.map((f) => f.filename) });

    const applied = await db.query("SELECT filename, checksum FROM schema_migrations");
    const appliedMap = new Map(applied.rows.map((r: { filename: string; checksum: string }) => [r.filename, r.checksum]));

    let count = 0;
    for (const file of files) {
      const sql = await readFile(file.key);
      const h = checksum(sql);

      if (appliedMap.has(file.filename)) {
        if (appliedMap.get(file.filename) !== h) {
          const msg = `Checksum mismatch for ${file.filename}`;
          await audit(ops, project, "migrate", file.filename, h, "error", msg, null);
          throw new Error(msg);
        }
        continue;
      }

      log("info", "Applying", { project, file: file.filename });
      const start = Date.now();

      await db.query("BEGIN");
      try {
        await db.query(sql);
        await db.query(
          "INSERT INTO schema_migrations (filename, checksum, duration_ms) VALUES ($1, $2, $3)",
          [file.filename, h, Date.now() - start]
        );
        await db.query("COMMIT");
        count++;
        const dur = Date.now() - start;
        log("info", "Applied", { project, file: file.filename, duration_ms: dur });
        await audit(ops, project, "migrate", file.filename, h, "success", null, dur);
      } catch (err) {
        await db.query("ROLLBACK");
        const msg = (err as Error).message;
        log("error", "Failed", { project, file: file.filename, error: msg });
        await audit(ops, project, "migrate", file.filename, h, "error", msg, Date.now() - start);
        throw err;
      }
    }

    log("info", "Migrate complete", { project, applied: count, total: files.length });
    return { operation: "migrate", project, applied: count };
  } finally {
    await releaseLock(ops, project);
    await db.end();
    await ops.end();
  }
}

async function rollback(project: string, config: ProjectConfig, target?: string) {
  const ops = await getOpsClient();
  const db = await connectTo(config.db_name);

  try {
    await acquireLock(ops, project);

    const applied = await db.query("SELECT filename FROM schema_migrations ORDER BY filename DESC");
    if (applied.rowCount === 0) {
      log("info", "Nothing to roll back", { project });
      return { operation: "rollback", project, rolled_back: 0 };
    }

    const rollbackFiles = await listFiles(`migrations/${project}/rollback/`);
    const rollbackMap = new Map(rollbackFiles.map((f) => [f.filename, f.key]));

    let count = 0;
    for (const row of applied.rows) {
      const filename = row.filename;
      if (target && filename <= target) break;

      const rollbackKey = rollbackMap.get(filename);
      if (!rollbackKey) {
        const msg = `No rollback file for ${filename}`;
        await audit(ops, project, "rollback", filename, null, "error", msg, null);
        throw new Error(msg);
      }

      const sql = await readFile(rollbackKey);
      log("info", "Rolling back", { project, file: filename });
      const start = Date.now();

      await db.query("BEGIN");
      try {
        await db.query(sql);
        await db.query("DELETE FROM schema_migrations WHERE filename = $1", [filename]);
        await db.query("COMMIT");
        count++;
        const dur = Date.now() - start;
        log("info", "Rolled back", { project, file: filename, duration_ms: dur });
        await audit(ops, project, "rollback", filename, null, "success", null, dur);
      } catch (err) {
        await db.query("ROLLBACK");
        const msg = (err as Error).message;
        log("error", "Rollback failed", { project, file: filename, error: msg });
        await audit(ops, project, "rollback", filename, null, "error", msg, Date.now() - start);
        throw err;
      }
    }

    log("info", "Rollback complete", { project, rolled_back: count });
    return { operation: "rollback", project, rolled_back: count };
  } finally {
    await releaseLock(ops, project);
    await db.end();
    await ops.end();
  }
}

async function seed(project: string, config: ProjectConfig) {
  const seedFiles = await listFiles(`migrations/${project}/seed/`);
  if (seedFiles.length === 0) {
    log("info", "No seed files", { project });
    return { operation: "seed", project, applied: 0 };
  }

  const ops = await getOpsClient();
  const db = await connectTo(config.db_name);

  try {
    await acquireLock(ops, project);

    let count = 0;
    for (const file of seedFiles) {
      const sql = await readFile(file.key);
      const h = checksum(sql);

      log("info", "Seeding", { project, file: file.filename });
      const start = Date.now();

      try {
        await db.query(sql);
        count++;
        const dur = Date.now() - start;
        log("info", "Seeded", { project, file: file.filename, duration_ms: dur });

        await ops.query(
          "INSERT INTO seed_runs (project, filename, checksum) VALUES ($1, $2, $3)",
          [project, file.filename, h]
        );
        await audit(ops, project, "seed", file.filename, h, "success", null, dur);
      } catch (err) {
        const msg = (err as Error).message;
        log("error", "Seed failed", { project, file: file.filename, error: msg });
        await audit(ops, project, "seed", file.filename, h, "error", msg, Date.now() - start);
        throw err;
      }
    }

    log("info", "Seed complete", { project, applied: count });
    return { operation: "seed", project, applied: count };
  } finally {
    await releaseLock(ops, project);
    await db.end();
    await ops.end();
  }
}

async function drop(project: string, config: ProjectConfig) {
  const ops = await getOpsClient();

  try {
    await acquireLock(ops, project);
    log("warn", "Dropping database", { project, db: config.db_name });

    const pg = await connectTo("postgres");
    try {
      await pg.query(
        `SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE datname = $1 AND pid <> pg_backend_pid()`,
        [config.db_name]
      );
      await pg.query(`DROP DATABASE IF EXISTS "${config.db_name}"`);
    } finally {
      await pg.end();
    }

    log("info", "Database dropped", { project, db: config.db_name });
    await audit(ops, project, "drop", null, null, "success", null, null);
    return { operation: "drop", project, db: config.db_name };
  } finally {
    await releaseLock(ops, project);
    await ops.end();
  }
}

async function noop(project: string, config: ProjectConfig, target: string, comment: string) {
  if (!target) throw new Error("noop requires target (migration filename)");
  if (!comment) throw new Error("noop requires comment explaining why the migration is being recorded without execution");

  await ensureDatabase(config.db_name);
  const ops = await getOpsClient();
  const db = await connectTo(config.db_name);

  try {
    await acquireLock(ops, project);
    await db.query(LOCAL_TRACKING);

    // Check if already applied
    const existing = await db.query("SELECT 1 FROM schema_migrations WHERE filename = $1", [target]);
    if (existing.rowCount && existing.rowCount > 0) {
      log("info", "Already recorded", { project, file: target });
      return { operation: "noop", project, file: target, status: "already_recorded" };
    }

    // Read the file to get its checksum (file must exist in S3)
    const files = await listFiles(`migrations/${project}/`);
    const file = files.find((f) => f.filename === target);
    if (!file) {
      throw new Error(`Migration file not found in S3: migrations/${project}/${target}`);
    }

    const sql = await readFile(file.key);
    const h = checksum(sql);

    log("info", "Recording noop migration", { project, file: target, comment });
    await db.query(
      "INSERT INTO schema_migrations (filename, checksum, noop, comment, duration_ms) VALUES ($1, $2, TRUE, $3, 0)",
      [target, h, comment]
    );

    await audit(ops, project, "noop", target, h, "success", null, 0, comment);
    log("info", "Noop recorded", { project, file: target, comment });
    return { operation: "noop", project, file: target, comment };
  } finally {
    await releaseLock(ops, project);
    await db.end();
    await ops.end();
  }
}

async function restore(project: string, config: ProjectConfig, target: string, comment: string) {
  if (!target) throw new Error("restore requires target (S3 key of the SQL dump)");
  if (!comment) throw new Error("restore requires comment explaining the restore context");

  await ensureDatabase(config.db_name);
  const ops = await getOpsClient();
  let db = await connectTo(config.db_name);

  try {
    await acquireLock(ops, project);

    // Drop and recreate the database for a clean restore
    log("info", "Dropping existing database for clean restore", { project, db: config.db_name });
    await db.end();
    const pg = await connectTo("postgres");
    try {
      await pg.query(
        `SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE datname = $1 AND pid <> pg_backend_pid()`,
        [config.db_name]
      );
      await pg.query(`DROP DATABASE IF EXISTS "${config.db_name}"`);
      await pg.query(`CREATE DATABASE "${config.db_name}"`);
    } finally {
      await pg.end();
    }
    db = await connectTo(config.db_name);

    // Restore the dump (strip psql metacommands that pg client can't execute)
    log("info", "Restoring from S3", { project, key: target });
    const raw = await readFile(target);
    const sql = raw
      .split("\n")
      .filter((line) => !line.startsWith("\\"))
      .map((line) =>
        line.includes("set_config('search_path'")
          ? "SELECT pg_catalog.set_config('search_path', 'public', false);"
          : line
      )
      .join("\n");
    const start = Date.now();
    await db.query(sql);
    const dur = Date.now() - start;
    log("info", "SQL restored", { project, key: target, duration_ms: dur });

    // Restore search_path (pg_dump clears it) then create tracking table
    await db.query("SET search_path TO public");
    await db.query(LOCAL_TRACKING);

    // Record all existing migration files as noops so future db-migrate
    // doesn't try to re-apply them against the restored schema
    const migrationFiles = await listFiles(`migrations/${project}/`);
    let baselined = 0;
    for (const file of migrationFiles) {
      const existing = await db.query("SELECT 1 FROM schema_migrations WHERE filename = $1", [file.filename]);
      if (existing.rowCount && existing.rowCount > 0) continue;

      const content = await readFile(file.key);
      const h = checksum(content);
      await db.query(
        "INSERT INTO schema_migrations (filename, checksum, noop, comment, duration_ms) VALUES ($1, $2, TRUE, $3, 0)",
        [file.filename, h, comment]
      );
      baselined++;
      log("info", "Baselined migration", { project, file: file.filename });
    }

    await audit(ops, project, "restore", target, null, "success", null, dur, comment);
    log("info", "Restore complete", { project, key: target, duration_ms: dur, baselined });
    return { operation: "restore", project, key: target, duration_ms: dur, baselined };
  } catch (err) {
    const msg = (err as Error).message;
    log("error", "Restore failed", { project, key: target, error: msg });
    await audit(ops, project, "restore", target, null, "error", msg, null, comment);
    throw err;
  } finally {
    await releaseLock(ops, project);
    await db.end();
    await ops.end();
  }
}

// =============================================================================
// Entry point
// =============================================================================

export async function handler(event: MigrationEvent) {
  log("info", "Event received", { event: JSON.stringify(event) });

  if ("detail" in event) {
    const key = event.detail.object.key;
    const parts = key.split("/");
    if (parts.length < 3 || parts[0] !== "migrations") {
      log("warn", "Ignoring non-migration upload", { key });
      return { statusCode: 200, body: "ignored" };
    }

    const project = parts[1];
    if (parts[2] === "rollback" || parts[2] === "seed") {
      log("info", "Ignoring rollback/seed upload", { key, project });
      return { statusCode: 200, body: "ignored" };
    }

    const config = requireProject(project);
    return await migrate(project, config);
  }

  const { operation, project, target, comment } = event as ManualEvent;
  const config = requireProject(project);

  switch (operation) {
    case "migrate":
      return await migrate(project, config);
    case "rollback":
      return await rollback(project, config, target);
    case "seed":
      return await seed(project, config);
    case "drop":
      return await drop(project, config);
    case "noop":
      return await noop(project, config, target!, comment!);
    case "restore":
      return await restore(project, config, target!, comment!);
    default:
      throw new Error(`Unknown operation: ${operation}`);
  }
}
