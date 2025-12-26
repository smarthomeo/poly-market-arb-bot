//! Polymarket Arbitrage Trading System
//!
//! A high-performance arbitrage bot for Polymarket prediction markets.
//! Supports binary (YES/NO) and multi-outcome markets with paper trading mode.

mod circuit_breaker;
mod config;
mod event_detector;
mod execution;
mod market_scanner;
mod metrics;
mod paper_trading;
mod polymarket;
mod polymarket_clob;
mod position_tracker;
mod types;

use anyhow::{Context, Result};
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{error, info, warn};

use circuit_breaker::{CircuitBreaker, CircuitBreakerConfig};
use config::{
    POLY_CLOB_HOST, POLYGON_CHAIN_ID, WS_RECONNECT_DELAY_SECS,
    MARKET_SCAN_INTERVAL_SECS, TOTAL_CAPITAL_CENTS,
    MIN_PROFIT_BINARY_CENTS, MIN_PROFIT_MULTI_CENTS,
};
use execution::{ExecutionEngine, create_execution_channel, run_execution_loop};
use market_scanner::MarketScanner;
use metrics::Metrics;
use paper_trading::PaperTrader;
use polymarket_clob::{PolymarketAsyncClient, PreparedCreds, SharedAsyncClient};
use position_tracker::{PositionTracker, create_position_channel, position_writer_loop};
use types::GlobalState;

