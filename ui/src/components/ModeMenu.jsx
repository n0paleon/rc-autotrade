import { useCallback, useEffect, useId, useRef, useState } from "react";
import { createPortal } from "react-dom";
import { Icon } from "./ModalShell.jsx";
import { MODES } from "../lib/constants.js";

/** Short description of what each mode does, shown inside the menu. */
// `i-power` is the exit-door glyph used by the lock-deck button, so `off`
// gets its own power-off ring instead of reading like a logout action.
const MODE_ICON = { off: "i-shutdown", shadow: "i-eye", live: "i-play" };
const MODE_TONE = {
  off: "text-gold",
  shadow: "text-info",
  live: "text-signal"
};

/** How long the menu keeps its closing frame; matches `menu-out`. */
const MENU_EXIT_MS = 110;

/**
 * The one trading-mode control. It replaces the old header button *and* the
 * runtime panel's segmented control, so "off / shadow / live" is chosen the
 * same way everywhere.
 *
 * The menu is rendered in a portal and anchored to the trigger's bounding box.
 * `deck-enter` leaves an identity transform on its ancestors, which would make
 * an absolutely positioned child resolve against the page instead of the
 * header; a portal sidesteps that entirely.
 */
export function ModeMenu({ mode, busy, onChange, variant = "header" }) {
  const [open, setOpen] = useState(false);
  const [closing, setClosing] = useState(false);
  const triggerRef = useRef(null);
  const menuRef = useRef(null);
  const closeTimer = useRef(null);
  const closingRef = useRef(false);
  const [anchor, setAnchor] = useState(null);
  const menuId = useId().replace(/[^a-zA-Z0-9_-]/g, "");
  const active = MODES.find((entry) => entry.id === mode) || MODES[0];

  const place = useCallback(() => {
    const rect = triggerRef.current?.getBoundingClientRect();
    if (!rect) return;
    const width = 264;
    const left = Math.min(
      Math.max(8, rect.right - width),
      window.innerWidth - width - 8
    );
    setAnchor({ top: rect.bottom + 6, left, width });
  }, []);

  // Keep the menu mounted for one short exit frame, so dismissing it reads as
  // folding back toward the trigger instead of vanishing mid-click.
  const close = useCallback((restoreFocus = false) => {
    if (closingRef.current) return;
    closingRef.current = true;
    setClosing(true);
    if (restoreFocus) triggerRef.current?.focus();
    closeTimer.current = window.setTimeout(() => {
      closingRef.current = false;
      setOpen(false);
      setClosing(false);
    }, MENU_EXIT_MS);
  }, []);

  const openMenu = useCallback(() => {
    clearTimeout(closeTimer.current);
    closingRef.current = false;
    setClosing(false);
    setOpen(true);
  }, []);

  useEffect(() => () => clearTimeout(closeTimer.current), []);

  useEffect(() => {
    if (!open) return undefined;
    place();
    const onPointerDown = (event) => {
      if (menuRef.current?.contains(event.target)) return;
      if (triggerRef.current?.contains(event.target)) return;
      close();
    };
    const onKeyDown = (event) => {
      if (event.key === "Escape") {
        event.preventDefault();
        close(true);
      }
    };
    const onReflow = () => place();
    document.addEventListener("pointerdown", onPointerDown, true);
    document.addEventListener("keydown", onKeyDown);
    window.addEventListener("resize", onReflow);
    window.addEventListener("scroll", onReflow, true);
    return () => {
      document.removeEventListener("pointerdown", onPointerDown, true);
      document.removeEventListener("keydown", onKeyDown);
      window.removeEventListener("resize", onReflow);
      window.removeEventListener("scroll", onReflow, true);
    };
  }, [open, place, close]);

  // Focus the selected row so the menu is usable from the keyboard straight
  // away. The portal only exists once `anchor` is measured, so that state has
  // to be part of the trigger: keying this on `open` alone leaves focus on the
  // trigger while the menu floats underneath it.
  useEffect(() => {
    if (!open || !anchor) return;
    const selected = menuRef.current?.querySelector('[aria-checked="true"]');
    selected?.focus();
  }, [open, anchor]);

  const choose = (next) => {
    close(true);
    if (next !== mode) onChange(next);
  };

  const onMenuKeyDown = (event) => {
    if (event.key === "Tab") {
      // The menu lives in a portal at the end of <body>; letting Tab run from
      // there would drop focus out of the page. Close and hand focus back to
      // the trigger instead.
      event.preventDefault();
      close(true);
      return;
    }
    const items = [...(menuRef.current?.querySelectorAll('[role="menuitemradio"]') || [])];
    if (items.length === 0) return;
    if (event.key === "Home" || event.key === "End") {
      event.preventDefault();
      items[event.key === "Home" ? 0 : items.length - 1]?.focus();
      return;
    }
    if (event.key !== "ArrowDown" && event.key !== "ArrowUp") return;
    event.preventDefault();
    const index = items.indexOf(document.activeElement);
    const step = event.key === "ArrowDown" ? 1 : -1;
    const next = (index + step + items.length) % items.length;
    items[next]?.focus();
  };

  // The mode picker is the primary control in the deck, so it keeps the same
  // gold treatment as every other primary button in every mode. The active
  // mode is communicated by the label and the status chip, not by recolouring
  // the button (which made "live" look like a secondary, inert action).
  const triggerClass =
    variant === "header"
      ? "btn btn-primary btn-sm max-sm:w-10 max-sm:px-0"
      : "btn btn-primary btn-sm w-full justify-between";

  return (
    <>
      <button
        ref={triggerRef}
        className={`${triggerClass}${busy ? " cursor-progress opacity-60" : ""}`}
        type="button"
        onClick={() => {
          if (busy) return;
          open ? close() : openMenu();
        }}
        onKeyDown={(event) => {
          if (busy) return;
          if (event.key !== "ArrowDown" && event.key !== "ArrowUp") return;
          event.preventDefault();
          openMenu();
        }}
        aria-disabled={busy || undefined}
        data-busy={busy ? "true" : undefined}
        aria-busy={busy}
        aria-haspopup="menu"
        aria-expanded={open && !closing}
        aria-controls={open ? menuId : undefined}
        aria-label={`Trading mode: ${active.label}. Change mode`}
        title={`Trading mode: ${active.label}. ${active.hint}`}
      >
        <Icon name={MODE_ICON[mode]} className="h-3 w-3" />
        <span className="hidden sm:inline">{busy ? "…" : active.short}</span>
        <Icon
          name="i-caret-down"
          className={`h-2.5 w-2.5 chevron${open ? " rotate-180" : ""}`}
        />
      </button>
      {open && anchor
        ? createPortal(
            <div
              ref={menuRef}
              id={menuId}
              role="menu"
              aria-label="Trading mode"
              onKeyDown={onMenuKeyDown}
              className={`panel mode-menu fixed z-[80] overflow-hidden py-1${closing ? " is-closing" : ""}`}
              style={{ top: `${anchor.top}px`, left: `${anchor.left}px`, width: `${anchor.width}px` }}
            >
              {MODES.map((entry) => {
                const selected = entry.id === mode;
                // Mirrors the backend guardrail: live is never armed straight
                // from off. Disabling here is clearer than a 422 after the
                // confirmation dialog.
                const blocked = entry.id === "live" && mode === "off";
                return (
                  <button
                    key={entry.id}
                    type="button"
                    role="menuitemradio"
                    aria-checked={selected}
                    aria-disabled={blocked || undefined}
                    onClick={() => {
                      if (!blocked) choose(entry.id);
                    }}
                    title={blocked ? "Pass through shadow first" : entry.hint}
                    className={`flex w-full items-start gap-2 px-3 py-2 text-left transition-colors focus-visible:bg-night-850 ${
                      blocked
                        ? "cursor-not-allowed opacity-40"
                        : "hover:bg-night-850"
                    } ${selected ? "bg-night-850/70" : ""}`}
                  >
                    <Icon
                      name={MODE_ICON[entry.id]}
                      className={`mt-0.5 h-3.5 w-3.5 shrink-0 ${MODE_TONE[entry.id]}`}
                    />
                    <span className="min-w-0 flex-1">
                      <span className="flex items-center gap-1.5 font-mono text-[12px] font-bold uppercase tracking-[0.08em] text-ink">
                        {entry.label}
                        {selected ? (
                          <Icon name="i-check" className={`h-3 w-3 ${MODE_TONE[entry.id]}`} />
                        ) : null}
                      </span>
                      <span className="mt-0.5 block text-[12px] leading-snug text-faint">
                        {blocked ? "Pass through shadow first." : entry.hint}
                      </span>
                    </span>
                  </button>
                );
              })}
            </div>,
            document.body
          )
        : null}
    </>
  );
}
