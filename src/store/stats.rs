use crate::clock::now_ms;
use crate::model::Stats;

use super::Store;

impl Store {
    // ── misc ────────────────────────────────────────────────────────────────

    pub fn reset_stats(&mut self) {
        let now = now_ms();
        self.state.stats = Stats {
            since: now,
            profit_rlt: 0.0,
            spent_rlt: 0.0,
            bought: 0,
            filled: 0,
            failed: 0,
            orders_sent: 0,
        };
        self.state.report_since = now;
        self.state.report.clear();
        // The loot log lives in `orders`; dropping the rows is what actually
        // makes the reset stick after the next history fetch or restart.
        self.state.history.clear();
        if let Err(error) = self.db.execute("DELETE FROM orders", []) {
            self.report_write_error("could not clear the loot log", &error);
        }
        if let Err(error) = self.db.execute("DELETE FROM report_products", []) {
            self.report_write_error("could not clear the profit report", &error);
        }
        if let Err(error) = self.write_stats() {
            self.report_write_error("could not reset stats", &error);
        }
        if let Err(error) = self.set_meta("report_since", &now.to_string()) {
            self.report_write_error("could not stamp report_since", &error);
        }
    }
}
