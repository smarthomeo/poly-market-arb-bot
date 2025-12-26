//! Performance tracking and statistics for the arbitrage bot.
//!
//! This module tracks all key metrics to understand bot performance,
//! identify issues, and make data-driven improvements.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use tracing::info;

use crate::config::MarketCategory;

/// Global metrics instance
pub struct Metrics {
    /// When metrics collection started
    pub started_at: Instant,

    /// Timing metrics
    pub timing: TimingMetrics,

    /// Success/failure counts
    pub counts: CountMetrics,

    /// Financial metrics (in cents for precision)
    pub financial: FinancialMetrics,

    /// Per-category statistics
    pub by_category: RwLock<HashMap<MarketCategory, CategoryStats>>,

    /// Per-outcome-count statistics
    pub by_outcome_count: RwLock<HashMap<u8, OutcomeStats>>,

    /// Hourly statistics for trend analysis
    pub hourly_stats: RwLock<Vec<HourlySnapshot>>,
}

/// Timing-related metrics
pub struct TimingMetrics {
    /// Detection latency samples (nanoseconds)
    detection_latency_sum_ns: AtomicU64,
    detection_latency_count: AtomicU64,
    detection_latency_max_ns: AtomicU64,

    /// Execution latency samples (nanoseconds)
    execution_latency_sum_ns: AtomicU64,
    execution_latency_count: AtomicU64,
    execution_latency_max_ns: AtomicU64,

    /// WebSocket message processing time
    ws_processing_sum_ns: AtomicU64,
    ws_processing_count: AtomicU64,
}

/// Count-based metrics
pub struct CountMetrics {
    /// Arbitrage opportunities detected
    pub arbs_detected: AtomicU64,

    /// Arbitrage attempts (sent to execution)
    pub arbs_attempted: AtomicU64,

    /// Successful arbitrages (profitable)
    pub arbs_profitable: AtomicU64,

    /// Failed arbitrages (partial fill, slippage, etc.)
    pub arbs_failed: AtomicU64,

    /// Arbs skipped due to insufficient liquidity
    pub arbs_skipped_liquidity: AtomicU64,

    /// Arbs skipped due to capital constraints
    pub arbs_skipped_capital: AtomicU64,

    /// Arbs missed (detected but couldn't execute in time)
    pub arbs_missed: AtomicU64,

    /// Total WebSocket messages processed
    pub ws_messages_processed: AtomicU64,

    /// Markets currently tracked
    pub markets_tracked: AtomicU64,
}

/// Financial metrics (all in cents for precision)
pub struct FinancialMetrics {
    /// Total profit in cents (can be negative)
    pub total_profit_cents: AtomicI64,

    /// Total volume traded in cents
    pub total_volume_cents: AtomicU64,

    /// Largest single win in cents
    pub largest_win_cents: AtomicI64,

    /// Largest single loss in cents (stored as positive)
    pub largest_loss_cents: AtomicI64,

    /// Total fees paid in cents (gas, etc.)
    pub total_fees_cents: AtomicU64,

    /// Current capital in cents
    pub current_capital_cents: AtomicI64,
}

/// Statistics per market category
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CategoryStats {
    pub arbs_detected: u64,
    pub arbs_executed: u64,
    pub arbs_profitable: u64,
    pub total_profit_cents: i64,
    pub avg_profit_cents: f64,
}

/// Statistics per outcome count
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct OutcomeStats {
    pub arbs_detected: u64,
    pub arbs_executed: u64,
    pub arbs_profitable: u64,
    pub total_profit_cents: i64,
    pub avg_detection_latency_ms: f64,
}

/// Hourly snapshot for trend analysis
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HourlySnapshot {
    pub hour: u64, // Unix timestamp (hour)
    pub arbs_detected: u64,
    pub arbs_executed: u64,
    pub profit_cents: i64,
    pub markets_tracked: u64,
}

/// Daily report summary
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DailyReport {
    pub date: String,
    pub mode: String, // "PAPER" or "LIVE"
    pub day_number: u32,
    pub markets_tracked: u64,
    pub arbs_detected: u64,
    pub arbs_executed: u64,
    pub arbs_profitable: u64,
    pub arbs_missed: u64,
    pub win_rate_pct: f64,
    pub total_profit_cents: i64,
    pub avg_profit_cents: f64,
    pub largest_win_cents: i64,
    pub largest_loss_cents: i64,
    pub avg_detection_latency_ms: f64,
    pub avg_execution_latency_ms: f64,
    pub best_category: Option<String>,
    pub recommendation: String,
}

