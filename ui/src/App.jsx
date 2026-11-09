import { lazy, memo, Suspense, useCallback, useEffect, useMemo, useRef, useState } from "react";
import TrendPanel, { TrendChip } from "./components/TrendPanel.jsx";
import { Icon } from "./components/ModalShell.jsx";
import { MonitorWindow } from "./components/MonitorWindow.jsx";
import { BackupModal } from "./components/BackupModal.jsx";
import { LootDetailModal } from "./components/LootDetailModal.jsx";
import {
  BridgeModal,
  ConfirmModal,
  KeyModal,
  SessionModal,
  TargetModal,
  copyText
} from "./components/Modals.jsx";
import { useWorker, URL_KEY, readStoredToken, isRemembered } from "./useWorker.js";
import { useToasts } from "./useToasts.js";
import { useMediaQuery, useNow } from "./lib/hooks.js";
import {
  cardState,
  fixedPrice,
  formatCountdown,
  formatDate,
  formatInt,
  formatRlt,
  formatSignedRlt,
  formatTime,
  formatUptime,
  isRecent,
  itemTypeLabel,
  itemTypeOf,
  ITEM_TYPE_ORDER,
  lastText,
  marketplaceUrl,
  matchesLootFilter,
  relTime,
  runtimeEffectiveStock,
  runtimeHot,
  runtimeReconciling,
  shortId,
  stateLabel,
  successStats,
  trimPrice,
  drawPixelAvatar,
  formatMs,
  latencyChannelLabel,
  latencyClass
} from "./lib/format.js";
import { sound } from "./lib/sound.js";
import { countTargetsByStatus, filterTargets } from "./lib/targets.js";
import { EVENT_RATE_STALE_MS, modeOf } from "./lib/constants.js";
import { ModeMenu } from "./components/ModeMenu.jsx";

/** Header chip text per trading mode. */
const MODE_CHIP = { off: "PAUSED", shadow: "SHADOW", live: "ARMED" };
const MODE_CHIP_CLASS = { off: "chip-paused", shadow: "chip-dry", live: "chip-armed" };

// Recharts is the single largest dependency and every target card mounts a
// price chart. Loading the chart bundle on demand keeps the first paint (and
// the login screen) free of it; the placeholders reserve the same space so
// nothing jumps when the chunk lands.
const EventChart = lazy(() => import("./components/EventChart.jsx"));
const PriceChart = lazy(() => import("./components/PriceChart.jsx"));

const TARGET_BATCH_MOBILE = 8;
const TARGET_BATCH_DESKTOP = 12;

function ChartSkeleton({ className = "h-[112px]" }) {
  return <div className={`border-2 border-line bg-night-950/60 ${className}`} aria-hidden="true" />;
}

function PanelSkeleton({ height = 300 }) {
  return (
    <section className="panel" aria-hidden="true">
      <div className="panel-head">
        <span className="block h-4 w-40 bg-line/60" />
      </div>
      <div className="p-4">
        <div className="border-2 border-line bg-night-950/60" style={{ height }} />
      </div>
    </section>
  );
}

/** Memoised Suspense boundary: a market tick must not redraw the activity chart. */
const MemoActivityChart = memo(function ActivityChart({ request }) {
  return (
    <Suspense fallback={<PanelSkeleton height={220} />}>
      <EventChart request={request} />
    </Suspense>
  );
});

function TargetPriceChart({ history, targetPrice, itemId, request, label }) {
  return (
    <Suspense fallback={<ChartSkeleton />}>
      <PriceChart
        history={history}
        targetPrice={targetPrice}
        itemId={itemId}
        request={request}
        label={label}
      />
    </Suspense>
  );
}

function useEventRate(count) {
  const [rate, setRate] = useState(0);
  const reference = useRef({ count: 0, at: 0 });
  useEffect(() => {
    const now = Date.now();
    const previous = reference.current;
    if (previous.at && Number.isFinite(count) && count >= previous.count) {
      const dt = (now - previous.at) / 1000;
      if (dt >= 0.4) {
        const instant = (count - previous.count) / dt;
        setRate((current) => (current ? current * 0.6 + instant * 0.4 : instant));
      }
    }
    reference.current = { count: Number(count) || 0, at: now };
  }, [count]);
  useEffect(() => {
    const timer = setInterval(() => {
      // The counter arrives in steps (one push per telemetry tick), so only
      // decay once the next step is clearly overdue.
      if (Date.now() - reference.current.at > EVENT_RATE_STALE_MS) {
        setRate((current) => (current > 0.05 ? current * 0.82 : 0));
      }
    }, 1000);
    return () => clearInterval(timer);
  }, []);
  return rate;
}

function sessionSummary(session = {}, settings = {}, worker = {}, now = Date.now()) {
  const remaining = session.expiresAt ? session.expiresAt - now : null;
  let value = "NO TOKEN";
  let sub = "PASTE A TOKEN TO GO LIVE";
  let width = 0;
  let cls = "";

  if (session.hasAccessToken) {
    if (session.lastError) {
      value = "TOKEN ERROR";
      sub = "PASTE A FRESH SESSION";
      width = 100;
      cls = "bg-danger";
    } else if (session.expiresAt && remaining <= 0) {
      value = "EXPIRED";
      sub = "PASTE A FRESH SESSION";
      width = 100;
      cls = "bg-danger";
    } else if (worker.mock && session.source === "mock") {
      value = "SIMULATED";
      sub = "MOCK TOKEN ACTIVE";
      width = 100;
      cls = "bg-signal";
    } else {
      const expiring = remaining != null && remaining <= 10 * 60_000;
      value = expiring ? "EXPIRING" : "LINKED";
      sub = remaining != null ? `REFRESH IN ${formatCountdown(remaining)}` : "AUTO REFRESH ON";
      width = remaining != null ? Math.max(2, Math.min(100, (remaining / 3_600_000) * 100)) : 100;
      cls = expiring ? "bg-warn" : "bg-signal";
    }
    if (!settings.autoRefresh && !session.lastError) {
      sub = remaining != null ? `REFRESH IN ${formatCountdown(remaining)} · AUTO OFF` : "AUTO REFRESH OFF";
    }
  }

  return { value, sub, width, cls, remaining };
}

function BootScreen({ onLogin, busy, error }) {
  const [url, setUrl] = useState(() => localStorage.getItem(URL_KEY) || window.location.origin);
  const [token, setToken] = useState(readStoredToken);
  const [remember, setRemember] = useState(isRemembered);
  const [showToken, setShowToken] = useState(false);

  const submit = (event) => {
    event.preventDefault();
    sound.ensure();
    onLogin(url, token.trim(), remember);
  };

  const perks = [
    { icon: "i-shield", tone: "border-signal/50 bg-signal/10 text-signal", text: "Nothing leaves this browser — local worker only" },
    { icon: "i-wifi", tone: "border-info/50 bg-info/10 text-info", text: "Live market feed, even when trading is off" },
    { icon: "i-bolt", tone: "border-gold/50 bg-gold/10 text-gold", text: "Price ceilings and burst protection per target" }
  ];

  return (
    <section className="relative flex min-h-dvh items-center justify-center overflow-hidden px-4 py-10 sm:px-6">
      <div className="pointer-events-none absolute inset-0" aria-hidden="true">
        <span className="sparkle absolute left-[7%] top-[16%] h-2 w-2 bg-gold" />
        <span className="sparkle absolute left-[16%] top-[70%] h-1.5 w-1.5 bg-pink [animation-delay:0.7s]" />
        <span className="sparkle absolute right-[12%] top-[24%] h-1.5 w-1.5 bg-info [animation-delay:1.2s]" />
        <span className="sparkle absolute right-[24%] bottom-[18%] h-2 w-2 bg-violet [animation-delay:0.4s]" />
        <span className="absolute left-[46%] top-[10%] h-1 w-1 bg-ink/40" />
      </div>
      <div className="relative grid w-full max-w-6xl items-center gap-10 lg:grid-cols-[1.08fr_0.92fr] lg:gap-14">
        <div className="boot-intro">
          <p className="inline-flex items-center gap-2 border-2 border-line bg-night-900 px-2 py-1 font-mono text-[12px] font-bold uppercase tracking-[0.08em] text-gold shadow-[3px_3px_0_#05030f]">
            <span className="led led-pulse bg-gold" /> Player 1 · TURU.RC
          </p>
          <div className="mt-5 flex items-start gap-4">
            <div className="hero-coin flex h-14 w-14 shrink-0 items-center justify-center border-2 border-[#05030f] bg-gold text-[#1a1000] shadow-[0_0_0_2px_var(--color-line-strong),4px_4px_0_#05030f]">
              <svg viewBox="0 0 16 16" className="h-8 w-8" aria-hidden="true"><use href="#i-coin" /></svg>
            </div>
            <h1 className="boot-title text-[clamp(2.6rem,7vw,5.4rem)]">
              MARKET<br /><span className="accent">SNIPER</span>
            </h1>
          </div>
          <p className="mt-4 max-w-md text-[14px] leading-relaxed text-muted sm:mt-6">
            A headless Rollercoin marketplace sniper. It watches the wire, prices your targets, and buys the dip while you sleep.
          </p>
          <ul className="mt-6 hidden space-y-2 sm:block">
            {perks.map((perk) => (
              <li key={perk.text} className="flex items-center gap-2.5 text-[13px] text-ink-dim">
                <span className={`flex h-8 w-8 shrink-0 items-center justify-center border-2 border-[#05030f] shadow-[2px_2px_0_#05030f] ${perk.tone}`}>
                  <Icon name={perk.icon} className="h-4 w-4" />
                </span>
                {perk.text}
              </li>
            ))}
          </ul>
          <div className="mt-7 hidden flex-wrap items-center gap-x-3 gap-y-1 font-mono text-[12px] uppercase tracking-[0.08em] text-faint sm:flex">
            <span className="text-gold">8-bit loot run</span>
            <span aria-hidden="true">■</span>
            <span>Control deck v1.0</span>
            <span aria-hidden="true">■</span>
            <span>Rollercoin marketplace</span>
          </div>
        </div>

        <div className="panel boot-intro-delay w-full overflow-hidden">
          <div className="flex items-center justify-between gap-3 border-b-2 border-line bg-night-850/70 px-4 py-3">
            <span className="panel-title"><span className="led led-pulse bg-signal" />Player login</span>
            <span className="font-mono text-[12px] font-bold uppercase tracking-[0.08em] text-gold">Insert key</span>
          </div>
          <form className="space-y-4 p-4 sm:p-5" autoComplete="off" onSubmit={submit}>
            <label className="field">
              <span>Worker URL</span>
              <input
                className="input"
                value={url}
                onChange={(event) => setUrl(event.target.value)}
                placeholder="http://127.0.0.1:8787"
                spellCheck="false"
                required
              />
            </label>
            <label className="field">
              <span>Access key</span>
              <div className="input-with-action">
                <input
                  className="input"
                  type={showToken ? "text" : "password"}
                  value={token}
                  onChange={(event) => setToken(event.target.value)}
                  placeholder="Paste access key"
                  spellCheck="false"
                  required
                />
                <button
                  className="icon-btn"
                  type="button"
                  aria-label={showToken ? "Hide access key" : "Show access key"}
                  aria-pressed={showToken}
                  onClick={() => setShowToken((value) => !value)}
                >
                  <Icon name={showToken ? "i-eye-off" : "i-eye"} />
                </button>
              </div>
            </label>
            <label className="toggle-row">
              <span>Remember on this device<small>Stay signed in after the tab is closed</small></span>
              <input
                type="checkbox"
                className="sr-only"
                checked={remember}
                onChange={(event) => setRemember(event.target.checked)}
              />
              <span className="switch" aria-hidden="true" />
            </label>
            {error ? (
              <div className="border-2 border-danger/60 bg-danger/10 px-3 py-2 font-mono text-[12px] uppercase tracking-[0.08em] text-danger shadow-[3px_3px_0_#05030f]" role="alert">
                {error}
              </div>
            ) : null}
            <button className="btn btn-primary btn-lg btn-block" type="submit" disabled={busy} data-busy={busy ? "true" : undefined} aria-busy={busy}>
              {busy ? "Connecting…" : <>Press start <Icon name="i-play" className="h-3.5 w-3.5" /></>}
            </button>
          </form>
          <div className="flex flex-wrap items-center justify-between gap-2 border-t-2 border-line bg-night-950/50 px-4 py-2.5 font-mono text-[12px] uppercase tracking-[0.08em] text-faint">
            <span className="flex items-center gap-1.5">
              <Icon name="i-shield" className="h-3 w-3 text-signal" />
              {remember ? "Key saved in this browser" : "Key kept for this tab only"}
            </span>
            <span className="text-violet">Worker v1.0</span>
          </div>
        </div>

        <p className="text-center text-[13px] text-faint lg:col-span-2">
          {remember
            ? "First run? The access key is printed in the worker terminal. Lock the deck to forget it."
            : "First run? The access key is printed in the worker terminal. It is dropped when this tab closes."}
        </p>
      </div>
    </section>
  );
}

