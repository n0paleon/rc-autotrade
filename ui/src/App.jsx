import { memo, useCallback, useEffect, useMemo, useRef, useState } from "react";
import PriceChart from "./components/PriceChart.jsx";
import { Icon, ModalShell } from "./components/ModalShell.jsx";
import {
  BridgeModal,
  ConfirmModal,
  KeyModal,
  SessionModal,
  TargetModal,
  copyText
} from "./components/Modals.jsx";
import { useWorker, URL_KEY, TOKEN_KEY } from "./useWorker.js";
import { useToasts } from "./useToasts.js";
import {
  cardState,
  fixedPrice,
  formatCountdown,
  formatInt,
  formatRlt,
  formatSignedRlt,
  formatTime,
  formatUptime,
  isRecent,
  lastText,
  relTime,
  runtimeHot,
  shortId,
  stateLabel,
  successStats,
  drawPixelAvatar,
  formatMs,
  latencyClass,
  sparkBars
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
      cls = "is-danger";
    } else if (session.expiresAt && remaining <= 0) {
      value = "EXPIRED";
      sub = "PASTE A FRESH SESSION";
      width = 100;
      cls = "is-danger";
    } else if (worker.mock && session.source === "mock") {
      value = "SIMULATED";
      sub = "MOCK TOKEN ACTIVE";
      width = 100;
      cls = "is-hot";
    } else {
      const expiring = remaining != null && remaining <= 10 * 60_000;
      value = expiring ? "EXPIRING" : "LINKED";
      sub = remaining != null ? `REFRESH IN ${formatCountdown(remaining)}` : "AUTO REFRESH ON";
      width = remaining != null ? Math.max(2, Math.min(100, (remaining / 3_600_000) * 100)) : 100;
      cls = expiring ? "is-warn" : "is-hot";
    }
    if (!settings.autoRefresh && !session.lastError) {
      sub = remaining != null ? `REFRESH IN ${formatCountdown(remaining)} · AUTO OFF` : "AUTO REFRESH OFF";
    }
  }

  return { value, sub, width, cls, remaining };
}

function BootScreen({ onLogin, busy, error }) {
  const [url, setUrl] = useState(() => localStorage.getItem(URL_KEY) || window.location.origin);
  const [token, setToken] = useState(() => sessionStorage.getItem(TOKEN_KEY) || "");
  const [showToken, setShowToken] = useState(false);

  const submit = (event) => {
    event.preventDefault();
    sound.ensure();
    onLogin(url, token.trim());
  };

  return (
    <section className="boot">
      <div className="boot-grid" aria-hidden="true" />
      <div className="boot-wrap">
        <div className="boot-brand">
          <div className="logo-coin" aria-hidden="true">
            <svg viewBox="0 0 16 16"><use href="#i-coin" /></svg>
          </div>
          <div>
            <p className="eyebrow">TURU.RC PROTOCOL</p>
            <h1>MARKET<br /><span>SNIPER</span></h1>
            <p className="boot-sub">PIXEL ARCADE EDITION · v1.0</p>
          </div>
        </div>

        <div className="boot-panel panel">
          <div className="boot-panel-head">
            <span className="led" />
            <span>CONTROL PLANE ACCESS</span>
            <span className="boot-blink">READY</span>
          </div>
          <form className="boot-form" autoComplete="off" onSubmit={submit}>
            <label className="field">
              <span>WORKER URL</span>
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
              <span>ACCESS KEY</span>
              <div className="input-with-action">
                <input
                  className="input"
                  type={showToken ? "text" : "password"}
                  value={token}
                  onChange={(event) => setToken(event.target.value)}
                  placeholder="paste the key printed by the worker"
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
            {error ? <div className="form-error" role="alert">{error}</div> : null}
            <button className="btn btn-primary btn-lg btn-block" type="submit" disabled={busy}>
              {busy ? "CONNECTING…" : <>PRESS START <span className="btn-arrow">▶</span></>}
            </button>
          </form>
          <div className="boot-panel-foot">
            <span><Icon name="i-shield" className="icon icon-xs" /> KEYS STAY ON THIS MACHINE</span>
            <span>WORKER v1.0</span>
          </div>
        </div>
        <p className="boot-tip">FIRST RUN? The access key is printed in the worker terminal.</p>
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
    <header className="topbar">
      <div className="brand">
        <div className="logo-coin logo-coin-sm" aria-hidden="true">
          <svg viewBox="0 0 16 16"><use href="#i-coin" /></svg>
        </div>
        <div className="brand-text">
          <h1><span className="brand-name">TURU.RC</span> <span className="brand-slash">//</span> <span className="brand-title">MARKET SNIPER</span></h1>
          <p>PIXEL ARCADE CONTROL DECK</p>
        </div>
      </div>
      <div className="topbar-status">
        <button
          className={`btn btn-sm topbar-pause ${settings.paused ? "btn-primary" : "btn-pause"}`}
          type="button"
          onClick={onTogglePause}
          disabled={pausing}
          aria-label={settings.paused ? "Start buying" : "Pause buying"}
          title={settings.paused ? "Resume all buying" : "Pause all buying; the market feed stays live"}
        >
          <Icon name={settings.paused ? "i-play" : "i-pause"} className="icon icon-xs" />
          <span>{pausing ? "…" : settings.paused ? "START" : "PAUSE"}</span>
        </button>
        <span id="trading-state" className={tradingClass}><span className="led" />{trading}</span>
        <span id="conn-chip" className={`chip ${connClass}`} title={snapshot.gateway?.lastError || "Control-plane link status"}>
          <span className="led" />{connLabel}
        </span>
        <span className="topbar-latency" title="Control link round-trip">
          <Icon name="i-wifi" className="icon icon-xs" />
          <span>{latency != null ? `${latency}ms` : "--"}</span>
        </span>
        <span className="topbar-clock" title="Local time">
          <Icon name="i-clock" className="icon icon-xs" />
          <span>{formatTime(now)}</span>
        </span>
        <button
          className="icon-btn"
          type="button"
          aria-label={soundOn ? "Mute sound" : "Enable sound"}
          aria-pressed={soundOn}
          title="Sound effects"
          onClick={onSound}
        >
          <Icon name={soundOn ? "i-sound" : "i-mute"} />
        </button>
        <button className="icon-btn icon-btn-danger" type="button" aria-label="Lock control deck" title="Lock deck" onClick={onLogout}>
          <Icon name="i-power" />
        </button>
      </div>
    </header>
  );
}

