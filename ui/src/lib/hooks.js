import { useEffect, useState } from "react";

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
