<script setup lang="ts">
import { computed, ref, watch, onUnmounted } from "vue";
import { uuid } from "@/lib/common/utils";
import { useI18n } from "vue-i18n";
import { useSqlHighlighter } from "@/composables/useSqlHighlighter";
import { isTauriRuntime } from "@/lib/backend/tauriRuntime";
import { Dialog, DialogFooter, DialogHeader, DialogScrollContent, DialogTitle } from "@/components/ui/dialog";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select";
import DatabaseIcon from "@/components/icons/DatabaseIcon.vue";
import ConnectionGroupBadge from "@/components/connection/ConnectionGroupBadge.vue";
import { useToast } from "@/composables/useToast";
import { useConnectionStore } from "@/stores/connectionStore";
import { useProductionSafetyStore } from "@/stores/productionSafetyStore";
import { productionContextForDatabase } from "@/lib/database/productionSafety";
import { fetchSqlFileTargetOptions } from "@/composables/useDatabaseOptions";
import { requiresSqlFileTargetDatabaseSelection } from "@/lib/connection/connectionLevelDatabaseBootstrap";
import { cancelSqlFileExecution, executeSqlFile, listenSqlFileProgress, previewSqlFile, releaseSqlFileUpload, type SqlFilePreview, type SqlFileProgress, type SqlFileStatus } from "@/lib/backend/api";
import * as api from "@/lib/backend/api";
import { useExportTracker } from "@/composables/useExportTracker";
import { setFileDropIntercepted } from "@/composables/useFileDrop";
import { collectSqlPaths, aggregateFileProgress, computeBatchTerminalStatus as computeBatchTerminalStatusPure, shouldContinueBatch, runWithConcurrency, isTerminalFileStatus, type BatchFileStatus } from "@/lib/sql/sqlFileBatch";
import { Check, CheckSquare, FileCode, FolderOpen, FolderSearch, Loader2, Play, Square, X, Trash2, ChevronDown, ChevronRight } from "@lucide/vue";

const { t } = useI18n();
const { toast } = useToast();
const { highlight } = useSqlHighlighter();
const { addSqlFileTask, updateSqlFileTask } = useExportTracker();
const open = defineModel<boolean>("open", { default: false });

const props = defineProps<{
  prefillConnectionId?: string;
  prefillDatabase?: string;
  prefillFilePath?: string;
}>();

const store = useConnectionStore();
const productionSafetyStore = useProductionSafetyStore();

// A single file in the batch list. `status`/`progress`/`error` are populated
// during execution; `executionId` is assigned right before the backend call.
interface BatchFileItem {
  id: string;
  filePath: string;
  preview: SqlFilePreview;
  executionId: string;
  status: BatchFileStatus;
  progress: SqlFileProgress | null;
  error: string;
  expanded: boolean;
}

const fileInput = ref<HTMLInputElement | null>(null);
const files = ref<BatchFileItem[]>([]);
const selectingFile = ref(false);
const connectionId = ref("");
const database = ref("");
const databaseOptions = ref<string[]>([]);
const loadingDatabases = ref(false);
const continueOnError = ref(false);
const executionMode = ref<"sequential" | "parallel">("sequential");
// Web mode loads each uploaded file fully into memory via std::fs::read, so
// parallel execution of several large files can exhaust memory. Parallel mode
// is disabled in web mode until streaming execution is available.
const isWebMode = computed(() => !isTauriRuntime());

const running = ref(false);
const cancelling = ref(false);
const cancelRequested = ref(false);
const executionStarted = ref(false);
const terminalStatus = ref<BatchFileStatus>("idle");
const terminalError = ref("");
const refreshedTarget = ref(false);

const sqlConnections = computed(() => store.connections.filter((c) => !["redis", "mongodb", "elasticsearch", "qdrant", "milvus", "weaviate", "chromadb", "etcd", "zookeeper", "mq", "nacos"].includes(c.db_type)));

const selectedConnection = computed(() => sqlConnections.value.find((c) => c.id === connectionId.value));

// The batch can start when there is at least one file with a preview, a
// connection is selected, the database requirement is satisfied, and no
// execution is in progress. Every file shares the same target, so the database
// gating uses the first file's `canExecuteWithoutSelectedDatabase` flag — if
// any file needs a selected database, the target must be filled in.
const canStart = computed(() => {
  const connection = selectedConnection.value;
  const readyFiles = files.value.filter((f) => f.preview);
  if (!readyFiles.length || !connection || running.value || selectingFile.value || loadingDatabases.value) return false;
  const needsDatabase = readyFiles.some((f) => requiresSqlFileTargetDatabaseSelection(connection, f.preview.canExecuteWithoutSelectedDatabase));
  return !!database.value.trim() || !needsDatabase;
});

const statusTone = computed(() => {
  if (terminalStatus.value === "done") return "text-green-600";
  if (terminalStatus.value === "error") return "text-destructive";
  if (terminalStatus.value === "cancelled") return "text-yellow-600";
  if (running.value) return "text-primary";
  return "text-muted-foreground";
});

