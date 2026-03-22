import { Client } from "pg";
import { S3Client, ListObjectsV2Command, GetObjectCommand } from "@aws-sdk/client-s3";

const s3 = new S3Client({});

interface ProjectConfig {
  db_name: string;
}

// EventBridge S3 event
interface S3Event {
  detail: {
    bucket: { name: string };
    object: { key: string };
  };
}

// Direct invocation for manual operations
interface ManualEvent {
  operation: "migrate" | "rollback" | "seed" | "drop";
  project: string;
  target?: string; // for rollback: filename to roll back to
}

type MigrationEvent = S3Event | ManualEvent;

const TRACKING_TABLE = `
CREATE TABLE IF NOT EXISTS schema_migrations (
  id SERIAL PRIMARY KEY,
  filename TEXT NOT NULL UNIQUE,
  checksum TEXT NOT NULL,
  applied_at TIMESTAMPTZ DEFAULT NOW(),
  duration_ms INTEGER
);
`;

function log(level: string, msg: string, data?: Record<string, unknown>) {
  console.log(JSON.stringify({ level, msg, ts: new Date().toISOString(), ...data }));
}

function getProjectMap(): Record<string, ProjectConfig> {
  const raw = process.env.PROJECT_MAP;
  if (!raw) throw new Error("PROJECT_MAP env var not set");
  return JSON.parse(raw);
}

function getBucket(): string {
  return process.env.MIGRATIONS_BUCKET!;
}

