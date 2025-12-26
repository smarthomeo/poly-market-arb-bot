//! Core type definitions and data structures for the Polymarket arbitrage bot.
//!
//! This module provides foundational types for market state management,
//! orderbook representation, and arbitrage opportunity detection.

use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Instant;
use rustc_hash::FxHashMap;

// =============================================================================
// PRICE AND SIZE TYPES
// =============================================================================

/// Price representation in cents (1-99 for $0.01-$0.99), 0 indicates no price
pub type PriceCents = u16;

/// Size representation in cents (dollar amount * 100)
pub type SizeCents = u16;

/// Sentinel value indicating no price available
pub const NO_PRICE: PriceCents = 0;

/// Maximum number of outcomes per market
pub const MAX_OUTCOMES: usize = 8;

/// Maximum number of concurrently tracked markets
pub const MAX_MARKETS: usize = 2048;

static MONO_START: OnceLock<Instant> = OnceLock::new();

// =============================================================================
// ATOMIC ORDERBOOK (per outcome)
// =============================================================================

/// Pack orderbook state into a single u64 for atomic operations.
/// Bit layout: [ask_price:16][bid_price:16][ask_size:16][bid_size:16]
#[inline(always)]
pub fn pack_orderbook(ask_price: PriceCents, bid_price: PriceCents, ask_size: SizeCents, bid_size: SizeCents) -> u64 {
    ((ask_price as u64) << 48) | ((bid_price as u64) << 32) | ((ask_size as u64) << 16) | (bid_size as u64)
}

/// Unpack a u64 orderbook representation
#[inline(always)]
pub fn unpack_orderbook(packed: u64) -> (PriceCents, PriceCents, SizeCents, SizeCents) {
    let ask_price = ((packed >> 48) & 0xFFFF) as PriceCents;
    let bid_price = ((packed >> 32) & 0xFFFF) as PriceCents;
    let ask_size = ((packed >> 16) & 0xFFFF) as SizeCents;
    let bid_size = (packed & 0xFFFF) as SizeCents;
    (ask_price, bid_price, ask_size, bid_size)
}

/// Lock-free orderbook state for a single outcome.
#[repr(align(64))]
pub struct AtomicOutcomeBook {
    /// Packed state: [ask:16][bid:16][ask_size:16][bid_size:16]
    packed: AtomicU64,
}

impl AtomicOutcomeBook {
    pub const fn new() -> Self {
        Self { packed: AtomicU64::new(0) }
    }

    /// Load current state (ask_price, bid_price, ask_size, bid_size)
    #[inline(always)]
    pub fn load(&self) -> (PriceCents, PriceCents, SizeCents, SizeCents) {
        unpack_orderbook(self.packed.load(Ordering::Acquire))
    }

    /// Get ask price only
    #[inline(always)]
    pub fn ask_price(&self) -> PriceCents {
        let packed = self.packed.load(Ordering::Acquire);
        ((packed >> 48) & 0xFFFF) as PriceCents
    }

    /// Get ask size only
    #[inline(always)]
    pub fn ask_size(&self) -> SizeCents {
        let packed = self.packed.load(Ordering::Acquire);
        ((packed >> 16) & 0xFFFF) as SizeCents
    }

    /// Store new state
    #[inline(always)]
    pub fn store(&self, ask_price: PriceCents, bid_price: PriceCents, ask_size: SizeCents, bid_size: SizeCents) {
        self.packed.store(pack_orderbook(ask_price, bid_price, ask_size, bid_size), Ordering::Release);
    }

    /// Update ask side only
    #[inline(always)]
    pub fn update_ask(&self, ask_price: PriceCents, ask_size: SizeCents) {
        let mut current = self.packed.load(Ordering::Acquire);
        loop {
            let (_, bid_price, _, bid_size) = unpack_orderbook(current);
            let new = pack_orderbook(ask_price, bid_price, ask_size, bid_size);
            match self.packed.compare_exchange_weak(current, new, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => break,
                Err(c) => current = c,
            }
        }
    }

    /// Update bid side only
    #[inline(always)]
    pub fn update_bid(&self, bid_price: PriceCents, bid_size: SizeCents) {
        let mut current = self.packed.load(Ordering::Acquire);
        loop {
            let (ask_price, _, ask_size, _) = unpack_orderbook(current);
            let new = pack_orderbook(ask_price, bid_price, ask_size, bid_size);
            match self.packed.compare_exchange_weak(current, new, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => break,
                Err(c) => current = c,
            }
        }
    }
}

impl Default for AtomicOutcomeBook {
    fn default() -> Self {
        Self::new()
    }
}

// =============================================================================
// MULTI-OUTCOME MARKET STATE
// =============================================================================