impl Metrics {
    pub fn new(initial_capital_cents: i64) -> Self {
        Self {
            started_at: Instant::now(),
            timing: TimingMetrics::new(),
            counts: CountMetrics::new(),
            financial: FinancialMetrics::new(initial_capital_cents),
            by_category: RwLock::new(HashMap::new()),
            by_outcome_count: RwLock::new(HashMap::new()),
            hourly_stats: RwLock::new(Vec::new()),
        }
    }

    /// Record an arb detection
    pub fn record_detection(&self, latency_ns: u64, category: MarketCategory, outcome_count: u8) {
        self.counts.arbs_detected.fetch_add(1, Ordering::Relaxed);
        self.timing.record_detection(latency_ns);

        // Update category stats
        tokio::spawn({
            let by_category = self.by_category.read();
            async move {
                // Note: In real impl, use write lock and update
            }
        });
    }

    /// Record an arb execution attempt
    pub fn record_execution(
        &self,
        latency_ns: u64,
        profit_cents: i64,
        success: bool,
        category: MarketCategory,
        outcome_count: u8,
    ) {
        self.counts.arbs_attempted.fetch_add(1, Ordering::Relaxed);
        self.timing.record_execution(latency_ns);

        if success && profit_cents > 0 {
            self.counts.arbs_profitable.fetch_add(1, Ordering::Relaxed);
            self.financial
                .total_profit_cents
                .fetch_add(profit_cents, Ordering::Relaxed);

            // Update largest win
            let mut current_max = self.financial.largest_win_cents.load(Ordering::Relaxed);
            while profit_cents > current_max {
                match self.financial.largest_win_cents.compare_exchange_weak(
                    current_max,
                    profit_cents,
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => break,
                    Err(c) => current_max = c,
                }
            }
        } else if !success || profit_cents < 0 {
            self.counts.arbs_failed.fetch_add(1, Ordering::Relaxed);

            if profit_cents < 0 {
                let loss = -profit_cents;
                self.financial
                    .total_profit_cents
                    .fetch_add(profit_cents, Ordering::Relaxed);

                // Update largest loss
                let mut current_max = self.financial.largest_loss_cents.load(Ordering::Relaxed);
                while loss > current_max {
                    match self.financial.largest_loss_cents.compare_exchange_weak(
                        current_max,
                        loss,
                        Ordering::Relaxed,
                        Ordering::Relaxed,
                    ) {
                        Ok(_) => break,
                        Err(c) => current_max = c,
                    }
                }
            }
        }
    }

    /// Record a skipped arb due to liquidity
    pub fn record_skip_liquidity(&self) {
        self.counts
            .arbs_skipped_liquidity
            .fetch_add(1, Ordering::Relaxed);
    }

    /// Record a skipped arb due to capital
    pub fn record_skip_capital(&self) {
        self.counts
            .arbs_skipped_capital
            .fetch_add(1, Ordering::Relaxed);
    }

    /// Record a missed arb (too slow)
    pub fn record_missed(&self) {
        self.counts.arbs_missed.fetch_add(1, Ordering::Relaxed);
    }

    /// Record WebSocket message processing
    pub fn record_ws_message(&self, processing_ns: u64) {
        self.counts
            .ws_messages_processed
            .fetch_add(1, Ordering::Relaxed);
        self.timing.record_ws_processing(processing_ns);
    }

    /// Update tracked markets count
    pub fn set_markets_tracked(&self, count: u64) {
        self.counts.markets_tracked.store(count, Ordering::Relaxed);
    }

    /// Get win rate percentage
    pub fn win_rate(&self) -> f64 {
        let attempted = self.counts.arbs_attempted.load(Ordering::Relaxed);
        let profitable = self.counts.arbs_profitable.load(Ordering::Relaxed);

        if attempted == 0 {
            0.0
        } else {
            (profitable as f64 / attempted as f64) * 100.0
        }
    }