function requireProject(project: string): ProjectConfig {
  const map = getProjectMap();
  const config = map[project];
  if (!config) {
    log("error", "Unknown project", { project, registered: Object.keys(map) });
    throw new Error(`Project "${project}" is not registered for migrations`);
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
  const client = await connectTo("postgres");
  try {
    const result = await client.query("SELECT 1 FROM pg_database WHERE datname = $1", [dbName]);
    if (result.rowCount === 0) {
      log("info", "Creating database", { db: dbName });
      await client.query(`CREATE DATABASE "${dbName}"`);
    }
  } finally {
    await client.end();
  }
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
  const command = new GetObjectCommand({ Bucket: getBucket(), Key: key });
  const response = await s3.send(command);
  return await response.Body!.transformToString();
}

function hash(content: string): string {
  const { createHash } = require("crypto");
  return createHash("sha256").update(content).digest("hex").slice(0, 16);
}

// --- Operations ---

async function migrate(project: string, config: ProjectConfig) {
  await ensureDatabase(config.db_name);
  const client = await connectTo(config.db_name);

  try {
    await client.query(TRACKING_TABLE);

    const files = await listFiles(`migrations/${project}/`);
    log("info", "Migration files", { project, count: files.length, files: files.map((f) => f.filename) });

    const applied = await client.query("SELECT filename, checksum FROM schema_migrations");
    const appliedMap = new Map(applied.rows.map((r: { filename: string; checksum: string }) => [r.filename, r.checksum]));

    let count = 0;
    for (const file of files) {
      const sql = await readFile(file.key);
      const h = hash(sql);

      if (appliedMap.has(file.filename)) {
        if (appliedMap.get(file.filename) !== h) {
          log("error", "Checksum mismatch", { project, file: file.filename, expected: appliedMap.get(file.filename), actual: h });
          throw new Error(`Checksum mismatch for ${file.filename} — migration was modified after being applied`);
        }
        continue;
      }

      log("info", "Applying migration", { project, file: file.filename });
      const start = Date.now();

      await client.query("BEGIN");
      try {
        await client.query(sql);
        await client.query(
          "INSERT INTO schema_migrations (filename, checksum, duration_ms) VALUES ($1, $2, $3)",
          [file.filename, h, Date.now() - start]
        );
        await client.query("COMMIT");
        count++;
        log("info", "Applied", { project, file: file.filename, duration_ms: Date.now() - start });
      } catch (err) {
        await client.query("ROLLBACK");
        log("error", "Failed", { project, file: file.filename, error: (err as Error).message });
        throw err;
      }
    }

    log("info", "Migrate complete", { project, applied: count, total: files.length });
    return { operation: "migrate", project, applied: count };
  } finally {
    await client.end();
  }
}

async function rollback(project: string, config: ProjectConfig, target?: string) {
  const client = await connectTo(config.db_name);

  try {
    const applied = await client.query(
      "SELECT filename FROM schema_migrations ORDER BY filename DESC"
    );

    if (applied.rowCount === 0) {
      log("info", "Nothing to roll back", { project });
      return { operation: "rollback", project, rolled_back: 0 };
    }

    // Find matching rollback files: migrations/<project>/rollback/001_initial.sql
    const rollbackFiles = await listFiles(`migrations/${project}/rollback/`);
    const rollbackMap = new Map(rollbackFiles.map((f) => [f.filename, f.key]));

    let count = 0;
    for (const row of applied.rows) {
      const filename = row.filename;

      // Stop if we've reached the target
      if (target && filename <= target) break;

      const rollbackKey = rollbackMap.get(filename);
      if (!rollbackKey) {
        log("error", "No rollback file", { project, file: filename });
        throw new Error(`No rollback file for ${filename} at migrations/${project}/rollback/${filename}`);
      }

      const sql = await readFile(rollbackKey);
      log("info", "Rolling back", { project, file: filename });

      await client.query("BEGIN");
      try {
        await client.query(sql);
        await client.query("DELETE FROM schema_migrations WHERE filename = $1", [filename]);
        await client.query("COMMIT");
        count++;
        log("info", "Rolled back", { project, file: filename });
      } catch (err) {
        await client.query("ROLLBACK");
        log("error", "Rollback failed", { project, file: filename, error: (err as Error).message });
        throw err;
      }
    }

    log("info", "Rollback complete", { project, rolled_back: count });
    return { operation: "rollback", project, rolled_back: count };
  } finally {
    await client.end();
  }
}

async function seed(project: string, config: ProjectConfig) {
  const seedFiles = await listFiles(`migrations/${project}/seed/`);
  if (seedFiles.length === 0) {
    log("info", "No seed files", { project });
    return { operation: "seed", project, applied: 0 };
  }

  const client = await connectTo(config.db_name);
  try {
    let count = 0;
    for (const file of seedFiles) {
      const sql = await readFile(file.key);
      log("info", "Seeding", { project, file: file.filename });
      const start = Date.now();
      await client.query(sql);
      count++;
      log("info", "Seeded", { project, file: file.filename, duration_ms: Date.now() - start });
    }

    log("info", "Seed complete", { project, applied: count });
    return { operation: "seed", project, applied: count };
  } finally {
    await client.end();
  }
}

async function drop(project: string, config: ProjectConfig) {
  log("warn", "Dropping database", { project, db: config.db_name });
  const client = await connectTo("postgres");
  try {
    // Terminate active connections
    await client.query(
      `SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE datname = $1 AND pid <> pg_backend_pid()`,
      [config.db_name]
    );
    await client.query(`DROP DATABASE IF EXISTS "${config.db_name}"`);
    log("info", "Database dropped", { project, db: config.db_name });
    return { operation: "drop", project, db: config.db_name };
  } finally {
    await client.end();
  }
}

// --- Entry point ---

export async function handler(event: MigrationEvent) {
  log("info", "Event received", { event: JSON.stringify(event) });

  // EventBridge S3 trigger → always runs migrate
  if ("detail" in event) {
    const key = event.detail.object.key;
    const parts = key.split("/");
    if (parts.length < 3 || parts[0] !== "migrations") {
      log("warn", "Ignoring non-migration upload", { key });
      return { statusCode: 200, body: "ignored" };
    }

    // Only trigger on migration files, not rollback/seed uploads
    const project = parts[1];
    if (parts[2] === "rollback" || parts[2] === "seed") {
      log("info", "Ignoring rollback/seed upload", { key, project });
      return { statusCode: 200, body: "ignored" };
    }

    const config = requireProject(project);
    const result = await migrate(project, config);
    return { statusCode: 200, body: JSON.stringify(result) };
  }

  // Direct invocation
  const { operation, project, target } = event as ManualEvent;
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
    default:
      throw new Error(`Unknown operation: ${operation}`);
  }
}
