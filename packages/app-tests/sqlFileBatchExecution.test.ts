import { test } from "vitest";
import assert from "node:assert/strict";
import { collectSqlPaths, aggregateFileProgress, computeBatchTerminalStatus, shouldContinueBatch, runWithConcurrency, isTerminalFileStatus } from "../../apps/desktop/src/lib/sql/sqlFileBatch.ts";
import type { SqlFileEntry, SqlFileProgress } from "../../apps/desktop/src/lib/backend/api.ts";

function entry(name: string, path: string, isDir = false, children: SqlFileEntry[] = []): SqlFileEntry {
  return { name, path, is_dir: isDir, children };
}

function progress(p: Partial<SqlFileProgress>): SqlFileProgress {
  return {
    executionId: p.executionId ?? "id",
    status: p.status ?? "running",
    statementIndex: p.statementIndex ?? 0,
    successCount: p.successCount ?? 0,
    failureCount: p.failureCount ?? 0,
    affectedRows: p.affectedRows ?? 0,
    elapsedMs: p.elapsedMs ?? 0,
    statementSummary: p.statementSummary ?? "",
    error: p.error ?? null,
  };
}

// ---------------------------------------------------------------------------
// collectSqlPaths — recursive folder walking
// ---------------------------------------------------------------------------

test("collectSqlPaths flattens a flat file list", () => {
  const entries = [entry("a.sql", "/f/a.sql"), entry("b.sql", "/f/b.sql")];
  assert.deepEqual(collectSqlPaths(entries), ["/f/a.sql", "/f/b.sql"]);
});

test("collectSqlPaths recurses into nested directories depth-first", () => {
  const entries = [entry("d1", "/f/d1", true, [entry("c.sql", "/f/d1/c.sql"), entry("d2", "/f/d1/d2", true, [entry("e.sql", "/f/d1/d2/e.sql")])]), entry("f.sql", "/f/f.sql")];
  assert.deepEqual(collectSqlPaths(entries), ["/f/d1/c.sql", "/f/d1/d2/e.sql", "/f/f.sql"]);
});

test("collectSqlPaths returns empty for an empty folder", () => {
  assert.deepEqual(collectSqlPaths([]), []);
  assert.deepEqual(collectSqlPaths([entry("empty", "/f/empty", true, [])]), []);
});

test("collectSqlPaths ignores directory entries but keeps their file children", () => {
  const entries = [entry("d", "/f/d", true, [entry("x.sql", "/f/d/x.sql")])];
  assert.deepEqual(collectSqlPaths(entries), ["/f/d/x.sql"]);
});

test("collectSqlPaths guards against cyclic directory paths", () => {
  // Synthetic cycle: a directory whose child references the same path. The
  // backend never produces this, but the guard must not infinite-loop.
  const cyclic: SqlFileEntry = entry("d", "/f/d", true, []);
  cyclic.children.push(cyclic);
  assert.deepEqual(collectSqlPaths([cyclic]), []);
});

// ---------------------------------------------------------------------------
// aggregateFileProgress — per-file progress aggregation
// ---------------------------------------------------------------------------

test("aggregateFileProgress sums counts and takes max for index/elapsed", () => {
  const result = aggregateFileProgress([progress({ successCount: 3, failureCount: 1, affectedRows: 10, statementIndex: 4, elapsedMs: 100 }), progress({ successCount: 2, failureCount: 0, affectedRows: 5, statementIndex: 2, elapsedMs: 250 })]);
  assert.deepEqual(result, { successCount: 5, failureCount: 1, affectedRows: 15, statementIndex: 4, elapsedMs: 250 });
});

test("aggregateFileProgress skips null/undefined entries", () => {
  const result = aggregateFileProgress([null, undefined, progress({ successCount: 7 })]);
  assert.equal(result.successCount, 7);
  assert.equal(result.failureCount, 0);
});

test("aggregateFileProgress returns zeros for an empty list", () => {
  assert.deepEqual(aggregateFileProgress([]), { successCount: 0, failureCount: 0, affectedRows: 0, statementIndex: 0, elapsedMs: 0 });
});

// ---------------------------------------------------------------------------
// computeBatchTerminalStatus — batch-level outcome resolution
// ---------------------------------------------------------------------------

test("computeBatchTerminalStatus returns done when all files are done", () => {
  assert.equal(computeBatchTerminalStatus(["done", "done"], false, "running"), "done");
});

test("computeBatchTerminalStatus returns error when any file errored", () => {
  assert.equal(computeBatchTerminalStatus(["done", "error"], false, "running"), "error");
});

test("computeBatchTerminalStatus returns cancelled when cancel requested and a file is in-flight", () => {
  assert.equal(computeBatchTerminalStatus(["done", "running"], true, "running"), "cancelled");
});

test("computeBatchTerminalStatus returns cancelled when cancel requested and a file is already cancelled", () => {
  assert.equal(computeBatchTerminalStatus(["done", "cancelled"], true, "running"), "cancelled");
});

test("computeBatchTerminalStatus keeps previous status when batch is still in flight and not cancelled", () => {
  assert.equal(computeBatchTerminalStatus(["done", "running"], false, "running"), "running");
});