    /// Get average detection latency in milliseconds
    pub fn avg_detection_latency_ms(&self) -> f64 {
        self.timing.avg_detection_latency_ms()
    }

    /// Get average execution latency in milliseconds
    pub fn avg_execution_latency_ms(&self) -> f64 {
        self.timing.avg_execution_latency_ms()
    }

    /// Generate daily report
    pub fn generate_daily_report(&self, mode: &str, day_number: u32) -> DailyReport {
        let arbs_detected = self.counts.arbs_detected.load(Ordering::Relaxed);
        let arbs_attempted = self.counts.arbs_attempted.load(Ordering::Relaxed);
        let arbs_profitable = self.counts.arbs_profitable.load(Ordering::Relaxed);
        let arbs_missed = self.counts.arbs_missed.load(Ordering::Relaxed);
        let total_profit = self.financial.total_profit_cents.load(Ordering::Relaxed);

        let win_rate = self.win_rate();
        let avg_profit = if arbs_attempted > 0 {
            total_profit as f64 / arbs_attempted as f64
        } else {
            0.0
        };

        let recommendation = if mode == "PAPER" {
            if day_number < 14 {
                format!("CONTINUE PAPER TRADING (Day {}/14)", day_number)
            } else if win_rate >= 60.0 && total_profit > 1000 {
                "READY FOR LIVE TRADING".to_string()
            } else if win_rate < 40.0 {
                "REVIEW STRATEGY - Win rate too low".to_string()
            } else {
                "EXTEND PAPER TRADING - Need more data".to_string()
            }
        } else if total_profit < -2000 {
            "WARNING: Consider pausing - significant losses".to_string()
        } else if win_rate < 50.0 {
            "MONITOR: Win rate below target".to_string()
        } else {
            "ON TRACK".to_string()
        };

        DailyReport {
            date: chrono::Utc::now().format("%Y-%m-%d").to_string(),
            mode: mode.to_string(),
            day_number,
            markets_tracked: self.counts.markets_tracked.load(Ordering::Relaxed),
            arbs_detected,
            arbs_executed: arbs_attempted,
            arbs_profitable,
            arbs_missed,
            win_rate_pct: win_rate,
            total_profit_cents: total_profit,
            avg_profit_cents: avg_profit,
            largest_win_cents: self.financial.largest_win_cents.load(Ordering::Relaxed),
            largest_loss_cents: self.financial.largest_loss_cents.load(Ordering::Relaxed),
            avg_detection_latency_ms: self.avg_detection_latency_ms(),
            avg_execution_latency_ms: self.avg_execution_latency_ms(),
            best_category: None, // TODO: Calculate from by_category
            recommendation,
        }
    }

    /// Print daily report to logs
    pub fn print_daily_report(&self, mode: &str, day_number: u32) {
        let report = self.generate_daily_report(mode, day_number);

        info!("=== DAILY ARB REPORT ===");
        info!("Date: {}", report.date);
        info!("Mode: {} (Day {}/14)", report.mode, report.day_number);
        info!("");
        info!("Markets Tracked: {}", report.markets_tracked);
        info!("Arbs Detected: {}", report.arbs_detected);
        info!(
            "Arbs Executed: {} (simulated)",
            report.arbs_executed
        );
        info!("Arbs Missed (too slow): {}", report.arbs_missed);
        info!("");
        info!(
            "Simulated P&L: {:+.2}",
            report.total_profit_cents as f64 / 100.0
        );
        info!(
            "Win Rate: {:.1}% ({}/{})",
            report.win_rate_pct, report.arbs_profitable, report.arbs_executed
        );
        info!("Avg Profit: ${:.2}", report.avg_profit_cents / 100.0);
        info!(
            "Avg Detection Latency: {:.0}ms",
            report.avg_detection_latency_ms
        );
        info!("");
        info!("Recommendation: {}", report.recommendation);
        info!("========================");
    }
}

impl TimingMetrics {
    fn new() -> Self {
        Self {
            detection_latency_sum_ns: AtomicU64::new(0),
            detection_latency_count: AtomicU64::new(0),
            detection_latency_max_ns: AtomicU64::new(0),
            execution_latency_sum_ns: AtomicU64::new(0),
            execution_latency_count: AtomicU64::new(0),
            execution_latency_max_ns: AtomicU64::new(0),
            ws_processing_sum_ns: AtomicU64::new(0),
            ws_processing_count: AtomicU64::new(0),
        }
    }