function Header({ snapshot, status, latency, soundOn, onSound, onLogout, onSetMode, modeBusy, onMonitor }) {
  const now = useNow(1000);
  const gateway = snapshot.gateway?.status;
  let connLabel = "LINK LOST";
  let connClass = "chip-offline";
  if (status === "online") {
    if (gateway === "online") {
      connLabel = "ONLINE";
      connClass = "chip-online";
    } else if (gateway === "connecting") {
      connLabel = "SYNCING";
      connClass = "chip-warn";
    } else if (gateway === "auth-missing") {
      connLabel = "NO TOKEN";
      connClass = "chip-warn";
    } else if (gateway === "auth-error") {
      connLabel = "AUTH ERR";
      connClass = "chip-offline";
    } else {
      connLabel = "FEED DOWN";
      connClass = "chip-offline";
    }
  } else if (status === "connecting") {
    connLabel = "LINKING";
    connClass = "chip-warn";
  }

  const settings = snapshot.settings || {};
  const mode = modeOf(settings);
  const trading = MODE_CHIP[mode];
  const tradingClass = `chip ${MODE_CHIP_CLASS[mode]}`;

  return (
    <header className="deck-enter sticky top-0 z-40 border-b-2 border-line bg-night-950 pt-[env(safe-area-inset-top)]">
      <div className="mx-auto flex min-h-14 w-full max-w-[1600px] items-center gap-3 px-3 sm:px-4 lg:px-6">
        <div className="flex min-w-0 items-center gap-2.5">
          <div className="flex h-9 w-9 shrink-0 items-center justify-center border-2 border-[#05030f] bg-gold text-[#1a1000] shadow-[0_0_0_2px_var(--color-line-strong),3px_3px_0_#05030f] max-[359px]:hidden">
            <svg viewBox="0 0 16 16" className="h-5 w-5" aria-hidden="true"><use href="#i-coin" /></svg>
          </div>
          <div className="min-w-0">
            <h1 className="flex items-center gap-2 leading-none">
              <span className="brand-wordmark text-[24px] leading-none text-ink">TURU.RC</span>
              <span className="hidden border border-pink/60 bg-pink/15 px-1 py-0.5 font-mono text-[12px] font-bold text-pink sm:inline">1P</span>
              <span className="hidden truncate font-mono text-[12px] font-bold uppercase tracking-[0.08em] text-pink sm:inline">Market Sniper</span>
            </h1>
            <p className="mt-1 hidden font-mono text-[12px] uppercase tracking-[0.08em] text-faint sm:block">
              Arcade control deck
            </p>
          </div>
        </div>
        <div className="ml-auto flex items-center gap-1 sm:gap-1.5">
          <ModeMenu mode={mode} busy={modeBusy} onChange={onSetMode} />
          <span id="trading-state" className={`${tradingClass} hidden sm:inline-flex`}>
            <span className="led led-pulse" />{trading}
          </span>
          <span id="conn-chip" className={`chip ${connClass}`} title={snapshot.gateway?.lastError || "Control-plane link status"}>
            <span className="led" />{connLabel}
          </span>
          <button
            className="hidden items-center gap-1 font-mono text-[12px] text-info hover:text-ink md:inline-flex"
            type="button"
            title="Open the monitor window — order benchmark, latency and system"
            onClick={onMonitor}
          >
            <Icon name="i-wifi" className="h-3 w-3" />
            <span className="tabular-nums">{latency != null ? `${latency}ms` : "--"}</span>
          </button>
          <span className="hidden items-center gap-1 font-mono text-[12px] text-violet lg:inline-flex" title="Local time">
            <Icon name="i-clock" className="h-3 w-3" />
            <span className="tabular-nums">{formatTime(now)}</span>
          </span>
          <button
            className="icon-btn hidden h-8 w-8 sm:inline-flex"
            type="button"
            aria-label="Open monitor window"
            title="Monitor — order benchmark, latency and system"
            onClick={onMonitor}
          >
            <Icon name="i-pulse" />
          </button>
          <button
            className="icon-btn h-10 w-10 sm:h-8 sm:w-8"
            type="button"
            aria-label={soundOn ? "Mute sound" : "Enable sound"}
            aria-pressed={soundOn}
            title="Sound effects"
            onClick={onSound}
          >
            <Icon name={soundOn ? "i-sound" : "i-mute"} />
          </button>
          <button className="icon-btn icon-btn-danger h-10 w-10 sm:h-8 sm:w-8" type="button" aria-label="Lock control deck" title="Lock deck" onClick={onLogout}>
            <Icon name="i-power" />
          </button>
        </div>
      </div>
      <div className="pixel-stripe" aria-hidden="true" />
    </header>
  );
}

function Ticker({ configs, mock }) {
  return (
    <div className="deck-enter deck-enter-1 hidden border-b-2 border-line bg-night-900/70 sm:block" aria-label="Market wire">
      <div className="mx-auto flex h-9 w-full max-w-[1600px] items-center gap-3 px-3 sm:px-4 lg:px-6">
        <div className="flex shrink-0 items-center gap-1.5 font-mono text-[12px] font-bold uppercase tracking-[0.08em] text-gold">
          <span className="led bg-gold" />Market wire
        </div>
        <div className="ticker-track min-w-0 flex-1">
          <div className="ticker-content">
            {configs.length === 0 ? (
              <span className="px-4 font-mono text-[12px] uppercase tracking-[0.08em] text-faint">
                No targets deployed. The market is quiet.
              </span>
            ) : (
              configs.map((config) => {
                const runtime = config.runtime || {};
                const hot = runtimeHot(config);
                return (
                  <span className="flex shrink-0 items-center gap-2 border-r-2 border-line/70 px-3 font-mono text-[12px]" key={config.itemId}>
                    <span className="text-ink-dim">{config.label || `ITEM ${shortId(config.itemId)}`}</span>
                    <span className={hot ? "text-gold" : "text-info"}>
                      {Number.isFinite(runtime.lastPrice) ? `${fixedPrice(runtime.lastPrice)} RLT` : "—"}
                    </span>
                    <span className="text-[12px] uppercase tracking-[0.08em] text-violet">
                      {Number.isFinite(runtime.lastStock) ? `Book ${formatInt(runtime.lastStock)}` : "Waiting"}
                    </span>
                    <span className="text-line-strong" aria-hidden="true">◆</span>
                  </span>
                );
              })
            )}
          </div>
        </div>
        <div className={`chip h-5 shrink-0 max-[359px]:hidden ${mock ? "chip-warn" : "chip-online"}`}>
          {mock ? "Mock" : "Live"}
        </div>
      </div>
    </div>
  );
}

function Hud({ snapshot, eps }) {
  const now = useNow(1000);
  const stats = snapshot.stats || {};
  const configs = snapshot.configs || [];
  const gateway = snapshot.gateway || {};
  const latency = snapshot.latency || {};
  const fresh = latency.freshest || null;
  const hot = configs.filter((config) => runtimeHot(config)).length;
  const armed = configs.filter((config) => config.active).length;
  const session = sessionSummary(snapshot.session, snapshot.settings, snapshot.worker, now);
  const profit = Number(stats.profitRlt) || 0;

  return (
    <section
      className="grid grid-cols-2 gap-[3px] overflow-hidden border-2 border-line bg-line p-[3px] sm:grid-cols-3 xl:grid-cols-7"
      aria-label="Session stats"
    >
      <article data-accent="gold" className="stat-tile col-span-2 sm:col-span-1 xl:col-span-2">
        <div className="flex items-center justify-between gap-2">
          <span className="label">Profit</span>
          <Icon name="i-coin" className="stat-icon h-4 w-4 max-sm:hidden" />
        </div>
        <div className={`stat-hero mt-1 text-[26px] leading-none [text-shadow:2px_2px_0_#05030f] sm:text-[36px] ${profit > 0 ? "text-gold" : "text-ink"}`} id="stat-profit">
          {formatRlt(stats.profitRlt)}
        </div>
        <div className="mt-1.5 font-mono text-[11px] uppercase tracking-[0.08em] text-faint sm:mt-2 sm:text-[12px]" id="stat-profit-sub">
          RLT · {formatInt(stats.bought)} items looted
        </div>
      </article>
      <article data-accent="lime" className="stat-tile">
        <div className="flex items-center justify-between gap-2">
          <span className="label">Loot secured</span>
          <Icon name="i-target" className="stat-icon h-4 w-4 max-sm:hidden" />
        </div>
        <div className="mt-1.5 font-mono text-[20px] font-bold leading-none tabular-nums text-signal" id="stat-filled">
          {formatInt(stats.filled)}
        </div>
        <div className="mt-2 font-mono text-[12px] uppercase tracking-[0.08em] text-faint">
          <span id="stat-failed">{formatInt(stats.failed)}</span> missed · <span id="stat-bought">{formatInt(stats.bought)}</span> items
        </div>
      </article>
      <article data-accent="cyan" className="stat-tile">
        <div className="flex items-center justify-between gap-2">
          <span className="label">Targets</span>
          <Icon name="i-bolt" className="stat-icon h-4 w-4 max-sm:hidden" />
        </div>
        <div className="mt-1.5 font-mono text-[20px] font-bold leading-none tabular-nums text-info" id="stat-targets">
          {formatInt(configs.length)}
        </div>
        <div className="mt-2 font-mono text-[12px] uppercase tracking-[0.08em] text-faint">
          <span id="stat-armed">{formatInt(armed)}</span> armed · <span id="stat-hot">{formatInt(hot)}</span> in range
        </div>
      </article>
      <article data-accent="pink" className="stat-tile">
        <div className="flex items-center justify-between gap-2">
          <span className="label">Market events</span>
          <Icon name="i-wifi" className="stat-icon h-4 w-4 max-sm:hidden" />
        </div>
        <div className="mt-1.5 font-mono text-[20px] font-bold leading-none tabular-nums text-pink" id="stat-events">
          {formatInt(gateway.eventCount || 0)}
        </div>
        <div className="mt-2 font-mono text-[12px] uppercase tracking-[0.08em] text-faint">
          <span id="stat-eps">{eps.toFixed(1)}</span>/s · <span id="stat-uptime">{formatUptime(now - (snapshot.startedAt || now))}</span>
        </div>
      </article>
      <article data-accent="violet" className="stat-tile">
        <div className="flex items-center justify-between gap-2">
          <span className="label">Session</span>
          <Icon name="i-link" className="stat-icon h-4 w-4 max-sm:hidden" />
        </div>
        <div className={`mt-1.5 font-mono text-[14px] font-bold leading-none ${session.cls === "bg-danger" ? "text-danger" : "text-violet"}`} id="stat-session">
          {session.value}
        </div>
        <div className="meter meter-thin mt-2.5">
          <i id="stat-session-meter" className={session.cls} style={{ "--meter-value": `${session.width}%` }} />
        </div>
        <div className="mt-2 font-mono text-[12px] uppercase tracking-[0.08em] text-faint" id="stat-session-sub">
          {session.sub}
        </div>
      </article>
      <article data-accent="orange" className="stat-tile col-span-2 sm:col-span-1">
        <div className="flex items-center justify-between gap-2">
          <span className="label">Upstream</span>
          <Icon name="i-wifi" className="stat-icon h-4 w-4 max-sm:hidden" />
        </div>
        <div className={`mt-1.5 font-mono text-[20px] font-bold leading-none tabular-nums ${fresh ? latencyClass(fresh.last) : "text-muted"}`} id="stat-upstream">
          {fresh ? formatMs(fresh.last) : "—"}
        </div>
        <div
          className="mt-2 truncate font-mono text-[12px] uppercase tracking-[0.08em] text-faint"
          id="stat-upstream-sub"
          title={fresh ? `${latencyChannelLabel(fresh.channel)} · ${relTime(fresh.lastAt)}` : "No samples yet"}
        >
          {fresh ? `${latencyChannelLabel(fresh.channel)} · ${relTime(fresh.lastAt)}` : "No samples yet"}
        </div>
      </article>
    </section>
  );
}

