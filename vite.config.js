import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import tailwindcss from "@tailwindcss/vite";

const backend = `http://127.0.0.1:${process.env.PORT || 8787}`;

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
    proxy: {
      "/api": backend,
      "/ws": {
        target: backend.replace(/^http/, "ws"),
        ws: true
      }
    }
  }
});