/// Outcome metadata
#[derive(Debug, Clone)]
pub struct OutcomeMeta {
    /// Outcome name (e.g., "Yes", "Trump", "Chiefs")
    pub name: Arc<str>,
    /// CLOB token ID
    pub token_id: Arc<str>,
}

/// Market state for a multi-outcome Polymarket market
pub struct MultiOutcomeState {
    /// Unique market identifier
    pub market_id: u16,
    /// Market condition ID
    pub condition_id: Arc<str>,
    /// Market question
    pub question: Arc<str>,
    /// Outcome metadata (names and token IDs)
    pub outcomes: Vec<OutcomeMeta>,
    /// Orderbooks for each outcome (index matches outcomes)
    pub books: Vec<AtomicOutcomeBook>,
    /// Number of outcomes
    pub outcome_count: usize,
}

impl MultiOutcomeState {
    pub fn new(market_id: u16, condition_id: &str, question: &str, outcomes: Vec<(String, String)>) -> Self {
        let outcome_count = outcomes.len();
        let books: Vec<AtomicOutcomeBook> = (0..outcome_count).map(|_| AtomicOutcomeBook::new()).collect();
        let outcome_metas: Vec<OutcomeMeta> = outcomes
            .into_iter()
            .map(|(name, token_id)| OutcomeMeta {
                name: name.into(),
                token_id: token_id.into(),
            })
            .collect();

        Self {
            market_id,
            condition_id: condition_id.into(),
            question: question.into(),
            outcomes: outcome_metas,
            books,
            outcome_count,
        }
    }

    /// Get sum of all ask prices in cents
    #[inline]
    pub fn sum_of_asks(&self) -> u32 {
        self.books.iter().map(|b| b.ask_price() as u32).sum()
    }

    /// Get minimum liquidity across all outcomes
    #[inline]
    pub fn min_liquidity(&self) -> SizeCents {
        self.books
            .iter()
            .map(|b| b.ask_size())
            .min()
            .unwrap_or(0)
    }

    /// Check if arb exists (sum < 100 cents)
    #[inline]
    pub fn has_arb(&self) -> bool {
        self.sum_of_asks() < 100
    }

    /// Calculate profit in cents (100 - sum)
    #[inline]
    pub fn profit_cents(&self) -> i16 {
        100 - self.sum_of_asks() as i16
    }

    /// Check if all outcomes have prices
    #[inline]
    pub fn all_priced(&self) -> bool {
        self.books.iter().all(|b| b.ask_price() > 0)
    }

    /// Check if market has sufficient liquidity
    #[inline]
    pub fn has_sufficient_liquidity(&self, min_contracts: SizeCents) -> bool {
        self.books.iter().all(|b| b.ask_size() >= min_contracts)
    }

    /// Get all ask prices
    pub fn ask_prices(&self) -> Vec<PriceCents> {
        self.books.iter().map(|b| b.ask_price()).collect()
    }

    /// Get all ask sizes
    pub fn ask_sizes(&self) -> Vec<SizeCents> {
        self.books.iter().map(|b| b.ask_size()).collect()
    }

    /// Is this a binary market?
    pub fn is_binary(&self) -> bool {
        self.outcome_count == 2
    }
}

// =============================================================================
// ARB TYPES AND EXECUTION REQUESTS
// =============================================================================

/// Arbitrage opportunity type
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArbType {
    /// Binary market: YES + NO < $1.00
    Binary,
    /// Multi-outcome: sum of all outcomes < $1.00
    MultiOutcome,
}

/// Execution request for an arbitrage opportunity
#[derive(Debug, Clone)]
pub struct ExecutionRequest {
    /// Market identifier
    pub market_id: u16,
    /// Condition ID for lookup
    pub condition_id: Arc<str>,
    /// Type of arbitrage
    pub arb_type: ArbType,
    /// Ask prices for each outcome (in cents)
    pub ask_prices: Vec<PriceCents>,
    /// Available sizes for each outcome (in cents)
    pub ask_sizes: Vec<SizeCents>,
    /// Token IDs for each outcome
    pub token_ids: Vec<Arc<str>>,
    /// Expected profit in cents
    pub expected_profit_cents: i16,
    /// Detection timestamp in nanoseconds
    pub detected_ns: u64,
}

impl ExecutionRequest {
    /// Calculate maximum contracts we can buy (limited by smallest size)
    pub fn max_contracts(&self) -> u16 {
        self.ask_sizes.iter().copied().min().unwrap_or(0) / 100
    }

    /// Calculate total cost in cents for N contracts
    pub fn total_cost_cents(&self, contracts: u16) -> u32 {
        self.ask_prices.iter().map(|p| *p as u32).sum::<u32>() * contracts as u32
    }
}

// =============================================================================
// GLOBAL STATE
// =============================================================================

