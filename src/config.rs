//! System configuration for Polymarket-only arbitrage bot.
//!
//! This module contains all configuration constants, market filters,
//! and capital management settings.

/// Polymarket WebSocket URL
pub const POLYMARKET_WS_URL: &str = "wss://ws-subscriptions-clob.polymarket.com/ws/market";

/// Gamma API base URL (Polymarket market data)
pub const GAMMA_API_BASE: &str = "https://gamma-api.polymarket.com";

/// Polymarket CLOB API host
pub const POLY_CLOB_HOST: &str = "https://clob.polymarket.com";

/// Polygon chain ID
pub const POLYGON_CHAIN_ID: u64 = 137;

// =============================================================================
// CAPITAL MANAGEMENT ($100 account)
// =============================================================================

/// Total capital in cents ($100 = 10000 cents)
pub const TOTAL_CAPITAL_CENTS: u32 = 10_000;

/// Maximum capital per arbitrage as percentage (50% = $50 max per arb)
pub const MAX_PER_ARB_PCT: f64 = 0.50;

/// Reserve percentage - always keep this much available (30% = $30)
pub const RESERVE_PCT: f64 = 0.30;

/// Maximum total exposure as percentage (70% = $70 max at risk)
pub const MAX_TOTAL_EXPOSURE_PCT: f64 = 0.70;

// =============================================================================
// PROFIT THRESHOLDS
// =============================================================================

/// Minimum profit for binary markets (YES+NO) in cents
pub const MIN_PROFIT_BINARY_CENTS: u16 = 1;

/// Minimum profit for multi-outcome markets (3+) in cents
pub const MIN_PROFIT_MULTI_CENTS: u16 = 3;

/// Minimum profit for 5+ outcome markets in cents (higher threshold)
pub const MIN_PROFIT_LARGE_MULTI_CENTS: u16 = 5;

/// Arb threshold: execute when total cost < this (e.g., 0.99 = 1% profit minimum)
pub const ARB_THRESHOLD_BINARY: f64 = 0.99;
pub const ARB_THRESHOLD_MULTI: f64 = 0.97;

// =============================================================================
// MARKET FILTERS
// =============================================================================

/// Minimum market volume in USD to consider
pub const MIN_VOLUME_USD: f64 = 50_000.0;

/// Minimum number of outcomes (2 = binary)
pub const MIN_OUTCOMES: usize = 2;

/// Maximum number of outcomes to track
pub const MAX_OUTCOMES: usize = 8;

/// Maximum days until expiry (skip long-term markets)
pub const MAX_DAYS_TO_EXPIRY: u32 = 30;

/// Market considered "new" if created within this many hours
pub const NEW_MARKET_HOURS: u64 = 24;

// =============================================================================
// LIQUIDITY REQUIREMENTS
// =============================================================================

/// Minimum liquidity in contracts at best ask
pub const MIN_LIQUIDITY_CONTRACTS: u16 = 10;

/// Maximum slippage allowed before aborting (in cents)
pub const MAX_SLIPPAGE_CENTS: u16 = 2;

// =============================================================================
// TIMING & EXECUTION
// =============================================================================

/// Paper trading duration in days before allowing live trading
pub const PAPER_TRADING_DAYS: u32 = 14;

/// Cooldown between arb attempts in milliseconds
pub const COOLDOWN_BETWEEN_ARBS_MS: u64 = 5000;

/// Maximum execution time before abort (milliseconds)
pub const MAX_EXECUTION_TIME_MS: u64 = 2000;

/// Polymarket ping interval (seconds) - keep WebSocket alive
pub const POLY_PING_INTERVAL_SECS: u64 = 30;

/// WebSocket reconnect delay (seconds)
pub const WS_RECONNECT_DELAY_SECS: u64 = 5;

/// Market scanner refresh interval (seconds)
pub const MARKET_SCAN_INTERVAL_SECS: u64 = 300; // 5 minutes

// =============================================================================
// CATEGORIES
// =============================================================================

/// Market categories to enable
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum MarketCategory {
    Sports,
    Politics,
    Crypto,
    Entertainment,
    Science,
    Business,
    Other,
}

impl MarketCategory {
    /// Check if this category is enabled for trading
    pub fn is_enabled(&self) -> bool {
        matches!(
            self,
            MarketCategory::Sports
                | MarketCategory::Politics
                | MarketCategory::Crypto
                | MarketCategory::Entertainment
        )
    }