const statusIcon = computed(() => {
  if (running.value) return Loader2;
  if (terminalStatus.value === "done") return Check;
  if (terminalStatus.value === "error" || terminalStatus.value === "cancelled") return X;
  return FileCode;
});

const aggregateProgress = computed(() => aggregateFileProgress(files.value.map((f) => f.progress)));

const progressPercent = computed(() => {
  if (terminalStatus.value === "done") return 100;
  const done = files.value.filter((f) => isTerminalFileStatus(f.status)).length;
  if (!files.value.length) return 0;
  if (done === files.value.length) return 100;
  if (done === 0) return running.value ? 8 : 0;
  return Math.min(95, Math.round((done / files.value.length) * 100));
});

function connectionIconType(id: string) {
  const config = store.getConfig(id);
  return config?.driver_profile || config?.db_type || "mysql";
}

function formatBytes(bytes: number) {
  if (bytes < 1024) return `${bytes} B`;
  const units = ["KB", "MB", "GB"];
  let value = bytes / 1024;
  let unit = units[0];
  for (let i = 1; i < units.length && value >= 1024; i += 1) {
    value /= 1024;
    unit = units[i];
  }
  return `${value >= 10 ? value.toFixed(1) : value.toFixed(2)} ${unit}`;
}

function formatElapsed(ms: number) {
  if (ms < 1000) return `${ms} ms`;
  const seconds = ms / 1000;
  if (seconds < 60) return `${seconds.toFixed(1)} s`;
  const minutes = Math.floor(seconds / 60);
  return `${minutes}m ${Math.round(seconds % 60)}s`;
}

function statusLabel(status: BatchFileStatus) {
  return t(`sqlFile.status.${status}`);
}

function fileStatusTone(status: BatchFileStatus) {
  if (status === "done") return "text-green-600";
  if (status === "error") return "text-destructive";
  if (status === "cancelled") return "text-yellow-600";
  if (status === "running" || status === "started" || status === "statementDone") return "text-primary";
  return "text-muted-foreground";
}

function resolveInitialConnectionId() {
  if (props.prefillConnectionId && sqlConnections.value.some((c) => c.id === props.prefillConnectionId)) {
    return props.prefillConnectionId;
  }
  return sqlConnections.value[0]?.id ?? "";
}

function chooseDatabase(names: string[], id: string) {
  const configDatabase = store.getConfig(id)?.database ?? "";
  if (names.length > 0) {
    if (props.prefillDatabase && names.includes(props.prefillDatabase)) return props.prefillDatabase;
    if (configDatabase && names.includes(configDatabase)) return configDatabase;
    return names.length === 1 ? names[0] : "";
  }
  return props.prefillDatabase ?? configDatabase;
}

function resetExecution() {
  running.value = false;
  cancelling.value = false;
  cancelRequested.value = false;
  executionStarted.value = false;
  terminalStatus.value = "idle";
  terminalError.value = "";
  refreshedTarget.value = false;
}

function resetState() {
  files.value = [];
  selectingFile.value = false;
  connectionId.value = resolveInitialConnectionId();
  database.value = "";
  databaseOptions.value = [];
  loadingDatabases.value = false;
  continueOnError.value = false;
  executionMode.value = "sequential";
  resetExecution();
}

let databaseLoadToken = 0;

async function loadDatabasesForConnection(id: string) {
  const token = databaseLoadToken + 1;
  databaseLoadToken = token;
  databaseOptions.value = [];

  if (!sqlConnections.value.some((c) => c.id === id)) {
    database.value = "";
    return;
  }

  loadingDatabases.value = true;
  try {
    await store.ensureConnected(id);
    const connection = store.getConfig(id);
    if (!connection) return;
    const names = await fetchSqlFileTargetOptions(id, connection);
    if (token !== databaseLoadToken) return;
    databaseOptions.value = names;
    database.value = chooseDatabase(names, id);
  } catch {
    if (token !== databaseLoadToken) return;
    databaseOptions.value = [];
    database.value = chooseDatabase([], id);
  } finally {
    if (token === databaseLoadToken) {
      loadingDatabases.value = false;
    }
  }
}

async function previewSelectedSqlFile(fileOrPath: string | File) {
  if (isTauriRuntime()) {
    return previewSqlFile(fileOrPath as string);
  }
  const { previewSqlFile: previewWebSqlFile } = await import("@/lib/backend/http");
  return previewWebSqlFile(fileOrPath as File);
}

function hasFilePath(path: string) {
  return files.value.some((f) => f.filePath === path);
}

async function addFilePaths(paths: string[]) {
  if (running.value) return;
  selectingFile.value = true;
  try {
    for (const path of paths) {
      if (!path || hasFilePath(path)) continue;
      try {
        const prev = await previewSelectedSqlFile(path);
        files.value.push({
          id: uuid(),
          filePath: prev.filePath,
          preview: prev,
          executionId: "",
          status: "idle",
          progress: null,
          error: "",
          expanded: false,
        });
      } catch (e: any) {
        toast(e?.message || String(e), 5000);
      }
    }
  } finally {
    selectingFile.value = false;
  }
}