const PAPER_TRADING_STATE_FILE: &str = "paper_trading_state.json";

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize logging
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("polymarket_arb=info".parse().unwrap()),
        )
        .init();

    info!("========================================");
    info!("  Polymarket Arbitrage Bot v2.0");
    info!("  Binary + Multi-Outcome Markets");
    info!("========================================");

    // Check mode
    let dry_run = std::env::var("DRY_RUN")
        .map(|v| v == "1" || v.to_lowercase() == "true")
        .unwrap_or(true);

    let paper_mode = std::env::var("PAPER_MODE")
        .map(|v| v == "1" || v.to_lowercase() == "true")
        .unwrap_or(true); // Default to paper mode for safety

    let force_live = std::env::var("FORCE_LIVE")
        .map(|v| v == "1" || v.to_lowercase() == "true")
        .unwrap_or(false);

    info!("Configuration:");
    info!("  Capital: ${:.2}", TOTAL_CAPITAL_CENTS as f64 / 100.0);
    info!("  Min profit (binary): {}¢", MIN_PROFIT_BINARY_CENTS);
    info!("  Min profit (multi): {}¢", MIN_PROFIT_MULTI_CENTS);
    info!("  Dry run: {}", dry_run);
    info!("  Paper mode: {}", paper_mode);

    // Initialize paper trader
    let paper_trader = if paper_mode {
        let trader = match PaperTrader::load(PAPER_TRADING_STATE_FILE).await {
            Ok(t) => {
                info!("[PAPER] Loaded existing paper trading state");
                t
            }
            Err(_) => {
                info!("[PAPER] Starting fresh paper trading session");
                PaperTrader::new()
            }
        };

        // Check if ready for live
        let (can_go_live, reason) = trader.can_go_live().await;
        if can_go_live && !force_live {
            info!("[PAPER] Ready for live trading: {}", reason);
            info!("[PAPER] Set FORCE_LIVE=1 to switch to live mode");
        } else if !can_go_live {
            info!("[PAPER] Not ready for live trading: {}", reason);
        }

        if force_live {
            warn!("[PAPER] FORCE_LIVE set - disabling paper trading safety");
            trader.disable();
        }

        Some(Arc::new(trader))
    } else {
        warn!("[LIVE] Running in LIVE mode - real money at risk!");
        None
    };

    let live_trading = !paper_mode || force_live;

    let poly_async = if live_trading {
        dotenvy::dotenv().ok();
        let poly_private_key = std::env::var("POLY_PRIVATE_KEY")
            .context("POLY_PRIVATE_KEY not set")?;
        let poly_funder = std::env::var("POLY_FUNDER")
            .context("POLY_FUNDER not set")?;

        info!("[POLY] Creating client and deriving API credentials...");
        let poly_client = PolymarketAsyncClient::new(
            POLY_CLOB_HOST,
            POLYGON_CHAIN_ID,
            &poly_private_key,
            &poly_funder,
        )?;
        let api_creds = poly_client.derive_api_key(0).await?;
        let prepared_creds = PreparedCreds::from_api_creds(&api_creds)?;
        let poly = Arc::new(SharedAsyncClient::new(poly_client, prepared_creds, POLYGON_CHAIN_ID));

        match poly.load_cache(".clob_market_cache.json") {
            Ok(count) => info!("[POLY] Loaded {} neg_risk entries from cache", count),
            Err(_) => info!("[POLY] No neg_risk cache found"),
        }

        info!("[POLY] Client ready for {}", &poly_funder[..10]);
        Some(poly)
    } else {
        info!("[POLY] Skipping credential load (paper mode)");
        None
    };

    // Initialize market scanner
    info!("[SCANNER] Initializing market scanner...");
    let scanner = Arc::new(MarketScanner::new());

    // Initial market scan
    let market_count = scanner.scan_markets().await?;
    info!("[SCANNER] Discovered {} markets", market_count);

    let stats = scanner.get_stats().await;
    info!("[SCANNER] {}", stats);

    // Build global state from scanned markets
    let state = Arc::new(RwLock::new(GlobalState::new()));
    let markets = scanner.get_markets().await;

    {
        let mut guard = state.write().await;
        for market in &markets {
            let outcomes: Vec<(String, String)> = market.outcomes
                .iter()
                .map(|o| (o.name.clone(), o.token_id.clone()))
                .collect();

            guard.add_market(&market.condition_id, &market.question, outcomes);
        }

        info!("[STATE] Tracking {} markets ({} binary, {} multi-outcome)",
            guard.market_count(), guard.binary_count(), guard.multi_count());
    }

    // Initialize metrics
    let metrics = Arc::new(Metrics::new(TOTAL_CAPITAL_CENTS as i64));

    // Initialize circuit breaker
    let cb_config = CircuitBreakerConfig::conservative();
    let circuit_breaker = Arc::new(CircuitBreaker::new(cb_config));

    // Initialize position tracker
    let position_tracker = Arc::new(RwLock::new(PositionTracker::load()));
    let (position_channel, position_rx) = create_position_channel();
    tokio::spawn(position_writer_loop(position_rx, position_tracker.clone()));

    // Initialize execution engine
    let (exec_tx, exec_rx) = create_execution_channel();

    let engine = Arc::new(ExecutionEngine::new(
        poly_async.clone(),
        circuit_breaker.clone(),
        paper_trader.clone(),
        dry_run,
    ));

    let exec_handle = tokio::spawn(run_execution_loop(exec_rx, engine));

    // Calculate min profit threshold
    let min_profit_cents = MIN_PROFIT_BINARY_CENTS as i16;

    // Start WebSocket connection
    let ws_state = state.clone();
    let ws_exec_tx = exec_tx.clone();
    let ws_handle = tokio::spawn(async move {
        loop {
            if let Err(e) = polymarket::run_ws(ws_state.clone(), ws_exec_tx.clone(), min_profit_cents).await {
                error!("[POLY] WebSocket disconnected: {} - reconnecting...", e);
            }
            tokio::time::sleep(tokio::time::Duration::from_secs(WS_RECONNECT_DELAY_SECS)).await;
        }
    });

    // Periodic market scanner refresh
    let scanner_clone = scanner.clone();
    let scanner_state = state.clone();
    let scanner_handle = tokio::spawn(async move {
        let mut interval = tokio::time::interval(
            tokio::time::Duration::from_secs(MARKET_SCAN_INTERVAL_SECS)
        );

        loop {
            interval.tick().await;
            match scanner_clone.scan_markets().await {
                Ok(count) => {
                    let stats = scanner_clone.get_stats().await;

                    let mut new_markets = 0;
                    let markets = scanner_clone.get_markets().await;
                    {
                        let mut guard = scanner_state.write().await;
                        for market in &markets {
                            if guard.has_condition(&market.condition_id) {
                                continue;
                            }

                            let outcomes: Vec<(String, String)> = market.outcomes
                                .iter()
                                .map(|o| (o.name.clone(), o.token_id.clone()))
                                .collect();

                            if guard.add_market(&market.condition_id, &market.question, outcomes).is_some() {
                                new_markets += 1;
                            }
                        }
                    }

                    if new_markets > 0 {
                        info!("[SCANNER] Added {} new markets", new_markets);
                    }

                    info!("[SCANNER] Refreshed: {} markets | {}", count, stats);
                }
                Err(e) => {
                    warn!("[SCANNER] Refresh failed: {}", e);
                }
            }
        }
    });

    // Heartbeat and status reporting
    let heartbeat_state = state.clone();
    let heartbeat_metrics = metrics.clone();
    let heartbeat_cb = circuit_breaker.clone();
    let heartbeat_paper = paper_trader.clone();
    let heartbeat_handle = tokio::spawn(async move {
        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(60));
        let mut day_counter = 1u32;

        loop {
            interval.tick().await;

            // Print status
            let (market_count, arb_candidates, best) = {
                let guard = heartbeat_state.read().await;
                let c = guard.market_count();
                let arbs = guard.get_arb_candidates(1);
                let best = arbs
                    .iter()
                    .max_by_key(|m| m.profit_cents())
                    .map(|m| (m.question.to_string(), m.outcome_count, m.sum_of_asks(), m.profit_cents()));
                (c, arbs.len(), best)
            };

            info!("----------------------------------------");
            info!("[HEARTBEAT] Markets: {} | Arb candidates: {}", market_count, arb_candidates);

            // Circuit breaker status
            let cb_status = heartbeat_cb.status().await;
            info!("{}", cb_status);

            // Paper trading status
            if let Some(paper) = &heartbeat_paper {
                let summary = paper.summary().await;
                info!(
                    "[PAPER] Day {}/14 | Balance: ${:.2} | P&L: {:+.2} | Win rate: {:.1}%",
                    summary.day_number,
                    summary.balance_cents as f64 / 100.0,
                    summary.pnl_cents as f64 / 100.0,
                    summary.win_rate * 100.0
                );
            }

            // Best arb opportunity
            if let Some((question, outcome_count, sum_of_asks, profit)) = best {
                if profit > 0 {
                    info!(
                        "[BEST] {} | {} outcomes | Sum: {}¢ | Profit: {}¢",
                        question, outcome_count, sum_of_asks, profit
                    );
                }
            }

            info!("----------------------------------------");

            // Increment day counter every 24 hours (1440 minutes)
            day_counter += 1;
            if day_counter % 1440 == 0 {
                if let Some(paper) = &heartbeat_paper {
                    paper.next_day();
                    if let Err(e) = paper.save(PAPER_TRADING_STATE_FILE).await {
                        warn!("[PAPER] Failed to save state: {}", e);
                    }
                }
            }
        }
    });

    // Daily report (runs once per day)
    let report_metrics = metrics.clone();
    let report_paper = paper_trader.clone();
    let report_handle = tokio::spawn(async move {
        // Wait until midnight, then run daily
        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(86400));

        loop {
            interval.tick().await;

            let mode = if report_paper.is_some() { "PAPER" } else { "LIVE" };
            let day = report_paper.as_ref()
                .map(|p| p.day_number.load(std::sync::atomic::Ordering::Relaxed))
                .unwrap_or(0);

            report_metrics.print_daily_report(mode, day);

            // Save paper trading state
            if let Some(paper) = &report_paper {
                if let Err(e) = paper.save(PAPER_TRADING_STATE_FILE).await {
                    warn!("[PAPER] Failed to save daily state: {}", e);
                }
            }
        }
    });

    info!("========================================");
    info!("  System Ready - Monitoring Markets");
    info!("========================================");

    // Run until termination
    let _ = tokio::join!(
        ws_handle,
        exec_handle,
        scanner_handle,
        heartbeat_handle,
        report_handle
    );

    Ok(())
}
