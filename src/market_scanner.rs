//! Market scanner for Polymarket discovery via Gamma API.
//!
//! This module handles discovering and filtering markets from Polymarket,
//! tracking multi-outcome markets, and maintaining market metadata.

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

use crate::config::{
    MarketCategory, GAMMA_API_BASE, MAX_DAYS_TO_EXPIRY, MAX_OUTCOMES, MIN_OUTCOMES,
    MIN_VOLUME_USD, NEW_MARKET_HOURS, should_skip_market,
};

/// Outcome within a market
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Outcome {
    /// Outcome name/description (e.g., "Yes", "No", "Trump", "Biden")
    pub name: String,
    /// CLOB token ID for this outcome
    pub token_id: String,
    /// Current best ask price (0.0-1.0)
    pub ask_price: f64,
    /// Size available at best ask
    pub ask_size: f64,
    /// Current best bid price
    pub bid_price: f64,
}

/// A tracked Polymarket market
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrackedMarket {
    /// Polymarket condition ID (unique identifier)
    pub condition_id: String,
    /// Market question/title
    pub question: String,
    /// Market slug for URL
    pub slug: String,
    /// All outcomes in this market
    pub outcomes: Vec<Outcome>,
    /// Total trading volume in USD
    pub volume_usd: f64,
    /// Total liquidity in USD
    pub liquidity_usd: f64,
    /// When market was created
    pub created_at: Option<DateTime<Utc>>,
    /// When market expires/resolves
    pub end_date: Option<DateTime<Utc>>,
    /// Market category
    pub category: MarketCategory,
    /// Whether market is active
    pub active: bool,
    /// Whether market is closed
    pub closed: bool,
    /// Is this a new market (< 24h old)
    pub is_new: bool,
    /// Last time we updated this market
    pub last_updated: DateTime<Utc>,
    /// Tags from Polymarket
    pub tags: Vec<String>,
}

impl TrackedMarket {
    /// Check if this is a binary market (2 outcomes)
    pub fn is_binary(&self) -> bool {
        self.outcomes.len() == 2
    }

    /// Get the sum of all outcome ask prices
    pub fn sum_of_asks(&self) -> f64 {
        self.outcomes.iter().map(|o| o.ask_price).sum()
    }

    /// Get minimum liquidity across all outcomes
    pub fn min_liquidity(&self) -> f64 {
        self.outcomes
            .iter()
            .map(|o| o.ask_size)
            .fold(f64::MAX, f64::min)
    }

    /// Calculate potential profit in cents if arb exists
    pub fn potential_profit_cents(&self) -> i16 {
        let sum = self.sum_of_asks();
        if sum < 1.0 {
            ((1.0 - sum) * 100.0).round() as i16
        } else {
            0
        }
    }

    /// Check if market has sufficient liquidity
    pub fn has_sufficient_liquidity(&self, min_contracts: f64) -> bool {
        self.outcomes.iter().all(|o| o.ask_size >= min_contracts)
    }
}

/// Gamma API response for market list
#[derive(Debug, Deserialize)]
struct GammaMarketsResponse(Vec<GammaMarket>);

/// Single market from Gamma API
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct GammaMarket {
    #[serde(rename = "conditionId")]
    condition_id: Option<String>,
    question: Option<String>,
    slug: Option<String>,
    outcomes: Option<String>, // JSON array as string
    #[serde(rename = "outcomePrices")]
    outcome_prices: Option<String>, // JSON array as string
    #[serde(rename = "clobTokenIds")]
    clob_token_ids: Option<String>, // JSON array as string
    volume: Option<String>,
    liquidity: Option<String>,
    #[serde(rename = "startDate")]
    start_date: Option<String>,
    #[serde(rename = "endDate")]
    end_date: Option<String>,
    active: Option<bool>,
    closed: Option<bool>,
    #[serde(default)]
    tags: Option<Vec<String>>,
}

/// Market scanner that discovers and tracks Polymarket markets
pub struct MarketScanner {
    http: reqwest::Client,
    /// All tracked markets keyed by condition_id
    markets: Arc<RwLock<HashMap<String, TrackedMarket>>>,
}

impl MarketScanner {
    pub fn new() -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .expect("Failed to build HTTP client");

