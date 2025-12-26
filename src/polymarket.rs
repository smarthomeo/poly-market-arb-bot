//! Polymarket WebSocket client for real-time price feeds.
//!
//! This module handles WebSocket connections to Polymarket for
//! real-time orderbook updates across all tracked outcomes.

use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, RwLock};
use tokio::time::{interval, Instant};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{debug, error, info, warn};

use crate::config::{POLYMARKET_WS_URL, POLY_PING_INTERVAL_SECS, GAMMA_API_BASE, MARKET_SCAN_INTERVAL_SECS};
use crate::types::{
    GlobalState, ExecutionRequest, ArbType, PriceCents, SizeCents,
    parse_price, parse_size, monotonic_now_ns,
};

// =============================================================================
// WEBSOCKET MESSAGE TYPES
// =============================================================================

#[derive(Deserialize, Debug)]
pub struct BookSnapshot {
    pub asset_id: String,
    #[allow(dead_code)]
    pub bids: Vec<PriceLevel>,
    pub asks: Vec<PriceLevel>,
}

#[derive(Deserialize, Debug)]
pub struct PriceLevel {
    pub price: String,
    pub size: String,
}

#[derive(Deserialize, Debug)]
pub struct PriceChangeEvent {
    pub event_type: Option<String>,
    #[serde(default)]
    pub price_changes: Option<Vec<PriceChangeItem>>,
}

#[derive(Deserialize, Debug)]
pub struct PriceChangeItem {
    pub asset_id: String,
    pub price: Option<String>,
    pub side: Option<String>,
}

#[derive(Serialize)]
struct SubscribeCmd {
    assets_ids: Vec<String>,
    #[serde(rename = "type")]
    sub_type: &'static str,
}

// =============================================================================
// GAMMA API CLIENT (for market discovery)
// =============================================================================

pub struct GammaClient {
    http: reqwest::Client,
}

impl GammaClient {
    pub fn new() -> Self {
        Self {
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .expect("Failed to build HTTP client"),
        }
    }

    /// Fetch all active markets from Gamma API
    pub async fn fetch_markets(&self) -> Result<Vec<GammaMarket>> {
        let url = format!("{}/markets?active=true&closed=false&limit=1000", GAMMA_API_BASE);

        let resp = self.http.get(&url).send().await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("Gamma API error {}: {}", status, body);
        }

        let markets: Vec<GammaMarket> = resp.json().await?;
        Ok(markets)
    }

    /// Fetch a specific market by condition ID
    pub async fn fetch_market(&self, condition_id: &str) -> Result<Option<GammaMarket>> {
        let url = format!("{}/markets?condition_id={}", GAMMA_API_BASE, condition_id);

        let resp = self.http.get(&url).send().await?;

        if !resp.status().is_success() {
            return Ok(None);
        }

        let markets: Vec<GammaMarket> = resp.json().await?;
        Ok(markets.into_iter().next())
    }
}

impl Default for GammaClient {
    fn default() -> Self {
        Self::new()
    }
}

/// Market data from Gamma API
#[derive(Debug, Deserialize, Clone)]
#[allow(dead_code)]
pub struct GammaMarket {
    #[serde(rename = "conditionId")]
    pub condition_id: Option<String>,
    pub question: Option<String>,
    pub slug: Option<String>,
    pub outcomes: Option<String>,
    #[serde(rename = "outcomePrices")]
    pub outcome_prices: Option<String>,
    #[serde(rename = "clobTokenIds")]
    pub clob_token_ids: Option<String>,
    pub volume: Option<String>,
    pub liquidity: Option<String>,
    #[serde(rename = "startDate")]
    pub start_date: Option<String>,
    #[serde(rename = "endDate")]
    pub end_date: Option<String>,
    pub active: Option<bool>,
    pub closed: Option<bool>,
    #[serde(default)]
    pub tags: Option<Vec<String>>,
}

