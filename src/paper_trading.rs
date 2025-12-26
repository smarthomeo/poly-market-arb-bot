//! Paper trading simulation framework.
//!
//! This module simulates order execution for testing strategies without
//! risking real capital. It adds realistic latency, partial fills, and
//! slippage to closely mimic live trading conditions.

use chrono::{DateTime, Utc};
use rand::Rng;
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Duration;
use tokio::sync::RwLock;
use tracing::{info, warn};

use crate::config::{
    PAPER_EXEC_DELAY_MAX_MS, PAPER_EXEC_DELAY_MIN_MS, PAPER_FILL_RATE_MAX, PAPER_FILL_RATE_MIN,
    PAPER_SLIPPAGE_MAX_CENTS, PAPER_SLIPPAGE_MIN_CENTS, PAPER_TRADING_DAYS, TOTAL_CAPITAL_CENTS,
};
use crate::market_scanner::TrackedMarket;

/// Paper trading state and statistics
pub struct PaperTrader {
    /// Whether paper trading mode is active
    pub enabled: AtomicBool,

    /// Virtual balance in cents
    pub virtual_balance_cents: RwLock<i64>,

    /// Starting balance in cents
    pub starting_balance_cents: i64,

    /// Day counter (starts at 1)
    pub day_number: AtomicU32,

    /// When paper trading started
    pub started_at: DateTime<Utc>,

    /// Daily statistics
    pub daily_stats: RwLock<Vec<DailyStats>>,

    /// All simulated trades
    pub trades: RwLock<Vec<SimulatedTrade>>,

    /// Configuration
    pub config: PaperTradingConfig,
}

/// Configuration for paper trading simulation
#[derive(Debug, Clone)]
pub struct PaperTradingConfig {
    /// Minimum simulated execution delay (ms)
    pub exec_delay_min_ms: u64,
    /// Maximum simulated execution delay (ms)
    pub exec_delay_max_ms: u64,
    /// Minimum fill rate (0.0-1.0)
    pub fill_rate_min: f64,
    /// Maximum fill rate (0.0-1.0)
    pub fill_rate_max: f64,
    /// Minimum slippage in cents
    pub slippage_min_cents: i16,
    /// Maximum slippage in cents
    pub slippage_max_cents: i16,
    /// Required days before live trading
    pub required_days: u32,
    /// Required win rate to graduate (0.0-1.0)
    pub required_win_rate: f64,
    /// Required profit in cents to graduate
    pub required_profit_cents: i64,
}

impl Default for PaperTradingConfig {
    fn default() -> Self {
        Self {
            exec_delay_min_ms: PAPER_EXEC_DELAY_MIN_MS,
            exec_delay_max_ms: PAPER_EXEC_DELAY_MAX_MS,
            fill_rate_min: PAPER_FILL_RATE_MIN,
            fill_rate_max: PAPER_FILL_RATE_MAX,
            slippage_min_cents: PAPER_SLIPPAGE_MIN_CENTS,
            slippage_max_cents: PAPER_SLIPPAGE_MAX_CENTS,
            required_days: PAPER_TRADING_DAYS,
            required_win_rate: 0.60,
            required_profit_cents: 1000, // $10
        }
    }
}

/// Daily statistics
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DailyStats {
    pub date: String,
    pub day_number: u32,
    pub arbs_detected: u64,
    pub arbs_executed: u64,
    pub arbs_profitable: u64,
    pub arbs_missed: u64,
    pub profit_cents: i64,
    pub largest_win_cents: i64,
    pub largest_loss_cents: i64,
    pub avg_detection_latency_ms: f64,
    pub balance_cents: i64,
}

/// A simulated trade record
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SimulatedTrade {
    pub id: u64,
    pub timestamp: DateTime<Utc>,
    pub market_question: String,
    pub outcome_count: usize,
    pub expected_profit_cents: i16,
    pub actual_profit_cents: i16,
    pub fill_rate: f64,
    pub slippage_cents: i16,
    pub execution_delay_ms: u64,
    pub success: bool,
    pub reason: String,
}

/// Result of a simulated execution
#[derive(Debug, Clone)]
pub struct SimulatedResult {
    pub success: bool,
    pub filled_outcomes: usize,
    pub total_outcomes: usize,
    pub expected_profit_cents: i16,
    pub actual_profit_cents: i16,
    pub slippage_cents: i16,
    pub execution_delay_ms: u64,
    pub reason: String,
}