    /// Parse category from Polymarket tags/metadata
    pub fn from_tags(tags: &[String]) -> Self {
        let tags_lower: Vec<String> = tags.iter().map(|t| t.to_lowercase()).collect();

        if tags_lower.iter().any(|t| {
            t.contains("sports")
                || t.contains("nfl")
                || t.contains("nba")
                || t.contains("soccer")
                || t.contains("football")
                || t.contains("mlb")
                || t.contains("nhl")
        }) {
            return MarketCategory::Sports;
        }

        if tags_lower.iter().any(|t| {
            t.contains("politic")
                || t.contains("election")
                || t.contains("president")
                || t.contains("congress")
                || t.contains("vote")
        }) {
            return MarketCategory::Politics;
        }

        if tags_lower.iter().any(|t| {
            t.contains("crypto")
                || t.contains("bitcoin")
                || t.contains("ethereum")
                || t.contains("btc")
                || t.contains("eth")
                || t.contains("defi")
        }) {
            return MarketCategory::Crypto;
        }

        if tags_lower.iter().any(|t| {
            t.contains("entertainment")
                || t.contains("movie")
                || t.contains("oscar")
                || t.contains("grammy")
                || t.contains("award")
                || t.contains("celebrity")
        }) {
            return MarketCategory::Entertainment;
        }

        if tags_lower
            .iter()
            .any(|t| t.contains("science") || t.contains("space") || t.contains("tech"))
        {
            return MarketCategory::Science;
        }

        if tags_lower.iter().any(|t| {
            t.contains("business") || t.contains("economy") || t.contains("stock")
        }) {
            return MarketCategory::Business;
        }

        MarketCategory::Other
    }
}

// =============================================================================
// DISABLED KEYWORDS (skip markets containing these)
// =============================================================================

/// Keywords that indicate markets to skip
pub const DISABLED_KEYWORDS: &[&str] = &[
    "weather",
    "temperature",
    "long-term",
    "2026",
    "2027",
    "2028",
    "2029",
    "2030",
    "local",
    "municipal",
];

/// Check if a market question should be skipped based on keywords
pub fn should_skip_market(question: &str) -> bool {
    let question_lower = question.to_lowercase();
    DISABLED_KEYWORDS
        .iter()
        .any(|kw| question_lower.contains(kw))
}

// =============================================================================
// PAPER TRADING
// =============================================================================

/// Simulated execution delay range (milliseconds)
pub const PAPER_EXEC_DELAY_MIN_MS: u64 = 100;
pub const PAPER_EXEC_DELAY_MAX_MS: u64 = 500;

/// Simulated fill rate range (percentage)
pub const PAPER_FILL_RATE_MIN: f64 = 0.70;
pub const PAPER_FILL_RATE_MAX: f64 = 1.00;

/// Simulated slippage range (cents)
pub const PAPER_SLIPPAGE_MIN_CENTS: i16 = 0;
pub const PAPER_SLIPPAGE_MAX_CENTS: i16 = 2;

// =============================================================================
// HELPER FUNCTIONS
// =============================================================================

/// Calculate maximum capital for a single arb in cents
pub fn max_capital_per_arb_cents() -> u32 {
    ((TOTAL_CAPITAL_CENTS as f64) * MAX_PER_ARB_PCT) as u32
}

/// Calculate reserve amount in cents
pub fn reserve_cents() -> u32 {
    ((TOTAL_CAPITAL_CENTS as f64) * RESERVE_PCT) as u32
}

/// Calculate available capital (total - reserve) in cents
pub fn available_capital_cents() -> u32 {
    TOTAL_CAPITAL_CENTS - reserve_cents()
}

/// Get minimum profit threshold based on outcome count
pub fn min_profit_for_outcomes(outcome_count: usize) -> u16 {
    match outcome_count {
        2 => MIN_PROFIT_BINARY_CENTS,
        3..=4 => MIN_PROFIT_MULTI_CENTS,
        _ => MIN_PROFIT_LARGE_MULTI_CENTS,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_capital_calculations() {
        assert_eq!(max_capital_per_arb_cents(), 5000); // $50
        assert_eq!(reserve_cents(), 3000); // $30
        assert_eq!(available_capital_cents(), 7000); // $70
    }

    #[test]
    fn test_profit_thresholds() {
        assert_eq!(min_profit_for_outcomes(2), 1);
        assert_eq!(min_profit_for_outcomes(3), 3);
        assert_eq!(min_profit_for_outcomes(4), 3);
        assert_eq!(min_profit_for_outcomes(5), 5);
        assert_eq!(min_profit_for_outcomes(8), 5);
    }

    #[test]
    fn test_should_skip_market() {
        assert!(should_skip_market("What will the weather be in NYC?"));
        assert!(should_skip_market("Who wins 2027 election?"));
        assert!(!should_skip_market("Who wins Super Bowl 2025?"));
        assert!(!should_skip_market("Will Bitcoin hit $100k?"));
    }

    #[test]
    fn test_category_enabled() {
        assert!(MarketCategory::Sports.is_enabled());
        assert!(MarketCategory::Politics.is_enabled());
        assert!(MarketCategory::Crypto.is_enabled());
        assert!(MarketCategory::Entertainment.is_enabled());
        assert!(!MarketCategory::Other.is_enabled());
    }
}