impl GammaMarket {
    /// Parse outcome names
    pub fn parse_outcomes(&self) -> Vec<String> {
        self.outcomes
            .as_ref()
            .and_then(|s| serde_json::from_str(s).ok())
            .unwrap_or_default()
    }

    /// Parse token IDs
    pub fn parse_token_ids(&self) -> Vec<String> {
        self.clob_token_ids
            .as_ref()
            .and_then(|s| serde_json::from_str(s).ok())
            .unwrap_or_default()
    }

    /// Get outcome count
    pub fn outcome_count(&self) -> usize {
        self.parse_outcomes().len()
    }
}

// =============================================================================
// WEBSOCKET RUNNER
// =============================================================================

/// Run WebSocket connection and price feed
pub async fn run_ws(
    state: Arc<RwLock<GlobalState>>,
    exec_tx: mpsc::Sender<ExecutionRequest>,
    min_profit_cents: i16,
) -> Result<()> {
    // Collect all token IDs from tracked markets
    let tokens: Vec<String> = {
        let guard = state.read().await;
        guard.markets.iter()
            .flat_map(|m| m.outcomes.iter().map(|o| o.token_id.to_string()))
            .collect()
    };

    if tokens.is_empty() {
        info!("[POLY] No tokens to monitor");
        tokio::time::sleep(Duration::from_secs(u64::MAX)).await;
        return Ok(());
    }

    info!("[POLY] Connecting to WebSocket...");
    let (ws_stream, _) = connect_async(POLYMARKET_WS_URL)
        .await
        .context("Failed to connect to Polymarket")?;

    info!("[POLY] Connected");

    let (mut write, mut read) = ws_stream.split();

    // Subscribe to all tokens
    let subscribe_msg = SubscribeCmd {
        assets_ids: tokens.clone(),
        sub_type: "market",
    };

    write.send(Message::Text(serde_json::to_string(&subscribe_msg)?)).await?;
    let market_count = {
        let guard = state.read().await;
        guard.market_count()
    };
    info!("[POLY] Subscribed to {} tokens across {} markets", tokens.len(), market_count);

    let mut ping_interval = interval(Duration::from_secs(POLY_PING_INTERVAL_SECS));
    let mut last_message = Instant::now();
    let start_time = Instant::now();

    loop {
        tokio::select! {
            _ = ping_interval.tick() => {
                if let Err(e) = write.send(Message::Ping(vec![])).await {
                    error!("[POLY] Ping failed: {}", e);
                    break;
                }
            }

            msg = read.next() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        last_message = Instant::now();

                        // Try book snapshot first
                        if let Ok(books) = serde_json::from_str::<Vec<BookSnapshot>>(&text) {
                            for book in &books {
                                process_book(&state, book, &exec_tx, min_profit_cents).await;
                            }
                        }
                        // Try price change event
                        else if let Ok(event) = serde_json::from_str::<PriceChangeEvent>(&text) {
                            if event.event_type.as_deref() == Some("price_change") {
                                if let Some(changes) = &event.price_changes {
                                    for change in changes {
                                        process_price_change(&state, change, &exec_tx, min_profit_cents).await;
                                    }
                                }
                            }
                        }
                    }
                    Some(Ok(Message::Ping(data))) => {
                        let _ = write.send(Message::Pong(data)).await;
                        last_message = Instant::now();
                    }
                    Some(Ok(Message::Pong(_))) => {
                        last_message = Instant::now();
                    }
                    Some(Ok(Message::Close(frame))) => {
                        warn!("[POLY] Server closed: {:?}", frame);
                        break;
                    }
                    Some(Err(e)) => {
                        error!("[POLY] WebSocket error: {}", e);
                        break;
                    }
                    None => {
                        warn!("[POLY] Stream ended");
                        break;
                    }
                    _ => {}
                }
            }
        }

        // Periodically reconnect to pick up newly scanned markets
        if start_time.elapsed() > Duration::from_secs(MARKET_SCAN_INTERVAL_SECS.saturating_mul(2) as u64) {
            warn!("[POLY] Refreshing subscription to include new markets");
            break;
        }

        // Reconnect if stale
        if last_message.elapsed() > Duration::from_secs(120) {
            warn!("[POLY] Stale connection, reconnecting...");
            break;
        }
    }

    Ok(())
}