async function addFileObjects(fileList: File[]) {
  if (running.value) return;
  const sqlFiles = fileList.filter((f) => f.name.toLowerCase().endsWith(".sql"));
  if (!sqlFiles.length) return;
  selectingFile.value = true;
  try {
    for (const file of sqlFiles) {
      try {
        const prev = await previewSelectedSqlFile(file);
        if (hasFilePath(prev.filePath)) continue;
        files.value.push({
          id: uuid(),
          filePath: prev.filePath,
          preview: prev,
          executionId: "",
          status: "idle",
          progress: null,
          error: "",
          expanded: false,
        });
      } catch (e: any) {
        toast(e?.message || String(e), 5000);
      }
    }
  } finally {
    selectingFile.value = false;
  }
}

async function selectFiles() {
  if (running.value) return;
  if (!isTauriRuntime()) {
    fileInput.value?.click();
    return;
  }
  selectingFile.value = true;
  try {
    const { open } = await import("@tauri-apps/plugin-dialog");
    const selected = await open({
      multiple: true,
      filters: [{ name: "SQL", extensions: ["sql"] }],
    });
    if (!selected) return;
    const paths = Array.isArray(selected) ? selected : [selected];
    await addFilePaths(paths);
  } catch (e: any) {
    toast(e?.message || String(e), 5000);
  } finally {
    selectingFile.value = false;
  }
}

async function selectFolder() {
  if (running.value) return;
  if (!isTauriRuntime()) {
    toast(t("sqlFileTree.desktopOnly"), 3000);
    return;
  }
  selectingFile.value = true;
  try {
    const { open } = await import("@tauri-apps/plugin-dialog");
    const folder = await open({ directory: true, multiple: false });
    if (!folder) return;
    const entries = await api.listSqlFilesInFolder(folder as string);
    const paths = collectSqlPaths(entries);
    if (!paths.length) {
      toast(t("sqlFile.noSqlFilesInFolder"), 3000);
      return;
    }
    await addFilePaths(paths);
  } catch (e: any) {
    toast(e?.message || String(e), 5000);
  } finally {
    selectingFile.value = false;
  }
}

async function handleFileInputChange(event: Event) {
  const input = event.target as HTMLInputElement;
  const fileList = Array.from(input.files ?? []);
  input.value = "";
  if (!fileList.length || running.value) return;
  await addFileObjects(fileList);
}

async function handleDrop(event: DragEvent) {
  if (running.value) return;
  const dropped = Array.from(event.dataTransfer?.files ?? []);
  if (!dropped.length) return;
  await addFileObjects(dropped);
}

function removeFile(id: string) {
  if (running.value) return;
  const file = files.value.find((f) => f.id === id);
  if (file && isWebMode.value && !file.executionId) {
    void releaseSqlFileUpload(file.preview.filePath).catch(() => {});
  }
  files.value = files.value.filter((f) => f.id !== id);
}

function clearFiles() {
  if (running.value) return;
  if (isWebMode.value) {
    for (const f of files.value) {
      if (!f.executionId) void releaseSqlFileUpload(f.preview.filePath).catch(() => {});
    }
  }
  files.value = [];
}

function toggleExpand(file: BatchFileItem) {
  file.expanded = !file.expanded;
}

// Tauri emits a single global progress stream keyed by executionId, so one
// listener covers the whole batch. Web mode emits per-execution-id events, so
// each file registers its own listener inside `runFile` via
// `registerWebFileListener`; the batch-level listener is a no-op there.
const fileUnlisteners: Array<() => void> = [];

async function listenProgress(handler: (next: SqlFileProgress) => void): Promise<() => void> {
  if (isTauriRuntime()) {
    return listenSqlFileProgress(handler);
  }
  return () => {};
}

async function registerWebFileListener(executionId: string) {
  if (isTauriRuntime()) return;
  const { listenSqlFileProgressById } = await import("@/lib/sql/httpSqlFileProgress");
  const unlisten = await listenSqlFileProgressById(executionId, applyProgress);
  fileUnlisteners.push(unlisten);
}

async function refreshTargetAfterImport() {
  if (refreshedTarget.value) return;
  refreshedTarget.value = true;
  try {
    await store.refreshDatabaseTreeNode(connectionId.value, database.value.trim());
  } catch (e: any) {
    toast(e?.message || String(e), 5000);
  }
}

// In web mode, `executeSqlFile` (POST) returns immediately after the backend
// spawns a background task — the POST resolving does NOT mean the file
// finished. The real terminal status arrives later via SSE. We therefore
// register a deferred promise per executionId that is resolved by
// `applyProgress` when it observes a terminal status, and `runFile` awaits
// that promise (racing with a safety timeout) before returning. In Tauri
// mode `executeSqlFile` is synchronous and only resolves after completion, so
// no waiting is needed and this map stays empty.
const terminalResolvers = new Map<string, (status: BatchFileStatus) => void>();