const TargetCard = memo(function TargetCard({ config, product, onEdit, onDelete, onToggle, notify, request, compact = false }) {
  const canvasRef = useRef(null);
  const [toggling, setToggling] = useState(false);
  const [expanded, setExpanded] = useState(false);
  const runtime = config.runtime || {};
  const state = cardState(config);
  const [statePulse, setStatePulse] = useState(0);
  const previousState = useRef(state);
  useEffect(() => {
    if (previousState.current === state) return;
    previousState.current = state;
    setStatePulse((value) => value + 1);
  }, [state]);
  const stats = successStats(config);
  const productProfit = Number(product?.profitRlt) || 0;
  const productFills = Number(product?.fills) || 0;
  const productQty = Number(product?.qty) || 0;
  const productMargin = product?.marginPct;
  const label = config.label || runtime.label || `ITEM ${shortId(config.itemId)}`;
  const itemType = itemTypeOf(config);
  const hot = runtimeHot(config);
  const reconciling = runtimeReconciling(config);
  const effectiveStock = runtimeEffectiveStock(config);
  const rate = stats.rate;
  const rangeMin = Number.isFinite(runtime.allowedPriceMin) ? trimPrice(runtime.allowedPriceMin) : null;
  const rangeMax = Number.isFinite(runtime.allowedPriceMax) ? trimPrice(runtime.allowedPriceMax) : null;
  const hasRange = rangeMin !== null || rangeMax !== null;
  const targetPrice = Number(config.targetPrice);

  const marketPrice = Number.isFinite(runtime.lastPrice) ? runtime.lastPrice : null;
  const previousMarketPrice = useRef(marketPrice);
  const [marketMove, setMarketMove] = useState(null);
  useEffect(() => {
    const previous = previousMarketPrice.current;
    previousMarketPrice.current = marketPrice;
    if (marketPrice === null || previous === null || marketPrice === previous) return;
    setMarketMove((last) => ({
      dir: marketPrice > previous ? "up" : "down",
      delta: marketPrice - previous,
      tick: (last?.tick || 0) + 1
    }));
  }, [marketPrice]);

  useEffect(() => {
    drawPixelAvatar(canvasRef.current, config.itemId, config.rarityColor);
  }, [config.itemId, config.rarityColor]);

  const rateColor = rate == null ? "" : rate >= 80 ? "text-signal" : rate >= 50 ? "text-gold" : "text-danger";
  const rateBar = rate == null ? "" : rate >= 80 ? "bg-signal" : rate >= 50 ? "bg-gold" : "bg-danger";
  const lastWin = runtime.lastResult?.status === "filled" && isRecent(runtime.lastResult.at, 10_000);
  const lastError = runtime.lastResult?.status === "rejected" && isRecent(runtime.lastResult.at, 12_000);

  return (
    <article
      className={`target-card group overflow-hidden${compact ? " target-card-compact" : ""}`}
      data-state={state}
      data-type={itemType}
      data-item-id={config.itemId}
      aria-label={`${label}, ${stateLabel(state)}`}
    >
      <span
        className={`absolute inset-y-0 left-0 w-1.5 border-r-2 border-[#05030f] bg-line-strong group-data-[state=hot]:bg-gold group-data-[state=buying]:bg-info group-data-[state=filled]:bg-signal group-data-[state=error]:bg-danger group-data-[state=off]:bg-faint ${state === "hot" ? "hot-edge" : ""}`}
        aria-hidden="true"
      />

      <div className="flex items-start gap-3">
        <canvas
          ref={canvasRef}
          className="item-avatar h-12 w-12 shrink-0 bg-night-950 max-[359px]:hidden"
          width="8"
          height="8"
          aria-hidden="true"
          style={config.rarityColor ? { "--rarity": `#${config.rarityColor}` } : undefined}
        />
        <div className="min-w-0 flex-1">
          <div className="flex min-w-0 items-center gap-2">
            <h3 className="product-title line-clamp-2 min-w-0 text-[14px] leading-tight text-ink" title={label}>{label}</h3>
          </div>
          <div className="mt-1 flex flex-wrap items-center gap-2">
            <span className="type-badge shrink-0" data-type={itemType} title="Marketplace item type">{itemTypeLabel(itemType)}</span>
            <button
              className="tap-pad -my-1 inline-flex min-h-6 items-center gap-1 whitespace-nowrap py-1 font-mono text-[12px] text-muted transition-colors hover:text-gold max-sm:min-h-9"
              type="button"
              title="Copy item ID"
              onClick={async () => {
                const ok = await copyText(config.itemId);
                notify(ok ? "info" : "error", ok ? "ITEM ID COPIED" : "COPY FAILED", config.itemId);
              }}
            >
              <code>{shortId(config.itemId)}</code>
              <Icon name="i-copy" className="h-3 w-3" />
            </button>
            {reconciling ? (
              <span
                className="recon-chip shrink-0"
                title={
                  Number.isFinite(runtime.lastStock) && effectiveStock !== null
                    ? `Reconciling a recent fill: the market still shows ${formatInt(runtime.lastStock)}, but orders use the local remainder (${formatInt(effectiveStock)}) until the window closes.`
                    : "Reconciling a recent fill: orders do not trust the market stock until the window closes."
                }
              >
                RECON
              </span>
            ) : null}
          </div>
        </div>
        <div className="flex shrink-0 flex-col items-end gap-2">
          <span key={statePulse} className={`state-chip${statePulse ? " state-chip-pulse" : ""}`} data-state={state}>{stateLabel(state)}</span>
          <label className="relative -m-2 inline-flex cursor-pointer items-center p-2" title="Arm or disarm this target">
            <input
              type="checkbox"
              className="peer sr-only"
              aria-label="Arm target"
              checked={Boolean(config.active)}
              disabled={toggling}
              onChange={async (event) => {
                setToggling(true);
                try {
                  await onToggle(config, event.target.checked);
                } finally {
                  setToggling(false);
                }
              }}
            />
            <span className="switch" aria-hidden="true" />
          </label>
        </div>
      </div>

      {compact && !expanded ? (
        <div className="grid grid-cols-3 gap-[2px] border-2 border-line bg-line">
          <div className="bg-night-900/70 px-2 py-1.5">
            <span className="label">Market</span>
            <p className={`mt-0.5 font-mono text-[15px] font-bold leading-none tabular-nums ${hot ? "text-gold" : "text-info"}`}>
              {Number.isFinite(runtime.lastPrice) ? fixedPrice(runtime.lastPrice) : "—"}
            </p>
          </div>
          <div className="bg-night-900/70 px-2 py-1.5">
            <span className="label">Target</span>
            <p className="mt-0.5 font-mono text-[15px] font-bold leading-none tabular-nums text-gold">
              {fixedPrice(targetPrice)}
            </p>
          </div>
          <div className="bg-night-900/70 px-2 py-1.5">
            <span className="label">Profit</span>
            <p className={`mt-0.5 font-mono text-[15px] font-bold leading-none tabular-nums ${profitClass(productProfit)}`}>
              {formatSignedRlt(productProfit)}
            </p>
          </div>
        </div>
      ) : null}

      {compact ? (
        <button
          className="target-expand"
          type="button"
          aria-expanded={expanded}
          aria-label={expanded ? `Hide details for ${label}` : `Show details for ${label}`}
          onClick={() => setExpanded((value) => !value)}
        >
          <span>{expanded ? "Hide details" : "Price chart, range and fills"}</span>
          <Icon name="i-chevron" className={`h-3 w-3 transition-transform${expanded ? " rotate-90" : ""}`} />
        </button>
      ) : null}

      {!compact || expanded ? (
        <>
      <TrendChip trend={runtime.trend} />

      <div className="grid grid-cols-2 gap-[2px] border-2 border-line bg-line">
        <div className="bg-info/5 px-2.5 py-2 sm:px-3">
          <div
            className="flex items-center gap-1"
            title={
              marketMove
                ? `Last frame moved ${marketMove.dir === "up" ? "up" : "down"} ${fixedPrice(Math.abs(marketMove.delta))} RLT.`
                : "Waiting for this item's next market frame."
            }
          >
            <span className="label">Market</span>
            {marketMove ? (
              <span key={marketMove.tick} className="price-tick" data-dir={marketMove.dir}>
                <Icon name={marketMove.dir === "up" ? "i-caret-up" : "i-caret-down"} className="h-2.5 w-2.5" />
                <span className="sr-only">{`Price ${marketMove.dir} ${fixedPrice(Math.abs(marketMove.delta))} since the previous frame.`}</span>
              </span>
            ) : null}
          </div>
          <div className="mt-0.5 flex items-baseline gap-1">
            <strong className={`font-mono text-[18px] font-bold leading-none tabular-nums min-[360px]:text-[20px] ${hot ? "text-gold" : "text-info"}`}>
              {Number.isFinite(runtime.lastPrice) ? fixedPrice(runtime.lastPrice) : "—"}
            </strong>
            <span className="font-mono text-[12px] text-faint">RLT</span>
            {runtime.stale && Number.isFinite(runtime.seenAt) ? (
              <span
                className="ml-auto font-mono text-[10px] uppercase tracking-[0.08em] text-faint"
                title={`Cached snapshot from ${relTime(runtime.seenAt)} — waiting for a live market frame`}
              >
                cached
              </span>
            ) : null}
          </div>
        </div>
        <div
          className="bg-gold/5 px-2.5 py-2 sm:px-3"
          title="Maximum price per item. Orders are sent only when the cheapest listing is at or below it."
        >
          <div className="flex items-center justify-between gap-1">
            <span className="label">Target</span>
          </div>
          <div className="mt-0.5 flex items-baseline gap-1">
            <strong className="target-price font-mono text-[18px] font-bold leading-none tabular-nums text-gold min-[360px]:text-[20px]">
              {fixedPrice(targetPrice)}
            </strong>
            <span className="font-mono text-[12px] text-faint">RLT</span>
          </div>
        </div>
        <div
          className="col-span-2 flex items-center justify-between gap-3 bg-gold/5 px-2.5 py-2 sm:px-3"
          title="All-time profit for this product: (target price − buy price) × qty. Persisted across worker restarts and loot-log rotation."
        >
          <div className="min-w-0">
            <span className="label">Profit · all time</span>
            <div className="mt-0.5 flex items-baseline gap-1">
              <strong className={`font-mono text-[18px] font-bold leading-none tabular-nums min-[360px]:text-[20px] ${profitClass(productProfit)}`}>
                {formatSignedRlt(productProfit)}
              </strong>
              <span className="font-mono text-[12px] text-faint">RLT</span>
            </div>
          </div>
          <div className="shrink-0 text-right font-mono text-[12px] uppercase leading-relaxed tracking-[0.08em] text-faint">
            <p>{formatInt(productFills)} fills · {formatInt(productQty)} qty</p>
            <p className="mt-0.5">{percentText(productMargin)} margin</p>
          </div>
        </div>
      </div>

      <div
        className="flex flex-wrap items-center justify-between gap-x-2 gap-y-1 border-2 border-dashed border-line/80 px-2.5 py-1.5"
        title={
          runtime.allowedAt
            ? `Allowed price band from the marketplace frame · updated ${relTime(runtime.allowedAt)}`
            : "The worker has not received an allowed price band for this item yet."
        }
      >
        <span className="label">Allowed range</span>
        {hasRange ? (
          <span className="flex items-baseline gap-1 font-mono text-[12px] tabular-nums text-ink-dim">
            <b className="text-info">{rangeMin ?? "—"}</b>
            <span className="text-faint">–</span>
            <b className="text-pink">{rangeMax ?? "—"}</b>
            <span className="text-[12px] uppercase tracking-wide text-faint">RLT</span>
          </span>
        ) : (
          <span className="font-mono text-[12px] uppercase tracking-wide text-faint">Waiting for frame</span>
        )}
      </div>

      <TargetPriceChart
        history={runtime.history || []}
        targetPrice={targetPrice}
        itemId={config.itemId}
        request={request}
        label={label}
      />

      <div
        className="space-y-1.5"
        title="Successful fills divided by completed purchase attempts (fills + failures) for this target in this worker session."
      >
        <div className="flex items-center justify-between gap-2">
          <span className="label">Purchase success</span>
          <span className={`font-mono text-[13px] font-bold tabular-nums ${rateColor}`}>
            {rate == null ? "—" : `${rate}%`}
          </span>
        </div>
        <div className="meter">
          <i className={rateBar} style={{ "--meter-value": `${rate ?? 0}%` }} />
        </div>
        <div className="flex items-center justify-between gap-2 font-mono text-[12px] uppercase tracking-[0.08em] text-faint">
          <span>{stats.completed ? `${formatInt(stats.fills)} filled · ${formatInt(stats.failures)} failed` : "No completed orders"}</span>
          <span className="tabular-nums">{stats.fills}/{stats.completed}</span>
        </div>
      </div>

      <div className="flex flex-wrap gap-1.5">
        <span className="tag">QTY <b>{formatInt(config.desiredQty)}</b></span>
        <span className="tag">HIGH <b>{formatInt(config.highStockThreshold)}</b></span>
        <span className="tag">MIN <b>{formatInt(config.minAcceptableQty)}</b></span>
        <span className="tag">MAX <b>{fixedPrice(targetPrice * config.desiredQty * 2)}</b></span>
      </div>

      <div className="mt-auto flex items-end justify-between gap-3 border-t-2 border-line pt-2.5">
        <span
          className={`min-w-0 truncate font-mono text-[12px] uppercase tracking-[0.08em] ${
            lastWin ? "text-signal" : lastError ? "text-danger" : "text-faint"
          }`}
        >
          {lastText(config)}
        </span>
        <div className="flex shrink-0 items-center gap-1">
          <a
            className="icon-btn h-10 w-10 sm:h-8 sm:w-8"
            href={marketplaceUrl(itemType, config.itemId)}
            target="_blank"
            rel="noopener noreferrer"
            title="Open on Rollercoin marketplace"
            aria-label={`Open ${label} on the Rollercoin marketplace`}
          >
            <Icon name="i-external" className="h-3.5 w-3.5" />
          </a>
          <button className="icon-btn h-10 w-10 sm:h-8 sm:w-8" type="button" aria-label="Edit target" title="Edit target" onClick={() => onEdit(config)}>
            <Icon name="i-edit" className="h-3.5 w-3.5" />
          </button>
          <button className="icon-btn icon-btn-danger h-10 w-10 sm:h-8 sm:w-8" type="button" aria-label="Delete target" title="Delete target" onClick={() => onDelete(config)}>
            <Icon name="i-trash" className="h-3.5 w-3.5" />
          </button>
        </div>
      </div>
        </>
      ) : null}
    </article>
  );
});

