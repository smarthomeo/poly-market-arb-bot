//! Event detection for arbitrage opportunities.
//!
//! This module detects market conditions that often create arbitrage
//! opportunities: new markets, volume spikes, and significant price movements.

use chrono::{DateTime, Utc};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{debug, info};

use crate::config::NEW_MARKET_HOURS;
use crate::market_scanner::TrackedMarket;

/// Event that may indicate an arbitrage opportunity
#[derive(Debug, Clone)]
pub enum ArbEvent {
    /// New market created (often inefficient initially)
    NewMarket {
        condition_id: String,
        question: String,
        age_hours: i64,
    },
    /// Significant volume spike
    VolumeSpike {
        condition_id: String,
        question: String,
        volume_increase_pct: f64,
        current_volume: f64,
    },
    /// Large price movement on an outcome
    PriceMovement {
        condition_id: String,
        question: String,
        outcome_name: String,
        old_price: f64,
        new_price: f64,
        change_pct: f64,
    },
    /// Sum of asks dropped below threshold
    ArbOpportunity {
        condition_id: String,
        question: String,
        sum_of_asks: f64,
        profit_cents: i16,
        outcome_count: usize,
    },
}

/// Historical data for a market (for detecting changes)
#[derive(Debug, Clone)]
struct MarketHistory {
    condition_id: String,
    last_volume: f64,
    last_prices: HashMap<String, f64>, // token_id -> price
    last_sum_of_asks: f64,
    last_updated: DateTime<Utc>,
}

/// Event detector that monitors for arbitrage conditions
pub struct EventDetector {
    /// Historical market data
    history: Arc<RwLock<HashMap<String, MarketHistory>>>,

    /// Volume spike threshold (e.g., 5.0 = 5x increase)
    volume_spike_threshold: f64,

    /// Price movement threshold (e.g., 0.10 = 10% change)
    price_movement_threshold: f64,

    /// Events detected
    events: Arc<RwLock<Vec<(DateTime<Utc>, ArbEvent)>>>,

    /// Maximum events to keep in memory
    max_events: usize,
}

impl EventDetector {
    pub fn new() -> Self {
        Self {
            history: Arc::new(RwLock::new(HashMap::new())),
            volume_spike_threshold: 5.0,
            price_movement_threshold: 0.10,
            events: Arc::new(RwLock::new(Vec::new())),
            max_events: 1000,
        }
    }

    pub fn with_thresholds(volume_spike: f64, price_movement: f64) -> Self {
        Self {
            history: Arc::new(RwLock::new(HashMap::new())),
            volume_spike_threshold: volume_spike,
            price_movement_threshold: price_movement,
            events: Arc::new(RwLock::new(Vec::new())),
            max_events: 1000,
        }
    }

    /// Process a market update and detect events
    pub async fn process_market(&self, market: &TrackedMarket) -> Vec<ArbEvent> {
        let mut detected_events = Vec::new();

        // Check for new market
        if let Some(event) = self.check_new_market(market) {
            detected_events.push(event);
        }

        // Get or create history
        let mut history = self.history.write().await;
        let prev = history.get(&market.condition_id);

        // Check for volume spike
        if let Some(prev) = prev {
            if let Some(event) = self.check_volume_spike(market, prev) {
                detected_events.push(event);
            }

            // Check for price movements
            for outcome in &market.outcomes {
                if let Some(event) = self.check_price_movement(market, outcome, prev) {
                    detected_events.push(event);
                }
            }
        }

        // Check for arb opportunity
        if let Some(event) = self.check_arb_opportunity(market) {
            detected_events.push(event);
        }

        // Update history
        let new_history = MarketHistory {
            condition_id: market.condition_id.clone(),
            last_volume: market.volume_usd,
            last_prices: market
                .outcomes
                .iter()
                .map(|o| (o.token_id.clone(), o.ask_price))
                .collect(),
            last_sum_of_asks: market.sum_of_asks(),
            last_updated: Utc::now(),
        };
        history.insert(market.condition_id.clone(), new_history);

        // Store events
        if !detected_events.is_empty() {
            let mut events = self.events.write().await;
            for event in &detected_events {
                events.push((Utc::now(), event.clone()));
            }

            // Trim old events
            if events.len() > self.max_events {
                let drain_count = events.len() - self.max_events;
                events.drain(0..drain_count);
            }
        }

        detected_events
    }