function applyProgress(next: SqlFileProgress) {
  const file = files.value.find((f) => f.executionId === next.executionId);
  if (!file) return;
  file.progress = next;
  file.status = next.status;
  if (next.error) file.error = next.error;
  updateSqlFileTask(next.executionId, next);
  if (next.status === "done") {
    void refreshTargetAfterImport();
  }
  // Resolve the web-mode terminal promise so `runFile` can return.
  if (isTerminalFileStatus(next.status)) {
    const resolver = terminalResolvers.get(next.executionId);
    if (resolver) {
      terminalResolvers.delete(next.executionId);
      resolver(next.status);
    }
  }
}

// Execute a single file end-to-end. Returns the terminal status so the
// orchestrator can decide whether to continue the batch.
async function runFile(file: BatchFileItem): Promise<BatchFileStatus> {
  if (cancelRequested.value) {
    file.status = "cancelled";
    return file.status;
  }
  const id = uuid();
  file.executionId = id;
  file.status = "running";
  file.progress = null;
  file.error = "";
  addSqlFileTask(id, file.preview.fileName, file.preview.filePath);

  const isWeb = !isTauriRuntime();
  // Web: create a deferred that resolves when SSE delivers a terminal status.
  // Tauri: executeSqlFile is synchronous, no deferred needed.
  let resolveTerminal: (status: BatchFileStatus) => void = () => {};
  const terminalPromise = isWeb
    ? new Promise<BatchFileStatus>((resolve) => {
        resolveTerminal = resolve;
        terminalResolvers.set(id, resolveTerminal);
      })
    : null;

  try {
    await executeSqlFile({
      executionId: id,
      connectionId: connectionId.value,
      database: database.value.trim(),
      filePath: file.preview.filePath,
      continueOnError: continueOnError.value,
    });

    if (isWeb) {
      // Subscribe to SSE only after the POST has successfully created the
      // task. The backend waits for the channel to appear (and saves the
      // terminal progress for late subscribers), so ordering POST before
      // GET avoids the race where the progress request arrives before the
      // channel is registered and the SSE error handler closes the
      // connection before the task even starts.
      await registerWebFileListener(id);
      // The POST returned, but the background task is still running. Wait
      // for the SSE stream to deliver a terminal status. Race with a safety
      // timeout in case the SSE connection drops silently (onerror closes
      // the EventSource without delivering a terminal event).
      const timeout = new Promise<BatchFileStatus>((resolve) => setTimeout(() => resolve(cancelRequested.value ? "cancelled" : "error"), 600_000));
      await Promise.race([terminalPromise!, timeout]);
      // applyProgress already set file.status via the SSE handler. If it
      // never fired (timeout), synthesize a terminal status.
      if (!isTerminalFileStatus(file.status)) {
        file.status = cancelRequested.value ? "cancelled" : "error";
        file.error = file.error || "Timed out waiting for execution result";
        const lastProgress = file.progress as SqlFileProgress | null;
        updateSqlFileTask(id, {
          executionId: id,
          status: file.status as SqlFileStatus,
          statementIndex: lastProgress?.statementIndex ?? 0,
          successCount: lastProgress?.successCount ?? 0,
          failureCount: lastProgress?.failureCount ?? 0,
          affectedRows: lastProgress?.affectedRows ?? 0,
          elapsedMs: lastProgress?.elapsedMs ?? 0,
          statementSummary: lastProgress?.statementSummary ?? "",
          error: file.error,
        });
      }
    } else {
      // Tauri: executeSqlFile is synchronous — the file is done now.
      // Synthesize a terminal status if the progress stream missed it.
      if (!isTerminalFileStatus(file.status)) {
        file.status = cancelRequested.value ? "cancelled" : "done";
        const lastProgress = file.progress as SqlFileProgress | null;
        updateSqlFileTask(id, {
          executionId: id,
          status: file.status as SqlFileStatus,
          statementIndex: lastProgress?.statementIndex ?? 0,
          successCount: lastProgress?.successCount ?? 0,
          failureCount: lastProgress?.failureCount ?? 0,
          affectedRows: lastProgress?.affectedRows ?? 0,
          elapsedMs: lastProgress?.elapsedMs ?? 0,
          statementSummary: lastProgress?.statementSummary ?? "",
          error: lastProgress?.error ?? null,
        });
        if (file.status === "done") {
          await refreshTargetAfterImport();
        }
      }
    }
  } catch (e: any) {
    file.status = cancelRequested.value ? "cancelled" : "error";
    file.error = e?.message || String(e);
    const lastProgress = file.progress as SqlFileProgress | null;
    updateSqlFileTask(id, {
      executionId: id,
      status: file.status as SqlFileStatus,
      statementIndex: lastProgress?.statementIndex ?? 0,
      successCount: lastProgress?.successCount ?? 0,
      failureCount: lastProgress?.failureCount ?? 0,
      affectedRows: lastProgress?.affectedRows ?? 0,
      elapsedMs: lastProgress?.elapsedMs ?? 0,
      statementSummary: lastProgress?.statementSummary ?? "",
      error: file.error,
    });
    if (!cancelRequested.value) {
      toast(`${file.preview.fileName}: ${file.error}`, 5000);
    }
  } finally {
    terminalResolvers.delete(id);
  }
  return file.status;
}

