//! Risk management and circuit breaker system.
//!
//! Simplified circuit breaker for Polymarket-only trading with
//! configurable risk limits and automatic trading halt.

use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use tracing::{error, warn, info};

use crate::config::{TOTAL_CAPITAL_CENTS, RESERVE_PCT};

/// Circuit breaker configuration
#[derive(Debug, Clone)]
pub struct CircuitBreakerConfig {
    /// Maximum position size per market (contracts)
    pub max_position_per_market: i64,
    /// Maximum total position across all markets (contracts)
    pub max_total_position: i64,
    /// Maximum daily loss in cents before halting
    pub max_daily_loss_cents: i64,
    /// Maximum consecutive errors before halting
    pub max_consecutive_errors: u32,
    /// Cooldown period after trip (seconds)
    pub cooldown_secs: u64,
    /// Whether circuit breaker is enabled
    pub enabled: bool,
}

impl CircuitBreakerConfig {
    pub fn from_env() -> Self {
        Self {
            max_position_per_market: std::env::var("CB_MAX_POSITION_PER_MARKET")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(100), // 100 contracts max per market

            max_total_position: std::env::var("CB_MAX_TOTAL_POSITION")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(200), // 200 contracts total

            max_daily_loss_cents: std::env::var("CB_MAX_DAILY_LOSS")
                .ok()
                .and_then(|v| v.parse::<f64>().ok())
                .map(|d| (d * 100.0) as i64)
                .unwrap_or(2000), // $20 max daily loss

            max_consecutive_errors: std::env::var("CB_MAX_CONSECUTIVE_ERRORS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(5),

            cooldown_secs: std::env::var("CB_COOLDOWN_SECS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(300), // 5 minutes

            enabled: std::env::var("CB_ENABLED")
                .map(|v| v != "0" && v.to_lowercase() != "false")
                .unwrap_or(true),
        }
    }

    /// Conservative config for $100 capital
    pub fn conservative() -> Self {
        Self {
            max_position_per_market: 50,
            max_total_position: 100,
            max_daily_loss_cents: 2000, // $20
            max_consecutive_errors: 3,
            cooldown_secs: 600, // 10 minutes
            enabled: true,
        }
    }
}

impl Default for CircuitBreakerConfig {
    fn default() -> Self {
        Self::from_env()
    }
}

/// Reason for circuit breaker trip
#[derive(Debug, Clone, PartialEq)]
pub enum TripReason {
    MaxPositionPerMarket { market: String, position: i64, limit: i64 },
    MaxTotalPosition { position: i64, limit: i64 },
    MaxDailyLoss { loss_cents: i64, limit_cents: i64 },
    ConsecutiveErrors { count: u32, limit: u32 },
    ManualHalt,
    CapitalDepleted,
}

impl std::fmt::Display for TripReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TripReason::MaxPositionPerMarket { market, position, limit } =>
                write!(f, "Max position per market: {} has {} contracts (limit: {})", market, position, limit),
            TripReason::MaxTotalPosition { position, limit } =>
                write!(f, "Max total position: {} contracts (limit: {})", position, limit),
            TripReason::MaxDailyLoss { loss_cents, limit_cents } =>
                write!(f, "Max daily loss: ${:.2} (limit: ${:.2})", *loss_cents as f64 / 100.0, *limit_cents as f64 / 100.0),
            TripReason::ConsecutiveErrors { count, limit } =>
                write!(f, "Consecutive errors: {} (limit: {})", count, limit),
            TripReason::ManualHalt =>
                write!(f, "Manual halt triggered"),
            TripReason::CapitalDepleted =>
                write!(f, "Capital depleted below reserve"),
        }
    }
}

/// Market position tracking
#[derive(Debug, Default, Clone)]
pub struct MarketPosition {
    pub contracts: i64,
    pub cost_cents: i64,
}

