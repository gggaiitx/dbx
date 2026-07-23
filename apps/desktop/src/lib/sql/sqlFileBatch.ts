import type { SqlFileEntry, SqlFileProgress, SqlFileStatus } from "@/lib/backend/api";

// Batch execution shared between the SqlFileExecutionDialog and unit tests.
// These helpers are intentionally pure (no Vue refs, no backend calls) so the
// orchestration logic — recursive folder walking, progress aggregation, batch
// terminal-status resolution, concurrency scheduling — can be exercised in
// isolation.

/**
 * Terminal status values a single file can reach during a batch. Non-terminal
 * states ("idle"/"pending"/"started"/"running"/"statementDone"/"statementFailed")
 * mean the file is still in flight or has not started yet.
 */
export type BatchFileStatus = SqlFileStatus | "idle" | "pending";

export interface BatchProgressAggregate {
  successCount: number;
  failureCount: number;
  affectedRows: number;
  statementIndex: number;
  elapsedMs: number;
}

export function isTerminalFileStatus(status: BatchFileStatus): boolean {
  return status === "done" || status === "error" || status === "cancelled";
}

/**
 * Walk a folder entry tree (as returned by `listSqlFilesInFolder`) and collect
 * every file path in depth-first order. Directories are recursed into; file
 * entries contribute their `path`. The function is defensive against cycles
 * (not expected from the backend, but cheap to guard) by tracking visited
 * paths.
 */
export function collectSqlPaths(entries: SqlFileEntry[], visited: Set<string> = new Set()): string[] {
  const result: string[] = [];
  for (const entry of entries) {
    if (!entry) continue;
    if (entry.is_dir) {
      if (visited.has(entry.path)) continue;
      visited.add(entry.path);
      result.push(...collectSqlPaths(entry.children, visited));
    } else {
      result.push(entry.path);
    }
  }
  return result;
}

/**
 * Sum per-file progress into batch totals. `statementIndex` and `elapsedMs`
 * are not additive across files (each file restarts from 0), so they take the
 * max across files — giving a "furthest single file" indicator rather than a
 * misleading sum.
 */
export function aggregateFileProgress(progresses: Array<SqlFileProgress | null | undefined>): BatchProgressAggregate {
  let successCount = 0;
  let failureCount = 0;
  let affectedRows = 0;
  let statementIndex = 0;
  let elapsedMs = 0;
  for (const p of progresses) {
    if (!p) continue;
    successCount += p.successCount;
    failureCount += p.failureCount;
    affectedRows += p.affectedRows;
    if (p.statementIndex > statementIndex) statementIndex = p.statementIndex;
    if (p.elapsedMs > elapsedMs) elapsedMs = p.elapsedMs;
  }
  return { successCount, failureCount, affectedRows, statementIndex, elapsedMs };
}

/**
 * Resolve the batch-level terminal status from the per-file outcomes.
 *
 * - If cancellation was requested and any file is still in-flight or already
 *   cancelled, the whole batch is "cancelled".
 * - Else if any file failed, the batch is "error" (partial success still
 *   surfaces as error at the batch level so the user notices).
 * - Else if every file is "done", the batch is "done".
 * - Otherwise the batch has no terminal status yet and the previous status is
 *   returned unchanged.
 */
export function computeBatchTerminalStatus(fileStatuses: BatchFileStatus[], cancelRequested: boolean, previous: BatchFileStatus): BatchFileStatus {
  if (!fileStatuses.length) return previous;
  if (cancelRequested && fileStatuses.some((s) => s === "cancelled" || !isTerminalFileStatus(s))) {
    return "cancelled";
  }
  if (fileStatuses.some((s) => s === "error")) {
    return "error";
  }
  if (fileStatuses.every((s) => s === "done")) {
    return "done";
  }
  return previous;
}

/**
 * In sequential mode, decide whether to proceed to the next file after a
 * terminal status. A cancelled file always stops the batch; an errored file
 * stops the batch unless `continueOnError` is set; any other terminal status
 * (done) lets the batch continue.
 */
export function shouldContinueBatch(lastStatus: BatchFileStatus, continueOnError: boolean): boolean {
  if (lastStatus === "done") return true;
  if (lastStatus === "error") return continueOnError;
  // cancelled (or any other terminal/non-terminal) — stop
  return false;
}

/**
 * Run an async task producer against a bounded worker pool. Items are pulled
 * from `items` in order and dispatched to up to `concurrency` workers. The
 * `worker` receives the item and may return a value; results are collected in
 * completion order (mirroring `Promise.allSettled` semantics for errors — a
 * thrown error in one worker does NOT abort the others; callers decide how to
 * aggregate).
 *
 * The `shouldStop` callback is polled before dequeueing each item so the pool
 * can be short-circuited (e.g. on cancel). Already-running tasks are allowed
 * to finish.
 *
 * Returns the per-item results in the ORIGINAL input order (not completion
 * order), so callers can correlate outcomes to source items.
 */
export async function runWithConcurrency<T, R>(items: T[], concurrency: number, worker: (item: T, index: number) => Promise<R>, shouldStop?: () => boolean): Promise<PromiseSettledResult<R>[]> {
  const results: PromiseSettledResult<R>[] = new Array(items.length);
  let cursor = 0;

  async function runWorker() {
    while (true) {
      if (shouldStop?.()) return;
      const index = cursor;
      cursor += 1;
      if (index >= items.length) return;
      try {
        const value = await worker(items[index], index);
        results[index] = { status: "fulfilled", value };
      } catch (reason: any) {
        results[index] = { status: "rejected", reason };
      }
    }
  }

  const pool = Math.max(1, Math.min(concurrency, items.length));
  const workers = Array.from({ length: pool }, () => runWorker());
  await Promise.all(workers);
  return results;
}
