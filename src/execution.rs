//! Order execution engine for Polymarket arbitrage.
//!
//! This module handles concurrent order execution across all outcomes,
//! capital management, and automatic exposure handling.

use anyhow::{Result, anyhow};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::{info, warn, error, debug};

use crate::config::{
    MAX_EXECUTION_TIME_MS, COOLDOWN_BETWEEN_ARBS_MS,
    max_capital_per_arb_cents, min_profit_for_outcomes,
    MIN_LIQUIDITY_CONTRACTS,
};
use crate::polymarket_clob::SharedAsyncClient;
use crate::types::{ExecutionRequest, ArbType, cents_to_price, monotonic_now_ns};
use crate::circuit_breaker::CircuitBreaker;
use crate::paper_trading::PaperTrader;
use crate::market_scanner::TrackedMarket;

// =============================================================================
// EXECUTION ENGINE
// =============================================================================

/// Execution engine for Polymarket arbitrage
pub struct ExecutionEngine {
    /// Polymarket CLOB client
    poly_client: Option<Arc<SharedAsyncClient>>,
    /// Circuit breaker for risk management
    circuit_breaker: Arc<CircuitBreaker>,
    /// Paper trader (if in paper mode)
    paper_trader: Option<Arc<PaperTrader>>,
    /// In-flight market tracking (deduplication)
    in_flight: Arc<[AtomicU64; 32]>, // 2048 markets via 32x u64 bitmask
    /// Dry run mode
    pub dry_run: bool,
    /// Last execution timestamp for cooldown
    last_execution: AtomicU64,
}

impl ExecutionEngine {
    pub fn new(
        poly_client: Option<Arc<SharedAsyncClient>>,
        circuit_breaker: Arc<CircuitBreaker>,
        paper_trader: Option<Arc<PaperTrader>>,
        dry_run: bool,
    ) -> Self {
        Self {
            poly_client,
            circuit_breaker,
            paper_trader,
            in_flight: Arc::new(std::array::from_fn(|_| AtomicU64::new(0))),
            dry_run,
            last_execution: AtomicU64::new(0),
        }
    }