        Self {
            http,
            markets: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Scan Gamma API for markets matching our criteria
    pub async fn scan_markets(&self) -> Result<usize> {
        // Log current state BEFORE scan
        let pre_count = self.markets.read().await.len();
        info!("[SCANNER] Fetching markets from Gamma API... (pre-scan count: {})", pre_count);

        let url = format!(
            "{}/markets?active=true&closed=false&limit=1000",
            GAMMA_API_BASE
        );

        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .context("Failed to fetch markets")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("Gamma API error {}: {}", status, body);
        }

        let markets: Vec<GammaMarket> = resp.json().await.context("Failed to parse markets")?;

        info!("[SCANNER] Received {} markets from API", markets.len());

        let mut tracked = self.markets.write().await;
        let mut added = 0;
        let mut updated = 0;
        let mut skipped = 0;
        let mut skip_reasons: std::collections::HashMap<&str, u32> = std::collections::HashMap::new();

        for market in markets {
            match self.process_market_with_reason(&market) {
                Ok(Some(tracked_market)) => {
                    let condition_id = tracked_market.condition_id.clone();
                    if tracked.contains_key(&condition_id) {
                        updated += 1;
                    } else {
                        added += 1;
                    }
                    tracked.insert(condition_id, tracked_market);
                }
                Ok(None) => {
                    skipped += 1;
                }
                Err(reason) => {
                    *skip_reasons.entry(reason).or_insert(0) += 1;
                    skipped += 1;
                }
            }
        }

        // Log skip reasons if any
        if !skip_reasons.is_empty() {
            let reasons: Vec<String> = skip_reasons.iter()
                .map(|(k, v)| format!("{}={}", k, v))
                .collect();
            info!("[SCANNER] Skip reasons: {}", reasons.join(", "));
        }

        info!(
            "[SCANNER] Markets: {} added, {} updated, {} skipped, {} total tracked",
            added,
            updated,
            skipped,
            tracked.len()
        );

        Ok(tracked.len())
    }

    /// Process market and return skip reason if filtered
    fn process_market_with_reason(&self, market: &GammaMarket) -> Result<Option<TrackedMarket>, &'static str> {
        let condition_id = market.condition_id.as_ref().ok_or("no_condition_id")?;
        let question = market.question.as_ref().ok_or("no_question")?;
        let slug = market.slug.clone().unwrap_or_default();

        if should_skip_market(question) {
            return Err("keyword_filter");
        }

        let outcomes_str = market.outcomes.as_ref().ok_or("no_outcomes")?;
        let outcome_names: Vec<String> =
            serde_json::from_str(outcomes_str).map_err(|_| "outcomes_parse_fail")?;

        if outcome_names.is_empty() {
            return Err("empty_outcomes");
        }
        if outcome_names.len() < MIN_OUTCOMES {
            return Err("too_few_outcomes");
        }
        if outcome_names.len() > MAX_OUTCOMES {
            return Err("too_many_outcomes");
        }

        let token_ids_str = market.clob_token_ids.as_ref().map(|s| s.as_str()).unwrap_or("[]");
        let token_ids: Vec<String> =
            serde_json::from_str(token_ids_str).map_err(|_| "token_parse_fail")?;

        if token_ids.len() != outcome_names.len() {
            return Err("token_outcome_mismatch");
        }

        let prices_str = market.outcome_prices.as_ref().map(|s| s.as_str()).unwrap_or("[]");
        let prices: Vec<String> =
            serde_json::from_str(prices_str).unwrap_or_else(|_| Vec::new());

        let mut outcomes = Vec::with_capacity(outcome_names.len());
        for (i, name) in outcome_names.into_iter().enumerate() {
            let ask_price: f64 = prices.get(i).and_then(|p| p.parse().ok()).unwrap_or(0.0);
            outcomes.push(Outcome {
                name,
                token_id: token_ids.get(i).cloned().unwrap_or_default(),
                ask_price,
                ask_size: 0.0,
                bid_price: 0.0,
            });
        }

        let volume_usd: f64 = market.volume.as_ref().and_then(|v| v.parse().ok()).unwrap_or(0.0);
        if volume_usd < MIN_VOLUME_USD {
            return Err("low_volume");
        }