/// Circuit breaker state
pub struct CircuitBreaker {
    config: CircuitBreakerConfig,
    /// Whether trading is halted
    halted: AtomicBool,
    /// Trip timestamp
    tripped_at: RwLock<Option<Instant>>,
    /// Trip reason
    trip_reason: RwLock<Option<TripReason>>,
    /// Consecutive error count
    consecutive_errors: AtomicI64,
    /// Daily P&L in cents
    daily_pnl_cents: AtomicI64,
    /// Current capital in cents
    current_capital_cents: AtomicI64,
    /// Positions per market
    positions: RwLock<std::collections::HashMap<String, MarketPosition>>,
}

impl CircuitBreaker {
    pub fn new(config: CircuitBreakerConfig) -> Self {
        info!("[CB] Circuit breaker initialized:");
        info!("[CB]   Enabled: {}", config.enabled);
        info!("[CB]   Max position per market: {} contracts", config.max_position_per_market);
        info!("[CB]   Max total position: {} contracts", config.max_total_position);
        info!("[CB]   Max daily loss: ${:.2}", config.max_daily_loss_cents as f64 / 100.0);
        info!("[CB]   Max consecutive errors: {}", config.max_consecutive_errors);
        info!("[CB]   Cooldown: {}s", config.cooldown_secs);

        Self {
            config,
            halted: AtomicBool::new(false),
            tripped_at: RwLock::new(None),
            trip_reason: RwLock::new(None),
            consecutive_errors: AtomicI64::new(0),
            daily_pnl_cents: AtomicI64::new(0),
            current_capital_cents: AtomicI64::new(TOTAL_CAPITAL_CENTS as i64),
            positions: RwLock::new(std::collections::HashMap::new()),
        }
    }

    /// Check if trading is allowed
    pub fn is_trading_allowed(&self) -> bool {
        if !self.config.enabled {
            return true;
        }
        !self.halted.load(Ordering::SeqCst)
    }

    /// Check if we can execute a trade
    pub async fn can_execute(&self, market_id: &str, contracts: i64) -> Result<(), TripReason> {
        if !self.config.enabled {
            return Ok(());
        }

        if self.halted.load(Ordering::SeqCst) {
            let reason = self.trip_reason.read().await;
            return Err(reason.clone().unwrap_or(TripReason::ManualHalt));
        }

        // Check capital reserve
        let reserve_cents = ((TOTAL_CAPITAL_CENTS as f64) * RESERVE_PCT) as i64;
        let current = self.current_capital_cents.load(Ordering::SeqCst);
        if current < reserve_cents {
            return Err(TripReason::CapitalDepleted);
        }

        // Check position limits
        let positions = self.positions.read().await;

        // Per-market limit
        if let Some(pos) = positions.get(market_id) {
            let new_position = pos.contracts + contracts;
            if new_position > self.config.max_position_per_market {
                return Err(TripReason::MaxPositionPerMarket {
                    market: market_id.to_string(),
                    position: new_position,
                    limit: self.config.max_position_per_market,
                });
            }
        }

        // Total position limit
        let total: i64 = positions.values().map(|p| p.contracts).sum();
        if total + contracts > self.config.max_total_position {
            return Err(TripReason::MaxTotalPosition {
                position: total + contracts,
                limit: self.config.max_total_position,
            });
        }

        // Daily loss limit
        let daily_pnl = self.daily_pnl_cents.load(Ordering::SeqCst);
        if -daily_pnl > self.config.max_daily_loss_cents {
            return Err(TripReason::MaxDailyLoss {
                loss_cents: -daily_pnl,
                limit_cents: self.config.max_daily_loss_cents,
            });
        }

        Ok(())
    }

    /// Record a successful execution
    pub async fn record_success(
        &self,
        market_id: &str,
        contracts: i64,
        _matched: i64,
        pnl_dollars: f64,
        cost_cents: i64,
        payout_cents: i64,
    ) {
        self.consecutive_errors.store(0, Ordering::SeqCst);

        let pnl_cents = (pnl_dollars * 100.0) as i64;
        // Reduce capital by cost, then add payout (net = pnl)
        self.current_capital_cents.fetch_sub(cost_cents, Ordering::SeqCst);
        self.current_capital_cents.fetch_add(payout_cents, Ordering::SeqCst);

        self.daily_pnl_cents.fetch_add(pnl_cents, Ordering::SeqCst);

        let mut positions = self.positions.write().await;
        let pos = positions.entry(market_id.to_string()).or_default();
        pos.contracts += contracts;
    }

