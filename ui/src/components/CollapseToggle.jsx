import { useCallback, useEffect, useRef } from "react";
import { Icon } from "./ModalShell.jsx";

/** How long the header rail keeps its sweep class; matches `rail-sweep`. */
const SWEEP_MS = 360;

/**
 * Disclosure control for a panel header.
 *
 * The panel keeps its title and count while collapsed, so the closed bar still
 * says what is inside without being opened. The chevron points right when
 * closed and down when open, matching the target cards.
 *
 * Toggling also fires a one-shot sweep on the panel's accent rail, which is
 * what makes the shutter feel mechanical instead of a silent layout jump.
 *
 * `controls` names the element being disclosed, so assistive tech can follow
 * the toggle to the content it opens.
 */
export function CollapseToggle({ expanded, label, panelRef, controls, onToggle }) {
  const action = expanded ? "Collapse" : "Expand";
  const timer = useRef(null);

  useEffect(() => () => clearTimeout(timer.current), []);

  const toggle = useCallback(() => {
    onToggle();
    const head = panelRef?.current?.querySelector(".panel-head");
    if (!head) return;
    // Restart the animation even on a fast double toggle.
    head.classList.remove("is-revealing");
    void head.offsetWidth;
    head.classList.add("is-revealing");
    clearTimeout(timer.current);
    timer.current = setTimeout(() => head.classList.remove("is-revealing"), SWEEP_MS);
  }, [onToggle, panelRef]);

  return (
    <button
      className="icon-btn shrink-0"
      type="button"
      onClick={toggle}
      aria-expanded={expanded}
      aria-controls={controls || undefined}
      aria-label={`${action} ${label}`}
      title={`${action} ${label}`}
    >
      <Icon
        name="i-chevron"
        className={`h-3 w-3 chevron${expanded ? " rotate-90" : ""}`}
      />
    </button>
  );
}