        let liquidity_usd: f64 = market.liquidity.as_ref().and_then(|v| v.parse().ok()).unwrap_or(0.0);

        let created_at = market.start_date.as_ref()
            .and_then(|d| DateTime::parse_from_rfc3339(d).ok())
            .map(|d| d.with_timezone(&Utc));

        let end_date = market.end_date.as_ref()
            .and_then(|d| DateTime::parse_from_rfc3339(d).ok())
            .map(|d| d.with_timezone(&Utc));

        if let Some(end) = end_date {
            let days_until_expiry = (end - Utc::now()).num_days();
            if days_until_expiry < 0 {
                return Err("expired");
            }
            if days_until_expiry > MAX_DAYS_TO_EXPIRY as i64 {
                return Err("too_far_expiry");
            }
        }

        let is_new = created_at
            .map(|c| {
                let hours_old = (Utc::now() - c).num_hours();
                hours_old >= 0 && hours_old < NEW_MARKET_HOURS as i64
            })
            .unwrap_or(false);

        let tags = market.tags.clone().unwrap_or_default();
        let category = MarketCategory::from_tags(&tags);

        if !category.is_enabled() {
            return Err("category_disabled");
        }

        Ok(Some(TrackedMarket {
            condition_id: condition_id.clone(),
            question: question.clone(),
            slug,
            outcomes,
            volume_usd,
            liquidity_usd,
            created_at,
            end_date,
            category,
            active: market.active.unwrap_or(false),
            closed: market.closed.unwrap_or(true),
            is_new,
            last_updated: Utc::now(),
            tags,
        }))
    }

    /// Process a single market from Gamma API
    fn process_market(&self, market: GammaMarket) -> Result<Option<TrackedMarket>> {
        let condition_id = market
            .condition_id
            .ok_or_else(|| anyhow::anyhow!("Missing condition_id"))?;
        let question = market
            .question
            .ok_or_else(|| anyhow::anyhow!("Missing question"))?;
        let slug = market.slug.unwrap_or_default();

        // Skip based on keywords
        if should_skip_market(&question) {
            return Ok(None);
        }

        // Parse outcomes
        let outcomes_str = market
            .outcomes
            .ok_or_else(|| anyhow::anyhow!("Missing outcomes"))?;
        let outcome_names: Vec<String> =
            serde_json::from_str(&outcomes_str).unwrap_or_else(|_| Vec::new());

        // Check outcome count
        if outcome_names.len() < MIN_OUTCOMES || outcome_names.len() > MAX_OUTCOMES {
            return Ok(None);
        }

        // Parse token IDs
        let token_ids_str = market.clob_token_ids.unwrap_or_else(|| "[]".to_string());
        let token_ids: Vec<String> =
            serde_json::from_str(&token_ids_str).unwrap_or_else(|_| Vec::new());

        if token_ids.len() != outcome_names.len() {
            return Ok(None);
        }

        // Parse prices
        let prices_str = market.outcome_prices.unwrap_or_else(|| "[]".to_string());
        let prices: Vec<String> =
            serde_json::from_str(&prices_str).unwrap_or_else(|_| Vec::new());

        // Build outcomes
        let mut outcomes = Vec::with_capacity(outcome_names.len());
        for (i, name) in outcome_names.into_iter().enumerate() {
            let ask_price: f64 = prices
                .get(i)
                .and_then(|p| p.parse().ok())
                .unwrap_or(0.0);

            outcomes.push(Outcome {
                name,
                token_id: token_ids.get(i).cloned().unwrap_or_default(),
                ask_price,
                ask_size: 0.0, // Will be updated from WebSocket
                bid_price: 0.0,
            });
        }

        // Parse volume
        let volume_usd: f64 = market
            .volume
            .and_then(|v| v.parse().ok())
            .unwrap_or(0.0);

        // Filter by volume
        if volume_usd < MIN_VOLUME_USD {
            return Ok(None);
        }

        // Parse liquidity
        let liquidity_usd: f64 = market
            .liquidity
            .and_then(|v| v.parse().ok())
            .unwrap_or(0.0);

        // Parse dates
        let created_at = market
            .start_date
            .and_then(|d| DateTime::parse_from_rfc3339(&d).ok())
            .map(|d| d.with_timezone(&Utc));

        let end_date = market
            .end_date
            .and_then(|d| DateTime::parse_from_rfc3339(&d).ok())
            .map(|d| d.with_timezone(&Utc));

        // Check expiry
        if let Some(end) = end_date {
            let days_until_expiry = (end - Utc::now()).num_days();
            if days_until_expiry < 0 || days_until_expiry > MAX_DAYS_TO_EXPIRY as i64 {
                return Ok(None);
            }
        }

        // Check if new market
        let is_new = created_at
            .map(|c| {
                let hours_old = (Utc::now() - c).num_hours();
                hours_old >= 0 && hours_old < NEW_MARKET_HOURS as i64
            })
            .unwrap_or(false);

        // Determine category
        let tags = market.tags.unwrap_or_default();
        let category = MarketCategory::from_tags(&tags);

        // Skip disabled categories
        if !category.is_enabled() {
            return Ok(None);
        }

        Ok(Some(TrackedMarket {
            condition_id,
            question,
            slug,
            outcomes,
            volume_usd,
            liquidity_usd,
            created_at,
            end_date,
            category,
            active: market.active.unwrap_or(false),
            closed: market.closed.unwrap_or(true),
            is_new,
            last_updated: Utc::now(),
            tags,
        }))
    }

    /// Get all tracked markets
    pub async fn get_markets(&self) -> Vec<TrackedMarket> {
        self.markets.read().await.values().cloned().collect()
    }

    /// Get market by condition ID
    pub async fn get_market(&self, condition_id: &str) -> Option<TrackedMarket> {
        self.markets.read().await.get(condition_id).cloned()
    }

    /// Get all token IDs for WebSocket subscription
    pub async fn get_all_token_ids(&self) -> Vec<String> {
        let markets = self.markets.read().await;
        markets
            .values()
            .flat_map(|m| m.outcomes.iter().map(|o| o.token_id.clone()))
            .collect()
    }

    /// Get markets with potential arbitrage opportunities
    pub async fn get_arb_candidates(&self, min_profit_cents: i16) -> Vec<TrackedMarket> {
        let markets = self.markets.read().await;
        markets
            .values()
            .filter(|m| m.potential_profit_cents() >= min_profit_cents)
            .cloned()
            .collect()
    }

    /// Get new markets (< 24h old)
    pub async fn get_new_markets(&self) -> Vec<TrackedMarket> {
        let markets = self.markets.read().await;
        markets.values().filter(|m| m.is_new).cloned().collect()
    }

    /// Update outcome price from WebSocket
    pub async fn update_outcome_price(
        &self,
        token_id: &str,
        ask_price: f64,
        ask_size: f64,
        bid_price: f64,
    ) {
        let mut markets = self.markets.write().await;

        // Find and update the outcome
        for market in markets.values_mut() {
            for outcome in &mut market.outcomes {
                if outcome.token_id == token_id {
                    outcome.ask_price = ask_price;
                    outcome.ask_size = ask_size;
                    outcome.bid_price = bid_price;
                    market.last_updated = Utc::now();
                    return;
                }
            }
        }
    }

    /// Get count of tracked markets
    pub async fn market_count(&self) -> usize {
        self.markets.read().await.len()
    }

    /// Get count of binary markets
    pub async fn binary_market_count(&self) -> usize {
        self.markets
            .read()
            .await
            .values()
            .filter(|m| m.is_binary())
            .count()
    }

    /// Get count of multi-outcome markets
    pub async fn multi_outcome_count(&self) -> usize {
        self.markets
            .read()
            .await
            .values()
            .filter(|m| !m.is_binary())
            .count()
    }

    /// Get market statistics
    pub async fn get_stats(&self) -> ScannerStats {
        let markets = self.markets.read().await;

        let mut stats = ScannerStats::default();
        stats.total_markets = markets.len();

        for market in markets.values() {
            if market.is_binary() {
                stats.binary_markets += 1;
            } else {
                stats.multi_outcome_markets += 1;
            }

            if market.is_new {
                stats.new_markets += 1;
            }

            match market.category {
                MarketCategory::Sports => stats.sports_markets += 1,
                MarketCategory::Politics => stats.politics_markets += 1,
                MarketCategory::Crypto => stats.crypto_markets += 1,
                MarketCategory::Entertainment => stats.entertainment_markets += 1,
                _ => stats.other_markets += 1,
            }

            if market.potential_profit_cents() > 0 {
                stats.arb_candidates += 1;
            }
        }

        stats
    }
}