function computeBatchTerminalStatus(): BatchFileStatus {
  return computeBatchTerminalStatusPure(
    files.value.map((f) => f.status),
    cancelRequested.value,
    terminalStatus.value,
  );
}

async function startExecution() {
  if (!canStart.value || !files.value.length) return;
  // Production safety: review the whole batch once before running. Previews
  // are truncated, so file execution is always reviewed rather than inferring
  // safety from partial previews.
  const productionContext = productionContextForDatabase(selectedConnection.value, database.value);
  if (productionContext.active) {
    const combinedPreview = files.value.map((f) => f.preview.preview).join("\n;\n");
    const confirmed = await productionSafetyStore.requestConfirmation({
      sql: combinedPreview,
      connectionName: selectedConnection.value?.name,
      database: database.value,
      productionDatabases: productionContext.databases,
      source: t("production.sourceSqlFile"),
    });
    if (!confirmed) return;
  }

  running.value = true;
  cancelling.value = false;
  cancelRequested.value = false;
  executionStarted.value = false;
  terminalStatus.value = "running";
  terminalError.value = "";
  refreshedTarget.value = false;
  for (const f of files.value) {
    f.status = "pending";
    f.progress = null;
    f.error = "";
    f.executionId = "";
  }

  let unlisten: (() => void) | undefined;
  try {
    await store.ensureConnected(connectionId.value);
    if (cancelRequested.value) {
      terminalStatus.value = "cancelled";
      return;
    }

    unlisten = await listenProgress(applyProgress);

    if (cancelRequested.value) {
      terminalStatus.value = "cancelled";
      return;
    }

    executionStarted.value = true;
    // Web mode loads each uploaded file fully into memory, so parallel
    // execution of several large files can exhaust memory. Force sequential
    // execution in web mode until streaming execution is available.
    if (executionMode.value === "sequential" || isWebMode.value) {
      for (const f of files.value) {
        if (cancelRequested.value) {
          f.status = "cancelled";
          break;
        }
        const status = await runFile(f);
        // In sequential mode `continueOnError` also gates whether a failed
        // file aborts the rest of the batch.
        if (!shouldContinueBatch(status, continueOnError.value)) break;
      }
    } else {
      // Parallel with a small concurrency cap to avoid overwhelming the
      // database with simultaneous connections.
      await runWithConcurrency(
        files.value,
        Math.min(4, files.value.length),
        (f) => runFile(f),
        () => cancelRequested.value,
      );
    }

    terminalStatus.value = computeBatchTerminalStatus();
    if (terminalStatus.value === "done") {
      await refreshTargetAfterImport();
    }
  } catch (e: any) {
    terminalStatus.value = cancelRequested.value ? "cancelled" : "error";
    terminalError.value = e?.message || String(e);
    if (!cancelRequested.value) {
      toast(terminalError.value, 5000);
    }
  } finally {
    unlisten?.();
    fileUnlisteners.forEach((u) => u());
    fileUnlisteners.length = 0;
    running.value = false;
    cancelling.value = false;
    executionStarted.value = false;
  }
}

async function cancelExecution() {
  if (!running.value || cancelling.value) return;
  cancelRequested.value = true;
  cancelling.value = true;
  if (!executionStarted.value) return;
  // Cancel every file that is still running or pending. Fire all cancel
  // requests in parallel so the batch aborts promptly in parallel mode.
  const active = files.value.filter((f) => f.executionId && (f.status === "running" || f.status === "started" || f.status === "statementDone" || f.status === "pending"));
  await Promise.allSettled(active.map((f) => cancelSqlFileExecution(f.executionId)));
}

function handleOpenChange(nextOpen: boolean) {
  open.value = nextOpen;
}

let dragDropUnlisten: (() => void) | undefined;

async function registerDragDrop() {
  if (!isTauriRuntime()) return;
  try {
    const { getCurrentWebview } = await import("@tauri-apps/api/webview");
    dragDropUnlisten = await getCurrentWebview().onDragDropEvent((event: any) => {
      if (event.payload?.type === "drop") {
        const paths: string[] = Array.isArray(event.payload.paths) ? event.payload.paths : [];
        const sqlPaths = paths.filter((p) => p.toLowerCase().endsWith(".sql"));
        if (sqlPaths.length) void addFilePaths(sqlPaths);
      }
    });
  } catch {
    // Drag-and-drop is best-effort; ignore if the webview API is unavailable.
  }
}

watch(connectionId, (id) => {
  loadDatabasesForConnection(id);
});

watch(sqlConnections, () => {
  if (!open.value || running.value || selectedConnection.value) return;
  connectionId.value = resolveInitialConnectionId();
});