impl PaperTrader {
    pub fn new() -> Self {
        Self {
            enabled: AtomicBool::new(true),
            virtual_balance_cents: RwLock::new(TOTAL_CAPITAL_CENTS as i64),
            starting_balance_cents: TOTAL_CAPITAL_CENTS as i64,
            day_number: AtomicU32::new(1),
            started_at: Utc::now(),
            daily_stats: RwLock::new(Vec::new()),
            trades: RwLock::new(Vec::new()),
            config: PaperTradingConfig::default(),
        }
    }

    pub fn with_config(config: PaperTradingConfig) -> Self {
        Self {
            enabled: AtomicBool::new(true),
            virtual_balance_cents: RwLock::new(TOTAL_CAPITAL_CENTS as i64),
            starting_balance_cents: TOTAL_CAPITAL_CENTS as i64,
            day_number: AtomicU32::new(1),
            started_at: Utc::now(),
            daily_stats: RwLock::new(Vec::new()),
            trades: RwLock::new(Vec::new()),
            config,
        }
    }

    /// Check if paper trading is enabled
    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    /// Disable paper trading (switch to live)
    pub fn disable(&self) {
        self.enabled.store(false, Ordering::Relaxed);
    }

    /// Get current virtual balance in cents
    pub async fn balance_cents(&self) -> i64 {
        *self.virtual_balance_cents.read().await
    }

    /// Get current profit/loss in cents
    pub async fn pnl_cents(&self) -> i64 {
        let balance = *self.virtual_balance_cents.read().await;
        balance - self.starting_balance_cents
    }

    /// Simulate execution of an arbitrage opportunity
    pub async fn simulate_execution(&self, market: &TrackedMarket) -> SimulatedResult {
        // Generate all random values before any await to avoid Send issues
        let (delay_ms, fill_rate, slippage, exposure_loss) = {
            let mut rng = rand::thread_rng();
            let delay = rng.gen_range(self.config.exec_delay_min_ms..=self.config.exec_delay_max_ms);
            let fill = rng.gen_range(self.config.fill_rate_min..=self.config.fill_rate_max);
            let slip = rng.gen_range(self.config.slippage_min_cents..=self.config.slippage_max_cents);
            let loss = rng.gen_range(1i16..=5i16);
            (delay, fill, slip, loss)
        };

        // Simulate execution delay
        tokio::time::sleep(Duration::from_millis(delay_ms)).await;

        let outcome_count = market.outcomes.len();
        let expected_profit = market.potential_profit_cents();

        // Simulate fill rate for each outcome
        let filled_outcomes = ((outcome_count as f64) * fill_rate).round() as usize;

        // Calculate actual profit
        let (success, actual_profit, reason) = if filled_outcomes < outcome_count {
            // Partial fill - not all outcomes filled
            // In reality, this creates exposure. Simulate closing at a loss.
            (
                false,
                -exposure_loss,
                format!(
                    "Partial fill: {}/{} outcomes",
                    filled_outcomes, outcome_count
                ),
            )
        } else if expected_profit <= slippage {
            // Slippage ate the profit
            (false, expected_profit - slippage, "Slippage exceeded profit".to_string())
        } else {
            // Successful arb
            let profit = expected_profit - slippage;
            (true, profit, "Success".to_string())
        };

        // Update virtual balance
        {
            let mut balance = self.virtual_balance_cents.write().await;
            *balance += actual_profit as i64;
        }

        // Record the trade
        let trade = SimulatedTrade {
            id: self.trades.read().await.len() as u64 + 1,
            timestamp: Utc::now(),
            market_question: market.question.clone(),
            outcome_count,
            expected_profit_cents: expected_profit,
            actual_profit_cents: actual_profit,
            fill_rate,
            slippage_cents: slippage,
            execution_delay_ms: delay_ms,
            success,
            reason: reason.clone(),
        };

        self.trades.write().await.push(trade);

        SimulatedResult {
            success,
            filled_outcomes,
            total_outcomes: outcome_count,
            expected_profit_cents: expected_profit,
            actual_profit_cents: actual_profit,
            slippage_cents: slippage,
            execution_delay_ms: delay_ms,
            reason,
        }
    }

