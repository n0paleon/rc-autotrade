import { useCallback, useEffect, useRef, useState } from "react";

export function useToasts() {
  const [toasts, setToasts] = useState([]);
  const timers = useRef(new Map());

  const dismiss = useCallback((id) => {
    setToasts((previous) => previous.filter((toast) => toast.id !== id));
    const timer = timers.current.get(id);
    if (timer) clearTimeout(timer);
    timers.current.delete(id);
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
      const timer = setTimeout(() => {
        setToasts((previous) => previous.filter((toast) => toast.id !== id));
        timers.current.delete(id);
      }, ttl);
      timers.current.set(id, timer);
    },
    []
  );

  useEffect(() => {
    const active = timers.current;
    return () => {
      for (const timer of active.values()) clearTimeout(timer);
      active.clear();
    };
  }, []);

  return { toasts, push, dismiss };
}