watch(
  open,
  async (value, _old, onCleanup) => {
    // `invalidated` is set to true by the cleanup function when the watcher
    // is re-triggered (e.g. the dialog closes while an async step is still
    // in flight). Each async continuation checks this flag before proceeding
    // so we don't register drag-drop listeners or add files to a dialog that
    // has already been closed.
    let invalidated = false;
    onCleanup(() => {
      invalidated = true;
      dragDropUnlisten?.();
      dragDropUnlisten = undefined;
    });

    if (!value) {
      // Restore global file-drop handling once the dialog closes.
      setFileDropIntercepted(false);
      // Release uploaded temp files for previewed-but-unexecuted files when
      // the dialog closes in web mode. Files that were already executed have
      // already been deleted by the backend's finalize_execution; the release
      // endpoint is idempotent so double deletion is harmless.
      if (isWebMode.value && !running.value) {
        for (const f of files.value) {
          if (!f.executionId) void releaseSqlFileUpload(f.preview.filePath).catch(() => {});
        }
      }
      return;
    }
    if (running.value) return;
    // Intercept global file drops while the dialog is open so dropped SQL
    // files are added to the batch list instead of opening query tabs.
    setFileDropIntercepted(true);
    resetState();
    if (connectionId.value) {
      loadDatabasesForConnection(connectionId.value);
    }
    // When opened from the SQL Files panel with a pre-selected file, load it
    // into the batch list as a single item so the dialog degrades to the
    // single-file experience.
    if (props.prefillFilePath) {
      await addFilePaths([props.prefillFilePath]);
      if (invalidated) return;
    }
    await registerDragDrop();
    if (invalidated) {
      // The dialog closed while `registerDragDrop` was in flight — clean up
      // the listener that was just registered.
      dragDropUnlisten?.();
      dragDropUnlisten = undefined;
    }
  },
  { immediate: true },
);

// Safety net: if the component unmounts while the dialog is still open, make
// sure the global file-drop interception is released.
onUnmounted(() => {
  setFileDropIntercepted(false);
});
</script>

