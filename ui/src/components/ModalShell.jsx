import { useEffect, useId, useRef } from "react";

export function Icon({ name, className = "icon" }) {
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
    document.body.classList.add("modal-open");
    document.body.style.overflow = "hidden";
    const timer = requestAnimationFrame(() => {
      const focusable = panelRef.current?.querySelector(
        'input:not([type="hidden"]):not([readonly]), textarea, button, select'
      );
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
      document.body.classList.remove("modal-open");
      document.body.style.overflow = previousOverflow;
    };
  }, []);

  return (
    <div className="modal-root">
      <div
        className="modal-backdrop"
        onMouseDown={(event) => {
          if (event.target === event.currentTarget) closeRef.current();
        }}
      >
        <div
          ref={panelRef}
          className={`modal panel${wide ? " modal-wide" : ""}${small ? " modal-sm" : ""}`}
          role={role}
          aria-modal="true"
          aria-labelledby={titleId}
        >
          <div className="modal-head">
            <h2 id={titleId} style={titleColor === "danger" ? { color: "var(--red)" } : undefined}>
              {title}
            </h2>
            <button className="icon-btn" type="button" onClick={onClose} aria-label="Close">
              <Icon name="i-x" />
            </button>
          </div>
          {children}
        </div>
      </div>
    </div>
  );
}
