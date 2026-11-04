import { useCallback, useEffect, useRef, useState } from "react";

const TOAST_EXIT_MS = 140;

export function useToasts() {
  const [toasts, setToasts] = useState([]);
  const timers = useRef(new Map());
  const leaving = useRef(new Set());

  const dismiss = useCallback((id) => {
    if (leaving.current.has(id)) return;
    leaving.current.add(id);
    setToasts((previous) =>
      previous.map((toast) => (toast.id === id ? { ...toast, leaving: true } : toast))
    );
    const existing = timers.current.get(id);
    if (existing) clearTimeout(existing);
    const exit = setTimeout(() => {
      setToasts((previous) => previous.filter((toast) => toast.id !== id));
      timers.current.delete(id);
      leaving.current.delete(id);
    }, TOAST_EXIT_MS);
    timers.current.set(id, exit);
  }, []);

  const push = useCallback(
    (kind, title, message, ttl = 4200) => {
      const signature = `${kind}|${title}|${message}`;
      const id = `${Date.now()}-${Math.random().toString(36).slice(2, 6)}`;
      setToasts((previous) => {
        if (previous.some((toast) => toast.signature === signature)) return previous;
        const max = window.matchMedia("(max-width: 760px)").matches ? 1 : 3;
        const next = [...previous, { id, kind, title, message, signature }];
        return next.length > max ? next.slice(-max) : next;
      });
      const timer = setTimeout(() => dismiss(id), ttl);
      timers.current.set(id, timer);
    },
    [dismiss]
  );

  useEffect(() => {
    const active = timers.current;
    const exiting = leaving.current;
    return () => {
      for (const timer of active.values()) clearTimeout(timer);
      active.clear();
      exiting.clear();
    };
  }, []);

  return { toasts, push, dismiss };
}