function TargetsPanel({ configs, reportByItem, search, typeFilter, statusFilter, onSearch, onTypeFilter, onStatusFilter, onAdd, onEdit, onDelete, onToggle, notify, request }) {
  const compact = useMediaQuery("(max-width: 1023px)");
  const batch = compact ? TARGET_BATCH_MOBILE : TARGET_BATCH_DESKTOP;
  const [limit, setLimit] = useState(batch);
  // Searching or switching a filter restarts from the top of the list.
  useEffect(() => {
    setLimit(batch);
  }, [batch, search, typeFilter, statusFilter]);

  const hasQuery = search.trim() !== "";
  const filtered = filterTargets(configs, { search, typeFilter, statusFilter });
  const counts = configs.reduce((tally, config) => {
    const type = itemTypeOf(config);
    tally[type] = (tally[type] || 0) + 1;
    return tally;
  }, {});
  const statusCounts = countTargetsByStatus(configs);
  const visible = filtered.slice(0, limit);
  const hidden = filtered.length - visible.length;
  const statusWord =
    statusFilter === "active" ? "active " : statusFilter === "inactive" ? "inactive " : "";
  // Empty states name the filter that emptied the list, so "no matches" never
  // leaves the operator guessing whether the fleet is empty or just filtered.
  const statusOnly = !hasQuery && typeFilter === "all";
  const emptyTitle =
    statusOnly && statusFilter === "active"
      ? "No active targets"
      : statusOnly && statusFilter === "inactive"
        ? "No inactive targets"
        : "No matches";
  const emptyNote = hasQuery
    ? `Nothing matches “${search}”. Try another label or item ID.`
    : typeFilter !== "all"
      ? `No ${statusWord}targets of type “${itemTypeLabel(typeFilter)}” are deployed.`
      : statusFilter === "active"
        ? "Every target is parked — arm one with its toggle to put it back to work."
        : statusFilter === "inactive"
          ? "Every target is armed — flip a card's toggle to park it here."
          : "No targets match the current filters.";


  return (
    <section className="panel min-w-0 max-lg:order-1 lg:col-start-1 lg:row-start-3" data-accent="cyan">
      <div className="panel-head flex-wrap gap-y-2">
        <h2 className="panel-title">
          <Icon name="i-target" className="h-4 w-4 text-info" />
          Target fleet
          <span className="border-2 border-[#05030f] bg-info px-1.5 py-0.5 font-mono text-[12px] font-bold text-[#001a24]">
            {configs.length}
          </span>
        </h2>
        <div className="ml-auto flex w-full items-center gap-2 sm:w-auto">
          <div className="relative min-w-0 flex-1 sm:w-52 sm:flex-none">
            <Icon name="i-search" className="pointer-events-none absolute left-2.5 top-1/2 h-3.5 w-3.5 -translate-y-1/2 text-faint" />
            <input
              className="input input-sm pl-8"
              type="search"
              value={search}
              onChange={(event) => onSearch(event.target.value)}
              placeholder="Search"
              aria-label="Search targets"
            />
          </div>
          <button className="btn btn-primary btn-sm" type="button" onClick={onAdd}>
            <Icon name="i-plus" className="h-3 w-3" /> Add target
          </button>
        </div>
      </div>

      {configs.length > 0 ? (
        <>
          <div
            className="type-filter-row flex flex-nowrap items-center gap-1.5 border-b-2 border-line px-4 py-2.5 sm:flex-wrap"
            role="group"
            aria-label="Filter targets by status"
          >
            <span className="label mr-1">Status</span>
            <button
              className="type-filter"
              data-status="all"
              type="button"
              title="Show every target, armed or parked"
              aria-pressed={statusFilter === "all"}
              onClick={() => onStatusFilter("all")}
            >
              all <span className="tabular-nums opacity-60">{formatInt(statusCounts.all)}</span>
            </button>
            <button
              className="type-filter"
              data-status="active"
              type="button"
              title="Armed targets: the worker hunts the market for them"
              aria-pressed={statusFilter === "active"}
              onClick={() => onStatusFilter(statusFilter === "active" ? "all" : "active")}
            >
              active <span className="tabular-nums opacity-60">{formatInt(statusCounts.active)}</span>
            </button>
            <button
              className="type-filter"
              data-status="inactive"
              type="button"
              title="Parked targets: disarmed, so the worker skips them"
              aria-pressed={statusFilter === "inactive"}
              onClick={() => onStatusFilter(statusFilter === "inactive" ? "all" : "inactive")}
            >
              inactive <span className="tabular-nums opacity-60">{formatInt(statusCounts.inactive)}</span>
            </button>
          </div>
          <div className="type-filter-row flex flex-nowrap items-center gap-1.5 border-b-2 border-line px-4 py-2.5 sm:flex-wrap" role="group" aria-label="Filter targets by item type">
            <span className="label mr-1">Type</span>
            <button
              className="type-filter"
              data-type="all"
              type="button"
              aria-pressed={typeFilter === "all"}
              onClick={() => onTypeFilter("all")}
            >
              all <span className="tabular-nums opacity-60">{configs.length}</span>
            </button>
            {ITEM_TYPE_ORDER.map((type) => (
              <button
                key={type}
                className="type-filter"
                data-type={type}
                type="button"
                aria-pressed={typeFilter === type}
                onClick={() => onTypeFilter(typeFilter === type ? "all" : type)}
              >
                {itemTypeLabel(type)} <span className="tabular-nums opacity-60">{counts[type] || 0}</span>
              </button>
            ))}
          </div>
        </>
      ) : null}

      {configs.length === 0 ? (
        <div className="flex flex-col items-center justify-center gap-3 px-6 py-14 text-center">
          <span className="flex h-14 w-14 items-center justify-center border-2 border-[#05030f] bg-night-850 text-cyan shadow-[0_0_0_2px_var(--color-line-strong),4px_4px_0_#05030f]">
            <Icon name="i-target" className="h-7 w-7" />
          </span>
          <div>
            <p className="font-mono text-[13px] font-semibold uppercase tracking-[0.14em] text-ink-dim">No targets deployed</p>
            <p className="mx-auto mt-1 max-w-sm text-[13px] leading-relaxed text-faint">
              Add an item ID, set your price ceiling, and the worker will hunt the market for you.
            </p>
          </div>
          <button className="btn btn-primary btn-sm" type="button" onClick={onAdd}>
            <Icon name="i-plus" className="h-3 w-3" /> Deploy first target
          </button>
        </div>
      ) : (
        <div className="grid grid-cols-1 gap-4 p-4 xl:grid-cols-2">
          {filtered.length === 0 ? (
            <div className="col-span-full flex flex-col items-center gap-2 py-10 text-center">
              <p className="font-mono text-[13px] font-semibold uppercase tracking-[0.14em] text-ink-dim">{emptyTitle}</p>
              <p className="max-w-sm text-[13px] leading-relaxed text-faint">{emptyNote}</p>
            </div>
          ) : (
            visible.map((config) => (
              <TargetCard
                key={config.itemId}
                config={config}
                product={reportByItem?.[String(config.itemId).toLowerCase()] || null}
                onEdit={onEdit}
                onDelete={onDelete}
                onToggle={onToggle}
                notify={notify}
                request={request}
                compact={compact}
              />
            ))
          )}
        </div>
      )}
      {filtered.length > 0 ? (
        <div className="flex flex-wrap items-center justify-between gap-2 border-t-2 border-line bg-night-900/70 px-4 py-3">
          <span className="font-mono text-[12px] font-semibold uppercase tracking-[0.08em] text-ink-dim">
            Showing {formatInt(visible.length)} of {formatInt(filtered.length)}
          </span>
          {hidden > 0 ? (
            <button
              className="btn btn-signal btn-sm font-bold"
              type="button"
              onClick={() => setLimit((value) => value + (compact ? TARGET_BATCH_MOBILE : TARGET_BATCH_DESKTOP))}
            >
              Show {formatInt(Math.min(hidden, compact ? TARGET_BATCH_MOBILE : TARGET_BATCH_DESKTOP))} more
              <Icon name="i-arrow-down" className="h-3.5 w-3.5" />
            </button>
          ) : null}
        </div>
      ) : null}
    </section>
  );
}