function Ticker({ configs, mock }) {
  const halves = [0, 1];
  return (
    <div className="ticker-bar" aria-label="Market wire">
      <div className="ticker-label"><span className="ticker-pip" />MARKET WIRE</div>
      <div className="ticker-track">
        <div className="ticker-content" style={{ animationDuration: `${Math.max(26, configs.length * 8)}s` }}>
          {halves.map((half) => (
            <div className="ticker-half" key={half}>
              {configs.length === 0 ? (
                <span className="ticker-empty">NO TARGETS DEPLOYED — THE MARKET IS QUIET</span>
              ) : (
                configs.map((config) => {
                  const runtime = config.runtime || {};
                  const hot = runtimeHot(config);
                  return (
                    <span className={`ticker-item${hot ? " is-hot" : ""}`} key={`${half}-${config.itemId}`}>
                      <span className="t-name">{config.label || `ITEM ${shortId(config.itemId)}`}</span>
                      <span className="t-price">{Number.isFinite(runtime.lastPrice) ? `${fixedPrice(runtime.lastPrice)} RLT` : "—"}</span>
                      <span className="t-state">{Number.isFinite(runtime.lastStock) ? `BOOK ${formatInt(runtime.lastStock)}` : "WAITING"}</span>
                    </span>
                  );
                })
              )}
            </div>
          ))}
        </div>
      </div>
      <div className={`ticker-mode${mock ? " is-mock" : ""}`}>{mock ? "MOCK" : "LIVE"}</div>
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

  return (
    <section className="hud" aria-label="Session stats">
      <article className="stat stat-profit">
        <div className="stat-top"><span className="stat-label">PROFIT</span><Icon name="i-coin" className="icon stat-icon" /></div>
        <div className="stat-value" id="stat-profit">{formatRlt(stats.profitRlt)}</div>
        <div className="stat-sub" id="stat-profit-sub">RLT · {formatInt(stats.bought)} ITEMS LOOTED</div>
        <div className="stat-spark" aria-hidden="true" />
      </article>
      <article className="stat">
        <div className="stat-top"><span className="stat-label">LOOT SECURED</span><Icon name="i-target" className="icon stat-icon" /></div>
        <div className="stat-value" id="stat-filled">{formatInt(stats.filled)}</div>
        <div className="stat-sub"><span id="stat-failed">{formatInt(stats.failed)}</span> MISSED · <span id="stat-bought">{formatInt(stats.bought)}</span> ITEMS</div>
      </article>
      <article className="stat">
        <div className="stat-top"><span className="stat-label">TARGETS</span><Icon name="i-bolt" className="icon stat-icon" /></div>
        <div className="stat-value" id="stat-targets">{formatInt(configs.length)}</div>
        <div className="stat-sub"><span id="stat-armed">{formatInt(armed)}</span> ARMED · <span id="stat-hot">{formatInt(hot)}</span> IN RANGE</div>
      </article>
      <article className="stat">
        <div className="stat-top"><span className="stat-label">MARKET EVENTS</span><Icon name="i-wifi" className="icon stat-icon" /></div>
        <div className="stat-value" id="stat-events">{formatInt(gateway.eventCount || 0)}</div>
        <div className="stat-sub"><span id="stat-eps">{eps.toFixed(1)}</span>/S · <span id="stat-uptime">{formatUptime(now - (snapshot.startedAt || now))}</span></div>
      </article>
      <article className="stat stat-session">
        <div className="stat-top"><span className="stat-label">SESSION</span><Icon name="i-link" className="icon stat-icon" /></div>
        <div className="stat-value stat-value-sm" id="stat-session">{session.value}</div>
        <div className="meter meter-thin"><i id="stat-session-meter" className={session.cls} style={{ width: `${session.width}%` }} /></div>
        <div className="stat-sub" id="stat-session-sub">{session.sub}</div>
      </article>
      <article className="stat stat-upstream">
        <div className="stat-top"><span className="stat-label">UPSTREAM</span><Icon name="i-wifi" className="icon stat-icon" /></div>
        <div className={`stat-value ${fresh ? latencyClass(fresh.last) : ""}`} id="stat-upstream">
          {fresh ? formatMs(fresh.last) : "—"}
        </div>
        <div className="stat-sub" id="stat-upstream-sub">
          {fresh ? `${fresh.channel.toUpperCase()} · ${relTime(fresh.lastAt)}` : "NO SAMPLES YET"}
        </div>
      </article>
    </section>
  );
}

function TargetCard({ config, onEdit, onDelete, onToggle, notify }) {
  const canvasRef = useRef(null);
  const [toggling, setToggling] = useState(false);
  const runtime = config.runtime || {};
  const state = cardState(config);
  const stats = successStats(config);
  const label = config.label || runtime.label || `ITEM ${shortId(config.itemId)}`;
  const hot = runtimeHot(config);
  const rate = stats.rate;

  useEffect(() => {
    drawPixelAvatar(canvasRef.current, config.itemId, config.rarityColor);
  }, [config.itemId, config.rarityColor]);

  const successClass = rate == null ? "" : rate >= 80 ? "is-hot" : rate >= 50 ? "is-warn" : "is-danger";

  return (
    <article
      className="target-card"
      data-state={state}
      data-item-id={config.itemId}
      aria-label={`${label}, ${state}`}
      style={config.rarityColor ? { "--rarity": `#${config.rarityColor}` } : undefined}
    >
      <div className="card-top">
        <canvas ref={canvasRef} className="avatar" width="8" height="8" aria-hidden="true" />
        <div className="card-ident">
          <h3 className="card-name">{label}</h3>
          <button
            className="id-copy"
            type="button"
            title="Copy item ID"
            onClick={async () => {
              const ok = await copyText(config.itemId);
              notify(ok ? "info" : "error", ok ? "ITEM ID COPIED" : "COPY FAILED", config.itemId);
            }}
          >
            <code>{shortId(config.itemId)}</code>
            <Icon name="i-copy" />
          </button>
        </div>
        <div className="card-controls">
          <span className="state-chip" data-state={state}>{stateLabel(state)}</span>
          <label className="mini-switch" title="Arm or disarm this target">
            <input
              type="checkbox"
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
            <span />
          </label>
        </div>
      </div>

      <div className="card-quote">
        <div className="quote">
          <span className="quote-label">MARKET</span>
          <strong className={`quote-value${hot ? " is-hot" : ""}`}>
            {Number.isFinite(runtime.lastPrice) ? fixedPrice(runtime.lastPrice) : "—"}
          </strong>
          <span className="quote-unit">RLT</span>
        </div>
        <div className="quote quote-right">
          <span className="quote-label">TARGET</span>
          <strong className="quote-value">{fixedPrice(config.targetPrice)}</strong>
          <span className="quote-unit">RLT</span>
        </div>
      </div>

      <MemoChart history={runtime.history || []} targetPrice={config.targetPrice} />

      <div
        className="success-block"
        title="Successful fills divided by completed purchase attempts (fills + failures) for this target in this worker session."
      >
        <div className="success-top">
          <span className="success-label">PURCHASE SUCCESS</span>
          <span className={`success-value${successClass ? ` ${successClass}` : ""}`}>{rate == null ? "—" : `${rate}%`}</span>
        </div>
        <div className="meter success-meter">
          <i className={`${successClass}`.trim()} style={{ width: `${rate ?? 0}%` }} />
        </div>
        <div className="success-foot">
          <span>{stats.completed ? `${formatInt(stats.fills)} FILLED · ${formatInt(stats.failures)} FAILED` : "NO COMPLETED ORDERS"}</span>
          <span className="js-success-ratio">{stats.fills}/{stats.completed}</span>
        </div>
      </div>

      <div className="card-tags">
        <span className="tag">QTY <b>{formatInt(config.desiredQty)}</b></span>
        <span className="tag">HIGH <b>{formatInt(config.highStockThreshold)}</b></span>
        <span className="tag">MIN <b>{formatInt(config.minAcceptableQty)}</b></span>
        <span className="tag">MAX <b>{fixedPrice(config.targetPrice * config.desiredQty * 2)}</b></span>
      </div>

      <div className="card-foot">
        <span className={`card-last${runtime.lastResult?.status === "filled" && isRecent(runtime.lastResult.at, 10_000) ? " is-win" : ""}${runtime.lastResult?.status === "rejected" && isRecent(runtime.lastResult.at, 12_000) ? " is-error" : ""}`}>
          {lastText(config)}
        </span>
        <div className="card-actions">
          <button className="icon-btn" type="button" aria-label="Edit target" title="Edit target" onClick={() => onEdit(config)}>
            <Icon name="i-edit" />
          </button>
          <button className="icon-btn icon-btn-danger" type="button" aria-label="Delete target" title="Delete target" onClick={() => onDelete(config)}>
            <Icon name="i-trash" />
          </button>
        </div>
      </div>
    </article>
  );
}

function TargetsPanel({ configs, search, onSearch, onAdd, onEdit, onDelete, onToggle, notify }) {
  const query = search.trim().toLowerCase();
  const filtered = query
    ? configs.filter(
        (config) =>
          (config.label || "").toLowerCase().includes(query) ||
          config.itemId.toLowerCase().includes(query)
      )
    : configs;

  return (
    <section className="panel targets-panel">
      <div className="panel-head">
        <h2><Icon name="i-target" /> TARGET LIST <span className="badge">{configs.length}</span></h2>
        <div className="panel-actions">
          <div className="search-wrap">
            <input
              className="input input-sm"
              type="search"
              value={search}
              onChange={(event) => onSearch(event.target.value)}
              placeholder="SEARCH TARGETS"
              aria-label="Search targets"
            />
          </div>
          <button className="btn btn-primary btn-sm" type="button" onClick={onAdd}>
            <Icon name="i-plus" className="icon icon-xs" /> ADD TARGET
          </button>
        </div>
      </div>

      {configs.length === 0 ? (
        <div className="empty">
          <Icon name="i-target" className="empty-icon" />
          <p className="empty-title">NO TARGETS DEPLOYED</p>
          <p className="empty-text">Add an item ID, set your price ceiling, and the worker will hunt the market for you.</p>
          <button className="btn btn-primary" type="button" onClick={onAdd}>
            <Icon name="i-plus" className="icon icon-xs" /> DEPLOY FIRST TARGET
          </button>
        </div>
      ) : (
        <div className="targets-grid">
          {filtered.length === 0 ? (
            <div className="empty" style={{ gridColumn: "1 / -1" }}>
              <p className="empty-title">NO MATCHES</p>
              <p className="empty-text">Nothing matches “{search}”. Try another label or item ID.</p>
            </div>
          ) : (
            filtered.map((config) => (
              <TargetCard
                key={config.itemId}
                config={config}
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

function LootLog({ history, onReset }) {
  return (
    <section className="panel history-panel">
      <div className="panel-head">
        <h2><Icon name="i-coin" /> LOOT LOG <span className="badge">{history.length}</span></h2>
        <button className="btn btn-ghost btn-sm" type="button" onClick={onReset}>
          <Icon name="i-trash" className="icon icon-xs" /> RESET
        </button>
      </div>
      {history.length === 0 ? (
        <div className="empty empty-compact">
          <Icon name="i-coin" className="empty-icon" />
          <p className="empty-title">NO LOOT YET</p>
          <p className="empty-text">Confirmed fills will drop here with profit and latency.</p>
        </div>
      ) : (
        <div className="table-wrap">
          <table className="loot-table">
            <thead>
              <tr>
                <th>TIME</th><th>ITEM</th><th>QTY</th><th>UNIT</th><th>TOTAL</th><th>PROFIT</th><th>LATENCY</th><th>STATUS</th>
              </tr>
            </thead>
            <tbody>
              {history.slice(0, 60).map((entry) => (
                <tr key={entry.id || `${entry.orderId}-${entry.at}`} title={entry.note || (entry.orderId ? `order ${entry.orderId}` : undefined)}>
                  <td className="cell-time">{formatTime(entry.at)}</td>
                  <td className="cell-item">{entry.label || shortId(entry.itemId)}</td>
                  <td>{formatInt(entry.qty)}</td>
                  <td>{fixedPrice(entry.unitPrice)}</td>
                  <td>{fixedPrice(entry.totalPrice)}</td>
                  <td className={`cell-profit ${entry.profitRlt > 0 ? "is-pos" : entry.profitRlt < 0 ? "is-neg" : ""}`}>
                    {entry.status === "failed" ? "0.000000" : formatSignedRlt(entry.profitRlt)}
                  </td>
                  <td className="cell-latency">{entry.latencyMs != null ? `${Math.round(entry.latencyMs)}ms` : "—"}</td>
                  <td><span className={`status-pill${entry.status === "failed" ? " is-failed" : ""}`}>{entry.status === "failed" ? "FAILED" : "FILLED"}</span></td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
    </section>
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
    <section className="panel tab-panel is-active">
      <div className="feed-head">
        <div className="feed-title"><span className="led" /> LIVE EVENT LOG</div>
        <div className="feed-tools">
          <button
            className={`feed-tail${isTailing ? " is-on" : ""}`}
            type="button"
            title={isTailing ? "Stop tailing and hold this line" : "Tail newest log lines"}
            aria-pressed={isTailing}
            onClick={toggleTail}
          >
            <Icon name="i-arrow-down" />
            <span>{isTailing ? "TAILING" : "TAIL LOGS"}</span>
          </button>
          <button className="icon-btn" type="button" title={paused ? "Resume feed" : "Pause feed"} aria-pressed={paused} onClick={togglePause}>
            <Icon name={paused ? "i-play" : "i-pause"} />
          </button>
          <button
            className="icon-btn"
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
            <Icon name="i-trash" />
          </button>
        </div>
      </div>
      <div
        className="feed"
        ref={feedRef}
        role="log"
        aria-live="polite"
        aria-label="Worker event log"
        onScroll={handleFeedScroll}
      >
        {shown.length === 0 ? <div className="feed-boot">[SYS] WAITING FOR EVENTS…</div> : null}
        {shown.map((entry) => (
          <div className="log-line" key={entry.id} data-level={entry.level || "info"} data-scope={String(entry.scope || "sys").toLowerCase()}>
            <span className="log-time">{formatTime(entry.at)}</span>
            <span className="log-scope">{String(entry.scope || "sys").toUpperCase()}</span>
            <span className="log-msg">{entry.message}</span>
          </div>
        ))}
      </div>
      <div className="feed-foot">
        <span id="feed-status" className={paused ? "is-paused" : !tailing ? "is-held" : ""}>
          <span className="led led-sm" /> {paused ? "PAUSED" : tailing ? "TAILING" : "TAIL PAUSED"}
        </span>
        {paused && unread > 0 ? <span className="feed-badge">{formatInt(unread)} BUFFERED</span> : null}
        {!paused && !tailing ? <span className="feed-badge">SCROLL HELD</span> : null}
        <span id="feed-count">{formatInt(shown.length)} LINES</span>
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
  const chip = sessionChip(session, now);
  const remaining = session.expiresAt ? session.expiresAt - now : null;
  let meterWidth = 100;
  let meterClass = "";
  let countdown = "OPAQUE";
  if (!session.hasAccessToken) {
    meterWidth = 0;
    countdown = "--:--";
  } else if (session.expiresAt) {
    if (remaining <= 0) {
      countdown = "EXPIRED";
      meterWidth = 100;
      meterClass = "is-danger";
    } else {
      countdown = formatCountdown(remaining);
      meterWidth = Math.max(2, Math.min(100, (remaining / 3_600_000) * 100));
      meterClass = remaining <= 10 * 60_000 ? "is-danger" : remaining <= 30 * 60_000 ? "is-warn" : "";
    }
  } else if (session.needsRefresh) {
    countdown = "REFRESH DUE";
    meterWidth = 50;
    meterClass = "is-warn";
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

  return (
    <section className="panel tab-panel is-active">
      <div className="panel-head">
        <h2><Icon name="i-link" /> SESSION LINK</h2>
        <span className={chip.cls}>{chip.text}</span>
      </div>
      <div className="panel-body">
        <div className="token-health">
          <div className="meter-top"><span>ACCESS TOKEN</span><span id="token-countdown">{countdown}</span></div>
          <div className="meter"><i id="token-meter" className={meterClass} style={{ width: `${meterWidth}%` }} /></div>
          <p className="muted small">
            {!session.hasAccessToken
              ? "No Rollercoin session linked. Paste a token or use the bridge."
              : session.lastError
                ? `${session.lastError} · source ${(session.source || "?").toUpperCase()}`
                : `SOURCE ${(session.source || "?").toUpperCase()} · UPDATED ${session.updatedAt ? relTime(session.updatedAt) : "—"} · ${formatInt(session.refreshCount)} REFRESH${session.refreshCount === 1 ? "" : "ES"}`}
          </p>
        </div>

        <dl className="kv">
          <div><dt>REFRESH TOKEN</dt><dd style={!session.hasRefreshToken ? { color: "var(--amber)" } : undefined}>{session.hasRefreshToken ? session.refreshPreview || "STORED" : "MISSING"}</dd></div>
          <div><dt>LAST REFRESH</dt><dd>{session.lastRefreshAt ? `${relTime(session.lastRefreshAt)} (${formatInt(session.refreshCount)}×)` : "NEVER"}</dd></div>
          <div><dt>SOURCE</dt><dd>{(session.source || "—").toUpperCase()}</dd></div>
          <div><dt>WORKER KEY</dt><dd>{snapshot.workerTokenPreview || "—"}{snapshot.accessKeyPinned ? " (ENV)" : ""}</dd></div>
        </dl>

        <div className="btn-row">
          <button className="btn btn-sm" type="button" onClick={refresh} disabled={!session.hasRefreshToken || refreshing}>
            <Icon name="i-refresh" className="icon icon-xs" /> {refreshing ? "REFRESHING…" : "REFRESH NOW"}
          </button>
          <button className="btn btn-primary btn-sm" type="button" onClick={onEditSession}>EDIT TOKENS</button>
        </div>
        <div className="btn-row">
          <button className="btn btn-ghost btn-sm" type="button" onClick={onBridge}>TOKEN BRIDGE</button>
          <button className="btn btn-danger-ghost btn-sm" type="button" onClick={onRotate}>ROTATE KEY</button>
        </div>
        <button
          className="btn btn-danger-ghost btn-sm btn-block"
          type="button"
          onClick={onClearSession}
          disabled={!session.hasAccessToken && !session.hasRefreshToken && !session.hasCookie}
        >
          <Icon name="i-trash" className="icon icon-xs" /> CLEAR SESSION
        </button>

        <label className="toggle-row">
          <span>AUTO REFRESH<small>worker renews the session before expiry</small></span>
          <input type="checkbox" checked={Boolean(settings.autoRefresh)} onChange={(event) => toggleAuto(event.target.checked)} hidden />
          <span className="switch" aria-hidden="true" />
        </label>
        <p className="hint"><Icon name="i-alert" className="icon icon-xs" /> Tokens are stored in <code>data/state.json</code> with owner-only permissions.</p>
      </div>
    </section>
  );
}

function LatencyChannel({ label, stats }) {
  const bars = sparkBars(stats?.recent || []);
  const count = stats?.count || 0;
  return (
    <div className="latency-row">
      <div className="latency-head">
        <span className="latency-label">{label}</span>
        <span className={`latency-value ${latencyClass(stats?.last)}`}>{formatMs(stats?.last)}</span>
        <span className="latency-count">{formatInt(count)} SAMPLES</span>
      </div>
      <div className="latency-stats">
        <span>AVG <b>{formatMs(stats?.avg)}</b></span>
        <span>P50 <b>{formatMs(stats?.p50)}</b></span>
        <span>P95 <b className={latencyClass(stats?.p95)}>{formatMs(stats?.p95)}</b></span>
        <span>MIN <b>{formatMs(stats?.min)}</b></span>
        <span>MAX <b>{formatMs(stats?.max)}</b></span>
        <span>JITTER <b>{formatMs(stats?.jitter)}</b></span>
      </div>
      <div className="latency-spark" aria-hidden="true">
        {bars.length === 0 ? (
          <span className="latency-empty">NO SAMPLES YET — BUY SOMETHING OR RUN A PROBE</span>
        ) : (
          bars.map((bar, index) => (
            <i
              key={index}
              className={latencyClass(bar.value)}
              style={{ height: `${bar.height}%` }}
              title={`${formatMs(bar.value)}`}
            />
          ))
        )}
      </div>
      {stats?.errors > 0 ? (
        <span className="latency-error">{formatInt(stats.errors)} ERRORS · {stats.lastError || "—"}</span>
      ) : null}
    </div>
  );
}

function ConfigPanel({ snapshot, worker, notify }) {
  const latency = snapshot.latency || {};
  const channels = latency.channels || {};
  const fresh = latency.freshest || null;
  const channelOrder = ["http", "keepalive", "wsHandshake", "wsPing"];
  const channelLabels = {
    http: "HTTP",
    keepalive: "WARM PROBE",
    wsHandshake: "WS HANDSHAKE",
    wsPing: "WS PING"
  };
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
    <section className="panel tab-panel is-active">
      <div className="panel-head"><h2><Icon name="i-wifi" /> UPSTREAM LATENCY</h2></div>
      <div className="panel-body">
        <div className="latency-panel">
          <div className="latency-panel-head">
            <span className="latency-panel-title">
              <Icon name="i-wifi" className="icon icon-xs" /> WORKER → UPSTREAM
            </span>
            <span className="latency-panel-actions">
              <span className="latency-live" title="Panel refreshes automatically every 5 seconds">
                <span className="led" /> AUTO · 5S
              </span>
              <button className="btn btn-sm" type="button" onClick={runProbe} disabled={probing}>
                {probing ? "PROBING…" : "UKUR SEKARANG"}
              </button>
            </span>
          </div>
          <p className="latency-note">
            {fresh
              ? `Freshest: ${fresh.channel.toUpperCase()} ${formatMs(fresh.last)} · ${relTime(fresh.lastAt)}`
              : "Passive — samples appear as the worker talks to Rollercoin."}
          </p>
          {channelOrder.map((key) => (
            <LatencyChannel key={key} label={channelLabels[key]} stats={channels[key] || { count: 0, recent: [] }} />
          ))}
          <p className="latency-note">
            HTTP covers purchases, item-info &amp; token refresh. Warm probe is the keepalive GET. WS handshake/ping
            cover the market feed. Failures still count — they used upstream time.
          </p>
        </div>
      </div>
    </section>
  );
}

function RuntimeConfigPanel({ snapshot, worker, notify, onReset, onReconnect, baseUrl }) {
  const settings = snapshot.settings || {};
  const [delay, setDelay] = useState(String(settings.delayPerOrder ?? 200));
  const [maxOrders, setMaxOrders] = useState(String(settings.maxOrdersPerMinute ?? 60));
  const [saving, setSaving] = useState(false);

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

  return (
    <section className="panel tab-panel is-active">
      <div className="panel-head"><h2><Icon name="i-gear" /> RUNTIME CONFIG</h2></div>
      <div className="panel-body">
        <form className="stack" onSubmit={saveNumbers}>
          <div className="form-grid">
            <label className="field">
              <span>ORDER COOLDOWN (MS)</span>
              <input className="input" type="number" inputMode="numeric" step="10" min="50" max="60000" value={delay} onChange={(event) => setDelay(event.target.value)} />
              <small>Minimum gap between purchases.</small>
            </label>
            <label className="field">
              <span>MAX ORDERS / MIN</span>
              <input className="input" type="number" inputMode="numeric" step="1" min="1" max="600" value={maxOrders} onChange={(event) => setMaxOrders(event.target.value)} />
              <small>Hard burst protection.</small>
            </label>
          </div>
          <button className="btn btn-primary btn-sm btn-block" type="submit" disabled={saving}>
            {saving ? "SAVING…" : "SAVE NUMERIC CONFIG"}
          </button>
        </form>

        <label className="toggle-row">
          <span>DRY RUN<small>log signals but never send orders</small></span>
          <input type="checkbox" checked={Boolean(settings.dryRun)} onChange={(event) => toggle({ dryRun: event.target.checked })} hidden />
          <span className="switch" aria-hidden="true" />
        </label>
        <label className="toggle-row">
          <span>PAUSED<small>keep listening, stop all buying</small></span>
          <input type="checkbox" checked={Boolean(settings.paused)} onChange={(event) => toggle({ paused: event.target.checked })} hidden />
          <span className="switch" aria-hidden="true" />
        </label>

        <div className="divider" />
        <div className="worker-meta">
          <div><span>WORKER</span><code>{baseUrl}</code></div>
          <div><span>UPSTREAM</span><code>{snapshot.gateway?.upstream || "—"}</code></div>
          <div><span>VERSION</span><code>v{snapshot.version || "1.0.0"}</code></div>
        </div>
        <button className="btn btn-ghost btn-sm btn-block" type="button" onClick={onReconnect}>
          <Icon name="i-refresh" className="icon icon-xs" /> RECONNECT FEED
        </button>

        <div className="divider" />
        <div className="danger-zone">
          <h3><Icon name="i-alert" className="icon icon-xs" /> DANGER ZONE</h3>
          <p className="muted small">Wipes lifetime profit, fill counters, per-target success rates and the loot log. Targets stay untouched.</p>
          <button className="btn btn-danger btn-sm btn-block" type="button" onClick={onReset}>RESET LIFETIME STATS</button>
        </div>
      </div>
    </section>
  );
}

function SidePanel({ tab, setTab, snapshot, logs, now, worker, notify, onEditSession, onBridge, onClearSession, onRotate, onReset, onReconnect, baseUrl }) {
  const tabs = [
    { id: "feed", label: "FEED" },
    { id: "link", label: "LINK" },
    { id: "config", label: "CONFIG" },
    { id: "latency", label: "LATENCY" }
  ];
  return (
    <aside className="side-col">
      <div className="tabbar" role="tablist" aria-label="Control panels">
        {tabs.map((item) => (
          <button
            key={item.id}
            className={`tab${tab === item.id ? " is-active" : ""}`}
            type="button"
            role="tab"
            aria-selected={tab === item.id}
            onClick={() => setTab(item.id)}
          >
            {item.label}
          </button>
        ))}
      </div>
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
    </aside>
  );
}

function ToastStack({ toasts, onDismiss }) {
  return (
    <div className="toast-root" aria-live="polite">
      {toasts.map((toast) => (
        <div key={toast.id} className={`toast ${toast.kind}`} onClick={() => onDismiss(toast.id)}>
          <Icon name={toast.kind === "success" ? "i-check" : toast.kind === "error" || toast.kind === "warn" ? "i-alert" : "i-bolt"} />
          <div className="toast-body">
            <span className="toast-title">{toast.title}</span>
            <span className="toast-message">{toast.message}</span>
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
    if (!confirmed) return;
    try {
      await worker.resetStats();
      notify("warn", "STATS RESET", "Starting a fresh scoreboard.");
    } catch (error) {
      notify("error", "RESET FAILED", error.message);
    }
  };

  const rotateKey = async () => {
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
      <Hud snapshot={snapshot} now={now} eps={eps} />
      <main className="layout">
        <section className="main-col">
          <TargetsPanel
            configs={snapshot.configs || []}
            search={search}
            onSearch={setSearch}
            onAdd={() => setModal({ type: "target", config: null })}
            onEdit={(config) => setModal({ type: "target", config })}
            onDelete={deleteTarget}
            onToggle={toggleTarget}
            notify={notify}
          />
          <LootLog history={snapshot.history || []} onReset={resetStats} />
        </section>
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
          baseUrl={baseUrl}
        />
      </main>
      <footer className="footer">
        <span><span id="footer-mode">{snapshot.worker?.mock ? "MOCK" : "LIVE"}</span> · WORKER v<span id="footer-version">{snapshot.version || "1.0.0"}</span></span>
        <span className="footer-credit">ORIGINAL BOT BY @NOPALEON.0F · ARCADE EDITION</span>
        <span>NOT AFFILIATED WITH ROLLERCOIN</span>
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
    <>
      <div className="crt" aria-hidden="true" />
      <div className={snapshot ? "app" : ""}>
        {snapshot ? (
          <Dashboard worker={worker} notify={push} toasts={toasts} dismissToast={dismiss} />
        ) : (
          <BootScreen onLogin={worker.login} busy={busy} error={authError} />
        )}
      </div>
      {!snapshot ? <ToastStack toasts={toasts} onDismiss={dismiss} /> : null}
    </>
  );
}
