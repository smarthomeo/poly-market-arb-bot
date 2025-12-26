//! Position tracking and P&L calculation for Polymarket.
//!
//! Tracks all positions across multi-outcome markets, calculates cost basis,
//! and maintains real-time profit and loss calculations.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, RwLock};
use tracing::{info, warn};

const POSITION_FILE: &str = "positions.json";

/// Position in a single outcome
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct OutcomePosition {
    /// Outcome name/identifier
    pub outcome_name: String,
    /// Token ID
    pub token_id: String,
    /// Number of contracts held
    pub contracts: f64,
    /// Total cost paid (dollars)
    pub cost_basis: f64,
    /// Average price per contract
    pub avg_price: f64,
}

impl OutcomePosition {
    pub fn new(outcome_name: &str, token_id: &str) -> Self {
        Self {
            outcome_name: outcome_name.to_string(),
            token_id: token_id.to_string(),
            contracts: 0.0,
            cost_basis: 0.0,
            avg_price: 0.0,
        }
    }

    pub fn add(&mut self, contracts: f64, price: f64) {
        let new_cost = contracts * price;
        self.cost_basis += new_cost;
        self.contracts += contracts;
        if self.contracts > 0.0 {
            self.avg_price = self.cost_basis / self.contracts;
        }
    }

    /// Value if this outcome wins ($1 per contract)
    pub fn value_if_win(&self) -> f64 {
        self.contracts
    }

    /// Profit if this outcome wins
    pub fn profit_if_win(&self) -> f64 {
        self.value_if_win() - self.cost_basis
    }
}

/// Position in a multi-outcome market (arb position)
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MarketPosition {
    /// Market condition ID
    pub condition_id: String,
    /// Market question/description
    pub description: String,
    /// Positions in each outcome
    pub outcomes: Vec<OutcomePosition>,
    /// Total fees paid
    pub total_fees: f64,
    /// When position was opened
    pub opened_at: String,
    /// Status: "open", "closed", "resolved"
    pub status: String,
    /// Realized P&L (set when resolved)
    pub realized_pnl: Option<f64>,
}

impl MarketPosition {
    pub fn new(condition_id: &str, description: &str, outcome_count: usize) -> Self {
        Self {
            condition_id: condition_id.to_string(),
            description: description.to_string(),
            outcomes: Vec::with_capacity(outcome_count),
            total_fees: 0.0,
            opened_at: chrono::Utc::now().to_rfc3339(),
            status: "open".to_string(),
            realized_pnl: None,
        }
    }

    /// Total cost across all outcomes
    pub fn total_cost(&self) -> f64 {
        self.outcomes.iter().map(|o| o.cost_basis).sum::<f64>() + self.total_fees
    }

    /// Total contracts (sum across outcomes)
    pub fn total_contracts(&self) -> f64 {
        self.outcomes.iter().map(|o| o.contracts).sum()
    }

    /// Minimum contracts across all outcomes (matched sets)
    pub fn matched_contracts(&self) -> f64 {
        self.outcomes
            .iter()
            .map(|o| o.contracts)
            .fold(f64::MAX, f64::min)
    }

    /// Guaranteed profit for a complete arb (1 outcome wins, pays $1)
    pub fn guaranteed_profit(&self) -> f64 {
        let matched = self.matched_contracts();
        matched - self.total_cost()
    }

    /// Mark as resolved with winning outcome
    pub fn resolve(&mut self, winning_outcome_idx: usize) {
        let payout = self.outcomes
            .get(winning_outcome_idx)
            .map(|o| o.contracts)
            .unwrap_or(0.0);

        self.realized_pnl = Some(payout - self.total_cost());
        self.status = "resolved".to_string();
    }
}

/// Position tracker with persistence
#[derive(Debug, Serialize, Deserialize)]
pub struct PositionTracker {
    /// All positions keyed by condition_id
    positions: HashMap<String, MarketPosition>,
    /// Daily realized P&L
    pub daily_realized_pnl: f64,
    /// Trading date
    pub trading_date: String,
    /// All-time P&L
    pub all_time_pnl: f64,
}

