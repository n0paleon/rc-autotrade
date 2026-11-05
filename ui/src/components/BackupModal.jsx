import { useRef, useState } from "react";
import { Icon, ModalShell, ToggleRow, useModalClose } from "./ModalShell.jsx";
import { formatBytes, formatDate, formatInt, formatTime } from "../lib/format.js";

const BACKUP_KIND = "turu-rc-backup";
const BACKUP_FORMAT_VERSION = 1;
const MAX_BACKUP_BYTES = 64 * 1024 * 1024;

function fileStamp(now = Date.now()) {
  return new Date(now).toISOString().replace(/[-:]/g, "").replace(/\.\d{3}Z$/, "Z");
}

function download(data, filename, type) {
  const blob = data instanceof Blob ? data : new Blob([data], { type });
  const url = URL.createObjectURL(blob);
  const link = document.createElement("a");
  link.href = url;
  link.download = filename;
  link.rel = "noopener";
  document.body.append(link);
  link.click();
  link.remove();
  setTimeout(() => URL.revokeObjectURL(url), 15_000);
}

/**
 * Client-side sanity check so a wrong file is rejected before the destructive
 * request. The worker repeats all of this validation and additionally checks
 * the database schema version.
 */
function inspectBackup(text, workerSchemaVersion) {
  let parsed;
  try {
    parsed = JSON.parse(text);
  } catch {
    throw new Error("That file is not valid JSON.");
  }
  if (!parsed || typeof parsed !== "object" || Array.isArray(parsed)) {
    throw new Error("The backup must be a JSON object.");
  }
  if (parsed.kind !== BACKUP_KIND) {
    throw new Error("That file is not a TURU.RC backup.");
  }

  const formatVersion = Number(parsed.formatVersion);
  if (!Number.isInteger(formatVersion) || formatVersion < 1) {
    throw new Error("The backup format version is missing.");
  }
  if (formatVersion > BACKUP_FORMAT_VERSION) {
    throw new Error(
      `This backup uses format v${formatVersion}; this deck only understands v${BACKUP_FORMAT_VERSION}. Update the worker first.`
    );
  }

  const schemaVersion = Number(parsed.schemaVersion);
  if (!Number.isInteger(schemaVersion) || schemaVersion < 1) {
    throw new Error("The backup database version is missing.");
  }

  const data = parsed.data && typeof parsed.data === "object" ? parsed.data : null;
  if (!data) throw new Error("The backup has no data block.");

  const counts = {
    targets: Array.isArray(data.targets) ? data.targets.length : 0,
    orders: Array.isArray(data.orders) ? data.orders.length : 0,
    reportProducts: Array.isArray(data.reportProducts) ? data.reportProducts.length : 0
  };

  const warnings = [];
  if (parsed.counts && typeof parsed.counts === "object") {
    for (const [key, actual] of Object.entries(counts)) {
      const claimed = Number(parsed.counts[key]);
      if (Number.isFinite(claimed) && claimed !== actual) {
        warnings.push(`The file claims ${formatInt(claimed)} ${key} but contains ${formatInt(actual)}.`);
      }
    }
  }

  const session = parsed.credentials?.session;
  const hasSession = Boolean(
    session && (session.accessToken || session.refreshToken || session.cookie)
  );
  if (!hasSession) {
    warnings.push("No Rollercoin session in this file; the current session will be kept.");
  } else {
    warnings.push("This file contains Rollercoin session tokens. Restore it privately.");
  }

  if (Number.isInteger(workerSchemaVersion) && schemaVersion > workerSchemaVersion) {
    warnings.push(
      `The file uses database schema v${schemaVersion}, newer than this worker (v${workerSchemaVersion}). The import will be rejected.`
    );
  }

  return {
    backup: parsed,
    counts,
    exportedAt: Number(parsed.exportedAt) || null,
    schemaVersion,
    appVersion: typeof parsed.app?.version === "string" ? parsed.app.version : "",
    hasSession,
    warnings
  };
}

function CountBox({ label, value }) {
  return (
    <div className="border-2 border-line bg-night-950/60 px-2.5 py-2">
      <span className="block font-mono text-[10px] uppercase tracking-[0.14em] text-faint">{label}</span>
      <strong className="font-mono text-[17px] tabular-nums text-gold">{formatInt(value)}</strong>
    </div>
  );
}