    /// Check if market is new
    fn check_new_market(&self, market: &TrackedMarket) -> Option<ArbEvent> {
        if !market.is_new {
            return None;
        }

        let age_hours = market.created_at.map(|c| (Utc::now() - c).num_hours())?;

        if age_hours >= 0 && age_hours < NEW_MARKET_HOURS as i64 {
            info!(
                "[EVENT] New market detected: {} ({}h old)",
                market.question, age_hours
            );
            Some(ArbEvent::NewMarket {
                condition_id: market.condition_id.clone(),
                question: market.question.clone(),
                age_hours,
            })
        } else {
            None
        }
    }

    /// Check for volume spike
    fn check_volume_spike(&self, market: &TrackedMarket, prev: &MarketHistory) -> Option<ArbEvent> {
        if prev.last_volume <= 0.0 {
            return None;
        }

        let increase = market.volume_usd / prev.last_volume;

        if increase >= self.volume_spike_threshold {
            info!(
                "[EVENT] Volume spike: {} ({:.1}x increase)",
                market.question, increase
            );
            Some(ArbEvent::VolumeSpike {
                condition_id: market.condition_id.clone(),
                question: market.question.clone(),
                volume_increase_pct: (increase - 1.0) * 100.0,
                current_volume: market.volume_usd,
            })
        } else {
            None
        }
    }

    /// Check for significant price movement
    fn check_price_movement(
        &self,
        market: &TrackedMarket,
        outcome: &crate::market_scanner::Outcome,
        prev: &MarketHistory,
    ) -> Option<ArbEvent> {
        let old_price = *prev.last_prices.get(&outcome.token_id)?;

        if old_price <= 0.0 {
            return None;
        }

        let change = (outcome.ask_price - old_price).abs() / old_price;

        if change >= self.price_movement_threshold {
            info!(
                "[EVENT] Price movement: {} / {} ({:.1}% change)",
                market.question,
                outcome.name,
                change * 100.0
            );
            Some(ArbEvent::PriceMovement {
                condition_id: market.condition_id.clone(),
                question: market.question.clone(),
                outcome_name: outcome.name.clone(),
                old_price,
                new_price: outcome.ask_price,
                change_pct: change * 100.0,
            })
        } else {
            None
        }
    }

    /// Check for arb opportunity (sum < 1.0)
    fn check_arb_opportunity(&self, market: &TrackedMarket) -> Option<ArbEvent> {
        let profit = market.potential_profit_cents();

        if profit > 0 {
            debug!(
                "[EVENT] Arb opportunity: {} | {} outcomes | {}¢ profit",
                market.question,
                market.outcomes.len(),
                profit
            );
            Some(ArbEvent::ArbOpportunity {
                condition_id: market.condition_id.clone(),
                question: market.question.clone(),
                sum_of_asks: market.sum_of_asks(),
                profit_cents: profit,
                outcome_count: market.outcomes.len(),
            })
        } else {
            None
        }
    }

    /// Get recent events
    pub async fn get_recent_events(&self, limit: usize) -> Vec<(DateTime<Utc>, ArbEvent)> {
        let events = self.events.read().await;
        events.iter().rev().take(limit).cloned().collect()
    }

    /// Get arb events only
    pub async fn get_arb_events(&self) -> Vec<(DateTime<Utc>, ArbEvent)> {
        let events = self.events.read().await;
        events
            .iter()
            .filter(|(_, e)| matches!(e, ArbEvent::ArbOpportunity { .. }))
            .cloned()
            .collect()
    }

    /// Get new market events
    pub async fn get_new_market_events(&self) -> Vec<(DateTime<Utc>, ArbEvent)> {
        let events = self.events.read().await;
        events
            .iter()
            .filter(|(_, e)| matches!(e, ArbEvent::NewMarket { .. }))
            .cloned()
            .collect()
    }