/// Global market state manager
pub struct GlobalState {
    /// All tracked markets
    pub markets: Vec<MultiOutcomeState>,
    /// Next available market ID
    next_market_id: u16,
    /// Token ID to market ID lookup
    pub token_to_market: FxHashMap<u64, (u16, usize)>, // hash -> (market_id, outcome_index)
    /// Condition ID to market ID lookup
    pub condition_to_market: FxHashMap<u64, u16>,
}

impl GlobalState {
    pub fn new() -> Self {
        Self {
            markets: Vec::with_capacity(MAX_MARKETS),
            next_market_id: 0,
            token_to_market: FxHashMap::default(),
            condition_to_market: FxHashMap::default(),
        }
    }

    /// Add a new market
    pub fn add_market(
        &mut self,
        condition_id: &str,
        question: &str,
        outcomes: Vec<(String, String)>, // (name, token_id)
    ) -> Option<u16> {
        if self.next_market_id as usize >= MAX_MARKETS {
            return None;
        }

        let market_id = self.next_market_id;
        self.next_market_id += 1;

        // Build outcome list and register token lookups
        for (idx, (_, token_id)) in outcomes.iter().enumerate() {
            let token_hash = fxhash_str(token_id);
            self.token_to_market.insert(token_hash, (market_id, idx));
        }

        // Register condition lookup
        let condition_hash = fxhash_str(condition_id);
        self.condition_to_market.insert(condition_hash, market_id);

        // Create market state
        let market = MultiOutcomeState::new(market_id, condition_id, question, outcomes);
        self.markets.push(market);

        Some(market_id)
    }

    /// Get market by ID
    #[inline]
    pub fn get_market(&self, market_id: u16) -> Option<&MultiOutcomeState> {
        self.markets.get(market_id as usize)
    }

    /// Get market by condition ID
    pub fn get_by_condition(&self, condition_id: &str) -> Option<&MultiOutcomeState> {
        let hash = fxhash_str(condition_id);
        let market_id = *self.condition_to_market.get(&hash)?;
        self.get_market(market_id)
    }

    /// Check if a condition is already tracked
    pub fn has_condition(&self, condition_id: &str) -> bool {
        let hash = fxhash_str(condition_id);
        self.condition_to_market.contains_key(&hash)
    }

    /// Find market and outcome index by token ID
    #[inline]
    pub fn find_by_token(&self, token_id: &str) -> Option<(u16, usize)> {
        let hash = fxhash_str(token_id);
        self.token_to_market.get(&hash).copied()
    }

    /// Update outcome price by token ID
    pub fn update_price(&self, token_id: &str, ask_price: PriceCents, ask_size: SizeCents) -> Option<u16> {
        let (market_id, outcome_idx) = self.find_by_token(token_id)?;
        let market = self.markets.get(market_id as usize)?;
        let book = market.books.get(outcome_idx)?;
        book.update_ask(ask_price, ask_size);
        Some(market_id)
    }

    /// Get all markets with potential arbs
    pub fn get_arb_candidates(&self, min_profit_cents: i16) -> Vec<&MultiOutcomeState> {
        self.markets
            .iter()
            .filter(|m| m.all_priced() && m.profit_cents() >= min_profit_cents)
            .collect()
    }

    /// Market count
    pub fn market_count(&self) -> usize {
        self.markets.len()
    }

    /// Binary market count
    pub fn binary_count(&self) -> usize {
        self.markets.iter().filter(|m| m.is_binary()).count()
    }

    /// Multi-outcome count
    pub fn multi_count(&self) -> usize {
        self.markets.iter().filter(|m| !m.is_binary()).count()
    }
}

impl Default for GlobalState {
    fn default() -> Self {
        Self::new()
    }
}

// =============================================================================
// UTILITY FUNCTIONS
// =============================================================================

/// Fast string hashing using FxHash
#[inline(always)]
pub fn fxhash_str(s: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = rustc_hash::FxHasher::default();
    s.hash(&mut hasher);
    hasher.finish()
}

/// Monotonic timestamp in nanoseconds since process start
#[inline(always)]
pub fn monotonic_now_ns() -> u64 {
    MONO_START
        .get_or_init(Instant::now)
        .elapsed()
        .as_nanos() as u64
}

/// Convert f64 price (0.01-0.99) to PriceCents (1-99)
#[inline(always)]
pub fn price_to_cents(price: f64) -> PriceCents {
    ((price * 100.0).round() as PriceCents).clamp(0, 99)
}

/// Convert PriceCents to f64
#[inline(always)]
pub fn cents_to_price(cents: PriceCents) -> f64 {
    cents as f64 / 100.0
}