function csvCell(value) {
  const text = String(value ?? "");
  return /[",\r\n]/.test(text) ? `"${text.replace(/"/g, '""')}"` : text;
}

function downloadCsv(filename, rows) {
  const csv = rows.map((row) => row.map(csvCell).join(",")).join("\r\n");
  const blob = new Blob([`\uFEFF${csv}`], { type: "text/csv;charset=utf-8" });
  const url = URL.createObjectURL(blob);
  const link = document.createElement("a");
  link.href = url;
  link.download = filename;
  document.body.appendChild(link);
  link.click();
  link.remove();
  setTimeout(() => URL.revokeObjectURL(url), 1000);
}

function profitClass(value) {
  const number = Number(value) || 0;
  return number > 0 ? "text-gold" : number < 0 ? "text-danger" : "text-ink-dim";
}

function percentText(value, digits = 1) {
  const number = Number(value);
  return value != null && Number.isFinite(number) ? `${number.toFixed(digits)}%` : "—";
}

function ReportPanel({ report, notify }) {
  const items = report?.items || [];
  const totals = report?.totals || null;
  const since = Number(report?.since);

  const exportCsv = () => {
    if (items.length === 0) return;
    const rows = [
      [
        "product",
        "item_id",
        "fills",
        "failed",
        "qty",
        "spent_rlt",
        "avg_buy_rlt",
        "profit_rlt",
        "margin_pct",
        "success_pct",
        "first_seen",
        "last_seen"
      ],
      ...items.map((item) => [
        item.label || item.itemId,
        item.itemId,
        item.fills,
        item.failed,
        item.qty,
        item.spentRlt,
        item.avgBuy,
        item.profitRlt,
        item.marginPct ?? "",
        item.successPct ?? "",
        item.firstAt ? new Date(item.firstAt).toISOString() : "",
        item.lastAt ? new Date(item.lastAt).toISOString() : ""
      ])
    ];
    downloadCsv(`turu-profit-by-product-${new Date().toISOString().slice(0, 10)}.csv`, rows);
    notify?.("success", "REPORT EXPORTED", `${items.length} products written to CSV.`);
  };

  return (
    <section className="panel min-w-0 max-lg:order-3 lg:col-start-1 lg:row-start-2" data-accent="gold">
      <div className="panel-head flex-wrap gap-y-2">
        <h2 className="panel-title">
          <Icon name="i-coin" className="h-4 w-4 text-gold" /> Profit by product
          <span className="border-2 border-[#05030f] bg-violet px-1.5 py-0.5 font-mono text-[12px] font-bold text-[#16002e]">{formatInt(items.length)}</span>
        </h2>
        <div className="flex items-center gap-2">
          <span className="font-mono text-[13px] uppercase tracking-[0.1em] text-faint">
            All time{Number.isFinite(since) && since > 0 ? ` · since ${formatDate(since)}` : ""}
          </span>
          <button
            className="btn btn-ghost btn-sm"
            type="button"
            onClick={exportCsv}
            disabled={items.length === 0}
            title="Download every product row as CSV"
          >
            <Icon name="i-download" className="h-3 w-3" /> CSV
          </button>
        </div>
      </div>

      {items.length === 0 ? (
        <div className="flex flex-col items-center justify-center gap-2 px-6 py-12 text-center">
          <Icon name="i-coin" className="h-6 w-6 text-faint" />
          <p className="font-mono text-[13px] uppercase tracking-[0.14em] text-ink-dim">No product data yet</p>
          <p className="text-[13px] text-faint">Filled and missed orders are aggregated here, per product, all time.</p>
        </div>
      ) : (
        <>
          <div className="grid grid-cols-2 gap-[2px] border-b-2 border-line bg-line sm:grid-cols-4">
            <div className="stat-tile" data-accent="gold">
              <span className="label">Total profit</span>
              <p className={`mt-1 font-mono text-[20px] font-semibold leading-none tabular-nums ${profitClass(totals?.profitRlt)}`}>
                {formatSignedRlt(totals?.profitRlt ?? 0)}
              </p>
              <p className="mt-1.5 font-mono text-[13px] uppercase tracking-[0.1em] text-faint">RLT · all products</p>
            </div>
            <div className="stat-tile" data-accent="cyan">
              <span className="label">Spend</span>
              <p className="mt-1 font-mono text-[20px] font-semibold leading-none tabular-nums text-info">
                {formatRlt(totals?.spentRlt ?? 0)}
              </p>
              <p className="mt-1.5 font-mono text-[13px] uppercase tracking-[0.1em] text-faint">
                RLT · {formatInt(totals?.qty ?? 0)} items
              </p>
            </div>
            <div className="stat-tile" data-accent="lime">
              <span className="label">Margin</span>
              <p className={`mt-1 font-mono text-[20px] font-semibold leading-none tabular-nums ${profitClass(totals?.marginPct)}`}>
                {percentText(totals?.marginPct)}
              </p>
              <p className="mt-1.5 font-mono text-[13px] uppercase tracking-[0.1em] text-faint">Profit ÷ spend</p>
            </div>
            <div className="stat-tile" data-accent="pink">
              <span className="label">Fills / missed</span>
              <p className="mt-1 font-mono text-[20px] font-semibold leading-none tabular-nums text-pink">
                {formatInt(totals?.fills ?? 0)}
                <span className="text-faint"> / {formatInt(totals?.failed ?? 0)}</span>
              </p>
              <p className="mt-1.5 font-mono text-[13px] uppercase tracking-[0.1em] text-faint">Confirmed / missed</p>
            </div>
          </div>

          <ul className="report-scroll divide-y divide-line/40 xl:hidden">
            {items.map((item) => (
              <li className="px-4 py-3" key={item.itemId || item.label}>
                <div className="flex items-start justify-between gap-3">
                  <div className="min-w-0">
                    <p className="truncate text-[13px] text-ink">{item.label || shortId(item.itemId)}</p>
                    <p className="mt-0.5 font-mono text-[13px] uppercase tracking-[0.08em] text-faint">
                      {formatInt(item.fills)} filled · {formatInt(item.failed)} missed · {formatInt(item.qty)} qty
                    </p>
                  </div>
                  <div className="shrink-0 text-right">
                    <p className={`font-mono text-[13px] tabular-nums ${profitClass(item.profitRlt)}`}>
                      {formatSignedRlt(item.profitRlt)}
                    </p>
                    <p className="mt-0.5 font-mono text-[13px] uppercase tracking-[0.08em] text-faint">
                      {percentText(item.marginPct)} margin
                    </p>
                  </div>
                </div>
                <div className="mt-2 flex items-center justify-between gap-3 font-mono text-[13px] uppercase tracking-[0.08em] text-faint">
                  <span>
                    Spend {formatRlt(item.spentRlt)} · Avg {fixedPrice(item.avgBuy)}
                  </span>
                  <span className="shrink-0">{item.successPct == null ? "—" : `${formatInt(item.successPct)}%`} win</span>
                </div>
              </li>
            ))}
          </ul>

          <div className="report-scroll hidden min-w-0 xl:block">
            <table className="w-full min-w-[880px] border-collapse text-left font-mono text-[13px] tabular-nums">
              <thead className="sticky top-0 z-10 bg-night-900">
                <tr className="border-b-2 border-line text-[12px] uppercase tracking-[0.08em] text-faint">
                  <th scope="col" className="px-4 py-2 font-medium">Product</th>
                  <th scope="col" className="px-3 py-2 font-medium">Fills</th>
                  <th scope="col" className="px-3 py-2 font-medium">Qty</th>
                  <th scope="col" className="px-3 py-2 font-medium">Spend</th>
                  <th scope="col" className="px-3 py-2 font-medium">Avg buy</th>
                  <th scope="col" className="px-3 py-2 font-medium">Profit</th>
                  <th scope="col" className="px-3 py-2 font-medium">Margin</th>
                  <th scope="col" className="px-3 py-2 font-medium">Success</th>
                  <th scope="col" className="px-4 py-2 font-medium">Last</th>
                </tr>
              </thead>
              <tbody className="divide-y divide-line/40">
                {items.map((item) => (
                  <tr
                    className="transition-colors hover:bg-night-850/60"
                    key={item.itemId || item.label}
                    title={item.itemId ? `item ${item.itemId}` : undefined}
                  >
                    <td className="max-w-[240px] truncate px-4 py-2 text-ink-dim">{item.label || shortId(item.itemId)}</td>
                    <td className="px-3 py-2">
                      <span className="text-signal">{formatInt(item.fills)}</span>
                      <span className="text-faint"> / {formatInt(item.failed)}</span>
                    </td>
                    <td className="px-3 py-2 text-ink-dim">{formatInt(item.qty)}</td>
                    <td className="px-3 py-2 text-ink-dim">{formatRlt(item.spentRlt)}</td>
                    <td className="px-3 py-2 text-ink-dim">{fixedPrice(item.avgBuy)}</td>
                    <td className={`px-3 py-2 ${profitClass(item.profitRlt)}`}>{formatSignedRlt(item.profitRlt)}</td>
                    <td className={`px-3 py-2 ${profitClass(item.marginPct)}`}>{percentText(item.marginPct)}</td>
                    <td className="px-3 py-2 text-muted">{item.successPct == null ? "—" : `${formatInt(item.successPct)}%`}</td>
                    <td className="whitespace-nowrap px-4 py-2 text-faint">
                      <span title={item.lastAt ? new Date(item.lastAt).toLocaleString() : undefined}>
                        {item.lastAt ? relTime(item.lastAt) : "—"}
                      </span>
                    </td>
                  </tr>
                ))}
              </tbody>
              <tfoot>
                <tr className="border-t-2 border-line text-[12px]">
                  <td className="px-4 py-2.5 uppercase tracking-[0.12em] text-faint">Total</td>
                  <td className="px-3 py-2.5 text-ink-dim">
                    {formatInt(totals?.fills ?? 0)}
                    <span className="text-faint"> / {formatInt(totals?.failed ?? 0)}</span>
                  </td>
                  <td className="px-3 py-2.5 text-ink-dim">{formatInt(totals?.qty ?? 0)}</td>
                  <td className="px-3 py-2.5 text-ink-dim">{formatRlt(totals?.spentRlt ?? 0)}</td>
                  <td className="px-3 py-2.5 text-faint">—</td>
                  <td className={`px-3 py-2.5 font-semibold ${profitClass(totals?.profitRlt)}`}>
                    {formatSignedRlt(totals?.profitRlt ?? 0)}
                  </td>
                  <td className={`px-3 py-2.5 ${profitClass(totals?.marginPct)}`}>{percentText(totals?.marginPct)}</td>
                  <td className="px-3 py-2.5 text-faint">—</td>
                  <td className="px-4 py-2.5 text-faint">—</td>
                </tr>
              </tfoot>
            </table>
          </div>
        </>
      )}
    </section>
  );
}

const LOOT_PAGE_SIZE = 50;
const LOOT_MAX_ROWS = 500;
const LOOT_SEARCH_DEBOUNCE_MS = 250;

function LootLog({ history, historyTotal, historyRevision, onReset, request, onMonitor, recentAvg, recentOrders }) {
  const [draft, setDraft] = useState("");
  const [query, setQuery] = useState("");
  const [statusFilter, setStatusFilter] = useState("");
  const [profitFilter, setProfitFilter] = useState("");
  // The unfiltered live view: while no filter/search is active we can prepend
  // socket entries and keep paging from the optimistic snapshot. Any active
  // filter forces a server query, like search does.
  const filtered = Boolean(query || statusFilter || profitFilter);
  const [entries, setEntries] = useState([]);
  const [total, setTotal] = useState(Number(historyTotal) || 0);
  const [hasMore, setHasMore] = useState(false);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState("");
  const [detail, setDetail] = useState(null);
  const idsRef = useRef(new Set());
  // The mobile list and the desktop table are the same data: render only the
  // one that fits, instead of mounting both and hiding one with CSS.
  const wide = useMediaQuery("(min-width: 1280px)");

  // Typing updates the box immediately; the server query is debounced so a
  // search over the permanent log is not re-run on every keystroke.
  useEffect(() => {
    const timer = setTimeout(() => setQuery(draft.trim()), LOOT_SEARCH_DEBOUNCE_MS);
    return () => clearTimeout(timer);
  }, [draft]);

  // Leaving filtered mode always falls back to the newest live page.
  useEffect(() => {
    if (filtered) return;
    const seed = (Array.isArray(history) ? history : []).filter(Boolean);
    idsRef.current = new Set(seed.map((entry) => entry.id));
    setEntries(seed);
    setError("");
    setLoading(false);
    setHasMore(idsRef.current.size < (Number(historyTotal) || 0));
  }, [filtered]);

  // A reset or backup restore replaces the permanent history. Re-seed this
  // locally paged view from the optimistic snapshot as soon as that mutation
  // starts, and again if it rolls back.
  useEffect(() => {
    const seed = (Array.isArray(history) ? history : []).filter(Boolean);
    idsRef.current = new Set(seed.map((entry) => entry.id));
    setDraft("");
    setQuery("");
    setStatusFilter("");
    setProfitFilter("");
    setEntries(seed);
    setTotal(Number(historyTotal) || 0);
    setHasMore(idsRef.current.size < (Number(historyTotal) || 0));
    setLoading(false);
    setError("");
  }, [historyRevision]);

  // Keep the counter in sync with the worker (new fills bump it; a reset
  // drops it to zero and clears whatever was loaded, search included).
  // While filtered, `total` is the filtered match count: unfiltered bumps
  // are handled by the live-prepend effect above, so only a reset to zero
  // needs the clear here.
  useEffect(() => {
    const nextTotal = Number(historyTotal);
    if (!Number.isFinite(nextTotal)) return;
    if (filtered) {
      if (nextTotal !== 0) return;
      idsRef.current = new Set();
      setEntries([]);
      setTotal(0);
      setHasMore(false);
      setError("");
      return;
    }
    setTotal(nextTotal);
    if (nextTotal === 0 && idsRef.current.size > 0) {
      idsRef.current = new Set();
      setEntries([]);
      setHasMore(false);
      setError("");
      return;
    }
    setHasMore(idsRef.current.size < nextTotal);
  }, [historyTotal, filtered]);

  // Live socket entries are prepended so the newest loot stays on screen even
  // while older pages are loaded below it. While a filter/search is active
  // only entries that pass the filter are prepended (mirroring the server
  // query), and the match count grows with each prepend — otherwise the
  // filtered view would freeze until the user re-ran the search.
  useEffect(() => {
    const list = Array.isArray(history) ? history : [];
    if (list.length === 0) return;
    const fresh = list.filter(
      (entry) =>
        entry &&
        !idsRef.current.has(entry.id) &&
        (!filtered ||
          matchesLootFilter(entry, {
            q: query,
            status: statusFilter,
            profit: profitFilter
          }))
    );
    for (const entry of fresh) idsRef.current.add(entry.id);
    const latestById = new Map(list.filter(Boolean).map((entry) => [entry.id, entry]));
    setEntries((previous) => {
      const updated = previous.map((entry) => {
        const latest = latestById.get(entry.id);
        return latest ? { ...entry, ...latest } : entry;
      });
      return [...fresh, ...updated].slice(0, LOOT_MAX_ROWS);
    });
    if (filtered && fresh.length > 0) {
      setTotal((previous) => previous + fresh.length);
    }
  }, [history, filtered, query, statusFilter, profitFilter]);

  // Server-side search/filter over the whole permanent log (product name / item
  // id, status, profit sign). Runs whenever the debounced search text or either
  // filter select changes.
  useEffect(() => {
    if (!filtered) return;
    let cancelled = false;
    idsRef.current = new Set();
    setEntries([]);
    setTotal(0);
    setHasMore(false);
    setError("");
    setLoading(true);
    const params = new URLSearchParams({ limit: String(LOOT_PAGE_SIZE) });
    if (query) params.set("q", query);
    if (statusFilter) params.set("status", statusFilter);
    if (profitFilter) params.set("profit", profitFilter);
    request(`/api/history?${params.toString()}`)
      .then((data) => {
        if (cancelled) return;
        const list = (Array.isArray(data.entries) ? data.entries : []).filter(Boolean);
        for (const entry of list) idsRef.current.add(entry.id);
        setEntries(list.slice(0, LOOT_MAX_ROWS));
        setTotal(Number(data.total) || 0);
        setHasMore(Boolean(data.hasMore));
      })
      .catch((failure) => {
        if (!cancelled) setError(failure?.message || "Search failed.");
      })
      .finally(() => {
        if (!cancelled) setLoading(false);
      });
    return () => {
      cancelled = true;
    };
  }, [filtered, query, statusFilter, profitFilter, request]);

  const loadOlder = useCallback(async () => {
    if (loading || !hasMore) return;
    const oldest = entries[entries.length - 1];
    const params = new URLSearchParams({ limit: String(LOOT_PAGE_SIZE) });
    if (query) params.set("q", query);
    if (statusFilter) params.set("status", statusFilter);
    if (profitFilter) params.set("profit", profitFilter);
    if (oldest) {
      params.set("beforeAt", String(oldest.at));
      params.set("beforeId", String(oldest.id));
    }
    setLoading(true);
    setError("");
    try {
      const data = await request(`/api/history?${params.toString()}`);
      const list = (Array.isArray(data.entries) ? data.entries : []).filter(
        (entry) => entry && !idsRef.current.has(entry.id)
      );
      for (const entry of list) idsRef.current.add(entry.id);
      // Older pages are user-requested, so let them accumulate instead of
      // trimming rows the user just asked for.
      setEntries((previous) => [...previous, ...list]);
      setTotal(Number(data.total) || 0);
      setHasMore(Boolean(data.hasMore));
    } catch (failure) {
      setError(failure?.message || "Could not load older loot.");
    } finally {
      setLoading(false);
    }
  }, [entries, hasMore, loading, query, statusFilter, profitFilter, request]);

  const resetLog = async () => {
    const didReset = await onReset();
    if (didReset === false) return;
    setDraft("");
    setQuery("");
    setStatusFilter("");
    setProfitFilter("");
    idsRef.current = new Set();
    setEntries([]);
    setTotal(0);
    setHasMore(false);
    setError("");
  };

  const shown = entries.length;
  const trimmed = shown < total;
  const emptyTitle = error ? "Loot log unavailable" : filtered ? "No matching loot" : "No loot yet";
  const emptyHint = error
    ? error
    : filtered
      ? "Nothing in the permanent log matches the current search/filter."
      : "Confirmed fills will drop here with profit and latency.";
  const latestDetail = detail
    ? entries.find((entry) => entry.id === detail.id) || detail
    : null;
  // `Number(null)` is 0, which used to turn "no data" into a fake 0ms.
  const avg =
    recentAvg == null || !Number.isFinite(Number(recentAvg)) ? null : Number(recentAvg);
  const recentCount =
    recentOrders == null || !Number.isFinite(Number(recentOrders))
      ? 0
      : Number(recentOrders);

  return (
    <>
    <section className="panel flex h-[clamp(17rem,52dvh,24rem)] min-w-0 flex-col max-lg:order-2 md:h-[380px] lg:col-start-1 lg:row-start-1" data-accent="gold">
      <div className="panel-head shrink-0 flex-wrap gap-y-2">
        <h2 className="panel-title">
          <Icon name="i-coin" className="h-4 w-4 text-gold" /> Loot log
          <span
            className="border-2 border-[#05030f] bg-gold px-1.5 py-0.5 font-mono text-[12px] font-bold text-[#1a1000]"
            title={trimmed ? `${formatInt(shown)} of ${formatInt(total)} stored entries shown` : `${formatInt(total)} stored entries`}
          >
            {formatInt(shown)}{trimmed ? ` / ${formatInt(total)}` : ""}
          </span>
        </h2>
        <button
          className="monitor-counter"
          type="button"
          onClick={onMonitor}
          aria-haspopup="dialog"
          title={
            avg == null
              ? "No timed orders yet — open the monitor window"
              : `Average order latency over the last 50 orders (${formatInt(recentCount)} timed orders) — open the monitor window`
          }
        >
          <span className="monitor-counter-icon" aria-hidden="true">
            <Icon name="i-pulse" className="h-3.5 w-3.5" />
          </span>
          <span className="monitor-counter-label">Avg 50 orders</span>
          <b className="monitor-counter-value">{avg == null ? "—" : formatMs(avg)}</b>
          <Icon name="i-chevron" className="monitor-counter-chevron" />
        </button>
        <div className="ml-auto flex w-full flex-wrap items-center gap-2 sm:w-auto sm:flex-nowrap">
          <div className="relative min-w-0 flex-1 sm:w-44 sm:flex-none">
            <Icon name="i-search" className="pointer-events-none absolute left-2.5 top-1/2 h-3.5 w-3.5 -translate-y-1/2 text-faint" />
            <input
              className="input input-sm pl-8"
              type="search"
              value={draft}
              onChange={(event) => setDraft(event.target.value)}
              onKeyDown={(event) => {
                if (event.key === "Escape") setDraft("");
              }}
              placeholder="Search product"
              aria-label="Search loot log by product name"
            />
          </div>
          <select
            className="input input-sm w-auto shrink-0 px-2"
            value={statusFilter}
            onChange={(event) => setStatusFilter(event.target.value)}
            aria-label="Filter loot log by status"
            title="Filter by status: success (filled) or failed"
          >
            <option value="">Status: All</option>
            <option value="filled">Status: Success</option>
            <option value="failed">Status: Failed</option>
          </select>
          <select
            className="input input-sm w-auto shrink-0 px-2"
            value={profitFilter}
            onChange={(event) => setProfitFilter(event.target.value)}
            aria-label="Filter loot log by profit"
            title="Filter by profit sign: positive or negative"
          >
            <option value="">Profit: All</option>
            <option value="positive">Profit: Positive (+)</option>
            <option value="negative">Profit: Negative (−)</option>
          </select>
          <button className="btn btn-ghost btn-sm" type="button" onClick={resetLog}>
            <Icon name="i-trash" className="h-3 w-3" /> Reset
          </button>
        </div>
      </div>
      {entries.length === 0 ? (
        <div className="flex flex-1 flex-col items-center justify-center gap-2 px-6 text-center">
          <Icon name="i-coin" className="h-6 w-6 text-faint" />
          <p className="font-mono text-[13px] uppercase tracking-[0.14em] text-ink-dim">
            {loading ? "Searching…" : emptyTitle}
          </p>
          <p className="text-[13px] text-faint">{loading ? "Reading the permanent loot log." : emptyHint}</p>
        </div>
      ) : (
        <>
          {wide ? null : (
          <ul className="min-h-0 flex-1 divide-y divide-line/40 overflow-y-auto overscroll-contain">
            {entries.map((entry) => (
              <li
                className={`flex cursor-pointer items-start justify-between gap-3 px-4 py-2.5 transition-colors hover:bg-night-850/60${isRecent(entry.at, 6000) ? " loot-entry" : ""}`}
                key={entry.id || `${entry.orderId}-${entry.at}`}
                data-status={entry.status}
                title={`${entry.note || (entry.orderId ? `order ${entry.orderId}` : "loot entry")} · click for latency details`}
                role="button"
                tabIndex={0}
                aria-label={`Open latency details for ${entry.label || shortId(entry.itemId)}`}
                onClick={() => setDetail(entry)}
                onKeyDown={(event) => {
                  if (event.key === "Enter" || event.key === " ") {
                    event.preventDefault();
                    setDetail(entry);
                  }
                }}
              >
                <div className="min-w-0">
                  <p className="truncate text-[13px] text-ink-dim">{entry.label || shortId(entry.itemId)}</p>
                  <p className="mt-0.5 font-mono text-[13px] uppercase tracking-[0.08em] text-faint">
                    {formatTime(entry.at)} · sent {formatInt(entry.requestedQty ?? entry.qty)} · ready{" "}
                    <span className={entry.stockAtTrigger != null && entry.stockAtTrigger !== (entry.requestedQty ?? entry.qty) ? "font-bold text-warn" : undefined}>
                      {entry.stockAtTrigger == null ? "—" : formatInt(entry.stockAtTrigger)}
                    </span>{" "}
                    · got {formatInt(entry.qty)} × {fixedPrice(entry.unitPrice)} RLT
                  </p>
                  <p className="mt-0.5 font-mono text-[13px] text-muted">
                    Target {entry.targetPrice > 0 ? fixedPrice(entry.targetPrice) : "—"}
                  </p>
                  {entry.status === "failed" ? (
                    <p className="mt-0.5 truncate font-mono text-[13px] text-danger/80" title={entry.message || entry.note || undefined}>
                      {entry.message || entry.note || "failed"}
                    </p>
                  ) : null}
                </div>
                <div className="shrink-0 text-right">
                  <p className={`font-mono text-[13px] tabular-nums ${entry.profitRlt > 0 ? "text-gold" : entry.profitRlt < 0 ? "text-danger" : "text-ink-dim"}`}>
                    {entry.status === "failed" ? "0.000000" : formatSignedRlt(entry.profitRlt)}
                  </p>
                  <span className={`status-pill mt-1${entry.status === "failed" ? " is-failed" : ""}`}>
                    {entry.status === "failed" ? "Failed" : "Filled"}
                  </span>
                </div>
              </li>
            ))}
          </ul>
          )}
          {wide ? (
          <div className="min-h-0 min-w-0 flex-1 overflow-auto overscroll-contain">
            <table className="w-full min-w-[860px] border-collapse text-left font-mono text-[13px] tabular-nums">
              <thead className="sticky top-0 z-10 bg-night-900">
                <tr className="border-b-2 border-line text-[12px] uppercase tracking-[0.08em] text-faint">
                  <th scope="col" className="px-4 py-2 font-medium">Time</th>
                  <th scope="col" className="px-3 py-2 font-medium">Item</th>
                  <th scope="col" className="px-3 py-2 font-medium" title="Quantity requested (sent to order)">Sent</th>
                  <th scope="col" className="px-3 py-2 font-medium" title="Stock available when the signal triggered this order">Ready</th>
                  <th scope="col" className="px-3 py-2 font-medium" title="Quantity actually obtained">Got</th>
                  <th scope="col" className="px-3 py-2 font-medium">Unit</th>
                  <th scope="col" className="px-3 py-2 font-medium">Total</th>
                  <th scope="col" className="px-3 py-2 font-medium">Target</th>
                  <th scope="col" className="px-3 py-2 font-medium">Profit</th>
                  <th scope="col" className="px-3 py-2 font-medium">Latency</th>
                  <th scope="col" className="px-3 py-2 font-medium">Status</th>
                  <th scope="col" className="px-4 py-2 font-medium">Message</th>
                </tr>
              </thead>
              <tbody className="divide-y divide-line/40">
                {entries.map((entry) => (
                  <tr
                    className={`cursor-pointer transition-colors hover:bg-night-850/60${isRecent(entry.at, 6000) ? " loot-entry" : ""}`}
                    key={entry.id || `${entry.orderId}-${entry.at}`}
                    data-status={entry.status}
                    title={`${entry.note || (entry.orderId ? `order ${entry.orderId}` : "loot entry")} · click for latency details`}
                    tabIndex={0}
                    aria-label={`Open latency details for ${entry.label || shortId(entry.itemId)}`}
                    onClick={() => setDetail(entry)}
                    onKeyDown={(event) => {
                      if (event.key === "Enter" || event.key === " ") {
                        event.preventDefault();
                        setDetail(entry);
                      }
                    }}
                  >
                    <td className="whitespace-nowrap px-4 py-2 text-faint">{formatTime(entry.at)}</td>
                    <td className="max-w-[180px] truncate px-3 py-2 text-ink-dim">{entry.label || shortId(entry.itemId)}</td>
                    <td className="px-3 py-2 text-ink-dim">{formatInt(entry.requestedQty ?? entry.qty)}</td>
                    <td
                      className={`px-3 py-2 ${entry.stockAtTrigger != null && entry.stockAtTrigger !== (entry.requestedQty ?? entry.qty) ? "font-bold text-warn" : "text-ink-dim"}`}
                      title={
                        entry.stockAtTrigger == null
                          ? "Not recorded (this loot entry predates stock capture)"
                          : entry.stockAtTrigger !== (entry.requestedQty ?? entry.qty)
                            ? `Ready ${formatInt(entry.stockAtTrigger)} differs from sent ${formatInt(entry.requestedQty ?? entry.qty)}`
                            : "Stock available when the signal triggered"
                      }
                    >
                      {entry.stockAtTrigger == null ? "—" : formatInt(entry.stockAtTrigger)}
                    </td>
                    <td className="px-3 py-2 text-ink-dim">{formatInt(entry.qty)}</td>
                    <td className="px-3 py-2 text-ink-dim">{fixedPrice(entry.unitPrice)}</td>
                    <td className="px-3 py-2 text-ink-dim">{fixedPrice(entry.totalPrice)}</td>
                    <td
                      className="px-3 py-2 text-muted"
                      title="Target price in effect when this order was executed"
                    >
                      {entry.targetPrice > 0 ? fixedPrice(entry.targetPrice) : "—"}
                    </td>
                    <td className={`px-3 py-2 ${entry.profitRlt > 0 ? "text-gold" : entry.profitRlt < 0 ? "text-danger" : "text-ink-dim"}`}>
                      {entry.status === "failed" ? "0.000000" : formatSignedRlt(entry.profitRlt)}
                    </td>
                    <td className="px-3 py-2 text-muted">{entry.latencyMs != null ? `${Math.round(entry.latencyMs)}ms` : "—"}</td>
                    <td className="px-3 py-2">
                      <span className={`status-pill${entry.status === "failed" ? " is-failed" : ""}`}>
                        {entry.status === "failed" ? "Failed" : "Filled"}
                      </span>
                    </td>
                    <td
                      className="max-w-[220px] truncate px-4 py-2 text-muted"
                      title={entry.status === "failed" ? entry.message || entry.note || undefined : undefined}
                    >
                      {entry.status === "failed" ? entry.message || entry.note || "failed" : "-"}
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
          ) : null}
        </>
      )}
      <div className="flex shrink-0 items-center justify-between gap-2 border-t-2 border-line bg-night-900/70 px-3 py-3">
        <span className="min-w-0 truncate font-mono text-[12px] font-semibold uppercase tracking-[0.08em] text-ink-dim">
          {filtered
            ? `${formatInt(shown)} match${shown === 1 ? "" : "es"}`
            : trimmed
              ? `Showing ${formatInt(shown)} of ${formatInt(total)}`
              : `${formatInt(total)} stored`}
        </span>
        <div className="flex shrink-0 items-center gap-2">
          {error ? (
            <span className="max-w-[220px] truncate text-[12px] text-danger" title={error}>
              {error}
            </span>
          ) : null}
          {hasMore ? (
            <button
              className="btn btn-signal btn-sm font-bold"
              type="button"
              onClick={loadOlder}
              disabled={loading}
              data-busy={loading ? "true" : undefined}
              aria-busy={loading}
            >
              {loading ? "Loading…" : "Load older"}
            </button>
          ) : null}
        </div>
      </div>
    </section>
    {latestDetail ? (
      <LootDetailModal entry={latestDetail} onClose={() => setDetail(null)} request={request} />
    ) : null}
    </>
  );
}

function FeedPanel({ logs, onClear, notify }) {
  const [paused, setPaused] = useState(false);
  const [tailing, setTailing] = useState(true);
  const [frozen, setFrozen] = useState([]);
  const [unread, setUnread] = useState(0);
  const feedRef = useRef(null);
  // The scrollback is capped: 400 mounted log lines cost layout on every
  // worker event and the oldest ones are far outside the visible screen.
  const FEED_WINDOW = 120;
  const shown = (paused ? frozen : logs).slice(-FEED_WINDOW);
  const isTailing = tailing && !paused;

  const scrollToLatest = useCallback(() => {
    const feed = feedRef.current;
    if (!feed) return;
    feed.scrollTop = feed.scrollHeight;
  }, []);

  useEffect(() => {
    if (paused) {
      setUnread(Math.max(0, logs.length - frozen.length));
      return;
    }
    if (!tailing) return;
    scrollToLatest();
  }, [logs, paused, tailing, frozen.length, scrollToLatest]);

  const handleFeedScroll = () => {
    if (paused) return;
    const feed = feedRef.current;
    if (!feed) return;
    const atBottom = feed.scrollHeight - feed.scrollTop - feed.clientHeight < 24;
    if (atBottom !== tailing) setTailing(atBottom);
  };

  const enableTail = useCallback(() => {
    if (paused) {
      setPaused(false);
      setFrozen([]);
      setUnread(0);
    }
    setTailing(true);
    requestAnimationFrame(scrollToLatest);
  }, [paused, scrollToLatest]);

  const toggleTail = () => {
    if (isTailing) {
      setTailing(false);
      return;
    }
    enableTail();
  };

  const togglePause = () => {
    if (paused) {
      enableTail();
      return;
    }
    setFrozen(logs);
    setUnread(0);
    setPaused(true);
  };

  return (
    <section className="panel flex h-[clamp(15rem,52dvh,27.5rem)] flex-col xl:h-[560px]" data-accent="lime">
      <div className="flex items-center justify-between gap-2 border-b border-line px-3 py-2.5">
        <div className="flex items-center gap-1.5 font-mono text-[12px] font-semibold uppercase tracking-[0.14em] text-muted">
          <span className="led led-pulse bg-signal" />Live event log
        </div>
        <div className="flex items-center gap-1">
          <button
            className={`btn btn-sm max-sm:w-10 max-sm:px-0 ${isTailing ? "btn-signal" : ""}`}
            type="button"
            title={isTailing ? "Stop tailing and hold this line" : "Tail newest log lines"}
            aria-pressed={isTailing}
            onClick={toggleTail}
          >
            <Icon name="i-arrow-down" className="h-3 w-3" />
            <span className="hidden sm:inline">{isTailing ? "Tailing" : "Tail logs"}</span>
          </button>
          <button className="icon-btn h-9 w-9 sm:h-8 sm:w-8" type="button" title={paused ? "Resume feed" : "Pause feed"} aria-pressed={paused} onClick={togglePause}>
            <Icon name={paused ? "i-play" : "i-pause"} className="h-3.5 w-3.5" />
          </button>
          <button
            className="icon-btn h-9 w-9 sm:h-8 sm:w-8"
            type="button"
            title="Clear feed"
            onClick={() => {
              setPaused(false);
              setTailing(true);
              setFrozen([]);
              setUnread(0);
              onClear();
            }}
          >
            <Icon name="i-trash" className="h-3.5 w-3.5" />
          </button>
        </div>
      </div>
      <div className="relative min-h-0 flex-1">
        <div className="pointer-events-none absolute inset-x-0 top-0 z-10 h-5 bg-gradient-to-b from-carbon-900 to-transparent" />
        <div
          className="feed-screen h-full overflow-y-auto"
          ref={feedRef}
          role="log"
          aria-live="polite"
          aria-label="Worker event log"
          onScroll={handleFeedScroll}
        >
          {shown.length === 0 ? <div className="px-3 py-2 font-mono text-[12px] text-faint">[SYS] Waiting for events…</div> : null}
          {shown.map((entry) => (
            <div className={`log-line${isRecent(entry.at, 4000) ? " log-line-new" : ""}`} key={entry.id} data-level={entry.level || "info"} data-scope={String(entry.scope || "sys").toLowerCase()}>
              <span className="log-time">{formatTime(entry.at)}</span>
              <span className="log-scope">{String(entry.scope || "sys").toUpperCase()}</span>
              <span className="log-msg">{entry.message}</span>
            </div>
          ))}
        </div>
      </div>
      <div className="flex items-center justify-between gap-2 border-t-2 border-line px-3 py-2 font-mono text-[12px] uppercase tracking-[0.08em] text-faint">
        <span className={`inline-flex items-center gap-1.5 ${paused ? "text-gold" : !tailing ? "text-info" : "text-signal"}`}>
          <span className="led led-sm" /> {paused ? "Paused" : tailing ? "Tailing" : "Tail paused"}
        </span>
        <span className="ml-auto flex items-center gap-2">
          {paused && unread > 0 ? <span className="text-gold">{formatInt(unread)} buffered</span> : null}
          {!paused && !tailing ? <span className="text-info">Scroll held</span> : null}
          <span>{formatInt(shown.length)} lines</span>
        </span>
      </div>
    </section>
  );
}

function sessionChip(session, now) {
  const remaining = session.expiresAt ? session.expiresAt - now : null;
  if (!session.hasAccessToken) return { text: "NO TOKEN", cls: "chip chip-warn" };
  if (session.lastError) return { text: "ERROR", cls: "chip chip-offline" };
  if (session.expiresAt && remaining <= 0) return { text: "EXPIRED", cls: "chip chip-offline" };
  if (session.needsRefresh || (remaining != null && remaining <= 10 * 60_000)) {
    return { text: "EXPIRING", cls: "chip chip-warn" };
  }
  return { text: "LINKED", cls: "chip chip-ok" };
}

function LinkPanel({ snapshot, worker, notify, onEditSession, onBridge, onClearSession, onRotate }) {
  const now = useNow(1000);
  const session = snapshot.session || {};
  const settings = snapshot.settings || {};
  const [refreshing, setRefreshing] = useState(false);
  const [rotating, setRotating] = useState(false);
  const chip = sessionChip(session, now);
  const remaining = session.expiresAt ? session.expiresAt - now : null;
  let meterWidth = 100;
  let meterClass = "";
  let countdown = "OPAQUE";
  if (snapshot.worker?.mock && session.source === "mock") {
    countdown = "SIMULATED";
    meterClass = "bg-signal";
  } else if (!session.hasAccessToken) {
    meterWidth = 0;
    countdown = "--:--";
  } else if (session.expiresAt) {
    if (remaining <= 0) {
      countdown = "EXPIRED";
      meterWidth = 100;
      meterClass = "bg-danger";
    } else {
      countdown = formatCountdown(remaining);
      meterWidth = Math.max(2, Math.min(100, (remaining / 3_600_000) * 100));
      meterClass = remaining <= 10 * 60_000 ? "bg-danger" : remaining <= 30 * 60_000 ? "bg-warn" : "";
    }
  } else if (session.needsRefresh) {
    countdown = "REFRESH DUE";
    meterWidth = 50;
    meterClass = "bg-warn";
  }

  const refresh = async () => {
    setRefreshing(true);
    try {
      const result = await worker.refreshSession();
      if (result.refreshed) notify("success", "TOKEN RENEWED", "A fresh access token is live.");
      else notify("warn", "REFRESH FAILED", result.session?.lastError || "The worker could not renew the token.");
    } catch (error) {
      notify("error", "REFRESH FAILED", error.message);
    } finally {
      setRefreshing(false);
    }
  };

  const toggleAuto = async (value) => {
    try {
      await worker.saveSettings({ autoRefresh: value });
    } catch (error) {
      notify("error", "CONFIG FAILED", error.message);
    }
  };

  const rotate = async () => {
    try {
      await onRotate(() => setRotating(true));
    } finally {
      setRotating(false);
    }
  };

  return (
    <section className="panel" data-accent="violet">
      <div className="panel-head">
        <h2 className="panel-title"><Icon name="i-link" className="h-4 w-4 text-violet" /> Session link</h2>
        <span className={chip.cls}>{chip.text}</span>
      </div>
      <div className="panel-body">
        <div className="border-2 border-line bg-night-950/60 p-3">
          <div className="flex items-center justify-between gap-2 font-mono text-[13px] uppercase tracking-[0.12em] text-faint">
            <span>Access token</span>
            <span className="tabular-nums text-violet">{countdown}</span>
          </div>
          <div className="meter mt-2">
            <i id="token-meter" className={meterClass} style={{ "--meter-value": `${meterWidth}%` }} />
          </div>
          <p className="mt-2 text-[13px] leading-snug text-faint">
            {!session.hasAccessToken
              ? "No Rollercoin session linked. Paste a token or use the bridge."
              : session.lastError
                ? `${session.lastError} · source ${(session.source || "?").toUpperCase()}`
                : `Source ${(session.source || "?").toUpperCase()} · updated ${session.updatedAt ? relTime(session.updatedAt) : "—"} · ${formatInt(session.refreshCount)} refresh${session.refreshCount === 1 ? "" : "es"}`}
          </p>
        </div>

        <dl className="kv">
          <div>
            <dt>Refresh token</dt>
            <dd style={!session.hasRefreshToken ? { color: "var(--color-warn)" } : undefined}>
              {session.hasRefreshToken ? session.refreshPreview || "STORED" : "MISSING"}
            </dd>
          </div>
          <div><dt>Last refresh</dt><dd>{session.lastRefreshAt ? `${relTime(session.lastRefreshAt)} (${formatInt(session.refreshCount)}×)` : "NEVER"}</dd></div>
          <div><dt>Source</dt><dd>{(session.source || "—").toUpperCase()}</dd></div>
          <div><dt>Worker key</dt><dd>{snapshot.workerTokenPreview || "—"}{snapshot.accessKeyPinned ? " (env)" : ""}</dd></div>
        </dl>

        <div className="grid grid-cols-2 gap-2">
          <button className="btn btn-sm" type="button" onClick={refresh} disabled={!session.hasRefreshToken || refreshing} data-busy={refreshing ? "true" : undefined} aria-busy={refreshing}>
            <Icon name="i-refresh" className="h-3 w-3" /> {refreshing ? "Refreshing…" : "Refresh now"}
          </button>
          <button className="btn btn-primary btn-sm" type="button" onClick={onEditSession}>Edit tokens</button>
        </div>
        <div className="grid grid-cols-2 gap-2">
          <button className="btn btn-ghost btn-sm" type="button" onClick={onBridge}>Token bridge</button>
          <button className="btn btn-danger-ghost btn-sm" type="button" onClick={rotate} disabled={rotating} data-busy={rotating ? "true" : undefined} aria-busy={rotating}>
            {rotating ? "Rotating…" : "Rotate key"}
          </button>
        </div>
        <button
          className="btn btn-danger-ghost btn-sm btn-block"
          type="button"
          onClick={onClearSession}
          disabled={!session.hasAccessToken && !session.hasRefreshToken && !session.hasCookie}
        >
          <Icon name="i-trash" className="h-3 w-3" /> Clear session
        </button>

        <label className="toggle-row">
          <span>Auto refresh<small>Worker renews the session before expiry</small></span>
          <input type="checkbox" className="sr-only" checked={Boolean(settings.autoRefresh)} onChange={(event) => toggleAuto(event.target.checked)} />
          <span className="switch" aria-hidden="true" />
        </label>
        <p className="flex items-start gap-1.5 text-[13px] leading-snug text-faint">
          <Icon name="i-alert" className="mt-0.5 h-3 w-3 text-warn/80" />
          <span>Tokens are stored in <code className="text-ink-dim">data/rollercoin.db</code> with owner-only permissions.</span>
        </p>
      </div>
    </section>
  );
}

function RuntimeConfigPanel({ snapshot, worker, notify, onReset, onReconnect, onBackup, baseUrl, onSetMode }) {
  const settings = snapshot.settings || {};
  const mode = modeOf(settings);
  const [delay, setDelay] = useState(String(settings.delayPerOrder ?? 200));
  const [maxOrders, setMaxOrders] = useState(String(settings.maxOrdersPerMinute ?? 60));
  const [saving, setSaving] = useState(false);
  const [modesBusy, setModesBusy] = useState(false);
  const [reconnecting, setReconnecting] = useState(false);

  useEffect(() => {
    setDelay(String(settings.delayPerOrder ?? 200));
    setMaxOrders(String(settings.maxOrdersPerMinute ?? 60));
  }, [settings.delayPerOrder, settings.maxOrdersPerMinute]);

  const changeMode = async (next) => {
    if (next === mode) return;
    setModesBusy(true);
    try {
      await onSetMode(next);
    } finally {
      setModesBusy(false);
    }
  };

  const saveNumbers = async (event) => {
    event.preventDefault();
    setSaving(true);
    try {
      await worker.saveSettings({
        delayPerOrder: Number(delay),
        maxOrdersPerMinute: Number(maxOrders)
      });
      notify("info", "CONFIG UPDATED", "Worker settings applied.");
    } catch (error) {
      notify("error", "CONFIG FAILED", error.message);
    } finally {
      setSaving(false);
    }
  };

  const reconnect = async () => {
    setReconnecting(true);
    try {
      await onReconnect();
    } finally {
      setReconnecting(false);
    }
  };

  return (
    <section className="panel @container" data-accent="pink">
      <div className="panel-head">
        <h2 className="panel-title"><Icon name="i-gear" className="h-4 w-4 text-pink" /> Runtime config</h2>
      </div>
      <div className="panel-body">
        <form className="space-y-3" onSubmit={saveNumbers}>
          {/* This panel lives in a ~360px sidebar, so the two fields stack. A
              viewport breakpoint would put them side by side at 640px while the
              column is still narrow, wrapping the labels unevenly. */}
          <div className="grid grid-cols-1 gap-3 @[30rem]:grid-cols-2">
            <label className="field">
              <span>Order cooldown (ms)</span>
              <input className="input" type="number" inputMode="numeric" step="10" min="50" max="60000" value={delay} onChange={(event) => setDelay(event.target.value)} />
              <small>Minimum gap between purchases.</small>
            </label>
            <label className="field">
              <span>Max orders / min</span>
              <input className="input" type="number" inputMode="numeric" step="1" min="1" max="600" value={maxOrders} onChange={(event) => setMaxOrders(event.target.value)} />
              <small>Hard burst protection.</small>
            </label>
          </div>
          <button className="btn btn-primary btn-sm btn-block" type="submit" disabled={saving} data-busy={saving ? "true" : undefined} aria-busy={saving}>
            {saving ? "Saving…" : "Save numeric config"}
          </button>
        </form>

        <div className="space-y-1.5">
          <span className="label">Trading mode</span>
          <ModeMenu mode={mode} busy={modesBusy} onChange={changeMode} variant="panel" />
        </div>

        <div className="space-y-1.5 border-2 border-line bg-night-950/60 p-3">
          <div className="flex items-center justify-between gap-3">
            <span className="label">Worker</span>
            <code className="truncate font-mono text-[12px] text-ink-dim">{baseUrl}</code>
          </div>
          <div className="flex items-center justify-between gap-3">
            <span className="label">Upstream</span>
            <code className="truncate font-mono text-[12px] text-ink-dim">{snapshot.gateway?.upstream || "—"}</code>
          </div>
          <div className="flex items-center justify-between gap-3">
            <span className="label">Version</span>
            <code className="font-mono text-[12px] text-ink-dim">v{snapshot.version || "1.0.0"}</code>
          </div>
        </div>
        <button className="btn btn-ghost btn-sm btn-block" type="button" onClick={reconnect} disabled={reconnecting} data-busy={reconnecting ? "true" : undefined} aria-busy={reconnecting}>
          <Icon name="i-refresh" className="h-3 w-3" /> {reconnecting ? "Reconnecting…" : "Reconnect feed"}
        </button>

        <div className="space-y-2 border-2 border-line bg-night-950/60 p-3">
          <h3 className="flex items-center gap-1.5 font-mono text-[12px] font-semibold uppercase tracking-[0.14em] text-gold">
            <Icon name="i-database" className="h-3.5 w-3.5" /> Backup &amp; migrate
          </h3>
          <p className="text-[13px] leading-snug text-muted">
            Export targets, settings, counters and the whole loot log to one portable file — or restore it
            onto another worker.
          </p>
          <button className="btn btn-ghost btn-sm btn-block" type="button" onClick={onBackup}>
            <Icon name="i-database" className="h-3.5 w-3.5" /> Open backup panel
          </button>
        </div>

        <div className="space-y-2 border-2 border-danger/50 bg-danger/5 p-3">
          <h3 className="flex items-center gap-1.5 font-mono text-[12px] font-semibold uppercase tracking-[0.14em] text-danger">
            <Icon name="i-alert" className="h-3.5 w-3.5" /> Danger zone
          </h3>
          <p className="text-[13px] leading-snug text-muted">
            Wipes lifetime profit, fill counters, per-target success rates and the loot log. Targets stay untouched.
          </p>
          <button className="btn btn-danger btn-sm btn-block" type="button" onClick={onReset}>Reset lifetime stats</button>
        </div>
      </div>
    </section>
  );
}

function SidePanel({ tab, setTab, snapshot, logs, worker, notify, onEditSession, onBridge, onClearSession, onRotate, onReset, onReconnect, onBackup, baseUrl, onSetMode }) {
  const tabs = [
    { id: "feed", label: "FEED" },
    { id: "link", label: "LINK" },
    { id: "config", label: "CONFIG" }
  ];
  return (
    <aside className="flex min-w-0 flex-col gap-3 max-lg:order-4 lg:sticky lg:top-24 lg:col-start-2 lg:row-start-1 lg:row-span-3 lg:max-h-[calc(100dvh-6rem)] lg:self-start lg:overflow-y-auto">
      <div className="tab-rail" role="tablist" aria-label="Control panels">
        {tabs.map((item) => (
          <button
            key={item.id}
            className="pixel-tab"
            type="button"
            role="tab"
            aria-selected={tab === item.id}
            onClick={() => setTab(item.id)}
          >
            {item.label}
          </button>
        ))}
      </div>
      <div key={tab} className="tab-panel flex min-w-0 flex-col gap-3">
        {tab === "feed" ? <MemoFeedPanel logs={logs} onClear={worker.clearLogs} notify={notify} /> : null}
        {tab === "link" ? (
          <LinkPanel
            snapshot={snapshot}
            worker={worker}
            notify={notify}
            onEditSession={onEditSession}
            onBridge={onBridge}
            onClearSession={onClearSession}
            onRotate={onRotate}
          />
        ) : null}
        {tab === "config" ? (
          <RuntimeConfigPanel
            snapshot={snapshot}
            worker={worker}
            notify={notify}
            onReset={onReset}
            onReconnect={onReconnect}
            onBackup={onBackup}
            baseUrl={baseUrl}
            onSetMode={onSetMode}
          />
        ) : null}
      </div>
    </aside>
  );
}

function ToastStack({ toasts, onDismiss }) {
  return (
    <div
      className="pointer-events-none fixed inset-x-3 bottom-[max(0.75rem,env(safe-area-inset-bottom))] z-[60] flex flex-col items-end gap-2 sm:inset-x-auto sm:bottom-4 sm:right-4 sm:w-[360px]"
      aria-live="polite"
    >
      {toasts.map((toast) => (
        <div key={toast.id} className={`toast ${toast.kind} w-full cursor-pointer${toast.leaving ? " is-leaving" : ""}`} onClick={() => onDismiss(toast.id)} role="status">
          <Icon
            name={toast.kind === "success" ? "i-check" : toast.kind === "error" || toast.kind === "warn" ? "i-alert" : "i-bolt"}
            className={`mt-0.5 h-3.5 w-3.5 ${
              toast.kind === "success"
                ? "text-signal"
                : toast.kind === "error"
                  ? "text-danger"
                  : toast.kind === "warn"
                    ? "text-gold"
                    : "text-info"
            }`}
          />
          <div className="min-w-0">
            <span className="block font-sans text-[12px] font-semibold uppercase tracking-[0.08em] text-ink">{toast.title}</span>
            <span className="mt-0.5 block text-[13px] leading-relaxed text-muted">{toast.message}</span>
          </div>
        </div>
      ))}
    </div>
  );
}

// These panels are expensive to reconcile and only change when their own data
// does; memoising them keeps a market tick or a toast from redrawing the deck.
const MemoLootLog = memo(LootLog);
const MemoReportPanel = memo(ReportPanel);
const MemoTargetsPanel = memo(TargetsPanel);
const MemoTrendPanel = memo(TrendPanel);
const MemoFeedPanel = memo(FeedPanel);

function Dashboard({ worker, notify, toasts, dismissToast }) {
  const { snapshot, logs, status, latency } = worker;
  const [tab, setTab] = useState("feed");
  const [search, setSearch] = useState("");
  const [typeFilter, setTypeFilter] = useState("all");
  const [statusFilter, setStatusFilter] = useState("all");
  const [modal, setModal] = useState(null);
  const [confirmState, setConfirmState] = useState(null);
  const [monitorOpen, setMonitorOpen] = useState(false);
  const [soundOn, setSoundOn] = useState(sound.enabled);
  const [modeBusy, setModeBusy] = useState(false);
  // Handlers stay referentially stable so memoised cards are not invalidated by
  // a new snapshot. The ref always points at the newest worker instance.
  const workerRef = useRef(worker);
  workerRef.current = worker;

  const openMonitor = useCallback(() => setMonitorOpen(true), []);
  const recentAverage = snapshot.benchmark?.recent?.avg;
  const recentAvg =
    recentAverage == null || !Number.isFinite(Number(recentAverage))
      ? null
      : Number(recentAverage);
  const recentOrderCount = snapshot.benchmark?.recent?.orders;
  const recentOrders =
    recentOrderCount == null || !Number.isFinite(Number(recentOrderCount))
      ? 0
      : Number(recentOrderCount);

  const eps = useEventRate(snapshot.gateway?.eventCount || 0);
  const baseUrl = worker.getConnection().baseUrl;
  const reportByItem = useMemo(() => {
    const map = {};
    for (const item of snapshot.report?.items || []) {
      const itemId = String(item.itemId || "").toLowerCase();
      if (itemId) map[itemId] = item;
    }
    return map;
  }, [snapshot.report]);

  const askConfirm = useCallback(
    (options) => new Promise((resolve) => setConfirmState({ ...options, resolve })),
    []
  );

  const closeConfirm = (value) => {
    confirmState?.resolve(value);
    setConfirmState(null);
  };

  const toggleTarget = useCallback(async (config, active) => {
    try {
      await workerRef.current.toggleTarget(config.itemId, active);
      notify("info", active ? "TARGET ARMED" : "TARGET DISARMED", config.label || shortId(config.itemId));
    } catch (error) {
      notify("error", "ARM FAILED", error.message);
    }
  }, [notify]);

  const setMode = useCallback(async (next) => {
    const current = modeOf(snapshot.settings);
    if (next === current) return;
    // Arming live is the only transition that can spend money, so it asks
    // first. Every other move (off, shadow) is immediate and safe.
    if (next === "live") {
      const confirmed = await askConfirm({
        title: "ARM LIVE TRADING",
        message:
          "Real orders will be sent to Rollercoin using your linked session. Every signal below its target price will spend RLT. Start in shadow if you have not watched the signals yet.",
        confirmLabel: "GO LIVE",
        danger: true
      });
      if (!confirmed) return;
    }
    setModeBusy(true);
    try {
      const result = await worker.control(next);
      const applied = modeOf(result?.settings);
      if (applied === "live") {
        notify("success", "LIVE TRADING ARMED", "The sniper is armed and hunting.");
      } else if (applied === "shadow") {
        notify("info", "SHADOW MODE", "Signals are logged; no orders will be sent.");
      } else {
        notify("warn", "TRADING OFF", "All buying is stopped; the market feed stays live.");
      }
    } catch (error) {
      notify("error", "MODE CHANGE FAILED", error.message);
    } finally {
      setModeBusy(false);
    }
  }, [askConfirm, notify, snapshot.settings, worker]);

  const clearSession = async () => {
    const confirmed = await askConfirm({
      title: "CLEAR ROLLERCOIN SESSION",
      message:
        "Remove the stored access token, refresh token and Cloudflare cookie? The market feed disconnects until you paste a new session. If the Token Bridge is running, stop it first or it will re-link within 30 seconds.",
      confirmLabel: "CLEAR SESSION",
      danger: true
    });
    if (!confirmed) return;
    try {
      await worker.clearSession();
      notify("warn", "SESSION CLEARED", "Stored Rollercoin credentials were removed.");
    } catch (error) {
      notify("error", "CLEAR FAILED", error.message);
    }
  };

  const deleteTarget = useCallback(async (config) => {
    const confirmed = await askConfirm({
      title: "DELETE TARGET",
      message: `Remove ${config.label || shortId(config.itemId)} from the hunt list? Open orders are not cancelled.`,
      confirmLabel: "DELETE TARGET",
      danger: true
    });
    if (!confirmed) return;
    try {
      await workerRef.current.deleteTarget(config.itemId);
      notify("warn", "TARGET REMOVED", config.label || shortId(config.itemId));
    } catch (error) {
      notify("error", "DELETE FAILED", error.message);
    }
  }, [askConfirm, notify]);

  const openTargetModal = useCallback((config) => {
    setModal({ type: "target", config });
  }, []);

  const addTargetModal = useCallback(() => {
    setModal({ type: "target", config: null });
  }, []);

  const resetStats = useCallback(async () => {
    const confirmed = await askConfirm({
      title: "RESET LIFETIME STATS",
      message: "Wipe profit, fill counters, per-target success rates and the entire loot log? Targets and sessions are not touched.",
      confirmLabel: "RESET STATS",
      danger: true
    });
    if (!confirmed) return false;
    try {
      await workerRef.current.resetStats();
      notify("warn", "STATS RESET", "Starting a fresh scoreboard.");
      return true;
    } catch (error) {
      notify("error", "RESET FAILED", error.message);
      return false;
    }
  }, [askConfirm, notify]);

  const rotateKey = async (onPending) => {
    if (snapshot.accessKeyPinned) {
      notify("warn", "KEY IS PINNED", "WORKER_TOKEN controls this key. Change the env var to rotate.");
      return;
    }
    const confirmed = await askConfirm({
      title: "ROTATE ACCESS KEY",
      message: "Generate a new key? Every other control socket is revoked and all tabs must log in again.",
      confirmLabel: "ROTATE KEY",
      danger: true
    });
    if (!confirmed) return;
    onPending?.();
    try {
      const result = await worker.rotateAccessKey();
      setModal({ type: "key", token: result.token });
      notify("success", "KEY ROTATED", "This tab will re-link automatically.");
    } catch (error) {
      notify("error", "ROTATION FAILED", error.message);
    }
  };

  const reconnect = async () => {
    try {
      await worker.control("reconnect");
      notify("info", "SIGNAL SENT", "Reconnecting the market feed…");
    } catch (error) {
      notify("error", "RECONNECT FAILED", error.message);
    }
  };

  const toggleSound = () => {
    const next = !sound.enabled;
    sound.setEnabled(next);
    setSoundOn(next);
  };

  return (
    <>
      <Header
        snapshot={snapshot}
        status={status}
        latency={latency}
        soundOn={soundOn}
        onSound={toggleSound}
        onLogout={worker.logout}
        onSetMode={setMode}
        modeBusy={modeBusy}
        onMonitor={openMonitor}
      />
      <Ticker configs={snapshot.configs || []} mock={snapshot.worker?.mock} />
      <div className="deck-enter deck-enter-2 mx-auto w-full max-w-[1600px] px-3 pt-4 sm:px-4 lg:px-6">
        <Hud snapshot={snapshot} eps={eps} />
      </div>
      <div className="deck-enter deck-enter-3 mx-auto mt-4 w-full max-w-[1600px] px-3 sm:px-4 lg:px-6">
        <MemoActivityChart request={worker.request} />
      </div>
      <div className="deck-enter deck-enter-3 mx-auto mt-4 w-full max-w-[1600px] px-3 sm:px-4 lg:px-6">
        <MemoTrendPanel configs={snapshot.configs || []} onEdit={openTargetModal} />
      </div>
      <main className="deck-enter deck-enter-3 mx-auto mt-4 grid w-full max-w-[1600px] gap-4 px-3 pb-10 sm:px-4 lg:grid-cols-[minmax(0,1fr)_360px] lg:items-start lg:px-6 xl:grid-cols-[minmax(0,1fr)_380px]">
        <MemoLootLog
          history={snapshot.history || []}
          historyTotal={snapshot.historyTotal}
          historyRevision={worker.historyRevision}
          onReset={resetStats}
          request={worker.request}
          onMonitor={openMonitor}
          recentAvg={recentAvg}
          recentOrders={recentOrders}
        />
        <MemoReportPanel report={snapshot.report} notify={notify} />
        <MemoTargetsPanel
          configs={snapshot.configs || []}
          reportByItem={reportByItem}
          search={search}
          typeFilter={typeFilter}
          statusFilter={statusFilter}
          onSearch={setSearch}
          onTypeFilter={setTypeFilter}
          onStatusFilter={setStatusFilter}
          onAdd={addTargetModal}
          onEdit={openTargetModal}
          onDelete={deleteTarget}
          onToggle={toggleTarget}
          notify={notify}
          request={worker.request}
        />
        <SidePanel
          tab={tab}
          setTab={setTab}
          snapshot={snapshot}
          logs={logs}
          worker={worker}
          notify={notify}
          onEditSession={() => setModal({ type: "session" })}
          onBridge={() => setModal({ type: "bridge" })}
          onClearSession={clearSession}
          onRotate={rotateKey}
          onReset={resetStats}
          onReconnect={reconnect}
          onBackup={() => setModal({ type: "backup" })}
          baseUrl={baseUrl}
          onSetMode={setMode}
        />
      </main>
      <div className="pixel-stripe mt-8" aria-hidden="true" />
      <footer className="border-t-2 border-line bg-night-950/70">
        <div className="mx-auto flex w-full max-w-[1600px] flex-col gap-1 px-3 pt-4 pb-[max(1rem,env(safe-area-inset-bottom))] font-mono text-[12px] uppercase tracking-[0.08em] text-faint sm:flex-row sm:items-center sm:justify-between sm:px-4 lg:px-6">
          <span>
            <span id="footer-mode" className="text-gold">{snapshot.worker?.mock ? "MOCK" : "LIVE"}</span> · WORKER v
            <span id="footer-version">{snapshot.version || "1.0.0"}</span>
          </span>
          <span className="text-violet">Original bot by @nopaleon.0f</span>
          <span>Not affiliated with Rollercoin</span>
        </div>
      </footer>

      <MonitorWindow
        open={monitorOpen}
        snapshot={snapshot}
        worker={worker}
        notify={notify}
        onClose={() => setMonitorOpen(false)}
      />
      {modal?.type === "target" ? (
        <TargetModal
          config={modal.config}
          onClose={() => setModal(null)}
          worker={worker}
          notify={notify}
        />
      ) : null}
      {modal?.type === "session" ? (
        <SessionModal
          session={snapshot.session || {}}
          onClose={() => setModal(null)}
          worker={worker}
          notify={notify}
        />
      ) : null}
      {modal?.type === "bridge" ? (
        <BridgeModal
          onClose={() => setModal(null)}
          getConnection={worker.getConnection}
          notify={notify}
        />
      ) : null}
      {modal?.type === "key" ? (
        <KeyModal token={modal.token} onClose={() => setModal(null)} notify={notify} />
      ) : null}
      {modal?.type === "backup" ? (
        <BackupModal
          snapshot={snapshot}
          worker={worker}
          notify={notify}
          onClose={() => setModal(null)}
        />
      ) : null}
      {confirmState ? (
        <ConfirmModal
          title={confirmState.title}
          message={confirmState.message}
          confirmLabel={confirmState.confirmLabel}
          danger={confirmState.danger}
          onCancel={() => closeConfirm(false)}
          onConfirm={() => closeConfirm(true)}
        />
      ) : null}

      <ToastStack toasts={toasts} onDismiss={dismissToast} />
    </>
  );
}

export default function App() {
  const { toasts, push, dismiss } = useToasts();
  const worker = useWorker({ notify: push });
  const { snapshot, authError, busy } = worker;

  return (
    <div className="deck-bg min-h-dvh">
      {snapshot ? (
        <Dashboard worker={worker} notify={push} toasts={toasts} dismissToast={dismiss} />
      ) : (
        <BootScreen onLogin={worker.login} busy={busy} error={authError} />
      )}
      {!snapshot ? <ToastStack toasts={toasts} onDismiss={dismiss} /> : null}
    </div>
  );
}