test("computeBatchTerminalStatus returns previous for an empty list", () => {
  assert.equal(computeBatchTerminalStatus([], false, "running"), "running");
  assert.equal(computeBatchTerminalStatus([], true, "idle"), "idle");
});

test("computeBatchTerminalStatus does not treat all-done as cancelled even if cancel was requested late", () => {
  // All files already finished successfully before the cancel signal could
  // affect anything — the batch is done, not cancelled.
  assert.equal(computeBatchTerminalStatus(["done", "done"], true, "running"), "done");
});

// ---------------------------------------------------------------------------
// shouldContinueBatch — sequential continue/abort decision
// ---------------------------------------------------------------------------

test("shouldContinueBatch continues after done", () => {
  assert.equal(shouldContinueBatch("done", false), true);
  assert.equal(shouldContinueBatch("done", true), true);
});

test("shouldContinueBatch stops after error when continueOnError is false", () => {
  assert.equal(shouldContinueBatch("error", false), false);
});

test("shouldContinueBatch continues after error when continueOnError is true", () => {
  assert.equal(shouldContinueBatch("error", true), true);
});

test("shouldContinueBatch stops after cancelled regardless of continueOnError", () => {
  assert.equal(shouldContinueBatch("cancelled", false), false);
  assert.equal(shouldContinueBatch("cancelled", true), false);
});

// ---------------------------------------------------------------------------
// isTerminalFileStatus
// ---------------------------------------------------------------------------

test("isTerminalFileStatus recognises done/error/cancelled", () => {
  assert.equal(isTerminalFileStatus("done"), true);
  assert.equal(isTerminalFileStatus("error"), true);
  assert.equal(isTerminalFileStatus("cancelled"), true);
});

test("isTerminalFileStatus rejects in-flight states", () => {
  assert.equal(isTerminalFileStatus("idle"), false);
  assert.equal(isTerminalFileStatus("pending"), false);
  assert.equal(isTerminalFileStatus("running"), false);
  assert.equal(isTerminalFileStatus("started"), false);
  assert.equal(isTerminalFileStatus("statementDone"), false);
});

// ---------------------------------------------------------------------------
// runWithConcurrency — bounded worker pool
// ---------------------------------------------------------------------------

test("runWithConcurrency runs all items and returns results in input order", async () => {
  const items = [1, 2, 3, 4, 5];
  const results = await runWithConcurrency(items, 2, async (n) => n * 10);
  assert.deepEqual(
    results.map((r) => (r.status === "fulfilled" ? r.value : "rejected")),
    [10, 20, 30, 40, 50],
  );
});

test("runWithConcurrency respects the concurrency cap", async () => {
  let active = 0;
  let maxActive = 0;
  const items = Array.from({ length: 10 }, (_, i) => i);
  await runWithConcurrency(items, 3, async () => {
    active += 1;
    maxActive = Math.max(maxActive, active);
    await new Promise((r) => setTimeout(r, 10));
    active -= 1;
  });
  assert.ok(maxActive <= 3, `expected maxActive <= 3, got ${maxActive}`);
  assert.ok(maxActive >= 2, `expected some parallelism (maxActive >= 2), got ${maxActive}`);
});

test("runWithConcurrency clamps concurrency to item count", async () => {
  const items = [1, 2];
  let active = 0;
  let maxActive = 0;
  await runWithConcurrency(
    items,
    10, // larger than items.length
    async () => {
      active += 1;
      maxActive = Math.max(maxActive, active);
      await new Promise((r) => setTimeout(r, 10));
      active -= 1;
    },
  );
  assert.ok(maxActive <= 2, `expected maxActive <= 2, got ${maxActive}`);
});

test("runWithConcurrency isolates failures: one rejection does not abort others", async () => {
  const items = [1, 2, 3];
  const results = await runWithConcurrency(items, 2, async (n) => {
    if (n === 2) throw new Error("boom");
    return n;
  });
  assert.equal(results[0].status, "fulfilled");
  assert.equal((results[0] as PromiseFulfilledResult<number>).value, 1);
  assert.equal(results[1].status, "rejected");
  assert.equal((results[1] as PromiseRejectedResult).reason.message, "boom");
  assert.equal(results[2].status, "fulfilled");
  assert.equal((results[2] as PromiseFulfilledResult<number>).value, 3);
});

test("runWithConcurrency short-circuits via shouldStop before dequeueing remaining items", async () => {
  let runs = 0;
  const items = Array.from({ length: 10 }, (_, i) => i);
  let stop = false;
  await runWithConcurrency(
    items,
    1, // concurrency 1 so order is deterministic
    async (n) => {
      runs += 1;
      if (n === 2) stop = true;
      await new Promise((r) => setTimeout(r, 5));
    },
    () => stop,
  );
  // After item index 2 sets stop=true, the worker should not dequeue index 3+.
  // Items 0,1,2 always run; with concurrency 1 and a synchronous stop check
  // before each dequeue, no further items start.
  assert.ok(runs <= 3, `expected runs <= 3, got ${runs}`);
});

test("runWithConcurrency handles an empty item list", async () => {
  const results = await runWithConcurrency([], 4, async (n) => n);
  assert.deepEqual(results, []);
});
