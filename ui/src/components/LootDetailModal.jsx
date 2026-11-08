import { useEffect, useState } from "react";

import { ModalShell } from "./ModalShell.jsx";
import {
  fixedPrice,
  formatInt,
  formatPreciseMs,
  formatSignedRlt,
  formatTime,
  shortId
} from "../lib/format.js";

const STAGE_COLORS = [
  "bg-info",
  "bg-violet",
  "bg-pink",
  "bg-gold",
  "bg-warn",
  "bg-signal",
  "bg-danger"
];

function mark(timings, key) {
  const value = Number(timings?.[key]);
  return Number.isFinite(value) && value > 0 ? value : null;
}

function delta(from, to) {
  if (!Number.isFinite(from) || !Number.isFinite(to) || to < from) return null;
  return to - from;
}

function clockMs(ts) {
  if (!Number.isFinite(ts)) return "—";
  const date = new Date(ts);
  const time = date.toLocaleTimeString("en-GB", { hour12: false });
  return `${time}.${String(date.getMilliseconds()).padStart(3, "0")}`;
}

function dnsValue(t) {
  // A reused socket performed no resolver lookup at all. Returning null keeps
  // that distinct from a real sub-millisecond lookup.
  if (t?.dnsMode === "cache" || t?.dnsMode === "reused") return null;
  const value = Number(t?.dnsMs);
  return Number.isFinite(value) && value >= 0 ? value : null;
}

function dnsText(t) {
  // A bare dash looked like missing telemetry. State the reason instead.
  if (t?.dnsMode === "reused") return "NO LOOKUP";
  if (t?.dnsMode === "cache") return "CACHED";
  if (t?.dnsMode === "skipped") return "SKIPPED";
  return null;
}

function dnsHint(t) {
  if (t?.dnsMode === "reused") return "Keep-alive socket reused — this request performed no DNS";
  if (t?.dnsMode === "cache") return "DNS cache hit — no resolver round-trip";
  if (t?.dnsMode === "pending") return "Resolver probe is still running; its result will be saved when it finishes";
  if (t?.dnsMode === "skipped") return "Host is local or already an IP address";
  if (t?.dnsMode === "probe") {
    return t.dnsError
      ? `Resolver probe failed: ${t.dnsError}`
      : `Parallel resolver probe${t.dnsAddress ? ` → ${t.dnsAddress}` : ""} (legacy row)`;
  }
  if (t?.dnsMode === "lookup") {
    if (t.dnsError) return `Resolver lookup failed: ${t.dnsError}`;
    return t.dnsAddress
      ? `Resolver lookup this request paid for → ${t.dnsAddress}`
      : "Resolver lookup this request paid for";
  }
  if (t?.dnsError) return t.dnsError;
  if (t?.dnsMode === "unknown") return "Connection opened but the lookup did not complete";
  if (t?.dnsHost) return "No resolver sample for this request";
  return "No sample (mock, IP host or local)";
}

function socketLabel(t) {
  if (t?.socketMode === "new") return "NEW";
  if (t?.socketMode === "reused") return "KEEP-ALIVE";
  if (t?.socketMode === "unknown") return "UNKNOWN";
  return null;
}

function socketHint(t) {
  if (t?.socketMode === "new") {
    return "Fresh connection: this request opened a socket (DNS when the host is a name, plus TCP + TLS)";
  }
  if (t?.socketMode === "reused") {
    return "Pooled keep-alive socket reused — no DNS, TCP or TLS on this request";
  }
  if (t?.socketMode === "unknown") return "No completed connection observed (mock, or the request failed while connecting)";
  return "No sample (mock or an older row)";
}

function Metric({ label, value, text, hint, tone = "text-ink" }) {
  return (
    <div className="border-2 border-line/70 bg-night-950/60 px-3 py-2.5">
      <p className="font-mono text-[11px] font-bold uppercase tracking-[0.08em] text-faint">{label}</p>
      <p className={`mt-1 font-mono text-[18px] tabular-nums ${tone}`}>
        {text ?? (value == null ? "—" : formatPreciseMs(value))}
      </p>
      {hint ? <p className="mt-0.5 text-[12px] leading-snug text-faint">{hint}</p> : null}
    </div>
  );
}