    /// Process an execution request
    pub async fn process(&self, req: ExecutionRequest) -> Result<ExecutionResult> {
        let market_id = req.market_id;
        let start_ns = monotonic_now_ns();

        // Check cooldown
        let last_exec = self.last_execution.load(Ordering::Relaxed);
        let cooldown_ns = COOLDOWN_BETWEEN_ARBS_MS * 1_000_000;
        if start_ns - last_exec < cooldown_ns {
            return Ok(ExecutionResult::skipped(market_id, "Cooldown active"));
        }

        // Deduplication check
        if market_id < 2048 {
            let slot = (market_id / 64) as usize;
            let bit = market_id % 64;
            let mask = 1u64 << bit;
            let prev = self.in_flight[slot].fetch_or(mask, Ordering::AcqRel);
            if prev & mask != 0 {
                return Ok(ExecutionResult::skipped(market_id, "Already in-flight"));
            }
        }

        // Verify profit threshold
        let min_profit = min_profit_for_outcomes(req.ask_prices.len());
        if req.expected_profit_cents < min_profit as i16 {
            self.release_in_flight(market_id);
            return Ok(ExecutionResult::skipped(market_id, "Below profit threshold"));
        }

        // Calculate position size
        let max_contracts = self.calculate_position_size(&req)?;
        if max_contracts < 1 {
            self.release_in_flight(market_id);
            return Ok(ExecutionResult::skipped(market_id, "Insufficient liquidity or capital"));
        }

        // Circuit breaker check
        if let Err(reason) = self.circuit_breaker.can_execute(&req.condition_id, max_contracts as i64).await {
            self.release_in_flight(market_id);
            return Ok(ExecutionResult::skipped(market_id, "Circuit breaker"));
        }

        let detection_latency_ms = (start_ns - req.detected_ns) / 1_000_000;
        info!(
            "[EXEC] {} | {:?} | {} outcomes | {}¢ profit | {}x | {}ms latency",
            req.condition_id, req.arb_type, req.ask_prices.len(),
            req.expected_profit_cents, max_contracts, detection_latency_ms
        );

        // Paper trading mode
        if let Some(paper_trader) = &self.paper_trader {
            if paper_trader.is_enabled() {
                // Build a TrackedMarket for simulation
                let market = self.build_tracked_market(&req);
                let sim_result = paper_trader.simulate_execution(&market).await;

                self.release_in_flight_delayed(market_id);
                self.last_execution.store(monotonic_now_ns(), Ordering::Relaxed);

                return Ok(ExecutionResult {
                    market_id,
                    success: sim_result.success,
                    profit_cents: sim_result.actual_profit_cents,
                    latency_ns: monotonic_now_ns() - start_ns,
                    filled_outcomes: sim_result.filled_outcomes,
                    total_outcomes: sim_result.total_outcomes,
                    contracts: max_contracts,
                    error: if sim_result.success { None } else { Some(sim_result.reason) },
                });
            }
        }

        // Dry run mode
        if self.dry_run {
            info!("[EXEC] DRY RUN - would execute {} contracts across {} outcomes",
                max_contracts, req.ask_prices.len());
            self.release_in_flight_delayed(market_id);
            return Ok(ExecutionResult {
                market_id,
                success: true,
                profit_cents: req.expected_profit_cents,
                latency_ns: monotonic_now_ns() - start_ns,
                filled_outcomes: req.ask_prices.len(),
                total_outcomes: req.ask_prices.len(),
                contracts: max_contracts,
                error: Some("DRY_RUN".to_string()),
            });
        }

        // Execute all orders in parallel
        let result = self.execute_all_outcomes(&req, max_contracts).await;

        self.release_in_flight_delayed(market_id);
        self.last_execution.store(monotonic_now_ns(), Ordering::Relaxed);

        match result {
            Ok((fills, total_cost)) => {
                let filled_count = fills.iter().filter(|f| f.filled > 0.0).count();
                let min_filled = fills.iter().map(|f| f.filled as u16).min().unwrap_or(0);

                // Calculate actual profit
                let actual_profit = if min_filled > 0 {
                    let guaranteed_payout = min_filled as i32 * 100; // $1 per matched set
                    (guaranteed_payout - total_cost as i32) as i16
                } else {
                    0
                };

                let success = filled_count == req.ask_prices.len() && actual_profit > 0;

                if success {
                    self.circuit_breaker.record_success(
                        &req.condition_id,
                        min_filled as i64,
                        min_filled as i64,
                        actual_profit as f64 / 100.0,
                        total_cost as i64,
                        (min_filled as i64) * 100,
                    ).await;
                    info!(
                        "[EXEC] SUCCESS | {} contracts matched | {}¢ profit",
                        min_filled, actual_profit
                    );
                } else if filled_count > 0 {
                    warn!(
                        "[EXEC] PARTIAL | {}/{} outcomes filled | exposure created",
                        filled_count, req.ask_prices.len()
                    );
                    // TODO: Auto-close excess exposure
                }

                Ok(ExecutionResult {
                    market_id,
                    success,
                    profit_cents: actual_profit,
                    latency_ns: monotonic_now_ns() - start_ns,
                    filled_outcomes: filled_count,
                    total_outcomes: req.ask_prices.len(),
                    contracts: min_filled,
                    error: if success { None } else { Some("Partial fill".to_string()) },
                })
            }
            Err(e) => {
                self.circuit_breaker.record_error().await;
                error!("[EXEC] FAILED: {}", e);
                Ok(ExecutionResult {
                    market_id,
                    success: false,
                    profit_cents: 0,
                    latency_ns: monotonic_now_ns() - start_ns,
                    filled_outcomes: 0,
                    total_outcomes: req.ask_prices.len(),
                    contracts: 0,
                    error: Some(e.to_string()),
                })
            }
        }
    }