function WarningList({ warnings }) {
  if (!warnings || warnings.length === 0) return null;
  return (
    <ul className="space-y-1 border-2 border-warn/40 bg-warn/5 p-2.5 text-[12px] leading-snug text-warn">
      {warnings.map((warning) => (
        <li key={warning} className="flex items-start gap-1.5">
          <Icon name="i-alert" className="mt-0.5 h-3 w-3 shrink-0" />
          <span>{warning}</span>
        </li>
      ))}
    </ul>
  );
}

export function BackupModal({ snapshot, worker, notify, onClose }) {
  const { closeRef, close } = useModalClose();
  const [tab, setTab] = useState("export");
  const [includeCredentials, setIncludeCredentials] = useState(false);
  const [busy, setBusy] = useState("");
  const [error, setError] = useState("");
  const [fileName, setFileName] = useState("");
  const [preview, setPreview] = useState(null);
  const [restoreCredentials, setRestoreCredentials] = useState(true);
  const [acknowledged, setAcknowledged] = useState(false);
  const [restored, setRestored] = useState(null);
  const fileRef = useRef(null);

  const counts = {
    targets: Array.isArray(snapshot.configs) ? snapshot.configs.length : 0,
    orders: Number(snapshot.historyTotal) || 0,
    reportProducts: Array.isArray(snapshot.report?.items) ? snapshot.report.items.length : 0
  };
  const hasSession = Boolean(
    snapshot.session?.hasAccessToken || snapshot.session?.hasRefreshToken || snapshot.session?.hasCookie
  );
  const workerSchemaVersion = Number(snapshot.database?.schemaVersion) || null;
  const busyNow = busy !== "";

  const switchTab = (next) => {
    if (busyNow) return;
    setTab(next);
    setError("");
  };

  const runExport = async (format) => {
    setBusy(format);
    setError("");
    try {
      if (format === "sqlite") {
        const blob = await worker.exportDatabase();
        download(blob, `turu-rc-db-${fileStamp()}.db`, "application/vnd.sqlite3");
        notify(
          "success",
          "DATABASE SNAPSHOT",
          "Binary copy downloaded. Restore it only with the worker stopped."
        );
      } else {
        const text = await worker.exportBackup({ credentials: includeCredentials });
        download(text, `turu-rc-backup-${fileStamp()}.json`, "application/json");
        notify(
          "success",
          "BACKUP DOWNLOADED",
          includeCredentials
            ? "Includes the Rollercoin session — keep the file private."
            : "Portable backup downloaded. Keep it somewhere safe."
        );
      }
    } catch (failure) {
      setError(failure?.message || "The worker could not build the backup.");
      notify("error", "EXPORT FAILED", failure?.message || "The worker could not build the backup.");
    } finally {
      setBusy("");
    }
  };

  const chooseFile = async (event) => {
    const file = event.target.files?.[0];
    event.target.value = "";
    setPreview(null);
    setRestored(null);
    setError("");
    setAcknowledged(false);
    setFileName("");
    if (!file) return;

    setFileName(file.name);
    if (file.size > MAX_BACKUP_BYTES) {
      setError(
        `That file is ${formatBytes(file.size)}; the limit is ${formatBytes(MAX_BACKUP_BYTES)}.`
      );
      return;
    }
    try {
      const inspected = inspectBackup(await file.text(), workerSchemaVersion);
      setPreview(inspected);
      setRestoreCredentials(inspected.hasSession);
    } catch (failure) {
      setError(failure?.message || "That file could not be read.");
    }
  };

  const runImport = async () => {
    if (!preview || !acknowledged || busyNow) return;
    setBusy("import");
    setError("");
    try {
      const result = await worker.importBackup(preview.backup, {
        restoreCredentials: preview.hasSession ? restoreCredentials : false
      });
      setRestored(result);
      setPreview(null);
      setFileName("");
      setAcknowledged(false);
      if (fileRef.current) fileRef.current.value = "";
      notify(
        "success",
        "BACKUP RESTORED",
        `${formatInt(result?.restored?.counts?.targets)} targets · ${formatInt(
          result?.restored?.counts?.orders
        )} loot entries`
      );
    } catch (failure) {
      setError(failure?.message || "The backup could not be restored.");
      notify("error", "RESTORE FAILED", failure?.message || "The backup could not be restored.");
    } finally {
      setBusy("");
    }
  };

  return (
    <ModalShell
      title="Backup & migrate"
      onClose={onClose}
      closeRef={closeRef}
      wide
      footer={
        <div className="flex justify-end">
          <button className="btn" type="button" onClick={close}>
            Close
          </button>
        </div>
      }
    >
      <div className="modal-body">
        <div className="tab-rail grid-cols-2" role="tablist" aria-label="Backup or restore">
          <button
            className="pixel-tab"
            type="button"
            role="tab"
            aria-selected={tab === "export"}
            onClick={() => switchTab("export")}
          >
            Export
          </button>
          <button
            className="pixel-tab"
            type="button"
            role="tab"
            aria-selected={tab === "import"}
            onClick={() => switchTab("import")}
          >
            Import
          </button>
        </div>

        {error ? (
          <p
            className="flex items-start gap-2 border-2 border-danger/50 bg-danger/5 p-2.5 text-[13px] leading-relaxed text-danger"
            role="alert"
          >
            <Icon name="i-alert" className="mt-0.5 h-3.5 w-3.5 shrink-0" />
            <span>{error}</span>
          </p>
        ) : null}

        {tab === "export" ? (
          <>
            <p className="text-[13px] leading-relaxed text-muted">
              One portable file with every target, runtime setting, lifetime counter, profit row and
              loot entry. Restore it on another worker to move machines without losing history.
            </p>
            <div className="grid grid-cols-3 gap-2">
              <CountBox label="Targets" value={counts.targets} />
              <CountBox label="Loot log" value={counts.orders} />
              <CountBox label="Products" value={counts.reportProducts} />
            </div>

            <div className="space-y-2 border-2 border-line bg-night-950/60 p-3">
              <div className="flex items-start justify-between gap-3">
                <div>
                  <h3 className="font-mono text-[12px] font-semibold uppercase tracking-[0.14em] text-gold">
                    Portable JSON backup
                  </h3>
                  <p className="mt-1 text-[13px] leading-relaxed text-muted">
                    The market cache is not included; the deck rebuilds it from live frames.
                  </p>
                </div>
                <Icon name="i-download" className="h-6 w-6 shrink-0 text-gold" />
              </div>
              <ToggleRow
                checked={includeCredentials}
                onChange={setIncludeCredentials}
                title="Include Rollercoin session"
                hint={
                  hasSession
                    ? "Access token, refresh token and Cloudflare cookie."
                    : "No session is stored on this worker."
                }
                disabled={!hasSession || busyNow}
              />
              {includeCredentials ? (
                <p className="flex items-start gap-2 border-2 border-danger/40 bg-danger/5 p-2.5 text-[12px] leading-relaxed text-danger">
                  <Icon name="i-shield" className="mt-0.5 h-3.5 w-3.5 shrink-0" />
                  <span>
                    Anyone with this file can act as you on Rollercoin until the tokens expire. Do not
                    upload it to a public drive or chat.
                  </span>
                </p>
              ) : null}
              <button
                className="btn btn-primary btn-sm btn-block"
                type="button"
                onClick={() => runExport("json")}
                disabled={busyNow}
                data-busy={busy === "json" ? "true" : undefined}
                aria-busy={busy === "json"}
              >
                <Icon name="i-download" className="h-3.5 w-3.5" />
                {busy === "json" ? "BUILDING BACKUP…" : "Download backup .json"}
              </button>
            </div>

            <div className="space-y-2 border-2 border-line bg-night-950/60 p-3">
              <h3 className="flex items-center gap-1.5 font-mono text-[12px] font-semibold uppercase tracking-[0.14em] text-violet">
                <Icon name="i-database" className="h-3.5 w-3.5" /> Raw SQLite snapshot
              </h3>
              <p className="text-[13px] leading-relaxed text-muted">
                A byte-complete, consistent copy of the whole database, including the stored session
                and market cache. It must be restored with the worker stopped.
              </p>
              <button
                className="btn btn-ghost btn-sm btn-block"
                type="button"
                onClick={() => runExport("db")}
                disabled={busyNow}
                data-busy={busy === "db" ? "true" : undefined}
                aria-busy={busy === "db"}
              >
                <Icon name="i-database" className="h-3.5 w-3.5" />
                {busy === "db" ? "COPYING DATABASE…" : "Download .db snapshot"}
              </button>
            </div>

            <p className="text-[12px] leading-relaxed text-faint">
              Manual restore: stop the worker, replace <code className="text-ink-dim">data/rollercoin.db</code>{" "}
              with the snapshot, delete <code className="text-ink-dim">rollercoin.db-wal</code> and{" "}
              <code className="text-ink-dim">rollercoin.db-shm</code>, then start it again.
            </p>
          </>
        ) : (
          <>
            <ol className="steps">
              <li>Download a backup from the old worker.</li>
              <li>Open this deck on the new worker and log in with its access key.</li>
              <li>Pick the file below and restore it. The access key stays unchanged.</li>
            </ol>

            <label
              className={`btn btn-ghost btn-sm btn-block cursor-pointer${busyNow ? " opacity-50" : ""}`}
            >
              <Icon name="i-upload" className="h-3.5 w-3.5" />
              {fileName || "Choose backup .json"}
              <input
                ref={fileRef}
                className="sr-only"
                type="file"
                accept=".json,application/json"
                onChange={chooseFile}
                disabled={busyNow}
              />
            </label>

            {preview ? (
              <div className="space-y-3 border-2 border-signal/40 bg-signal/5 p-3">
                <div className="flex items-start justify-between gap-3">
                  <div className="min-w-0">
                    <h3 className="truncate font-mono text-[12px] font-semibold uppercase tracking-[0.14em] text-signal">
                      {fileName}
                    </h3>
                    <p className="mt-1 text-[12px] leading-relaxed text-muted">
                      Exported{" "}
                      {preview.exportedAt
                        ? `${formatDate(preview.exportedAt)} ${formatTime(preview.exportedAt)}`
                        : "unknown"}{" "}
                      · worker v{preview.appVersion || "?"} · schema v{preview.schemaVersion}
                    </p>
                  </div>
                  <Icon name="i-check" className="h-5 w-5 shrink-0 text-signal" />
                </div>

                <div className="grid grid-cols-3 gap-2">
                  <CountBox label="Targets" value={preview.counts.targets} />
                  <CountBox label="Loot log" value={preview.counts.orders} />
                  <CountBox label="Products" value={preview.counts.reportProducts} />
                </div>

                <WarningList warnings={preview.warnings} />

                {preview.hasSession ? (
                  <ToggleRow
                    checked={restoreCredentials}
                    onChange={setRestoreCredentials}
                    title="Restore Rollercoin session"
                    hint="Off keeps the session currently stored on this worker."
                    disabled={busyNow}
                  />
                ) : null}

                <ToggleRow
                  checked={acknowledged}
                  onChange={setAcknowledged}
                  title="Replace this worker's data"
                  hint="Targets, settings, counters and the entire loot log are overwritten. A .db safety snapshot is written first."
                  disabled={busyNow}
                />

                <button
                  className="btn btn-danger btn-sm btn-block"
                  type="button"
                  onClick={runImport}
                  disabled={!acknowledged || busyNow}
                  data-busy={busy === "import" ? "true" : undefined}
                  aria-busy={busy === "import"}
                >
                  <Icon name="i-upload" className="h-3.5 w-3.5" />
                  {busy === "import" ? "RESTORING…" : "Restore this backup"}
                </button>
              </div>
            ) : null}

            {restored ? (
              <div className="space-y-2 border-2 border-signal/50 bg-signal/5 p-3">
                <h3 className="flex items-center gap-1.5 font-mono text-[12px] font-semibold uppercase tracking-[0.14em] text-signal">
                  <Icon name="i-check" className="h-3.5 w-3.5" /> Restore complete
                </h3>
                <div className="grid grid-cols-3 gap-2">
                  <CountBox label="Targets" value={restored.restored?.counts?.targets} />
                  <CountBox label="Loot log" value={restored.restored?.counts?.orders} />
                  <CountBox label="Products" value={restored.restored?.counts?.reportProducts} />
                </div>
                <p className="text-[12px] leading-relaxed text-muted">
                  Rollercoin session:{" "}
                  {restored.restored?.credentialsRestored ? "restored from the file" : "kept as it was"}.
                </p>
                {restored.restored?.safetyBackup ? (
                  <p className="text-[12px] leading-relaxed text-faint">
                    Safety snapshot:{" "}
                    <code className="break-all text-ink-dim">{restored.restored.safetyBackup}</code>
                  </p>
                ) : null}
                <WarningList warnings={restored.warnings} />
              </div>
            ) : null}

            <p className="flex items-start gap-2 text-[12px] leading-relaxed text-faint">
              <Icon name="i-shield" className="mt-0.5 h-3.5 w-3.5 shrink-0 text-violet" />
              <span>
                The worker access key is never part of a backup. This worker keeps its own key, so
                existing bridge snippets and open tabs stay valid. Only <code>.json</code> backups can
                be restored here; a raw <code>.db</code> file must be swapped in with the worker
                stopped.
              </span>
            </p>
          </>
        )}
      </div>
    </ModalShell>
  );
}