function RawEventPayload({ label, payload, missingText }) {
  return (
    <details className="border-2 border-line/60">
      <summary className="cursor-pointer px-3 py-2 font-mono text-[11px] font-bold uppercase tracking-[0.08em] text-faint hover:text-ink-dim">
        {label}
      </summary>
      <div className="border-t border-line/60 px-3 py-2">
        {payload ? (
          <>
            {payload.__truncatedForStorage ? (
              <p className="mb-2 text-[12px] text-warn">
                Payload was {formatInt(payload.originalBytes)} bytes; showing a bounded preview.
              </p>
            ) : null}
            <pre className="max-h-[320px] overflow-auto whitespace-pre-wrap break-all font-mono text-[11px] leading-relaxed text-ink-dim">
              {payload.__truncatedForStorage
                ? payload.preview || "No preview available."
                : JSON.stringify(payload, null, 2)}
            </pre>
          </>
        ) : (
          <p className="text-[12px] leading-relaxed text-faint">{missingText}</p>
        )}
      </div>
    </details>
  );
}

function buildStages(t) {
  if (!t) return [];
  const signalAt = mark(t, "signalAt");
  const stages = [
    {
      key: "delivery",
      label: "Feed delivery → engine",
      hint: "WebSocket frame received until the engine handler starts",
      from: signalAt,
      to: mark(t, "handleAt"),
      color: STAGE_COLORS[0]
    },
    {
      key: "decision",
      label: "Signal evaluated",
      hint: "Target match, depth pricing, safety gates and cooldown checks",
      from: mark(t, "handleAt"),
      to: mark(t, "orderAt"),
      color: STAGE_COLORS[1]
    },
    {
      key: "commit",
      label: "Order commit & broadcast",
      hint: "Counter update, SQLite write and UI order event",
      from: mark(t, "orderAt"),
      to: mark(t, "dispatchAt"),
      color: STAGE_COLORS[2]
    },
    {
      key: "submit",
      label: "Submit → fetch",
      hint: "Purchase payload built and the HTTP request dispatched",
      from: mark(t, "dispatchAt"),
      to: mark(t, "sendAt"),
      color: STAGE_COLORS[0]
    },
    {
      key: "ttfb",
      label: "Purchase request → response headers",
      hint:
        Number(t.attempts) > 1
          ? "Includes the 401 token refresh and the retried request"
          : "Network transit plus Rollercoin processing the purchase",
      from: mark(t, "sendAt"),
      to: mark(t, "responseAt"),
      color: STAGE_COLORS[3]
    },
    {
      key: "body",
      label: "Response headers → body parsed",
      hint: "Rollercoin response body read and JSON parsed",
      from: mark(t, "responseAt"),
      to: mark(t, "completeAt"),
      color: STAGE_COLORS[4]
    }
  ];

  const acceptedAt = mark(t, "acceptedAt");
  const fillAt = mark(t, "fillAt");
  const fillHandledAt = mark(t, "fillHandledAt");
  const failedAt = mark(t, "failedAt");

  if (acceptedAt != null) {
    stages.push(
      {
        key: "accepted",
        label: "Engine accepted order",
        hint: "Order id stored, pending confirmation registered and broadcast",
        from: mark(t, "completeAt"),
        to: acceptedAt,
        color: STAGE_COLORS[5]
      },
      {
        key: "fill-wait",
        label: "Await fulfillment event",
        hint: "Rollercoin marketplace matching until its result event arrives",
        from: acceptedAt,
        to: fillAt,
        color: STAGE_COLORS[0]
      },
      {
        key: "fill-handle",
        label: "Fulfillment handled",
        hint: "Fill event processed and written to the loot log",
        from: fillAt,
        to: fillHandledAt,
        color: STAGE_COLORS[5]
      }
    );
  } else if (failedAt != null) {
    stages.push({
      key: "failed",
      label: "Rejection handled",
      hint: "Purchase rejection logged and the target backed off",
      from: mark(t, "completeAt"),
      to: failedAt,
      color: STAGE_COLORS[6]
    });
  }

  return stages.map((stage) => ({ ...stage, ms: delta(stage.from, stage.to) }));
}