    fn record_detection(&self, latency_ns: u64) {
        self.detection_latency_sum_ns
            .fetch_add(latency_ns, Ordering::Relaxed);
        self.detection_latency_count.fetch_add(1, Ordering::Relaxed);

        let mut current_max = self.detection_latency_max_ns.load(Ordering::Relaxed);
        while latency_ns > current_max {
            match self.detection_latency_max_ns.compare_exchange_weak(
                current_max,
                latency_ns,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(c) => current_max = c,
            }
        }
    }

    fn record_execution(&self, latency_ns: u64) {
        self.execution_latency_sum_ns
            .fetch_add(latency_ns, Ordering::Relaxed);
        self.execution_latency_count.fetch_add(1, Ordering::Relaxed);

        let mut current_max = self.execution_latency_max_ns.load(Ordering::Relaxed);
        while latency_ns > current_max {
            match self.execution_latency_max_ns.compare_exchange_weak(
                current_max,
                latency_ns,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(c) => current_max = c,
            }
        }
    }

    fn record_ws_processing(&self, processing_ns: u64) {
        self.ws_processing_sum_ns
            .fetch_add(processing_ns, Ordering::Relaxed);
        self.ws_processing_count.fetch_add(1, Ordering::Relaxed);
    }

    fn avg_detection_latency_ms(&self) -> f64 {
        let sum = self.detection_latency_sum_ns.load(Ordering::Relaxed);
        let count = self.detection_latency_count.load(Ordering::Relaxed);

        if count == 0 {
            0.0
        } else {
            (sum as f64 / count as f64) / 1_000_000.0
        }
    }

    fn avg_execution_latency_ms(&self) -> f64 {
        let sum = self.execution_latency_sum_ns.load(Ordering::Relaxed);
        let count = self.execution_latency_count.load(Ordering::Relaxed);

        if count == 0 {
            0.0
        } else {
            (sum as f64 / count as f64) / 1_000_000.0
        }
    }
}

impl CountMetrics {
    fn new() -> Self {
        Self {
            arbs_detected: AtomicU64::new(0),
            arbs_attempted: AtomicU64::new(0),
            arbs_profitable: AtomicU64::new(0),
            arbs_failed: AtomicU64::new(0),
            arbs_skipped_liquidity: AtomicU64::new(0),
            arbs_skipped_capital: AtomicU64::new(0),
            arbs_missed: AtomicU64::new(0),
            ws_messages_processed: AtomicU64::new(0),
            markets_tracked: AtomicU64::new(0),
        }
    }
}

impl FinancialMetrics {
    fn new(initial_capital_cents: i64) -> Self {
        Self {
            total_profit_cents: AtomicI64::new(0),
            total_volume_cents: AtomicU64::new(0),
            largest_win_cents: AtomicI64::new(0),
            largest_loss_cents: AtomicI64::new(0),
            total_fees_cents: AtomicU64::new(0),
            current_capital_cents: AtomicI64::new(initial_capital_cents),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_metrics_initialization() {
        let metrics = Metrics::new(10_000);
        assert_eq!(
            metrics
                .financial
                .current_capital_cents
                .load(Ordering::Relaxed),
            10_000
        );
        assert_eq!(metrics.win_rate(), 0.0);
    }

    #[test]
    fn test_win_rate_calculation() {
        let metrics = Metrics::new(10_000);

        // Simulate 3 profitable, 2 failed
        for _ in 0..3 {
            metrics.record_execution(1000, 100, true, MarketCategory::Sports, 2);
        }
        for _ in 0..2 {
            metrics.record_execution(1000, -50, false, MarketCategory::Sports, 2);
        }

        assert!((metrics.win_rate() - 60.0).abs() < 0.1);
    }

    #[test]
    fn test_latency_tracking() {
        let metrics = Metrics::new(10_000);

        metrics.timing.record_detection(1_000_000); // 1ms
        metrics.timing.record_detection(2_000_000); // 2ms
        metrics.timing.record_detection(3_000_000); // 3ms

        assert!((metrics.avg_detection_latency_ms() - 2.0).abs() < 0.1);
    }
}