    /// Clear history (for testing)
    pub async fn clear_history(&self) {
        self.history.write().await.clear();
        self.events.write().await.clear();
    }

    /// Get event counts
    pub async fn event_counts(&self) -> EventCounts {
        let events = self.events.read().await;

        let mut counts = EventCounts::default();
        for (_, event) in events.iter() {
            match event {
                ArbEvent::NewMarket { .. } => counts.new_markets += 1,
                ArbEvent::VolumeSpike { .. } => counts.volume_spikes += 1,
                ArbEvent::PriceMovement { .. } => counts.price_movements += 1,
                ArbEvent::ArbOpportunity { .. } => counts.arb_opportunities += 1,
            }
        }

        counts
    }
}

impl Default for EventDetector {
    fn default() -> Self {
        Self::new()
    }
}

/// Event count statistics
#[derive(Debug, Clone, Default)]
pub struct EventCounts {
    pub new_markets: usize,
    pub volume_spikes: usize,
    pub price_movements: usize,
    pub arb_opportunities: usize,
}

impl std::fmt::Display for EventCounts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Events: {} new markets, {} volume spikes, {} price moves, {} arbs",
            self.new_markets, self.volume_spikes, self.price_movements, self.arb_opportunities
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::MarketCategory;
    use crate::market_scanner::Outcome;

    fn make_test_market(sum_of_asks: f64) -> TrackedMarket {
        let outcomes = vec![
            Outcome {
                name: "Yes".to_string(),
                token_id: "1".to_string(),
                ask_price: sum_of_asks / 2.0,
                ask_size: 100.0,
                bid_price: sum_of_asks / 2.0 - 0.01,
            },
            Outcome {
                name: "No".to_string(),
                token_id: "2".to_string(),
                ask_price: sum_of_asks / 2.0,
                ask_size: 100.0,
                bid_price: sum_of_asks / 2.0 - 0.01,
            },
        ];

        TrackedMarket {
            condition_id: "test".to_string(),
            question: "Test market?".to_string(),
            slug: "test".to_string(),
            outcomes,
            volume_usd: 100000.0,
            liquidity_usd: 10000.0,
            created_at: Some(Utc::now()),
            end_date: None,
            category: MarketCategory::Sports,
            active: true,
            closed: false,
            is_new: true,
            last_updated: Utc::now(),
            tags: vec![],
        }
    }

    #[tokio::test]
    async fn test_arb_detection() {
        let detector = EventDetector::new();

        // Market with sum = 0.96 (4% profit)
        let market = make_test_market(0.96);
        let events = detector.process_market(&market).await;

        // Should detect arb opportunity
        let arb_events: Vec<_> = events
            .iter()
            .filter(|e| matches!(e, ArbEvent::ArbOpportunity { .. }))
            .collect();

        assert_eq!(arb_events.len(), 1);

        if let ArbEvent::ArbOpportunity { profit_cents, .. } = &arb_events[0] {
            assert_eq!(*profit_cents, 4);
        }
    }

    #[tokio::test]
    async fn test_no_arb_when_efficient() {
        let detector = EventDetector::new();

        // Market with sum = 1.02 (no arb)
        let market = make_test_market(1.02);
        let events = detector.process_market(&market).await;

        // Should not detect arb opportunity
        let arb_events: Vec<_> = events
            .iter()
            .filter(|e| matches!(e, ArbEvent::ArbOpportunity { .. }))
            .collect();

        assert_eq!(arb_events.len(), 0);
    }

    #[tokio::test]
    async fn test_new_market_detection() {
        let detector = EventDetector::new();

        let mut market = make_test_market(1.0);
        market.is_new = true;
        market.created_at = Some(Utc::now() - chrono::Duration::hours(2));

        let events = detector.process_market(&market).await;

        let new_market_events: Vec<_> = events
            .iter()
            .filter(|e| matches!(e, ArbEvent::NewMarket { .. }))
            .collect();

        assert_eq!(new_market_events.len(), 1);
    }
}