impl Default for PositionTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl PositionTracker {
    pub fn new() -> Self {
        Self {
            positions: HashMap::new(),
            daily_realized_pnl: 0.0,
            trading_date: today_string(),
            all_time_pnl: 0.0,
        }
    }

    /// Load from file
    pub fn load() -> Self {
        Self::load_from(POSITION_FILE)
    }

    pub fn load_from<P: AsRef<Path>>(path: P) -> Self {
        match std::fs::read_to_string(path.as_ref()) {
            Ok(contents) => {
                match serde_json::from_str::<Self>(&contents) {
                    Ok(mut tracker) => {
                        let today = today_string();
                        if tracker.trading_date != today {
                            info!("[POSITIONS] New trading day, resetting daily P&L");
                            tracker.daily_realized_pnl = 0.0;
                            tracker.trading_date = today;
                        }
                        info!("[POSITIONS] Loaded {} positions", tracker.positions.len());
                        tracker
                    }
                    Err(e) => {
                        warn!("[POSITIONS] Parse error: {}", e);
                        Self::new()
                    }
                }
            }
            Err(_) => {
                info!("[POSITIONS] No file found, starting fresh");
                Self::new()
            }
        }
    }

    /// Save to file
    pub fn save(&self) -> Result<()> {
        self.save_to(POSITION_FILE)
    }

    pub fn save_to<P: AsRef<Path>>(&self, path: P) -> Result<()> {
        let json = serde_json::to_string_pretty(self)?;
        std::fs::write(path, json)?;
        Ok(())
    }

    /// Save asynchronously
    pub fn save_async(&self) {
        let json = match serde_json::to_string_pretty(self) {
            Ok(j) => j,
            Err(_) => return,
        };

        if tokio::runtime::Handle::try_current().is_ok() {
            tokio::spawn(async move {
                let _ = tokio::fs::write(POSITION_FILE, json).await;
            });
        } else {
            let _ = std::fs::write(POSITION_FILE, json);
        }
    }

    /// Record a fill
    pub fn record_fill(&mut self, fill: &FillRecord) {
        let position = self.positions
            .entry(fill.condition_id.clone())
            .or_insert_with(|| MarketPosition::new(&fill.condition_id, &fill.description, fill.outcome_count));

        // Find or create outcome position
        let outcome = position.outcomes
            .iter_mut()
            .find(|o| o.token_id == fill.token_id);

        if let Some(outcome) = outcome {
            outcome.add(fill.contracts, fill.price);
        } else {
            let mut new_outcome = OutcomePosition::new(&fill.outcome_name, &fill.token_id);
            new_outcome.add(fill.contracts, fill.price);
            position.outcomes.push(new_outcome);
        }

        position.total_fees += fill.fees;

        info!(
            "[POSITIONS] Fill: {} | {} | {} @${:.2} x{:.0}",
            fill.condition_id, fill.outcome_name, fill.token_id,
            fill.price, fill.contracts
        );

        self.save_async();
    }

    /// Get position
    pub fn get(&self, condition_id: &str) -> Option<&MarketPosition> {
        self.positions.get(condition_id)
    }

    /// Get all open positions
    pub fn open_positions(&self) -> Vec<&MarketPosition> {
        self.positions.values().filter(|p| p.status == "open").collect()
    }

    /// Summary statistics
    pub fn summary(&self) -> PositionSummary {
        let mut summary = PositionSummary::default();

        for position in self.positions.values() {
            match position.status.as_str() {
                "open" => {
                    summary.open_positions += 1;
                    summary.total_cost_basis += position.total_cost();
                    summary.total_guaranteed_profit += position.guaranteed_profit();
                    summary.total_contracts += position.total_contracts() as i64;
                }
                "resolved" => {
                    summary.resolved_positions += 1;
                    summary.realized_pnl += position.realized_pnl.unwrap_or(0.0);
                }
                _ => {}
            }
        }

        summary
    }
}

