// PM2 runs the Rust binary directly: no interpreter, no Bun runtime.
// Build it first with `bun run pm2:start` (vite build + cargo build --release).
module.exports = {
  apps: [
    {
      name: "rc-autotrade",
      cwd: __dirname,
      script: "target/release/rollercoin-worker",
      interpreter: "none",
      instances: 1,
      exec_mode: "fork",
      autorestart: true,
      watch: false,
      restart_delay: 2000,
      exp_backoff_restart_delay: 200,
      kill_timeout: 8000,
      max_memory_restart: "512M",
      time: true,
      merge_logs: true,
      env: {
        PORT: "8787",
        RC_DATA_DIR: "./data"
      }
    }
  ]
};
