import { useCallback, useEffect, useId, useRef, useState } from "react";
import { createPortal } from "react-dom";

export function Icon({ name, className = "h-4 w-4 shrink-0" }) {
  return (
    <svg className={className} aria-hidden="true">
      <use href={`#${name}`} />
    </svg>
  );
}

export function useModalClose() {
  const closeRef = useRef(() => {});
  const close = useCallback(() => closeRef.current?.(), []);
  return { closeRef, close };
}

/** Shared pixel switch row used by the modal forms. */
export function ToggleRow({ checked, onChange, title, hint, disabled = false }) {
  return (
    <label className="toggle-row">
      <span>
        {title}
        <small>{hint}</small>
      </span>
      <input
        type="checkbox"
        className="sr-only"
        checked={checked}
        onChange={(event) => onChange(event.target.checked)}
        disabled={disabled}
      />
      <span className="switch" aria-hidden="true" />
    </label>
  );
}

export function ModalShell({
  title,
  onClose,
  children,
  footer,
  closeRef: externalCloseRef,
  wide = false,
  small = false,
  size = null,
  role = "dialog",
  titleColor = "lime"
}) {
  const titleId = useId().replace(/[^a-zA-Z0-9_-]/g, "");
  const panelRef = useRef(null);
  const closeRef = useRef(onClose);
  closeRef.current = onClose;
  const closeTimer = useRef(null);
  const closingRef = useRef(false);
  const [closing, setClosing] = useState(false);

  const requestClose = useCallback(() => {
    if (closingRef.current) return;
    closingRef.current = true;
    setClosing(true);
    closeTimer.current = window.setTimeout(() => closeRef.current(), 140);
  }, []);

  const requestCloseRef = useRef(requestClose);
  requestCloseRef.current = requestClose;

  useEffect(() => {
    if (!externalCloseRef) return undefined;
    externalCloseRef.current = requestClose;
    return () => {
      externalCloseRef.current = null;
    };
  }, [externalCloseRef, requestClose]);

  useEffect(() => {
    const previousOverflow = document.body.style.overflow;
    document.body.style.overflow = "hidden";
    const timer = requestAnimationFrame(() => {
      const panel = panelRef.current;
      const focusable =
        panel?.querySelector("[data-autofocus]") ||
        panel?.querySelector('input:not([type="hidden"]):not([readonly]), textarea, select') ||
        panel?.querySelector("button");
      focusable?.focus();
    });

    const onKeyDown = (event) => {
      if (event.key === "Escape") {
        event.preventDefault();
        requestCloseRef.current();
        return;
      }
      if (event.key !== "Tab") return;
      const focusable = [...(panelRef.current?.querySelectorAll(
        'a[href], button:not([disabled]), input:not([disabled]):not([type="hidden"]), textarea:not([disabled]), select:not([disabled]), [tabindex]:not([tabindex="-1"])'
      ) || [])].filter((element) => element.offsetParent !== null);
      if (focusable.length === 0) return;
      const first = focusable[0];
      const last = focusable[focusable.length - 1];
      if (event.shiftKey && document.activeElement === first) {
        event.preventDefault();
        last.focus();
      } else if (!event.shiftKey && document.activeElement === last) {
        event.preventDefault();
        first.focus();
      }
    };

    document.addEventListener("keydown", onKeyDown);
    return () => {
      cancelAnimationFrame(timer);
      document.removeEventListener("keydown", onKeyDown);
      document.body.style.overflow = previousOverflow;
      if (closeTimer.current) clearTimeout(closeTimer.current);
    };
  }, []);

  // Render outside the dashboard tree: animated ancestors leave an identity
  // transform behind, which would turn `fixed` into "relative to the page"
  // and let tall modals overflow the viewport.
  return createPortal(
    <div className="fixed inset-0 z-[70] flex items-end justify-center pl-[env(safe-area-inset-left)] pr-[env(safe-area-inset-right)] sm:items-center sm:p-6">
      <div
        className={`absolute inset-0 bg-night-950/85 backdrop-blur-sm modal-backdrop${closing ? " is-closing" : ""}`}
        onMouseDown={requestClose}
      />
      <div
        ref={panelRef}
        className={`panel modal-panel relative z-10 flex max-h-[92dvh] w-full flex-col overflow-hidden rounded-none sm:rounded-none ${
          size === "monitor"
            ? "sm:h-[92dvh] sm:max-w-[min(1500px,96vw)]"
            : wide
              ? "sm:max-w-3xl"
              : small
                ? "sm:max-w-md"
                : "sm:max-w-lg"
        }${closing ? " is-closing" : ""}`}
        role={role}
        aria-modal="true"
        aria-labelledby={titleId}
      >
        <div className="flex items-center justify-between gap-4 border-b-2 border-line bg-night-850/70 px-4 py-3 sm:px-5">
          <h2
            id={titleId}
            className={`flex items-center gap-2 font-sans text-[15px] font-bold uppercase tracking-[0.08em] ${
              titleColor === "danger" ? "text-danger" : "text-gold"
            }`}
          >
            <span className={`h-2 w-2 ${titleColor === "danger" ? "bg-danger" : "bg-gold"}`} aria-hidden="true" />
            {title}
          </h2>
          <button className="icon-btn -mr-1.5 h-10 w-10 sm:h-8 sm:w-8" type="button" onClick={requestClose} aria-label="Close">
            <Icon name="i-x" />
          </button>
        </div>
        <div className="min-h-0 flex-1 overflow-y-auto">{children}</div>
        {footer ? (
          <div className="shrink-0 border-t-2 border-line bg-night-900/95 px-4 pt-3 pb-[max(0.75rem,env(safe-area-inset-bottom))] backdrop-blur sm:px-5">
            {footer}
          </div>
        ) : null}
      </div>
    </div>,
    document.body
  );
}
