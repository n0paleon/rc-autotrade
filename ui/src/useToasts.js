import { useCallback, useEffect, useRef, useState } from "react";

const TOAST_EXIT_MS = 140;
// Every toast clears itself after the same window: a pinned warning used to sit
// in the stack until another toast or a manual close pushed it out.
const TOAST_TTL_MS = 7000;

export function useToasts() {
  const [toasts, setToasts] = useState([]);
  const timers = useRef(new Map());
  const exits = useRef(new Map());
  const leaving = useRef(new Set());

  const dismiss = useCallback((id) => {
    if (leaving.current.has(id)) return;
    leaving.current.add(id);
    setToasts((previous) =>
      previous.map((toast) => (toast.id === id ? { ...toast, leaving: true } : toast))
    );
    const existing = timers.current.get(id);
    if (existing) clearTimeout(existing);
    timers.current.delete(id);
    const exit = setTimeout(() => {
      setToasts((previous) => previous.filter((toast) => toast.id !== id));
      exits.current.delete(id);
      leaving.current.delete(id);
    }, TOAST_EXIT_MS);
    exits.current.set(id, exit);
  }, []);

  const push = useCallback(
    (kind, title, message, ttl) => {
      const signature = `${kind}|${title}|${message}`;
      const id = `${Date.now()}-${Math.random().toString(36).slice(2, 6)}`;
      const lifetime = ttl == null ? TOAST_TTL_MS : Number(ttl) || 0;
      setToasts((previous) => {
        if (previous.some((toast) => toast.signature === signature)) return previous;
        const max = window.matchMedia("(max-width: 760px)").matches ? 1 : 3;
        const next = [...previous, { id, kind, title, message, signature }];
        return next.length > max ? next.slice(-max) : next;
      });
      if (lifetime > 0) {
        const timer = setTimeout(() => {
          timers.current.delete(id);
          dismiss(id);
        }, lifetime);
        timers.current.set(id, timer);
      }
    },
    [dismiss]
  );

  // When the stack is full a new toast evicts the oldest one; its pending timer
  // has to go with it instead of firing against an id that is no longer shown.
  useEffect(() => {
    const live = new Set(toasts.map((toast) => toast.id));
    for (const [id, timer] of timers.current) {
      if (live.has(id)) continue;
      clearTimeout(timer);
      timers.current.delete(id);
    }
  }, [toasts]);

  useEffect(() => {
    const active = timers.current;
    const pendingExits = exits.current;
    const exiting = leaving.current;
    return () => {
      for (const timer of active.values()) clearTimeout(timer);
      for (const timer of pendingExits.values()) clearTimeout(timer);
      active.clear();
      pendingExits.clear();
      exiting.clear();
    };
  }, []);

  return { toasts, push, dismiss };
}