fn today_string() -> String {
    chrono::Utc::now().format("%Y-%m-%d").to_string()
}

/// Fill record for position tracking
#[derive(Debug, Clone)]
pub struct FillRecord {
    pub condition_id: String,
    pub description: String,
    pub outcome_name: String,
    pub token_id: String,
    pub outcome_count: usize,
    pub contracts: f64,
    pub price: f64,
    pub fees: f64,
    pub order_id: String,
    pub timestamp: String,
}

impl FillRecord {
    pub fn new(
        condition_id: &str,
        description: &str,
        outcome_name: &str,
        token_id: &str,
        outcome_count: usize,
        contracts: f64,
        price: f64,
        fees: f64,
        order_id: &str,
    ) -> Self {
        Self {
            condition_id: condition_id.to_string(),
            description: description.to_string(),
            outcome_name: outcome_name.to_string(),
            token_id: token_id.to_string(),
            outcome_count,
            contracts,
            price,
            fees,
            order_id: order_id.to_string(),
            timestamp: chrono::Utc::now().to_rfc3339(),
        }
    }
}

/// Summary statistics
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PositionSummary {
    pub total_cost_basis: f64,
    pub total_guaranteed_profit: f64,
    pub realized_pnl: f64,
    pub open_positions: usize,
    pub resolved_positions: usize,
    pub total_contracts: i64,
}

// Channel-based position tracking
pub type SharedPositionTracker = Arc<RwLock<PositionTracker>>;

#[derive(Clone)]
pub struct PositionChannel {
    tx: mpsc::UnboundedSender<FillRecord>,
}

impl PositionChannel {
    pub fn new(tx: mpsc::UnboundedSender<FillRecord>) -> Self {
        Self { tx }
    }

    #[inline]
    pub fn record_fill(&self, fill: FillRecord) {
        let _ = self.tx.send(fill);
    }
}

pub fn create_position_channel() -> (PositionChannel, mpsc::UnboundedReceiver<FillRecord>) {
    let (tx, rx) = mpsc::unbounded_channel();
    (PositionChannel::new(tx), rx)
}

pub async fn position_writer_loop(
    mut rx: mpsc::UnboundedReceiver<FillRecord>,
    tracker: SharedPositionTracker,
) {
    let mut batch = Vec::with_capacity(16);
    let mut interval = tokio::time::interval(Duration::from_millis(100));

    loop {
        tokio::select! {
            biased;

            Some(fill) = rx.recv() => {
                batch.push(fill);
                if batch.len() >= 16 {
                    let mut guard = tracker.write().await;
                    for fill in batch.drain(..) {
                        guard.record_fill(&fill);
                    }
                }
            }
            _ = interval.tick() => {
                if !batch.is_empty() {
                    let mut guard = tracker.write().await;
                    for fill in batch.drain(..) {
                        guard.record_fill(&fill);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_outcome_position() {
        let mut pos = OutcomePosition::new("Yes", "token1");
        pos.add(10.0, 0.45);

        assert_eq!(pos.contracts, 10.0);
        assert!((pos.cost_basis - 4.50).abs() < 0.001);
        assert!((pos.profit_if_win() - 5.50).abs() < 0.001);
    }

    #[test]
    fn test_market_position_profit() {
        let mut pos = MarketPosition::new("cond1", "Test", 2);

        pos.outcomes.push(OutcomePosition {
            outcome_name: "Yes".to_string(),
            token_id: "t1".to_string(),
            contracts: 10.0,
            cost_basis: 4.50,
            avg_price: 0.45,
        });

        pos.outcomes.push(OutcomePosition {
            outcome_name: "No".to_string(),
            token_id: "t2".to_string(),
            contracts: 10.0,
            cost_basis: 5.00,
            avg_price: 0.50,
        });

        // 10 matched sets, cost $9.50, payout $10
        assert!((pos.total_cost() - 9.50).abs() < 0.001);
        assert!((pos.matched_contracts() - 10.0).abs() < 0.001);
        assert!((pos.guaranteed_profit() - 0.50).abs() < 0.001);
    }
}
