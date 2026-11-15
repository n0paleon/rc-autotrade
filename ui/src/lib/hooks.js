import { useEffect, useState } from "react";

/**
 * Roving arrow-key behaviour for a real tablist. It moves focus and activates
 * the next tab; every panel behind these tabs is a local state switch, so
 * automatic activation is the right model rather than a second Enter press.
 */
export function tabKeyDown(event) {
  const keys = ["ArrowLeft", "ArrowRight", "ArrowUp", "ArrowDown", "Home", "End"];
  if (!keys.includes(event.key)) return;
  const list = event.currentTarget.closest('[role="tablist"]');
  const tabs = list
    ? [...list.querySelectorAll('[role="tab"]')].filter((tab) => !tab.disabled)
    : [];
  const index = tabs.indexOf(event.currentTarget);
  if (index < 0 || tabs.length === 0) return;
  let next = index;
  if (event.key === "ArrowRight" || event.key === "ArrowDown") next = (index + 1) % tabs.length;
  else if (event.key === "ArrowLeft" || event.key === "ArrowUp") next = (index - 1 + tabs.length) % tabs.length;
  else if (event.key === "Home") next = 0;
  else if (event.key === "End") next = tabs.length - 1;
  event.preventDefault();
  tabs[next].focus();
  tabs[next].click();
}

/** Storage key prefix for collapsed panels; the panel id completes it. */
const COLLAPSE_KEY = "turuc.collapsed.";

/**
 * Remembers whether a panel is collapsed, per browser.
 *
 * The deck is a monitoring surface: an operator who parks the activity table
 * to get the loot log on screen expects it to stay parked after a reload.
 * Storage failures (private mode, quota) fall back to the default instead of
 * breaking the panel.
 */
export function useCollapsed(id, initial = false) {
  const [collapsed, setCollapsed] = useState(() => {
    if (typeof window === "undefined") return initial;
    try {
      const saved = window.localStorage.getItem(`${COLLAPSE_KEY}${id}`);
      return saved === null ? initial : saved === "1";
    } catch {
      return initial;
    }
  });

  useEffect(() => {
    try {
      window.localStorage.setItem(`${COLLAPSE_KEY}${id}`, collapsed ? "1" : "0");
    } catch {
      /* storage is unavailable; the panel still toggles for this session */
    }
  }, [collapsed, id]);

  return [collapsed, setCollapsed];
}

/**
 * Subscribes to a CSS media query from React.
 *
 * Used to render one layout instead of mounting both and hiding one with
 * CSS: an `xl` table that is `display:none` still costs DOM nodes, layout
 * and reconciliation on a phone.
 */
export function useMediaQuery(query) {
  const [matches, setMatches] = useState(() =>
    typeof window !== "undefined" && typeof window.matchMedia === "function"
      ? window.matchMedia(query).matches
      : false
  );

  useEffect(() => {
    if (typeof window === "undefined" || typeof window.matchMedia !== "function") {
      return undefined;
    }
    const list = window.matchMedia(query);
    const update = () => setMatches(list.matches);
    update();
    list.addEventListener("change", update);
    return () => list.removeEventListener("change", update);
  }, [query]);

  return matches;
}

/**
 * A clock that only re-renders the component that needs it.
 *
 * The deck used to hold this tick in `Dashboard`, so every second the whole
 * tree — every target card and chart — reconciled just to move the header
 * clock. Keeping the interval local confines that work to the header and the
 * HUD.
 */
export function useNow(intervalMs = 1000) {
  const [now, setNow] = useState(() => Date.now());

  useEffect(() => {
    const timer = setInterval(() => setNow(Date.now()), intervalMs);
    return () => clearInterval(timer);
  }, [intervalMs]);

  return now;
}