    /// Calculate position size based on capital and liquidity
    fn calculate_position_size(&self, req: &ExecutionRequest) -> Result<u16> {
        let available = self.circuit_breaker.available_capital_cents();
        if available <= 0 {
            return Ok(0);
        }

        let available_u32 = available as u32;
        let max_per_arb = max_capital_per_arb_cents();

        // Calculate cost per "set" (1 contract of each outcome)
        let cost_per_set: u32 = req.ask_prices.iter().map(|p| *p as u32).sum();

        if cost_per_set == 0 || cost_per_set >= 100 {
            return Ok(0); // No arb or invalid
        }

        // Max sets we can afford
        let max_by_capital = (max_per_arb.min(available_u32) / cost_per_set) as u16;

        // Max sets by liquidity (minimum across all outcomes)
        let max_by_liquidity = req.ask_sizes.iter()
            .map(|s| *s / 100) // Convert cents to contracts
            .min()
            .unwrap_or(0);

        // Apply minimum liquidity check
        if max_by_liquidity < MIN_LIQUIDITY_CONTRACTS {
            return Ok(0);
        }

        // Take minimum of capital and liquidity constraints
        let max_contracts = max_by_capital.min(max_by_liquidity);

        // Safety cap at 100 contracts
        Ok(max_contracts.min(100))
    }

    /// Execute FAK orders for all outcomes in parallel
    async fn execute_all_outcomes(
        &self,
        req: &ExecutionRequest,
        contracts: u16,
    ) -> Result<(Vec<FillResult>, u32)> {
        let client = self.poly_client.as_ref().ok_or_else(|| anyhow!("Execution client not initialized"))?;

        let futures: Vec<_> = req.token_ids.iter()
            .zip(req.ask_prices.iter())
            .map(|(token_id, price)| {
                let client = client.clone();
                let token = token_id.to_string();
                let price_f64 = cents_to_price(*price);
                let size = contracts as f64;

                async move {
                    let result = tokio::time::timeout(
                        Duration::from_millis(MAX_EXECUTION_TIME_MS),
                        client.buy_fak(&token, price_f64, size),
                    ).await;

                    match result {
                        Ok(Ok(fill)) => FillResult {
                            token_id: token,
                            filled: fill.filled_size,
                            cost: fill.fill_cost,
                            success: true,
                        },
                        Ok(Err(e)) => {
                            warn!("[EXEC] Order failed for {}: {}", token, e);
                            FillResult {
                                token_id: token,
                                filled: 0.0,
                                cost: 0.0,
                                success: false,
                            }
                        }
                        Err(_) => {
                            warn!("[EXEC] Order timeout for {}", token);
                            FillResult {
                                token_id: token,
                                filled: 0.0,
                                cost: 0.0,
                                success: false,
                            }
                        }
                    }
                }
            })
            .collect();

        let fills: Vec<FillResult> = futures_util::future::join_all(futures).await;

        let total_cost = (fills.iter().map(|f| f.cost).sum::<f64>() * 100.0) as u32;

        Ok((fills, total_cost))
    }

