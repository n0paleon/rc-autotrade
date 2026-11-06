import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import tailwindcss from "@tailwindcss/vite";

const backend = `http://127.0.0.1:${process.env.PORT || 8787}`;

// The token bridge is pasted into rollercoin.com and posts to whichever origin
// served this deck (Vite's :5173 during `bun run dev`). Vite answers CORS
// preflights itself and only trusts localhost by default, so the browser never
// reaches the Rust proxy (which already allows everything) and the request
// fails with "No 'Access-Control-Allow-Origin' header". Trust the Rollercoin
// origins here, and keep the built-in localhost allowance for other tooling.
const devCorsOrigins = [
  /^https:\/\/(?:[a-z0-9-]+\.)*rollercoin\.com$/i,
  /^https?:\/\/(?:(?:[^:]+\.)?localhost|127\.0\.0\.1|\[::1\])(?::\d+)?$/
];

export default defineConfig({
  root: "ui",
  plugins: [react(), tailwindcss()],
  publicDir: "../public",
  build: {
    outDir: "../dist",
    emptyOutDir: true,
    sourcemap: false
  },
  server: {
    host: "127.0.0.1",
    port: 5173,
    cors: {
      origin: devCorsOrigins,
      methods: ["GET", "HEAD", "PUT", "PATCH", "POST", "DELETE", "OPTIONS"],
      // Reflecting the requested headers keeps the bridge working if the
      // browser adds `cache-control`/`pragma` for its `cache: "no-store"` fetch.
      maxAge: 600
    },
    proxy: {
      "/api": backend,
      "/ws": {
        target: backend.replace(/^http/, "ws"),
        ws: true
      }
    }
  }
});
