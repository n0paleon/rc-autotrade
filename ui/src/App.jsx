import { memo, useCallback, useEffect, useMemo, useRef, useState } from "react";
import PriceChart from "./components/PriceChart.jsx";
import TrendPanel, { TrendChip } from "./components/TrendPanel.jsx";
import { Icon, ModalShell } from "./components/ModalShell.jsx";
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
  relTime,
  runtimeHot,
  shortId,
  stateLabel,
  successStats,
  trimPrice,
  drawPixelAvatar,
  formatMs,
  formatPreciseMs,
  formatBytes,
  formatPercent,
  latencyChannelLabel,
  latencyClass,
  sparkBars,
  thresholdClass,
  utilizationClass
} from "./lib/format.js";
import { sound } from "./lib/sound.js";

const MemoChart = memo(PriceChart);

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
      if (Date.now() - reference.current.at > 6500) {
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
    { icon: "i-wifi", tone: "border-info/50 bg-info/10 text-info", text: "Live market feed, even when buying is paused" },
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
            <h1 className="boot-title text-[clamp(3rem,7vw,5.4rem)]">
              MARKET<br /><span className="accent">SNIPER</span>
            </h1>
          </div>
          <p className="mt-6 max-w-md text-[14px] leading-relaxed text-muted">
            A headless Rollercoin marketplace sniper. It watches the wire, prices your targets, and buys the dip while you sleep.
          </p>
          <ul className="mt-6 space-y-2">
            {perks.map((perk) => (
              <li key={perk.text} className="flex items-center gap-2.5 text-[13px] text-ink-dim">
                <span className={`flex h-8 w-8 shrink-0 items-center justify-center border-2 border-[#05030f] shadow-[2px_2px_0_#05030f] ${perk.tone}`}>
                  <Icon name={perk.icon} className="h-4 w-4" />
                </span>
                {perk.text}
              </li>
            ))}
          </ul>
          <div className="mt-7 flex flex-wrap items-center gap-x-3 gap-y-1 font-mono text-[12px] uppercase tracking-[0.08em] text-faint">
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

function Header({ snapshot, status, latency, now, soundOn, onSound, onLogout, onTogglePause, pausing }) {
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
  let trading = "ARMED";
  let tradingClass = "chip chip-armed";
  if (settings.paused) {
    trading = "PAUSED";
    tradingClass = "chip chip-paused";
  } else if (settings.dryRun) {
    trading = "DRY RUN";
    tradingClass = "chip chip-dry";
  }

  return (
    <header className="deck-enter sticky top-0 z-40 border-b-2 border-line bg-night-950/92 pt-[env(safe-area-inset-top)] backdrop-blur-md">
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
          <button
            className={`btn btn-sm ${settings.paused ? "btn-primary" : ""}`}
            type="button"
            onClick={onTogglePause}
            disabled={pausing}
            data-busy={pausing ? "true" : undefined}
            aria-busy={pausing}
            aria-label={settings.paused ? "Start buying" : "Pause buying"}
            title={settings.paused ? "Resume all buying" : "Pause all buying; the market feed stays live"}
          >
            <Icon name={settings.paused ? "i-play" : "i-pause"} className="h-3 w-3" />
            <span className="hidden sm:inline">{pausing ? "…" : settings.paused ? "Start" : "Pause"}</span>
          </button>
          <span id="trading-state" className={`${tradingClass} hidden sm:inline-flex`}>
            <span className="led led-pulse" />{trading}
          </span>
          <span id="conn-chip" className={`chip ${connClass}`} title={snapshot.gateway?.lastError || "Control-plane link status"}>
            <span className="led" />{connLabel}
          </span>
          <span className="hidden items-center gap-1 font-mono text-[12px] text-info md:inline-flex" title="Control link round-trip">
            <Icon name="i-wifi" className="h-3 w-3" />
            <span className="tabular-nums">{latency != null ? `${latency}ms` : "--"}</span>
          </span>
          <span className="hidden items-center gap-1 font-mono text-[12px] text-violet lg:inline-flex" title="Local time">
            <Icon name="i-clock" className="h-3 w-3" />
            <span className="tabular-nums">{formatTime(now)}</span>
          </span>
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
  const halves = [0, 1];
  const [paused, setPaused] = useState(false);
  return (
    <div className="deck-enter deck-enter-1 border-b-2 border-line bg-night-900/70" aria-label="Market wire">
      <div className="mx-auto flex h-11 w-full max-w-[1600px] items-center gap-3 px-3 sm:h-9 sm:px-4 lg:px-6">
        <div className="flex shrink-0 items-center gap-1.5 font-mono text-[12px] font-bold uppercase tracking-[0.08em] text-gold">
          <span className="led led-pulse bg-gold" />Market wire
          <button
            className="ticker-pause icon-btn h-10 w-10 sm:h-7 sm:w-7"
            type="button"
            aria-label={paused ? "Play market wire" : "Pause market wire"}
            aria-pressed={paused}
            title={paused ? "Resume the wire" : "Pause the wire"}
            onClick={() => setPaused((value) => !value)}
          >
            <Icon name={paused ? "i-play" : "i-pause"} className="h-3 w-3" />
          </button>
        </div>
        <div className="ticker-track min-w-0 flex-1" data-paused={paused ? "true" : undefined}>
          <div className="ticker-content" style={{ animationDuration: `${Math.max(26, configs.length * 8)}s` }}>
            {halves.map((half) => (
              <div className="flex shrink-0 items-center" key={half}>
                {configs.length === 0 ? (
                  <span className="px-4 font-mono text-[12px] uppercase tracking-[0.08em] text-faint">
                    No targets deployed. The market is quiet.
                  </span>
                ) : (
                  configs.map((config) => {
                    const runtime = config.runtime || {};
                    const hot = runtimeHot(config);
                    return (
                      <span className="flex shrink-0 items-center gap-2 border-r-2 border-line/70 px-3 font-mono text-[12px]" key={`${half}-${config.itemId}`}>
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
            ))}
          </div>
        </div>
        <div className={`chip h-5 shrink-0 max-[359px]:hidden ${mock ? "chip-warn" : "chip-online"}`}>
          {mock ? "Mock" : "Live"}
        </div>
      </div>
    </div>
  );
}

function Hud({ snapshot, now, eps }) {
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
        <div className={`stat-hero mt-1 text-[32px] leading-none [text-shadow:2px_2px_0_#05030f] sm:text-[36px] ${profit > 0 ? "text-gold" : "text-ink"}`} id="stat-profit">
          {formatRlt(stats.profitRlt)}
        </div>
        <div className="mt-2 font-mono text-[12px] uppercase tracking-[0.08em] text-faint" id="stat-profit-sub">
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

function TargetCard({ config, product, onEdit, onDelete, onToggle, notify }) {
  const canvasRef = useRef(null);
  const [toggling, setToggling] = useState(false);
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
      className="target-card group overflow-hidden"
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
              className="-my-1 inline-flex min-h-6 items-center gap-1 whitespace-nowrap py-1 font-mono text-[12px] text-muted transition-colors hover:text-gold"
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
            <a
              className="-m-1.5 inline-flex h-6 w-6 items-center justify-center text-muted transition-colors hover:text-info"
              href={marketplaceUrl(itemType, config.itemId)}
              target="_blank"
              rel="noopener noreferrer"
              title="Open on Rollercoin marketplace"
              aria-label={`Open ${label} on the Rollercoin marketplace`}
            >
              <Icon name="i-external" className="h-3 w-3" />
            </a>
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

      <MemoChart history={runtime.history || []} targetPrice={targetPrice} />

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
          <button className="icon-btn h-9 w-9 sm:h-8 sm:w-8" type="button" aria-label="Edit target" title="Edit target" onClick={() => onEdit(config)}>
            <Icon name="i-edit" className="h-3.5 w-3.5" />
          </button>
          <button className="icon-btn icon-btn-danger h-9 w-9 sm:h-8 sm:w-8" type="button" aria-label="Delete target" title="Delete target" onClick={() => onDelete(config)}>
            <Icon name="i-trash" className="h-3.5 w-3.5" />
          </button>
        </div>
      </div>
    </article>
  );
}

function TargetsPanel({ configs, reportByItem, search, typeFilter, onSearch, onTypeFilter, onAdd, onEdit, onDelete, onToggle, notify }) {
  const query = search.trim().toLowerCase();
  const filtered = configs.filter((config) => {
    if (typeFilter !== "all" && itemTypeOf(config) !== typeFilter) return false;
    if (!query) return true;
    return (
      (config.label || "").toLowerCase().includes(query) ||
      config.itemId.toLowerCase().includes(query)
    );
  });
  const counts = configs.reduce((tally, config) => {
    const type = itemTypeOf(config);
    tally[type] = (tally[type] || 0) + 1;
    return tally;
  }, {});


  return (
    <section className="panel min-w-0 max-xl:order-3 xl:col-start-1 xl:row-start-3" data-accent="cyan">
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
        <div className="flex flex-wrap items-center gap-1.5 border-b-2 border-line px-4 py-2.5" role="group" aria-label="Filter targets by item type">
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
        <div className="grid grid-cols-1 gap-4 p-4 lg:grid-cols-2">
          {filtered.length === 0 ? (
            <div className="col-span-full flex flex-col items-center gap-2 py-10 text-center">
              <p className="font-mono text-[13px] font-semibold uppercase tracking-[0.14em] text-ink-dim">No matches</p>
              <p className="max-w-sm text-[13px] leading-relaxed text-faint">
                {query
                  ? `Nothing matches “${search}”. Try another label or item ID.`
                  : `No targets of type “${itemTypeLabel(typeFilter)}” are deployed.`}
              </p>
            </div>
          ) : (
            filtered.map((config) => (
              <TargetCard
                key={config.itemId}
                config={config}
                product={reportByItem?.[String(config.itemId).toLowerCase()] || null}
                onEdit={onEdit}
                onDelete={onDelete}
                onToggle={onToggle}
                notify={notify}
              />
            ))
          )}
        </div>
      )}
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
    <section className="panel min-w-0 max-xl:order-2 xl:col-start-1 xl:row-start-2" data-accent="gold">
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

          <ul className="report-scroll divide-y divide-line/40 md:hidden">
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

          <div className="report-scroll hidden min-w-0 md:block">
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

function LootLog({ history, historyTotal, historyRevision, onReset, request }) {
  const [draft, setDraft] = useState("");
  const [query, setQuery] = useState("");
  const [entries, setEntries] = useState([]);
  const [total, setTotal] = useState(Number(historyTotal) || 0);
  const [hasMore, setHasMore] = useState(false);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState("");
  const [detail, setDetail] = useState(null);
  const idsRef = useRef(new Set());

  // Typing updates the box immediately; the server query is debounced so a
  // search over the permanent log is not re-run on every keystroke.
  useEffect(() => {
    const timer = setTimeout(() => setQuery(draft.trim()), LOOT_SEARCH_DEBOUNCE_MS);
    return () => clearTimeout(timer);
  }, [draft]);

  // Leaving search mode always falls back to the newest live page.
  useEffect(() => {
    if (query) return;
    const seed = (Array.isArray(history) ? history : []).filter(Boolean);
    idsRef.current = new Set(seed.map((entry) => entry.id));
    setEntries(seed);
    setError("");
    setLoading(false);
    setHasMore(idsRef.current.size < (Number(historyTotal) || 0));
  }, [query]);

  // A reset or backup restore replaces the permanent history. Re-seed this
  // locally paged view from the optimistic snapshot as soon as that mutation
  // starts, and again if it rolls back.
  useEffect(() => {
    const seed = (Array.isArray(history) ? history : []).filter(Boolean);
    idsRef.current = new Set(seed.map((entry) => entry.id));
    setDraft("");
    setQuery("");
    setEntries(seed);
    setTotal(Number(historyTotal) || 0);
    setHasMore(idsRef.current.size < (Number(historyTotal) || 0));
    setLoading(false);
    setError("");
  }, [historyRevision]);

  // Keep the counter in sync with the worker (new fills bump it; a reset
  // drops it to zero and clears whatever was loaded, search included).
  useEffect(() => {
    const nextTotal = Number(historyTotal);
    if (!Number.isFinite(nextTotal)) return;
    if (query) {
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
  }, [historyTotal, query]);

  // Live socket entries are prepended so the newest loot stays on screen even
  // while older pages are loaded below it.
  useEffect(() => {
    const list = Array.isArray(history) ? history : [];
    if (list.length === 0) return;
    const fresh = query
      ? []
      : list.filter((entry) => entry && !idsRef.current.has(entry.id));
    for (const entry of fresh) idsRef.current.add(entry.id);
    const latestById = new Map(list.filter(Boolean).map((entry) => [entry.id, entry]));
    setEntries((previous) => {
      const updated = previous.map((entry) => {
        const latest = latestById.get(entry.id);
        return latest ? { ...entry, ...latest } : entry;
      });
      return [...fresh, ...updated].slice(0, LOOT_MAX_ROWS);
    });
  }, [history, query]);

  // Server-side search over the whole permanent log (product name / item id).
  useEffect(() => {
    if (!query) return;
    let cancelled = false;
    idsRef.current = new Set();
    setEntries([]);
    setTotal(0);
    setHasMore(false);
    setError("");
    setLoading(true);
    const params = new URLSearchParams({ q: query, limit: String(LOOT_PAGE_SIZE) });
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
  }, [query, request]);

  const loadOlder = useCallback(async () => {
    if (loading || !hasMore) return;
    const oldest = entries[entries.length - 1];
    const params = new URLSearchParams({ limit: String(LOOT_PAGE_SIZE) });
    if (query) params.set("q", query);
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
  }, [entries, hasMore, loading, query, request]);

  const resetLog = async () => {
    const didReset = await onReset();
    if (didReset === false) return;
    setDraft("");
    setQuery("");
    idsRef.current = new Set();
    setEntries([]);
    setTotal(0);
    setHasMore(false);
    setError("");
  };

  const shown = entries.length;
  const trimmed = shown < total;
  const emptyTitle = error ? "Loot log unavailable" : query ? "No matching loot" : "No loot yet";
  const emptyHint = error
    ? error
    : query
      ? `Nothing in the permanent log matches “${query}”.`
      : "Confirmed fills will drop here with profit and latency.";
  const latestDetail = detail
    ? entries.find((entry) => entry.id === detail.id) || detail
    : null;

  return (
    <>
    <section className="panel flex h-[380px] min-w-0 flex-col max-xl:order-1 xl:col-start-1 xl:row-start-1" data-accent="gold">
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
        <div className="ml-auto flex w-full items-center gap-2 sm:w-auto">
          <div className="relative min-w-0 flex-1 sm:w-56 sm:flex-none">
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
          <ul className="min-h-0 flex-1 divide-y divide-line/40 overflow-y-auto overscroll-contain md:hidden">
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
                    {formatTime(entry.at)} · {formatInt(entry.qty)} × {fixedPrice(entry.unitPrice)} RLT
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
          <div className="hidden min-h-0 min-w-0 flex-1 overflow-auto overscroll-contain md:block">
            <table className="w-full min-w-[760px] border-collapse text-left font-mono text-[13px] tabular-nums">
              <thead className="sticky top-0 z-10 bg-night-900">
                <tr className="border-b-2 border-line text-[12px] uppercase tracking-[0.08em] text-faint">
                  <th scope="col" className="px-4 py-2 font-medium">Time</th>
                  <th scope="col" className="px-3 py-2 font-medium">Item</th>
                  <th scope="col" className="px-3 py-2 font-medium">Qty</th>
                  <th scope="col" className="px-3 py-2 font-medium">Unit</th>
                  <th scope="col" className="px-3 py-2 font-medium">Total</th>
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
                    <td className="px-3 py-2 text-ink-dim">{formatInt(entry.qty)}</td>
                    <td className="px-3 py-2 text-ink-dim">{fixedPrice(entry.unitPrice)}</td>
                    <td className="px-3 py-2 text-ink-dim">{fixedPrice(entry.totalPrice)}</td>
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
        </>
      )}
      <div className="flex shrink-0 items-center justify-between gap-2 border-t-2 border-line px-3 py-2">
        <span className="min-w-0 truncate font-mono text-[12px] uppercase tracking-[0.08em] text-faint">
          {query
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
              className="btn btn-ghost btn-sm"
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
      <LootDetailModal entry={latestDetail} onClose={() => setDetail(null)} />
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
  const shown = paused ? frozen : logs;
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
            className={`btn btn-sm ${isTailing ? "btn-signal" : ""}`}
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

function LinkPanel({ snapshot, now, worker, notify, onEditSession, onBridge, onClearSession, onRotate }) {
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
          <span>Tokens are stored in <code className="text-ink-dim">data/state.json</code> with owner-only permissions.</span>
        </p>
      </div>
    </section>
  );
}

function LatencyChannel({ label, stats }) {
  const bars = sparkBars(stats?.recent || []);
  const count = stats?.count || 0;
  const errors = stats?.errors || 0;
  const metrics = [
    ["Avg", stats?.avg],
    ["P50", stats?.p50],
    ["P95", stats?.p95, latencyClass(stats?.p95)],
    ["Min", stats?.min],
    ["Max", stats?.max],
    ["Jitter", stats?.jitter]
  ];

  return (
    <article className="lat-card @container" aria-label={`${label} latency`}>
      <div className="flex items-baseline justify-between gap-3">
        <div className="min-w-0">
          <h3 className="label truncate">{label}</h3>
          <span className="mt-1 block font-mono text-[11px] uppercase tracking-[0.08em] text-faint">
            {formatInt(count)} {count === 1 ? "sample" : "samples"}
            {errors > 0 ? <b className="text-danger"> · {formatInt(errors)} err</b> : null}
          </span>
        </div>
        <b className={`shrink-0 font-mono text-[20px] font-bold leading-none tabular-nums ${latencyClass(stats?.last) || "text-muted"}`}>
          {formatMs(stats?.last)}
        </b>
      </div>
      <div className="lat-spark mt-2.5" aria-hidden="true">
        {bars.length === 0 ? (
          <span className="lat-spark-empty">No samples yet</span>
        ) : (
          bars.map((bar, index) => (
            <i
              key={index}
              className={latencyClass(bar.value)}
              style={{ height: `${bar.height}%` }}
              title={formatMs(bar.value)}
            />
          ))
        )}
      </div>
      <dl className="mt-2.5 grid grid-cols-3 gap-1.5 @[27rem]:grid-cols-6">
        {metrics.map(([key, value, tone]) => (
          <div key={key} className="lat-metric">
            <dt>{key}</dt>
            <dd className={tone || ""}>{formatMs(value)}</dd>
          </div>
        ))}
      </dl>
      {errors > 0 ? (
        <p className="mt-2 flex items-start gap-1.5 font-mono text-[11px] leading-snug text-danger">
          <Icon name="i-alert" className="mt-px h-3 w-3 shrink-0" />
          <span className="break-words">{stats.lastError || "Upstream call failed"}</span>
        </p>
      ) : null}
    </article>
  );
}

function ConfigPanel({ snapshot, worker, notify }) {
  const latency = snapshot.latency || {};
  const channels = latency.channels || {};
  const fresh = latency.freshest || null;
  const channelOrder = ["http", "keepalive", "wsHandshake", "wsPing"];
  const [probing, setProbing] = useState(false);
  const { fetchLatency } = worker;

  // Safety net: the worker pushes latency every 5s, but poll quietly too so
  // the panel stays fresh even if the push stream is missed (e.g. an older
  // worker binary that has not been restarted since this update).
  useEffect(() => {
    if (typeof fetchLatency !== "function") return undefined;
    const timer = setInterval(() => {
      fetchLatency().catch(() => {
        /* the push stream or a reconnect will deliver it — stay silent */
      });
    }, 10_000);
    return () => clearInterval(timer);
  }, [fetchLatency]);

  const runProbe = async () => {
    setProbing(true);
    try {
      const result = await worker.refreshLatency();
      if (result?.probe?.skipped) {
        notify("info", "PROBE SKIPPED", "Keepalive probe is disabled (or mock mode).");
      } else if (result?.probe?.ok) {
        notify("success", "PROBE DONE", "Fresh upstream sample recorded.");
      } else {
        notify("warn", "PROBE FAILED", result?.probe?.error || "No response from upstream.");
      }
    } catch (error) {
      notify("error", "PROBE FAILED", error.message);
    } finally {
      setProbing(false);
    }
  };

  return (
    <section className="panel @container" data-accent="cyan">
      <div className="panel-head">
        <h2 className="panel-title"><Icon name="i-wifi" className="h-4 w-4 text-info" /> Upstream latency</h2>
        <span
          className="tag hidden shrink-0 gap-1.5 text-info @[19rem]:inline-flex"
          title="The worker pushes latency every 5 seconds; this panel polls it as a fallback"
        >
          <span className="led led-pulse" aria-hidden="true" /> Auto · 5s
        </span>
      </div>
      <div className="panel-body">
        <div className="flex flex-wrap items-end justify-between gap-3 border-2 border-line bg-night-950/70 p-3">
          <div className="min-w-0">
            <span className="label flex items-center gap-1.5"><Icon name="i-wifi" className="h-3 w-3" /> Worker → upstream</span>
            <div className="mt-1 flex flex-wrap items-baseline gap-x-2 gap-y-1">
              <b className={`font-mono text-[28px] font-bold leading-none tabular-nums ${fresh ? latencyClass(fresh.last) : "text-muted"}`}>
                {fresh ? formatMs(fresh.last) : "—"}
              </b>
              <span className="min-w-0 truncate font-mono text-[11px] uppercase tracking-[0.08em] text-faint">
                {fresh ? `${latencyChannelLabel(fresh.channel)} · ${relTime(fresh.lastAt)}` : "Waiting for the first sample"}
              </span>
            </div>
          </div>
          <button className="btn btn-sm" type="button" onClick={runProbe} disabled={probing} data-busy={probing ? "true" : undefined} aria-busy={probing}>
            {probing ? "Probing…" : "Probe now"}
          </button>
        </div>
        <div className="grid grid-cols-1 gap-2 @[37rem]:grid-cols-2">
          {channelOrder.map((key) => (
            <LatencyChannel key={key} label={latencyChannelLabel(key)} stats={channels[key] || { count: 0, recent: [] }} />
          ))}
        </div>
        <p className="text-[13px] leading-relaxed text-faint">
          HTTP covers purchases, item info and token refresh. Warm probe is the keepalive GET. WS handshake and ping cover the market feed. Failures still count because they used upstream time.
        </p>
      </div>
    </section>
  );
}

/**
 * Worker health panel. The worker samples itself every few seconds and pushes
 * a `system` event, so this panel is almost entirely a render of
 * `snapshot.system`; a slow poll is only a safety net.
 */
function SystemMeter({ label, value, detail, tone }) {
  const number = Number(value);
  const width = Number.isFinite(number) ? Math.max(0, Math.min(100, number)) : 0;
  const fill =
    tone === "text-danger" ? "bg-danger" : tone === "text-warn" ? "bg-warn" : "bg-signal";
  return (
    <div className="border-2 border-line bg-night-950/60 p-3">
      <div className="flex items-center justify-between gap-2">
        <span className="label">{label}</span>
        <b className={`font-mono text-[14px] font-semibold tabular-nums ${tone || "text-ink-dim"}`}>
          {formatPercent(value, 1)}
        </b>
      </div>
      <div className="meter meter-thin mt-2">
        <i className={fill} style={{ "--meter-value": `${width}%` }} />
      </div>
      <span className="mt-1.5 block font-mono text-[12px] uppercase tracking-[0.08em] text-faint">
        {detail}
      </span>
    </div>
  );
}

function StatTile({ label, value, tone, meta }) {
  return (
    <span className="flex min-w-0 flex-col gap-0.5 border border-line/70 bg-night-900/60 px-1.5 py-1 font-mono text-[12px] uppercase tracking-[0.08em] text-faint">
      <span className="truncate leading-tight">{label}</span>
      <b className={`whitespace-nowrap text-[12px] font-semibold leading-tight tracking-normal tabular-nums ${tone || "text-ink-dim"}`}>
        {value}
      </b>
      {meta ? <span className="whitespace-nowrap text-[11px] leading-tight tracking-[0.04em]">{meta}</span> : null}
    </span>
  );
}

function HistoryStrip({ label, bars, format, tone }) {
  return (
    <div className="@container border-2 border-line bg-night-950/60 p-3">
      <div className="flex items-center justify-between gap-2">
        <span className="label min-w-0 whitespace-nowrap">{label}</span>
        <span className="hidden shrink-0 whitespace-nowrap font-mono text-[12px] uppercase tracking-[0.08em] text-faint @[20rem]:inline">
          {bars.length} samples
        </span>
      </div>
      <div className="mt-2 flex h-10 items-end gap-[2px]" aria-hidden="true">
        {bars.length === 0 ? (
          <span className="font-mono text-[12px] uppercase tracking-wide text-faint">Collecting…</span>
        ) : (
          bars.map((bar, index) => (
            <i
              key={index}
              className={`max-w-[10px] min-w-[2px] flex-1 bg-current ${tone ? tone(bar.value) : "text-signal"}`}
              style={{ height: `${bar.height}%` }}
              title={format(bar.value)}
            />
          ))
        )}
      </div>
    </div>
  );
}

function SystemPanel({ snapshot, worker }) {
  const system = snapshot.system || null;
  const health = system?.health || { level: "ok", reasons: [] };
  const proc = system?.process || {};
  const cpu = system?.cpu || {};
  const memory = system?.memory || {};
  const host = system?.system || {};
  const loop = system?.eventLoop || {};
  const lag = loop.lag || {};
  const history = system?.history || [];
  const db = system?.db || null;
  const dbFiles = db?.files || {};
  const dbStorage = db?.storage || {};
  const dbSqlite = db?.sqlite || {};
  const dbQueries = db?.queries || {};
  const dbWindow = db?.window || {};
  const dbTransactions = db?.transactions || {};
  const dbWarn = Number(db?.thresholds?.p95WarnMs) || 25;
  const dbCritical = Number(db?.thresholds?.p95CriticalMs) || 100;

  // Safety net: the worker pushes `system` alongside gateway/latency every 5s.
  // Quietly poll as well so the panel survives a missed push after a reconnect.
  // Depend on the stable callback, not the whole worker object: the deck
  // re-renders every second and would otherwise restart this timer forever.
  const fetchSystem = worker?.fetchSystem;
  useEffect(() => {
    if (typeof fetchSystem !== "function") return undefined;
    const timer = setInterval(() => {
      fetchSystem().catch(() => {
        /* a reconnect or the push stream will refresh it */
      });
    }, 10_000);
    return () => clearInterval(timer);
  }, [fetchSystem]);

  if (!system) {
    return (
      <section className="panel" data-accent="lime">
        <div className="panel-head">
          <h2 className="panel-title"><Icon name="i-bolt" className="h-4 w-4 text-signal" /> Worker health</h2>
        </div>
        <div className="panel-body">
          <p className="text-[13px] leading-relaxed text-faint">
            Waiting for the first metrics sample. The worker samples CPU, memory and event-loop
            health every few seconds; make sure it is running the latest build.
          </p>
        </div>
      </section>
    );
  }

  const healthLabel = health.level === "critical" ? "CRITICAL" : health.level === "warn" ? "DEGRADED" : "HEALTHY";
  const healthChip =
    health.level === "critical" ? "chip-offline" : health.level === "warn" ? "chip-warn" : "chip-online";
  const heapTone = utilizationClass(memory.heapPercent, 80, 92);
  const hostTone = utilizationClass(host.usedPercent, 88, 96);
  const cpuTone = thresholdClass(cpu.processPercent, 80, 100);
  const loadPerCore = Number.isFinite(Number(cpu.load1)) && cpu.cores ? cpu.load1 / cpu.cores : null;

  return (
    <section className="panel @container" data-accent="lime">
      <div className="panel-head">
        <h2 className="panel-title"><Icon name="i-bolt" className="h-4 w-4 text-signal" /> Worker health</h2>
        <span className={`chip ${healthChip}`} title={health.reasons?.join(" · ") || "All health checks passed"}>
          <span className="led led-pulse" />{healthLabel}
        </span>
      </div>
      <div className="panel-body">
        <div className="flex flex-wrap items-center justify-between gap-2">
          <span className="label flex items-center gap-1.5">
            <Icon name="i-clock" className="h-3 w-3" /> Uptime {formatUptime(proc.uptimeMs)}
          </span>
          <span className="font-mono text-[12px] uppercase tracking-[0.08em] text-faint">
            PID {proc.pid} · {proc.runtime || proc.node}
          </span>
        </div>

        {health.level !== "ok" ? (
          <p className="border-2 border-line bg-night-950/60 px-3 py-2 font-mono text-[12px] uppercase tracking-[0.08em] text-warn">
            {health.reasons?.length ? health.reasons.join(" · ") : "Health check degraded"}
          </p>
        ) : null}

        <div className="grid grid-cols-1 gap-2 @2xl:grid-cols-3">
          <SystemMeter
            label="Worker CPU"
            value={cpu.processPercent}
            detail={`${formatInt(cpu.cores)} cores · host ${formatPercent(cpu.hostPercent, 1)}`}
            tone={cpuTone}
          />
          <SystemMeter
            label="Heap"
            value={memory.heapPercent}
            detail={`${formatBytes(memory.heapUsed)} / ${formatBytes(memory.heapLimit)}`}
            tone={heapTone}
          />
          <SystemMeter
            label="System RAM"
            value={host.usedPercent}
            detail={`${formatBytes(host.used)} / ${formatBytes(host.total)}`}
            tone={hostTone}
          />
        </div>

        <div className="grid grid-cols-3 gap-1.5 @xl:grid-cols-6">
          {[
            ["RSS", formatBytes(memory.rss), "text-ink-dim"],
            ["External", formatBytes(memory.external), "text-ink-dim"],
            ["Free RAM", formatBytes(host.available), thresholdClass(host.usedPercent, 88, 96)],
            ["Load 1m", Number.isFinite(Number(cpu.load1)) ? Number(cpu.load1).toFixed(2) : "—", thresholdClass(loadPerCore ? loadPerCore * 100 : null, 70, 95)],
            ["Loop p95", formatMs(lag.p95), thresholdClass(lag.p95, 100, 250)],
            ["Handles", proc.activeHandles == null ? "—" : formatInt(proc.activeHandles), "text-ink-dim"]
          ].map(([label, value, tone]) => (
            <StatTile key={label} label={label} value={value} tone={tone} />
          ))}
        </div>

        <div className="border-2 border-line bg-night-950/60 p-3">
          <div className="flex flex-wrap items-center justify-between gap-2">
            <span className="label flex items-center gap-1.5">
              <Icon name="i-download" className="h-3 w-3" /> SQLite · {dbSqlite.driver || "—"} ·{" "}
              {dbSqlite.journalMode || "—"}
            </span>
            <span
              className={`font-mono text-[12px] uppercase tracking-[0.08em] ${
                db?.instrumented ? "text-signal" : "text-faint"
              }`}
              title="Statements are timed through wrapped prepare/transaction calls"
            >
              {db?.instrumented ? "instrumented" : "not instrumented"}
            </span>
          </div>
          <div className="mt-2 grid grid-cols-2 gap-1.5 @xs:grid-cols-4">
            {[
              ["DB", formatBytes(dbFiles.db), "text-ink-dim"],
              ["WAL", formatBytes(dbFiles.wal), thresholdClass(dbFiles.wal, db?.thresholds?.walWarnBytes, db?.thresholds?.walCriticalBytes)],
              ["Q/S", Number(dbQueries.rate).toFixed(1), "text-violet"],
              ["P95", formatPreciseMs(dbWindow.p95Ms), thresholdClass(dbWindow.p95Ms, dbWarn, dbCritical)],
              ["AVG", formatPreciseMs(dbWindow.avgMs), "text-ink-dim"],
              ["MAX", formatPreciseMs(dbWindow.maxMs), thresholdClass(dbWindow.maxMs, dbWarn * 4, dbCritical)],
              ["TX", formatInt(dbTransactions.count || 0), "text-ink-dim"],
              [
                "ERR",
                formatInt((dbWindow.errors || 0) + (dbWindow.busy || 0)),
                dbWindow.errors + dbWindow.busy > 0 ? "text-danger" : "text-ink-dim"
              ]
            ].map(([label, value, tone]) => (
              <StatTile key={label} label={label} value={value} tone={tone} />
            ))}
          </div>
          {db?.recentSlow?.length ? (
            <div className="mt-2 space-y-1">
              {db.recentSlow.slice(0, 2).map((item, index) => (
                <div
                  key={`${item.at}-${index}`}
                  className="flex items-baseline gap-2 font-mono text-[11px] uppercase tracking-[0.06em]"
                >
                  <b className="shrink-0 text-warn">{formatPreciseMs(item.ms)}</b>
                  <code className="truncate text-faint" title={item.sql}>
                    {item.sql}
                  </code>
                </div>
              ))}
            </div>
          ) : (
            <span className="mt-1.5 block font-mono text-[11px] uppercase tracking-[0.08em] text-faint">
              {formatInt(dbWindow.count || 0)} statements · no slow queries in the last 60s
            </span>
          )}
        </div>

        <div className="grid grid-cols-1 gap-2 @xs:grid-cols-2">
          <HistoryStrip
            label="CPU"
            bars={sparkBars(history.map((point) => point.cpu ?? 0))}
            format={(value) => formatPercent(value, 1)}
            tone={(value) => thresholdClass(value, 80, 100)}
          />
          <HistoryStrip
            label="RSS"
            bars={sparkBars(history.map((point) => point.rss ?? 0))}
            format={(value) => formatBytes(value * 1024 * 1024)}
            tone={() => "text-info"}
          />
          <HistoryStrip
            label="SQL"
            bars={sparkBars((db?.history || []).map((point) => point.qps ?? 0))}
            format={(value) => `${Number(value).toFixed(1)}/s`}
            tone={() => "text-violet"}
          />
        </div>

        <p className="text-[13px] leading-relaxed text-faint">
          Sampled every {formatInt(Math.round((system.intervalMs || 2000) / 1000)) || 1}s, pushed every
          5s. CPU is the worker process as a percentage of one core; system RAM uses MemAvailable on
          Linux. Warnings trigger at 80% heap, 88% RAM, 100ms event-loop p95, a DB p95 above{" "}
          {formatInt(dbWarn)}ms, or a WAL above{" "}
          {formatBytes(db?.thresholds?.walWarnBytes || 64 * 1024 * 1024)}. Query timings include only
          the SQL text, never bound values.
        </p>
      </div>
    </section>
  );
}

function RuntimeConfigPanel({ snapshot, worker, notify, onReset, onReconnect, onBackup, baseUrl }) {
  const settings = snapshot.settings || {};
  const [delay, setDelay] = useState(String(settings.delayPerOrder ?? 200));
  const [maxOrders, setMaxOrders] = useState(String(settings.maxOrdersPerMinute ?? 60));
  const [saving, setSaving] = useState(false);
  const [reconnecting, setReconnecting] = useState(false);

  useEffect(() => {
    setDelay(String(settings.delayPerOrder ?? 200));
    setMaxOrders(String(settings.maxOrdersPerMinute ?? 60));
  }, [settings.delayPerOrder, settings.maxOrdersPerMinute]);

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

  const toggle = async (patch, label) => {
    try {
      await worker.saveSettings(patch);
    } catch (error) {
      notify("error", "CONFIG FAILED", error.message);
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
    <section className="panel" data-accent="pink">
      <div className="panel-head">
        <h2 className="panel-title"><Icon name="i-gear" className="h-4 w-4 text-pink" /> Runtime config</h2>
      </div>
      <div className="panel-body">
        <form className="space-y-3" onSubmit={saveNumbers}>
          <div className="grid grid-cols-1 gap-3 sm:grid-cols-2">
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

        <label className="toggle-row">
          <span>Dry run<small>Log signals but never send orders</small></span>
          <input type="checkbox" className="sr-only" checked={Boolean(settings.dryRun)} onChange={(event) => toggle({ dryRun: event.target.checked })} />
          <span className="switch" aria-hidden="true" />
        </label>
        <label className="toggle-row">
          <span>Paused<small>Keep listening, stop all buying</small></span>
          <input type="checkbox" className="sr-only" checked={Boolean(settings.paused)} onChange={(event) => toggle({ paused: event.target.checked })} />
          <span className="switch" aria-hidden="true" />
        </label>

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

function SidePanel({ tab, setTab, snapshot, logs, now, worker, notify, onEditSession, onBridge, onClearSession, onRotate, onReset, onReconnect, onBackup, baseUrl }) {
  const tabs = [
    { id: "feed", label: "FEED" },
    { id: "link", label: "LINK" },
    { id: "config", label: "CONFIG" },
    { id: "latency", label: "LATENCY" },
    { id: "system", label: "SYSTEM" }
  ];
  return (
    <aside className="flex min-w-0 flex-col gap-3 max-xl:order-4 xl:sticky xl:top-24 xl:col-start-2 xl:row-start-1 xl:row-span-3 xl:max-h-[calc(100dvh-6rem)] xl:self-start xl:overflow-y-auto">
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
        {tab === "feed" ? <FeedPanel logs={logs} onClear={worker.clearLogs} notify={notify} /> : null}
        {tab === "link" ? (
          <LinkPanel
            snapshot={snapshot}
            now={now}
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
          />
        ) : null}
        {tab === "latency" ? (
          <ConfigPanel
            snapshot={snapshot}
            worker={worker}
            notify={notify}
          />
        ) : null}
        {tab === "system" ? <SystemPanel snapshot={snapshot} worker={worker} /> : null}
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

function Dashboard({ worker, notify, toasts, dismissToast }) {
  const { snapshot, logs, status, latency } = worker;
  const [tab, setTab] = useState("feed");
  const [search, setSearch] = useState("");
  const [typeFilter, setTypeFilter] = useState("all");
  const [modal, setModal] = useState(null);
  const [confirmState, setConfirmState] = useState(null);
  const [soundOn, setSoundOn] = useState(sound.enabled);
  const [pausing, setPausing] = useState(false);
  const [now, setNow] = useState(Date.now());

  useEffect(() => {
    const timer = setInterval(() => setNow(Date.now()), 1000);
    return () => clearInterval(timer);
  }, []);

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

  const toggleTarget = async (config, active) => {
    try {
      await worker.toggleTarget(config.itemId, active);
      notify("info", active ? "TARGET ARMED" : "TARGET DISARMED", config.label || shortId(config.itemId));
    } catch (error) {
      notify("error", "ARM FAILED", error.message);
    }
  };

  const togglePause = async () => {
    setPausing(true);
    try {
      const result = await worker.control("toggle");
      const paused = Boolean(result?.settings?.paused);
      notify(
        paused ? "warn" : "success",
        paused ? "BOT PAUSED" : "BOT STARTED",
        paused ? "All buying is paused; the market feed stays live." : "The sniper is armed and hunting again."
      );
    } catch (error) {
      notify("error", "CONTROL FAILED", error.message);
    } finally {
      setPausing(false);
    }
  };

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

  const deleteTarget = async (config) => {
    const confirmed = await askConfirm({
      title: "DELETE TARGET",
      message: `Remove ${config.label || shortId(config.itemId)} from the hunt list? Open orders are not cancelled.`,
      confirmLabel: "DELETE TARGET",
      danger: true
    });
    if (!confirmed) return;
    try {
      await worker.deleteTarget(config.itemId);
      notify("warn", "TARGET REMOVED", config.label || shortId(config.itemId));
    } catch (error) {
      notify("error", "DELETE FAILED", error.message);
    }
  };

  const resetStats = async () => {
    const confirmed = await askConfirm({
      title: "RESET LIFETIME STATS",
      message: "Wipe profit, fill counters, per-target success rates and the entire loot log? Targets and sessions are not touched.",
      confirmLabel: "RESET STATS",
      danger: true
    });
    if (!confirmed) return false;
    try {
      await worker.resetStats();
      notify("warn", "STATS RESET", "Starting a fresh scoreboard.");
      return true;
    } catch (error) {
      notify("error", "RESET FAILED", error.message);
      return false;
    }
  };

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
        now={now}
        soundOn={soundOn}
        onSound={toggleSound}
        onLogout={worker.logout}
        onTogglePause={togglePause}
        pausing={pausing}
      />
      <Ticker configs={snapshot.configs || []} mock={snapshot.worker?.mock} />
      <div className="deck-enter deck-enter-2 mx-auto w-full max-w-[1600px] px-3 pt-4 sm:px-4 lg:px-6">
        <Hud snapshot={snapshot} now={now} eps={eps} />
      </div>
      <div className="deck-enter deck-enter-3 mx-auto mt-4 w-full max-w-[1600px] px-3 sm:px-4 lg:px-6">
        <TrendPanel
          configs={snapshot.configs || []}
          onEdit={(config) => setModal({ type: "target", config })}
        />
      </div>
      <main className="deck-enter deck-enter-3 mx-auto mt-4 grid w-full max-w-[1600px] gap-4 px-3 pb-10 sm:px-4 lg:px-6 xl:grid-cols-[minmax(0,1fr)_380px] xl:items-start">
        <LootLog
          history={snapshot.history || []}
          historyTotal={snapshot.historyTotal}
          historyRevision={worker.historyRevision}
          onReset={resetStats}
          request={worker.request}
        />
        <ReportPanel report={snapshot.report} notify={notify} />
        <TargetsPanel
          configs={snapshot.configs || []}
          reportByItem={reportByItem}
          search={search}
          typeFilter={typeFilter}
          onSearch={setSearch}
          onTypeFilter={setTypeFilter}
          onAdd={() => setModal({ type: "target", config: null })}
          onEdit={(config) => setModal({ type: "target", config })}
          onDelete={deleteTarget}
          onToggle={toggleTarget}
          notify={notify}
        />
        <SidePanel
          tab={tab}
          setTab={setTab}
          snapshot={snapshot}
          logs={logs}
          now={now}
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
        />
      </main>
      <div className="pixel-stripe mt-8" aria-hidden="true" />
      <footer className="border-t-2 border-line bg-night-950/70">
        <div className="mx-auto flex w-full max-w-[1600px] flex-col gap-1 px-3 py-4 font-mono text-[12px] uppercase tracking-[0.08em] text-faint sm:flex-row sm:items-center sm:justify-between sm:px-4 lg:px-6">
          <span>
            <span id="footer-mode" className="text-gold">{snapshot.worker?.mock ? "MOCK" : "LIVE"}</span> · WORKER v
            <span id="footer-version">{snapshot.version || "1.0.0"}</span>
          </span>
          <span className="text-violet">Original bot by @nopaleon.0f</span>
          <span>Not affiliated with Rollercoin</span>
        </div>
      </footer>

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
