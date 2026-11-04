module.exports = {
  apps: [
    {
      name: "rc-autotrade",
      cwd: __dirname,
      script: "src/worker/index.js",
      // The worker itself runs on Bun (bun:sqlite, Bun fetch, Bun test).
      // PM2 may still be installed under Node; only the child interpreter
      // changes here.
      interpreter: "bun",
      instances: 1,
      exec_mode: "fork",
      autorestart: true,
      watch: false,
      restart_delay: 2000,
      exp_backoff_restart_delay: 200,
      kill_timeout: 5000,
      max_memory_restart: "512M",
      time: true,
      merge_logs: true,
      env: {
        NODE_ENV: "production"
      }
    }
  ]
};