/// Parse price from string "0.XX" format
#[inline(always)]
pub fn parse_price(s: &str) -> PriceCents {
    let bytes = s.as_bytes();
    if bytes.len() == 4 && bytes[0] == b'0' && bytes[1] == b'.' {
        let d1 = bytes[2].wrapping_sub(b'0');
        let d2 = bytes[3].wrapping_sub(b'0');
        if d1 < 10 && d2 < 10 {
            return (d1 as u16 * 10 + d2 as u16) as PriceCents;
        }
    }
    if bytes.len() == 3 && bytes[0] == b'0' && bytes[1] == b'.' {
        let d = bytes[2].wrapping_sub(b'0');
        if d < 10 {
            return (d as u16 * 10) as PriceCents;
        }
    }
    s.parse::<f64>().map(price_to_cents).unwrap_or(0)
}

/// Parse size from string (dollars to cents)
#[inline(always)]
pub fn parse_size(s: &str) -> SizeCents {
    s.parse::<f64>()
        .map(|size| (size * 100.0).round() as SizeCents)
        .unwrap_or(0)
}

// =============================================================================
// TESTS
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pack_unpack() {
        let test_cases = vec![
            (50, 48, 1000, 900),
            (1, 1, 100, 100),
            (99, 97, 65535, 65535),
            (0, 0, 0, 0),
        ];

        for (ask, bid, ask_sz, bid_sz) in test_cases {
            let packed = pack_orderbook(ask, bid, ask_sz, bid_sz);
            let (a, b, as_, bs) = unpack_orderbook(packed);
            assert_eq!((a, b, as_, bs), (ask, bid, ask_sz, bid_sz));
        }
    }

    #[test]
    fn test_atomic_outcome_book() {
        let book = AtomicOutcomeBook::new();
        book.store(45, 43, 500, 400);

        let (ask, bid, ask_sz, bid_sz) = book.load();
        assert_eq!((ask, bid, ask_sz, bid_sz), (45, 43, 500, 400));

        book.update_ask(47, 600);
        let (ask, bid, ask_sz, bid_sz) = book.load();
        assert_eq!((ask, bid, ask_sz, bid_sz), (47, 43, 600, 400));
    }

    #[test]
    fn test_multi_outcome_state() {
        let outcomes = vec![
            ("Yes".to_string(), "token1".to_string()),
            ("No".to_string(), "token2".to_string()),
        ];

        let market = MultiOutcomeState::new(0, "cond1", "Will it rain?", outcomes);

        assert!(market.is_binary());
        assert_eq!(market.outcome_count, 2);

        // Set prices: YES=45, NO=52 = 97 total
        market.books[0].update_ask(45, 100);
        market.books[1].update_ask(52, 100);

        assert_eq!(market.sum_of_asks(), 97);
        assert_eq!(market.profit_cents(), 3);
        assert!(market.has_arb());
        assert!(market.all_priced());
    }

    #[test]
    fn test_multi_outcome_no_arb() {
        let outcomes = vec![
            ("A".to_string(), "t1".to_string()),
            ("B".to_string(), "t2".to_string()),
            ("C".to_string(), "t3".to_string()),
        ];

        let market = MultiOutcomeState::new(0, "cond", "Who wins?", outcomes);

        // Set prices: 35 + 35 + 35 = 105 (no arb)
        market.books[0].update_ask(35, 100);
        market.books[1].update_ask(35, 100);
        market.books[2].update_ask(35, 100);

        assert_eq!(market.sum_of_asks(), 105);
        assert!(!market.has_arb());
        assert_eq!(market.profit_cents(), -5);
    }

    #[test]
    fn test_global_state() {
        let mut state = GlobalState::new();

        let outcomes = vec![
            ("Yes".to_string(), "token_yes".to_string()),
            ("No".to_string(), "token_no".to_string()),
        ];

        let id = state.add_market("condition_1", "Test market?", outcomes);
        assert_eq!(id, Some(0));
        assert_eq!(state.market_count(), 1);

        // Update via token
        state.update_price("token_yes", 48, 500);
        state.update_price("token_no", 50, 600);

        let market = state.get_market(0).unwrap();
        assert_eq!(market.sum_of_asks(), 98);
        assert!(market.has_arb());
    }

    #[test]
    fn test_parse_price() {
        assert_eq!(parse_price("0.50"), 50);
        assert_eq!(parse_price("0.01"), 1);
        assert_eq!(parse_price("0.99"), 99);
        assert_eq!(parse_price("0.5"), 50);
        assert_eq!(parse_price("0.505"), 51);
    }

    #[test]
    fn test_price_conversions() {
        assert_eq!(price_to_cents(0.50), 50);
        assert_eq!(price_to_cents(0.01), 1);
        assert_eq!(price_to_cents(0.99), 99);

        assert!((cents_to_price(50) - 0.50).abs() < 0.001);
        assert!((cents_to_price(1) - 0.01).abs() < 0.001);
    }
}
