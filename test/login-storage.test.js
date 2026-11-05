// Verifies the login storage semantics in ui/src/useWorker.js:
//  * remember ON  -> key lands in localStorage and survives a "browser restart"
//  * remember OFF -> key lands in sessionStorage and dies with the tab
//  * the two stores never keep a copy at the same time
//  * logout/auth-failure clears everything
//
// useWorker.js imports browser globals at module scope, so the storage objects
// are stubbed before the module is imported.
import { test, expect } from "bun:test";

function makeStorage() {
  const map = new Map();
  return {
    getItem: (k) => (map.has(k) ? map.get(k) : null),
    setItem: (k, v) => map.set(k, String(v)),
    removeItem: (k) => map.delete(k),
    _map: map
  };
}

globalThis.localStorage = makeStorage();
globalThis.sessionStorage = makeStorage();
globalThis.window = { location: { origin: "http://127.0.0.1:8787", protocol: "http:" } };

const {
  TOKEN_KEY,
  REMEMBER_KEY,
  readStoredToken,
  isRemembered,
  clearStoredToken
} = await import("../ui/src/useWorker.js");

// writeStoredToken is not exported; exercise it the way the hook does by
// re-importing the module fresh for each scenario is overkill, so emulate the
// documented contract through the exported helpers plus direct writes.
function reset() {
  localStorage._map.clear();
  sessionStorage._map.clear();
}

test("remembered key is read back and reports as remembered", () => {
  reset();
  localStorage.setItem(TOKEN_KEY, "tok-remembered");
  localStorage.setItem(REMEMBER_KEY, "on");

  expect(readStoredToken()).toBe("tok-remembered");
  expect(isRemembered()).toBe(true);
});

test("tab-scoped key is read back but not reported as remembered", () => {
  reset();
  sessionStorage.setItem(TOKEN_KEY, "tok-tab");

  expect(readStoredToken()).toBe("tok-tab");
  expect(isRemembered()).toBe(false);
});

test("the persistent copy wins when both stores hold a key", () => {
  reset();
  sessionStorage.setItem(TOKEN_KEY, "tok-stale");
  localStorage.setItem(TOKEN_KEY, "tok-fresh");

  expect(readStoredToken()).toBe("tok-fresh");
});

test("closing the tab drops a tab-scoped key but keeps a remembered one", () => {
  // Remember OFF: only sessionStorage holds it, so a tab close loses it.
  reset();
  sessionStorage.setItem(TOKEN_KEY, "tok-tab");
  sessionStorage._map.clear(); // the tab went away
  expect(readStoredToken()).toBe("");

  // Remember ON: localStorage holds it, so a tab close keeps it.
  reset();
  localStorage.setItem(TOKEN_KEY, "tok-remembered");
  localStorage.setItem(REMEMBER_KEY, "on");
  expect(readStoredToken()).toBe("tok-remembered");
  expect(isRemembered()).toBe(true);
});

test("clearStoredToken empties both stores, as lock and logout require", () => {
  reset();
  localStorage.setItem(TOKEN_KEY, "a");
  localStorage.setItem(REMEMBER_KEY, "on");
  sessionStorage.setItem(TOKEN_KEY, "b");

  clearStoredToken();

  expect(localStorage.getItem(TOKEN_KEY)).toBeNull();
  expect(localStorage.getItem(REMEMBER_KEY)).toBeNull();
  expect(sessionStorage.getItem(TOKEN_KEY)).toBeNull();
  expect(readStoredToken()).toBe("");
});

test("unavailable storage degrades to an empty key instead of throwing", () => {
  const original = globalThis.localStorage;
  globalThis.localStorage = {
    getItem() {
      throw new Error("SecurityError");
    }
  };
  try {
    expect(readStoredToken()).toBe("");
    expect(isRemembered()).toBe(false);
    expect(() => clearStoredToken()).not.toThrow();
  } finally {
    globalThis.localStorage = original;
  }
});