<template>
  <Dialog :open="open" @update:open="handleOpenChange">
    <DialogScrollContent class="flex max-h-[calc(var(--dbx-viewport-height)-6rem)] min-h-0 min-w-0 flex-col overflow-hidden sm:max-w-[860px]" :trap-focus="false" @interact-outside.prevent>
      <DialogHeader class="shrink-0">
        <DialogTitle class="flex items-center gap-2">
          <FileCode class="w-4 h-4" />
          {{ t("sqlFile.title") }}
        </DialogTitle>
      </DialogHeader>

      <!-- Keep terminal actions reachable while long previews and errors scroll inside the viewport. -->
      <div class="grid min-h-0 min-w-0 flex-1 gap-4 overflow-y-auto py-3">
        <div class="min-w-0 space-y-3">
          <div class="flex items-center justify-between gap-2">
            <div class="text-xs font-medium text-muted-foreground uppercase tracking-wider">
              {{ t("sqlFile.file") }}
              <span v-if="files.length" class="ml-1 text-muted-foreground/70 normal-case font-normal">{{ t("sqlFile.filesCount", { count: files.length }) }}</span>
            </div>
            <Button v-if="files.length" variant="ghost" size="sm" class="h-6 text-xs text-muted-foreground hover:text-destructive" :disabled="running" @click="clearFiles">
              <Trash2 class="w-3 h-3 mr-1" />
              {{ t("sqlFile.clearAll") }}
            </Button>
          </div>

          <div class="flex items-center gap-2">
            <input ref="fileInput" type="file" accept=".sql,text/sql" multiple class="hidden" @change="handleFileInputChange" />
            <Button variant="outline" size="sm" class="h-8 shrink-0" :disabled="running || selectingFile" @click="selectFiles">
              <Loader2 v-if="selectingFile" class="w-3.5 h-3.5 mr-1.5 animate-spin" />
              <FolderOpen v-else class="w-3.5 h-3.5 mr-1.5" />
              {{ t("sqlFile.browse") }}
            </Button>
            <Button variant="outline" size="sm" class="h-8 shrink-0" :disabled="running || selectingFile" @click="selectFolder">
              <FolderSearch class="w-3.5 h-3.5 mr-1.5" />
              {{ t("sqlFile.selectFolder") }}
            </Button>
          </div>

          <div class="min-w-0 max-w-full rounded-md border border-dashed" :class="files.length ? 'border-border' : 'border-muted-foreground/30 p-4'" @dragover.prevent @drop.prevent="handleDrop">
            <div v-if="!files.length" class="flex flex-col items-center justify-center gap-1 py-6 text-xs text-muted-foreground">
              <FileCode class="w-6 h-6 text-muted-foreground/40" />
              <span>{{ t("sqlFile.noFiles") }}</span>
              <span class="text-muted-foreground/70">{{ t("sqlFile.dropHint") }}</span>
            </div>
            <div v-else class="divide-y">
              <div v-for="file in files" :key="file.id" class="min-w-0">
                <div class="flex items-center gap-2 px-2 py-1.5 text-xs">
                  <button type="button" class="shrink-0 text-muted-foreground hover:text-foreground" :disabled="running" @click="toggleExpand(file)">
                    <ChevronDown v-if="file.expanded" class="w-3.5 h-3.5" />
                    <ChevronRight v-else class="w-3.5 h-3.5" />
                  </button>
                  <Loader2 v-if="file.status === 'running' || file.status === 'started' || file.status === 'statementDone'" class="w-3.5 h-3.5 shrink-0 animate-spin text-primary" />
                  <Check v-else-if="file.status === 'done'" class="w-3.5 h-3.5 shrink-0 text-green-600" />
                  <X v-else-if="file.status === 'error' || file.status === 'cancelled'" class="w-3.5 h-3.5 shrink-0" :class="file.status === 'error' ? 'text-destructive' : 'text-yellow-600'" />
                  <FileCode v-else class="w-3.5 h-3.5 shrink-0 text-muted-foreground" />
                  <span class="min-w-0 flex-1 truncate font-medium">{{ file.preview.fileName }}</span>
                  <span class="shrink-0 text-muted-foreground">{{ formatBytes(file.preview.sizeBytes) }}</span>
                  <span v-if="file.progress" class="shrink-0" :class="fileStatusTone(file.status)"> {{ file.progress.successCount }}/{{ file.progress.statementIndex }} </span>
                  <Button variant="ghost" size="icon" class="h-5 w-5 shrink-0 text-muted-foreground hover:text-destructive" :disabled="running" @click="removeFile(file.id)">
                    <X class="w-3 h-3" />
                  </Button>
                </div>
                <div v-if="file.error" class="px-2 pb-1.5 text-xs text-destructive whitespace-pre-wrap break-all">
                  {{ file.error }}
                </div>
                <div v-if="file.expanded" class="sql-file-preview-viewer flex max-h-[min(42vh,360px)] max-w-full overflow-auto bg-muted/15 text-xs">
                  <div class="sticky left-0 z-10 select-none border-r bg-background/95 px-2 py-3 text-right font-mono leading-5 text-muted-foreground/70">
                    <div v-for="lineNumber in file.preview.preview.split(/\r\n|\r|\n/).length" :key="lineNumber">{{ lineNumber }}</div>
                  </div>
                  <pre class="min-w-max flex-1 p-3 font-mono leading-5 whitespace-pre" v-html="highlight(file.preview.preview)"></pre>
                </div>
              </div>
            </div>
          </div>
        </div>

        <div class="min-w-0 space-y-3">
          <div class="text-xs font-medium text-muted-foreground uppercase tracking-wider">
            {{ t("sqlFile.target") }}
          </div>

          <div class="grid grid-cols-1 gap-3 sm:grid-cols-2">
            <div class="space-y-1.5">
              <Label class="text-xs">{{ t("sqlFile.connection") }}</Label>
              <Select v-model="connectionId" :disabled="running">
                <SelectTrigger class="h-8 text-xs">
                  <div v-if="connectionId" class="flex items-center gap-1.5 min-w-0">
                    <DatabaseIcon :db-type="connectionIconType(connectionId)" class="w-3.5 h-3.5 shrink-0" />
                    <span class="truncate">{{ selectedConnection?.name ?? connectionId }}</span>
                  </div>
                  <SelectValue v-else :placeholder="t('sqlFile.selectConnection')" />
                </SelectTrigger>
                <SelectContent position="popper">
                  <SelectItem v-for="c in sqlConnections" :key="c.id" :value="c.id">
                    <div class="flex min-w-0 items-center gap-1.5">
                      <DatabaseIcon :db-type="c.driver_profile || c.db_type" class="w-3.5 h-3.5 shrink-0" />
                      <ConnectionGroupBadge :connection-id="c.id" />
                      <span class="min-w-0 flex-1 truncate">{{ c.name }}</span>
                    </div>
                  </SelectItem>
                </SelectContent>
              </Select>
            </div>

            <div class="space-y-1.5">
              <Label class="text-xs">{{ t("sqlFile.database") }}</Label>
              <Select v-if="databaseOptions.length" v-model="database" :disabled="running || loadingDatabases">
                <SelectTrigger class="h-8 text-xs">
                  <SelectValue :placeholder="t('sqlFile.selectDatabase')" />
                </SelectTrigger>
                <SelectContent>
                  <SelectItem v-for="db in databaseOptions" :key="db" :value="db">{{ db }}</SelectItem>
                </SelectContent>
              </Select>
              <div v-else class="relative">
                <Input v-model="database" class="h-8 text-xs" :disabled="running || loadingDatabases" :placeholder="t('sqlFile.databasePlaceholder')" />
                <Loader2 v-if="loadingDatabases" class="absolute right-2 top-2 w-3.5 h-3.5 animate-spin text-muted-foreground" />
              </div>
            </div>
          </div>
        </div>

        <div class="min-w-0 space-y-2.5">
          <div class="text-xs font-medium text-muted-foreground uppercase tracking-wider">
            {{ t("sqlFile.options") }}
          </div>

          <div class="flex items-center gap-3 text-xs">
            <Label class="text-xs text-muted-foreground">{{ t("sqlFile.executionMode") }}</Label>
            <div class="inline-flex rounded-md border border-border overflow-hidden">
              <button type="button" class="px-2.5 py-1 transition-colors" :class="executionMode === 'sequential' ? 'bg-primary text-primary-foreground' : 'text-muted-foreground hover:bg-muted'" :disabled="running" @click="executionMode = 'sequential'">
                {{ t("sqlFile.sequential") }}
              </button>
              <button
                type="button"
                class="px-2.5 py-1 transition-colors border-l border-border"
                :class="executionMode === 'parallel' ? 'bg-primary text-primary-foreground' : 'text-muted-foreground hover:bg-muted'"
                :disabled="running || isWebMode"
                :title="isWebMode ? t('sqlFile.parallelDisabledInWeb') : undefined"
                @click="executionMode = 'parallel'"
              >
                {{ t("sqlFile.parallel") }}
              </button>
            </div>
          </div>

          <button type="button" class="flex items-center gap-2 text-xs text-left" :disabled="running" @click="continueOnError = !continueOnError">
            <CheckSquare v-if="continueOnError" class="w-3.5 h-3.5 text-primary shrink-0" />
            <Square v-else class="w-3.5 h-3.5 text-muted-foreground/40 shrink-0" />
            {{ t("sqlFile.continueOnError") }}
          </button>
        </div>

        <div v-if="running || terminalStatus !== 'idle' || files.some((f) => f.progress)" class="min-w-0 space-y-3">
          <div class="flex items-center justify-between gap-3 text-xs">
            <div class="flex items-center gap-1.5 min-w-0" :class="statusTone">
              <component :is="statusIcon" class="w-3.5 h-3.5 shrink-0" :class="{ 'animate-spin': running }" />
              <span class="font-medium truncate">
                {{ cancelling ? t("sqlFile.cancelling") : statusLabel(terminalStatus) }}
              </span>
            </div>
            <span v-if="aggregateProgress.elapsedMs" class="text-muted-foreground shrink-0">
              {{ formatElapsed(aggregateProgress.elapsedMs) }}
            </span>
          </div>

          <div class="w-full bg-muted rounded-full h-2 overflow-hidden">
            <div class="h-full rounded-full transition-[width] duration-300" :class="terminalStatus === 'error' ? 'bg-destructive' : terminalStatus === 'cancelled' ? 'bg-yellow-500' : 'bg-primary'" :style="{ width: `${progressPercent}%` }" />
          </div>

          <div class="grid grid-cols-2 sm:grid-cols-4 gap-2 text-xs">
            <div class="border rounded-md px-2 py-1.5 min-w-0">
              <div class="text-muted-foreground truncate">{{ t("sqlFile.succeeded") }}</div>
              <div class="font-medium text-green-600 truncate">
                {{ aggregateProgress.successCount }}
              </div>
            </div>
            <div class="border rounded-md px-2 py-1.5 min-w-0">
              <div class="text-muted-foreground truncate">{{ t("sqlFile.failed") }}</div>
              <div class="font-medium text-destructive truncate">
                {{ aggregateProgress.failureCount }}
              </div>
            </div>
            <div class="border rounded-md px-2 py-1.5 min-w-0">
              <div class="text-muted-foreground truncate">{{ t("sqlFile.affectedRows") }}</div>
              <div class="font-medium truncate">
                {{ aggregateProgress.affectedRows.toLocaleString() }}
              </div>
            </div>
            <div class="border rounded-md px-2 py-1.5 min-w-0">
              <div class="text-muted-foreground truncate">{{ t("sqlFile.statement") }}</div>
              <div class="font-medium truncate">
                {{ aggregateProgress.statementIndex }}
              </div>
            </div>
          </div>

          <div v-if="terminalError" class="max-w-full overflow-auto rounded-md border bg-destructive/5 p-2 text-xs text-destructive whitespace-pre-wrap">
            {{ terminalError }}
          </div>
        </div>
      </div>

      <DialogFooter class="shrink-0">
        <template v-if="running">
          <Button variant="outline" size="sm" @click="open = false">
            {{ t("sqlFile.runInBackground") }}
          </Button>
          <Button variant="destructive" size="sm" :disabled="cancelling" @click="cancelExecution">
            <Loader2 v-if="cancelling" class="w-3.5 h-3.5 mr-1.5 animate-spin" />
            <X v-else class="w-3.5 h-3.5 mr-1.5" />
            {{ cancelling ? t("sqlFile.cancelling") : t("sqlFile.cancel") }}
          </Button>
        </template>
        <template v-else>
          <Button variant="outline" size="sm" @click="open = false">
            {{ t("common.close") }}
          </Button>
          <Button size="sm" :disabled="!canStart" @click="startExecution">
            <Play class="w-3.5 h-3.5 mr-1.5" />
            {{ t("sqlFile.execute") }}
          </Button>
        </template>
      </DialogFooter>
    </DialogScrollContent>
  </Dialog>
</template>