    /// Calculate win rate
    pub async fn win_rate(&self) -> f64 {
        let trades = self.trades.read().await;
        if trades.is_empty() {
            return 0.0;
        }

        let wins = trades.iter().filter(|t| t.success).count();
        wins as f64 / trades.len() as f64
    }

    /// Calculate total trades
    pub async fn total_trades(&self) -> usize {
        self.trades.read().await.len()
    }

    /// Calculate profitable trades
    pub async fn profitable_trades(&self) -> usize {
        self.trades.read().await.iter().filter(|t| t.success).count()
    }

    /// Check if ready to graduate to live trading
    pub async fn can_go_live(&self) -> (bool, String) {
        let days = self.day_number.load(Ordering::Relaxed);
        let win_rate = self.win_rate().await;
        let pnl = self.pnl_cents().await;
        let trades = self.total_trades().await;

        // Check days requirement
        if days < self.config.required_days {
            return (
                false,
                format!(
                    "Need {} more days of paper trading",
                    self.config.required_days - days
                ),
            );
        }

        // Check minimum trades
        if trades < 10 {
            return (false, format!("Need at least 10 trades, have {}", trades));
        }

        // Check win rate
        if win_rate < self.config.required_win_rate {
            return (
                false,
                format!(
                    "Win rate {:.1}% below required {:.1}%",
                    win_rate * 100.0,
                    self.config.required_win_rate * 100.0
                ),
            );
        }

        // Check profit
        if pnl < self.config.required_profit_cents {
            return (
                false,
                format!(
                    "Simulated profit ${:.2} below required ${:.2}",
                    pnl as f64 / 100.0,
                    self.config.required_profit_cents as f64 / 100.0
                ),
            );
        }

        (true, "All requirements met!".to_string())
    }

    /// Increment day counter (call daily)
    pub fn next_day(&self) {
        self.day_number.fetch_add(1, Ordering::Relaxed);
    }

    /// Get summary for logging
    pub async fn summary(&self) -> PaperTradingSummary {
        let balance = self.balance_cents().await;
        let pnl = self.pnl_cents().await;
        let trades = self.trades.read().await;

        let wins = trades.iter().filter(|t| t.success).count();
        let losses = trades.len() - wins;

        let largest_win = trades
            .iter()
            .filter(|t| t.actual_profit_cents > 0)
            .map(|t| t.actual_profit_cents)
            .max()
            .unwrap_or(0);

        let largest_loss = trades
            .iter()
            .filter(|t| t.actual_profit_cents < 0)
            .map(|t| -t.actual_profit_cents)
            .max()
            .unwrap_or(0);

        let avg_profit = if !trades.is_empty() {
            trades.iter().map(|t| t.actual_profit_cents as f64).sum::<f64>() / trades.len() as f64
        } else {
            0.0
        };

        let avg_delay = if !trades.is_empty() {
            trades.iter().map(|t| t.execution_delay_ms as f64).sum::<f64>() / trades.len() as f64
        } else {
            0.0
        };

        PaperTradingSummary {
            enabled: self.is_enabled(),
            day_number: self.day_number.load(Ordering::Relaxed),
            started_at: self.started_at,
            balance_cents: balance,
            pnl_cents: pnl,
            total_trades: trades.len(),
            wins,
            losses,
            win_rate: if trades.is_empty() {
                0.0
            } else {
                wins as f64 / trades.len() as f64
            },
            largest_win_cents: largest_win,
            largest_loss_cents: largest_loss,
            avg_profit_cents: avg_profit,
            avg_execution_delay_ms: avg_delay,
        }
    }

    /// Print summary to logs
    pub async fn print_summary(&self) {
        let summary = self.summary().await;
        let (can_go_live, reason) = self.can_go_live().await;

        info!("=== PAPER TRADING STATUS ===");
        info!(
            "Day {}/{} | Started: {}",
            summary.day_number,
            self.config.required_days,
            summary.started_at.format("%Y-%m-%d")
        );
        info!(
            "Balance: ${:.2} | P&L: {:+.2}",
            summary.balance_cents as f64 / 100.0,
            summary.pnl_cents as f64 / 100.0
        );
        info!(
            "Trades: {} total | {} wins | {} losses | {:.1}% win rate",
            summary.total_trades,
            summary.wins,
            summary.losses,
            summary.win_rate * 100.0
        );
        info!(
            "Best: +{}¢ | Worst: -{}¢ | Avg: {:.1}¢",
            summary.largest_win_cents, summary.largest_loss_cents, summary.avg_profit_cents
        );
        info!("Avg Execution Delay: {:.0}ms", summary.avg_execution_delay_ms);
        info!("");

        if can_go_live {
            info!("✅ READY FOR LIVE TRADING: {}", reason);
        } else {
            warn!("⏳ NOT READY: {}", reason);
        }

        info!("============================");
    }