    /// Build TrackedMarket from ExecutionRequest for paper trading
    fn build_tracked_market(&self, req: &ExecutionRequest) -> TrackedMarket {
        use crate::market_scanner::Outcome;
        use crate::config::MarketCategory;
        use chrono::Utc;

        let outcomes: Vec<Outcome> = req.token_ids.iter()
            .zip(req.ask_prices.iter())
            .zip(req.ask_sizes.iter())
            .enumerate()
            .map(|(i, ((token, price), size))| {
                Outcome {
                    name: format!("Outcome {}", i + 1),
                    token_id: token.to_string(),
                    ask_price: *price as f64 / 100.0,
                    ask_size: *size as f64 / 100.0,
                    bid_price: (*price as f64 - 1.0) / 100.0,
                }
            })
            .collect();

        TrackedMarket {
            condition_id: req.condition_id.to_string(),
            question: format!("Market {}", req.market_id),
            slug: String::new(),
            outcomes,
            volume_usd: 100000.0,
            liquidity_usd: 10000.0,
            created_at: None,
            end_date: None,
            category: MarketCategory::Other,
            active: true,
            closed: false,
            is_new: false,
            last_updated: Utc::now(),
            tags: vec![],
        }
    }

    #[inline(always)]
    fn release_in_flight(&self, market_id: u16) {
        if market_id < 2048 {
            let slot = (market_id / 64) as usize;
            let bit = market_id % 64;
            let mask = !(1u64 << bit);
            self.in_flight[slot].fetch_and(mask, Ordering::Release);
        }
    }

    fn release_in_flight_delayed(&self, market_id: u16) {
        if market_id < 2048 {
            let in_flight = self.in_flight.clone();
            let slot = (market_id / 64) as usize;
            let bit = market_id % 64;
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(10)).await;
                let mask = !(1u64 << bit);
                in_flight[slot].fetch_and(mask, Ordering::Release);
            });
        }
    }
}

/// Result of a single outcome fill
#[derive(Debug)]
struct FillResult {
    token_id: String,
    filled: f64,
    cost: f64,
    success: bool,
}

/// Result of an execution attempt
#[derive(Debug, Clone)]
pub struct ExecutionResult {
    pub market_id: u16,
    pub success: bool,
    pub profit_cents: i16,
    pub latency_ns: u64,
    pub filled_outcomes: usize,
    pub total_outcomes: usize,
    pub contracts: u16,
    pub error: Option<String>,
}

impl ExecutionResult {
    fn skipped(market_id: u16, reason: &str) -> Self {
        Self {
            market_id,
            success: false,
            profit_cents: 0,
            latency_ns: 0,
            filled_outcomes: 0,
            total_outcomes: 0,
            contracts: 0,
            error: Some(reason.to_string()),
        }
    }
}

// =============================================================================
// EXECUTION CHANNEL AND LOOP
// =============================================================================

/// Create execution request channel
pub fn create_execution_channel() -> (mpsc::Sender<ExecutionRequest>, mpsc::Receiver<ExecutionRequest>) {
    mpsc::channel(256)
}

/// Main execution event loop
pub async fn run_execution_loop(
    mut rx: mpsc::Receiver<ExecutionRequest>,
    engine: Arc<ExecutionEngine>,
) {
    info!("[EXEC] Execution engine started (dry_run={})", engine.dry_run);

    while let Some(req) = rx.recv().await {
        let engine = engine.clone();

        tokio::spawn(async move {
            match engine.process(req).await {
                Ok(result) if result.success => {
                    info!(
                        "[EXEC] market_id={} | {}¢ profit | {}/{} outcomes | {}µs",
                        result.market_id, result.profit_cents,
                        result.filled_outcomes, result.total_outcomes,
                        result.latency_ns / 1000
                    );
                }
                Ok(result) => {
                    if !matches!(result.error.as_deref(), Some("Already in-flight") | Some("Cooldown active")) {
                        debug!(
                            "[EXEC] market_id={}: {:?}",
                            result.market_id, result.error
                        );
                    }
                }
                Err(e) => {
                    error!("[EXEC] Error: {}", e);
                }
            }
        });
    }

    info!("[EXEC] Execution engine stopped");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_monotonic_clock() {
        let t1 = monotonic_now_ns();
        std::thread::sleep(Duration::from_millis(10));
        let t2 = monotonic_now_ns();
        assert!(t2 > t1);
        assert!(t2 - t1 >= 10_000_000); // At least 10ms in ns
    }
}