    /// Record an error
    pub async fn record_error(&self) {
        let errors = self.consecutive_errors.fetch_add(1, Ordering::SeqCst) + 1;

        if errors >= self.config.max_consecutive_errors as i64 {
            self.trip(TripReason::ConsecutiveErrors {
                count: errors as u32,
                limit: self.config.max_consecutive_errors,
            }).await;
        }
    }

    /// Trip the circuit breaker
    pub async fn trip(&self, reason: TripReason) {
        if !self.config.enabled {
            return;
        }

        error!("[CB] CIRCUIT BREAKER TRIPPED: {}", reason);

        self.halted.store(true, Ordering::SeqCst);
        *self.tripped_at.write().await = Some(Instant::now());
        *self.trip_reason.write().await = Some(reason);
    }

    /// Reset the circuit breaker
    pub async fn reset(&self) {
        info!("[CB] Circuit breaker reset");
        self.halted.store(false, Ordering::SeqCst);
        *self.tripped_at.write().await = None;
        *self.trip_reason.write().await = None;
        self.consecutive_errors.store(0, Ordering::SeqCst);
    }

    /// Reset daily P&L
    pub fn reset_daily_pnl(&self) {
        info!("[CB] Daily P&L reset");
        self.daily_pnl_cents.store(0, Ordering::SeqCst);
    }

    /// Available capital after reserving safety buffer
    pub fn available_capital_cents(&self) -> i64 {
        let reserve_cents = ((TOTAL_CAPITAL_CENTS as f64) * RESERVE_PCT) as i64;
        let current = self.current_capital_cents.load(Ordering::SeqCst);
        (current - reserve_cents).max(0)
    }

    /// Get current status
    pub async fn status(&self) -> CircuitBreakerStatus {
        let positions = self.positions.read().await;
        let total_position: i64 = positions.values().map(|p| p.contracts).sum();

        CircuitBreakerStatus {
            enabled: self.config.enabled,
            halted: self.halted.load(Ordering::SeqCst),
            trip_reason: self.trip_reason.read().await.clone(),
            consecutive_errors: self.consecutive_errors.load(Ordering::SeqCst) as u32,
            daily_pnl_cents: self.daily_pnl_cents.load(Ordering::SeqCst),
            current_capital_cents: self.current_capital_cents.load(Ordering::SeqCst),
            total_position,
            market_count: positions.len(),
        }
    }
}

/// Circuit breaker status
#[derive(Debug, Clone)]
pub struct CircuitBreakerStatus {
    pub enabled: bool,
    pub halted: bool,
    pub trip_reason: Option<TripReason>,
    pub consecutive_errors: u32,
    pub daily_pnl_cents: i64,
    pub current_capital_cents: i64,
    pub total_position: i64,
    pub market_count: usize,
}

impl std::fmt::Display for CircuitBreakerStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if !self.enabled {
            return write!(f, "Circuit Breaker: DISABLED");
        }

        if self.halted {
            write!(f, "[CB] HALTED")?;
            if let Some(reason) = &self.trip_reason {
                write!(f, " ({})", reason)?;
            }
        } else {
            write!(f, "[CB] OK")?;
        }

        write!(
            f,
            " | P&L: ${:.2} | Capital: ${:.2} | Pos: {} | Errors: {}",
            self.daily_pnl_cents as f64 / 100.0,
            self.current_capital_cents as f64 / 100.0,
            self.total_position,
            self.consecutive_errors
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_circuit_breaker_basic() {
        let config = CircuitBreakerConfig::conservative();
        let cb = CircuitBreaker::new(config);

        assert!(cb.is_trading_allowed());
        assert!(cb.can_execute("market1", 10).await.is_ok());
    }

    #[tokio::test]
    async fn test_consecutive_errors() {
        let mut config = CircuitBreakerConfig::conservative();
        config.max_consecutive_errors = 3;
        let cb = CircuitBreaker::new(config);

        cb.record_error().await;
        cb.record_error().await;
        assert!(cb.is_trading_allowed());

        cb.record_error().await;
        assert!(!cb.is_trading_allowed());
    }
}