/// Process orderbook snapshot
async fn process_book(
    state: &Arc<RwLock<GlobalState>>,
    book: &BookSnapshot,
    exec_tx: &mpsc::Sender<ExecutionRequest>,
    min_profit_cents: i16,
) {
    // Find best ask
    let (best_ask, ask_size) = book.asks.iter()
        .filter_map(|l| {
            let price = parse_price(&l.price);
            let size = parse_size(&l.size);
            if price > 0 { Some((price, size)) } else { None }
        })
        .min_by_key(|(p, _)| *p)
        .unwrap_or((0, 0));

    // Update state
    let guard = state.read().await;
    if let Some(market_id) = guard.update_price(&book.asset_id, best_ask, ask_size) {
        if let Some(market) = guard.get_market(market_id) {
            if market.all_priced() && market.profit_cents() >= min_profit_cents {
                send_execution_request(market, exec_tx).await;
            }
        }
    }
}

/// Process price change event
async fn process_price_change(
    state: &Arc<RwLock<GlobalState>>,
    change: &PriceChangeItem,
    exec_tx: &mpsc::Sender<ExecutionRequest>,
    min_profit_cents: i16,
) {
    // Only process ASK updates
    if !matches!(change.side.as_deref(), Some("ASK" | "ask")) {
        return;
    }

    let Some(price_str) = &change.price else { return };
    let price = parse_price(price_str);
    if price == 0 { return; }

    // Find and update the outcome
    let guard = state.read().await;

    if let Some((market_id, outcome_idx)) = guard.find_by_token(&change.asset_id) {
        if let Some(market) = guard.get_market(market_id) {
            let book = &market.books[outcome_idx];
            let current_size = book.ask_size();

            // Always update to reflect current market state
            book.update_ask(price, current_size);

            // Check for arb
            if market.all_priced() && market.profit_cents() >= min_profit_cents {
                send_execution_request(market, exec_tx).await;
            }
        }
    }
}

/// Send execution request if arb detected
async fn send_execution_request(
    market: &crate::types::MultiOutcomeState,
    exec_tx: &mpsc::Sender<ExecutionRequest>,
) {
    let arb_type = if market.is_binary() {
        ArbType::Binary
    } else {
        ArbType::MultiOutcome
    };

    let req = ExecutionRequest {
        market_id: market.market_id,
        condition_id: market.condition_id.clone(),
        arb_type,
        ask_prices: market.ask_prices(),
        ask_sizes: market.ask_sizes(),
        token_ids: market.outcomes.iter().map(|o| o.token_id.clone()).collect(),
        expected_profit_cents: market.profit_cents(),
        detected_ns: monotonic_now_ns(),
    };

    debug!(
        "[POLY] Arb detected: {} | {} outcomes | {}¢ profit",
        market.question, market.outcome_count, req.expected_profit_cents
    );

    let _ = exec_tx.try_send(req);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_gamma_market_parsing() {
        let market = GammaMarket {
            condition_id: Some("cond1".to_string()),
            question: Some("Test?".to_string()),
            slug: Some("test".to_string()),
            outcomes: Some(r#"["Yes","No"]"#.to_string()),
            outcome_prices: Some(r#"["0.45","0.55"]"#.to_string()),
            clob_token_ids: Some(r#"["token1","token2"]"#.to_string()),
            volume: Some("100000".to_string()),
            liquidity: Some("10000".to_string()),
            start_date: None,
            end_date: None,
            active: Some(true),
            closed: Some(false),
            tags: Some(vec!["sports".to_string()]),
        };

        assert_eq!(market.parse_outcomes(), vec!["Yes", "No"]);
        assert_eq!(market.parse_token_ids(), vec!["token1", "token2"]);
        assert_eq!(market.outcome_count(), 2);
    }
}