    /// Save state to file
    pub async fn save(&self, path: &str) -> anyhow::Result<()> {
        let state = PaperTradingState {
            enabled: self.is_enabled(),
            balance_cents: self.balance_cents().await,
            starting_balance_cents: self.starting_balance_cents,
            day_number: self.day_number.load(Ordering::Relaxed),
            started_at: self.started_at,
            trades: self.trades.read().await.clone(),
            daily_stats: self.daily_stats.read().await.clone(),
        };

        let json = serde_json::to_string_pretty(&state)?;
        tokio::fs::write(path, json).await?;
        Ok(())
    }

    /// Load state from file
    pub async fn load(path: &str) -> anyhow::Result<Self> {
        let json = tokio::fs::read_to_string(path).await?;
        let state: PaperTradingState = serde_json::from_str(&json)?;

        let trader = Self::new();
        trader.enabled.store(state.enabled, Ordering::Relaxed);
        *trader.virtual_balance_cents.write().await = state.balance_cents;
        trader
            .day_number
            .store(state.day_number, Ordering::Relaxed);
        *trader.trades.write().await = state.trades;
        *trader.daily_stats.write().await = state.daily_stats;

        Ok(trader)
    }
}

impl Default for PaperTrader {
    fn default() -> Self {
        Self::new()
    }
}

/// Summary of paper trading status
#[derive(Debug, Clone)]
pub struct PaperTradingSummary {
    pub enabled: bool,
    pub day_number: u32,
    pub started_at: DateTime<Utc>,
    pub balance_cents: i64,
    pub pnl_cents: i64,
    pub total_trades: usize,
    pub wins: usize,
    pub losses: usize,
    pub win_rate: f64,
    pub largest_win_cents: i16,
    pub largest_loss_cents: i16,
    pub avg_profit_cents: f64,
    pub avg_execution_delay_ms: f64,
}

/// Serializable state for persistence
#[derive(Debug, Clone, Serialize, Deserialize)]
struct PaperTradingState {
    enabled: bool,
    balance_cents: i64,
    starting_balance_cents: i64,
    day_number: u32,
    started_at: DateTime<Utc>,
    trades: Vec<SimulatedTrade>,
    daily_stats: Vec<DailyStats>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_paper_trader_initialization() {
        let trader = PaperTrader::new();
        assert!(trader.is_enabled());
        assert_eq!(trader.balance_cents().await, TOTAL_CAPITAL_CENTS as i64);
        assert_eq!(trader.pnl_cents().await, 0);
    }

    #[tokio::test]
    async fn test_can_go_live_checks() {
        let trader = PaperTrader::new();

        // Should fail - not enough days
        let (can, reason) = trader.can_go_live().await;
        assert!(!can);
        assert!(reason.contains("days"));

        // Simulate 14 days
        for _ in 0..14 {
            trader.next_day();
        }

        // Should still fail - no trades
        let (can, reason) = trader.can_go_live().await;
        assert!(!can);
        assert!(reason.contains("trades"));
    }

    #[tokio::test]
    async fn test_win_rate_calculation() {
        let trader = PaperTrader::new();

        // Add some simulated trades directly
        {
            let mut trades = trader.trades.write().await;
            for i in 0..10 {
                trades.push(SimulatedTrade {
                    id: i,
                    timestamp: Utc::now(),
                    market_question: "Test".to_string(),
                    outcome_count: 2,
                    expected_profit_cents: 5,
                    actual_profit_cents: if i < 7 { 4 } else { -2 },
                    fill_rate: 1.0,
                    slippage_cents: 1,
                    execution_delay_ms: 100,
                    success: i < 7,
                    reason: "Test".to_string(),
                });
            }
        }

        let win_rate = trader.win_rate().await;
        assert!((win_rate - 0.7).abs() < 0.01); // 70% win rate
    }
}