impl Default for MarketScanner {
    fn default() -> Self {
        Self::new()
    }
}

/// Scanner statistics
#[derive(Debug, Clone, Default)]
pub struct ScannerStats {
    pub total_markets: usize,
    pub binary_markets: usize,
    pub multi_outcome_markets: usize,
    pub new_markets: usize,
    pub sports_markets: usize,
    pub politics_markets: usize,
    pub crypto_markets: usize,
    pub entertainment_markets: usize,
    pub other_markets: usize,
    pub arb_candidates: usize,
}

impl std::fmt::Display for ScannerStats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Markets: {} total ({} binary, {} multi) | New: {} | Arb candidates: {} | By category: Sports={}, Politics={}, Crypto={}, Entertainment={}",
            self.total_markets,
            self.binary_markets,
            self.multi_outcome_markets,
            self.new_markets,
            self.arb_candidates,
            self.sports_markets,
            self.politics_markets,
            self.crypto_markets,
            self.entertainment_markets
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tracked_market_profit() {
        let market = TrackedMarket {
            condition_id: "test".to_string(),
            question: "Test?".to_string(),
            slug: "test".to_string(),
            outcomes: vec![
                Outcome {
                    name: "Yes".to_string(),
                    token_id: "1".to_string(),
                    ask_price: 0.45,
                    ask_size: 100.0,
                    bid_price: 0.44,
                },
                Outcome {
                    name: "No".to_string(),
                    token_id: "2".to_string(),
                    ask_price: 0.52,
                    ask_size: 100.0,
                    bid_price: 0.51,
                },
            ],
            volume_usd: 100000.0,
            liquidity_usd: 10000.0,
            created_at: None,
            end_date: None,
            category: MarketCategory::Sports,
            active: true,
            closed: false,
            is_new: false,
            last_updated: Utc::now(),
            tags: vec![],
        };

        // 0.45 + 0.52 = 0.97 < 1.0
        // Profit = (1.0 - 0.97) * 100 = 3 cents
        assert_eq!(market.sum_of_asks(), 0.97);
        assert_eq!(market.potential_profit_cents(), 3);
        assert!(market.is_binary());
    }

    #[test]
    fn test_multi_outcome_profit() {
        let market = TrackedMarket {
            condition_id: "test".to_string(),
            question: "Who wins?".to_string(),
            slug: "test".to_string(),
            outcomes: vec![
                Outcome {
                    name: "A".to_string(),
                    token_id: "1".to_string(),
                    ask_price: 0.30,
                    ask_size: 50.0,
                    bid_price: 0.29,
                },
                Outcome {
                    name: "B".to_string(),
                    token_id: "2".to_string(),
                    ask_price: 0.25,
                    ask_size: 50.0,
                    bid_price: 0.24,
                },
                Outcome {
                    name: "C".to_string(),
                    token_id: "3".to_string(),
                    ask_price: 0.20,
                    ask_size: 30.0,
                    bid_price: 0.19,
                },
                Outcome {
                    name: "D".to_string(),
                    token_id: "4".to_string(),
                    ask_price: 0.20,
                    ask_size: 40.0,
                    bid_price: 0.19,
                },
            ],
            volume_usd: 100000.0,
            liquidity_usd: 10000.0,
            created_at: None,
            end_date: None,
            category: MarketCategory::Politics,
            active: true,
            closed: false,
            is_new: false,
            last_updated: Utc::now(),
            tags: vec![],
        };

        // 0.30 + 0.25 + 0.20 + 0.20 = 0.95
        // Profit = 5 cents
        assert_eq!(market.sum_of_asks(), 0.95);
        assert_eq!(market.potential_profit_cents(), 5);
        assert!(!market.is_binary());
        assert_eq!(market.min_liquidity(), 30.0);
    }
}