export function LootDetailModal({ entry: summary, onClose, request }) {
  const [detail, setDetail] = useState(null);
  const [loadingDetail, setLoadingDetail] = useState(false);
  const [detailError, setDetailError] = useState(false);

  // Lists only carry the columns a row renders. A live order event still
  // arrives with its frames inline, so only fetch when they are missing.
  useEffect(() => {
    setDetail(null);
    setDetailError(false);
    if (!request || !summary?.id || summary.rawEvents) {
      setLoadingDetail(false);
      return undefined;
    }
    let cancelled = false;
    setLoadingDetail(true);
    request(`/api/history/${encodeURIComponent(summary.id)}`)
      .then((result) => {
        if (!cancelled) setDetail(result?.entry || null);
      })
      .catch(() => {
        if (!cancelled) {
          setDetail(null);
          setDetailError(true);
        }
      })
      .finally(() => {
        if (!cancelled) setLoadingDetail(false);
      });
    return () => {
      cancelled = true;
    };
  }, [summary?.id, summary?.rawEvents, request]);

  const entry =
    detail && detail.id === summary.id ? { ...summary, ...detail } : summary;
  const t = entry.timings || null;
  const signalAt = mark(t, "signalAt");
  const sendAt = mark(t, "sendAt");
  const responseAt = mark(t, "responseAt");
  const completeAt = mark(t, "completeAt");
  const fillAt = mark(t, "fillAt");
  const fillHandledAt = mark(t, "fillHandledAt");
  const failedAt = mark(t, "failedAt");
  const rawEvents = entry.rawEvents || null;
  const timelineEnd =
    fillHandledAt ?? fillAt ?? failedAt ?? completeAt ?? responseAt ?? sendAt ?? null;
  const span = delta(signalAt, timelineEnd);
  const stages = buildStages(t).filter((stage) => stage.ms != null);
  const bottleneck = stages.reduce(
    (worst, stage) => (worst == null || stage.ms > worst.ms ? stage : worst),
    null
  );

  const rawMarks = t
    ? [
        ["Signal received", mark(t, "signalAt")],
        ["Engine handler", mark(t, "handleAt")],
        ["Order decided", mark(t, "orderAt")],
        ["Submit called", mark(t, "dispatchAt")],
        ["Fetch sent", mark(t, "sendAt")],
        ["Response headers", mark(t, "responseAt")],
        ["Response complete", mark(t, "completeAt")],
        ["Order accepted", mark(t, "acceptedAt")],
        ["Fulfillment event", mark(t, "fillAt")],
        ["Fulfillment handled", mark(t, "fillHandledAt")],
        ["Failure logged", mark(t, "failedAt")]
      ].filter(([, value]) => value != null)
    : [];

  return (
    <ModalShell
      title={`Loot telemetry · ${entry.label || shortId(entry.itemId)}`}
      onClose={onClose}
      wide
    >
      <div className="space-y-4 p-4 sm:p-5">
        <div className="flex flex-wrap items-start justify-between gap-3">
          <div className="min-w-0">
            <p className="truncate font-sans text-[17px] font-bold text-ink">
              {entry.label || shortId(entry.itemId)}
            </p>
            <p className="mt-0.5 font-mono text-[12px] uppercase tracking-[0.08em] text-faint">
              {formatTime(entry.at)} · order {entry.orderId ? shortId(entry.orderId) : "—"}
            </p>
            <p className="mt-0.5 break-all font-mono text-[11px] text-faint" title={entry.itemId}>
              item {entry.itemId || "—"}
            </p>
          </div>
          <span
            className={`status-pill shrink-0${entry.status === "failed" ? " is-failed" : ""}`}
          >
            {entry.status === "failed" ? "Failed" : "Filled"}
          </span>
        </div>

        <div className="grid grid-cols-2 gap-x-4 gap-y-2 border-2 border-line/70 bg-night-950/40 px-3 py-3 font-mono text-[12px] sm:grid-cols-3">
          <div>
            <p className="text-[11px] uppercase tracking-[0.08em] text-faint">Qty</p>
            <p className="text-ink-dim">{formatInt(entry.qty)}</p>
          </div>
          <div>
            <p className="text-[11px] uppercase tracking-[0.08em] text-faint">Unit</p>
            <p className="text-ink-dim">{fixedPrice(entry.unitPrice)} RLT</p>
          </div>
          <div>
            <p className="text-[11px] uppercase tracking-[0.08em] text-faint">Total</p>
            <p className="text-ink-dim">{fixedPrice(entry.totalPrice)} RLT</p>
          </div>
          <div>
            <p className="text-[11px] uppercase tracking-[0.08em] text-faint">Profit</p>
            <p className={entry.profitRlt > 0 ? "text-gold" : entry.profitRlt < 0 ? "text-danger" : "text-ink-dim"}>
              {entry.status === "failed" ? "0.000000" : formatSignedRlt(entry.profitRlt)}
            </p>
          </div>
          <div>
            <p className="text-[11px] uppercase tracking-[0.08em] text-faint">Target</p>
            <p className="text-ink-dim">{fixedPrice(entry.targetPrice)} RLT</p>
          </div>
          <div>
            <p className="text-[11px] uppercase tracking-[0.08em] text-faint">Order id</p>
            <p className="break-all text-ink-dim" title={entry.orderId || undefined}>
              {entry.orderId || "—"}
            </p>
          </div>
        </div>

        {entry.status === "failed" ? (
          <div className="border-2 border-danger/50 bg-danger/10 px-3 py-2">
            <p className="font-mono text-[11px] font-bold uppercase tracking-[0.08em] text-danger">
              Failure reason
            </p>
            <p className="mt-0.5 text-[13px] text-ink-dim">
              {entry.message || entry.note || "failed"}
            </p>
          </div>
        ) : null}

        <div className="border-2 border-line bg-night-950/50 p-3 sm:p-4">
          <div className="flex flex-wrap items-center justify-between gap-2">
            <h3 className="panel-title text-[13px]">
              <span className="led bg-gold" /> Latency debugger
            </h3>
            <span className="font-mono text-[12px] tabular-nums text-faint">
              {span == null ? "no timing data" : `${formatPreciseMs(span)} total span`}
            </span>
          </div>

          {t ? (
            <>
              <div className="mt-3 grid grid-cols-1 gap-2 sm:grid-cols-2">
                <Metric
                  label="Signal → purchase sent"
                  value={delta(signalAt, sendAt)}
                  hint="Until fetch() is invoked; no response awaited"
                  tone="text-info"
                />
                <Metric
                  label="Signal → response headers"
                  value={delta(signalAt, responseAt)}
                  hint="The network round-trip is complete"
                  tone="text-gold"
                />
                <Metric
                  label="Signal → response complete"
                  value={delta(signalAt, completeAt)}
                  hint="Until the response body is read and parsed"
                  tone="text-warn"
                />
                <Metric
                  label="Signal → fulfillment event"
                  value={delta(signalAt, fillAt)}
                  hint="The marketplace result frame, not the purchase HTTP reply"
                  tone="text-signal"
                />
                <Metric
                  label="DNS lookup"
                  text={dnsText(t)}
                  value={dnsValue(t)}
                  hint={dnsHint(t)}
                  tone="text-violet"
                />
                <Metric
                  label="Connection"
                  text={socketLabel(t)}
                  hint={socketHint(t)}
                  tone="text-info"
                />
              </div>

              {bottleneck ? (
                <p className="mt-3 border-l-2 border-warn bg-warn/5 px-2.5 py-1.5 text-[12px] leading-relaxed text-muted">
                  Largest stage: <span className="text-warn">{bottleneck.label}</span>{" "}
                  <span className="font-mono tabular-nums">
                    {formatPreciseMs(bottleneck.ms)}
                  </span>
                  {span > 0 ? (
                    <span className="font-mono tabular-nums text-faint">
                      {" "}({Math.round((bottleneck.ms / span) * 100)}% of the span)
                    </span>
                  ) : null}
                </p>
              ) : null}

              {span > 0 ? (
                <div className="mt-3 flex h-2.5 w-full overflow-hidden border border-line bg-night-900">
                  {stages.map((stage) => (
                    <span
                      key={stage.key}
                      className={stage.color}
                      style={{ width: `${Math.max(0, (stage.ms / span) * 100)}%` }}
                      title={`${stage.label}: ${formatPreciseMs(stage.ms)}`}
                    />
                  ))}
                </div>
              ) : null}

              <ul className="mt-3 space-y-2">
                {stages.map((stage) => (
                  <li
                    key={stage.key}
                    className="grid grid-cols-[10px_minmax(0,1fr)_auto] items-start gap-2.5"
                  >
                    <span className={`mt-1.5 h-2.5 w-2.5 shrink-0 ${stage.color}`} aria-hidden="true" />
                    <div className="min-w-0">
                      <p className="text-[13px] leading-snug text-ink-dim">{stage.label}</p>
                      <p className="text-[12px] leading-snug text-faint">{stage.hint}</p>
                    </div>
                    <div className="text-right">
                      <p
                        className={`font-mono text-[13px] tabular-nums ${
                          bottleneck && stage.key === bottleneck.key ? "text-warn" : "text-ink-dim"
                        }`}
                      >
                        {formatPreciseMs(stage.ms)}
                      </p>
                      <p className="font-mono text-[11px] tabular-nums text-faint">
                        {span > 0 ? `${Math.round((stage.ms / span) * 100)}%` : ""}
                      </p>
                    </div>
                  </li>
                ))}
              </ul>

              <details className="mt-3 border-2 border-line/60">
                <summary className="cursor-pointer px-3 py-2 font-mono text-[11px] font-bold uppercase tracking-[0.08em] text-faint hover:text-ink-dim">
                  Raw timestamps
                </summary>
                <div className="border-t border-line/60 px-3 py-2">
                  <dl className="grid gap-x-5 gap-y-1 font-mono text-[12px] sm:grid-cols-2">
                    {rawMarks.map(([label, value]) => (
                      <div className="flex justify-between gap-3" key={label}>
                        <dt className="text-faint">{label}</dt>
                        <dd className="tabular-nums text-ink-dim">{clockMs(value)}</dd>
                      </div>
                    ))}
                  </dl>
                  <p className="mt-2 border-t border-line/40 pt-2 font-mono text-[11px] leading-relaxed text-faint">
                    Frame: {t.signalCmd || "—"} · attempt{t.attempts === 1 ? "" : "s"}:{" "}
                    {t.attempts || "—"}
                    {t.unauthorized ? " · 401 refresh" : ""}
                    {t.dnsHost ? ` · DNS ${t.dnsHost}` : ""}
                  </p>
                </div>
              </details>

              <p className="mt-3 text-[11px] leading-relaxed text-faint">
                The DNS tile is the resolver lookup this order actually paid for. A pooled
                keep-alive socket skips DNS entirely, so the tile shows no measurement and the
                Connection tile says KEEP-ALIVE. The Connection label comes from the HTTP
                client&apos;s connector: a new socket is NEW, a request served from the pool is
                KEEP-ALIVE, and a failed connection attempt is UNKNOWN. IP upstreams bypass the
                resolver, so their DNS tile is skipped, but their socket is still classified.
                Rows from the old worker keep their parallel-probe value and are labelled as
                legacy.
              </p>
            </>
          ) : loadingDetail ? (
            <div className="mt-3 border-2 border-line/60 bg-night-900/50 px-3 py-3">
              <p className="text-[13px] text-muted">Loading latency marks…</p>
            </div>
          ) : detailError ? (
            <div className="mt-3 border-2 border-line/60 bg-night-900/50 px-3 py-3">
              <p className="text-[13px] text-muted">
                Could not load this entry&apos;s telemetry from the worker. Reopen the row to
                retry.
              </p>
            </div>
          ) : (
            <div className="mt-3 border-2 border-line/60 bg-night-900/50 px-3 py-3">
              <p className="text-[13px] text-muted">
                This loot entry predates the latency debugger, so only the end-to-end number is
                available.
              </p>
              <p className="mt-1 font-mono text-[13px] tabular-nums text-ink-dim">
                Signal → fulfillment: {entry.latencyMs != null ? `${Math.round(entry.latencyMs)}ms` : "—"}
              </p>
            </div>
          )}
        </div>
      </div>

      <section className="border-2 border-line bg-night-950/50 p-3 sm:p-4">
        <h3 className="panel-title text-[13px]">
          <span className="led bg-info" /> Raw event payloads
        </h3>
        <div className="mt-3 space-y-2">
          <RawEventPayload
            label="Price change event"
            payload={rawEvents?.priceEvent}
            missingText={
              loadingDetail
                ? "Loading raw event payloads…"
                : detailError
                  ? "Could not load the raw event payloads."
                  : rawEvents
                    ? "No price change event was attached to this loot entry."
                    : "Raw event capture was not available when this loot entry was recorded."
            }
          />
          <RawEventPayload
            label="Order result event"
            payload={rawEvents?.resultEvent}
            missingText={
              loadingDetail
                ? "Loading raw event payloads…"
                : detailError
                  ? "Could not load the raw event payloads."
                  : rawEvents
                    ? "No order result event was received for this loot entry."
                    : "Raw event capture was not available when this loot entry was recorded."
            }
          />
        </div>
      </section>
    </ModalShell>
  );
}
