import { useEffect, useId, useRef } from "react";

export function Icon({ name, className = "h-4 w-4 shrink-0" }) {
  return (
    <svg className={className} aria-hidden="true">
      <use href={`#${name}`} />
    </svg>
  );
}

export function ModalShell({
  title,
  onClose,
  children,
  footer,
  wide = false,
  small = false,
  role = "dialog",
  titleColor = "lime"
}) {
  const titleId = useId().replace(/[^a-zA-Z0-9_-]/g, "");
  const panelRef = useRef(null);
  const closeRef = useRef(onClose);
  closeRef.current = onClose;

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
        closeRef.current();
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
    };
  }, []);

  return (
    <div className="fixed inset-0 z-[70] flex items-end justify-center pl-[env(safe-area-inset-left)] pr-[env(safe-area-inset-right)] sm:items-center sm:p-6">
      <div
        className="absolute inset-0 bg-carbon-950/80 backdrop-blur-sm"
        onMouseDown={() => closeRef.current()}
      />
      <div
        ref={panelRef}
        className={`panel modal-panel relative z-10 flex max-h-[92dvh] w-full flex-col overflow-hidden rounded-t-2xl sm:rounded-xl ${
          wide ? "sm:max-w-3xl" : small ? "sm:max-w-md" : "sm:max-w-lg"
        }`}
        role={role}
        aria-modal="true"
        aria-labelledby={titleId}
      >
        <div className="flex items-center justify-between gap-4 border-b border-line px-4 py-3 sm:px-5">
          <h2
            id={titleId}
            className={`font-mono text-[11px] font-semibold uppercase tracking-[0.18em] ${
              titleColor === "danger" ? "text-danger" : "text-signal"
            }`}
          >
            {title}
          </h2>
          <button className="icon-btn -mr-1.5 h-10 w-10 sm:h-8 sm:w-8" type="button" onClick={onClose} aria-label="Close">
            <Icon name="i-x" />
          </button>
        </div>
        <div className="min-h-0 flex-1 overflow-y-auto">{children}</div>
        {footer ? (
          <div className="shrink-0 border-t border-line bg-carbon-900/95 px-4 pt-3 pb-[max(0.75rem,env(safe-area-inset-bottom))] backdrop-blur sm:px-5">
            {footer}
          </div>
        ) : null}
      </div>
    </div>
  );
}
