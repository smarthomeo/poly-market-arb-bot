//! Polymarket Arbitrage Bot Library
//!
//! A high-performance arbitrage trading system for Polymarket prediction markets.
//! Supports both binary (YES/NO) and multi-outcome markets.

pub mod circuit_breaker;
pub mod config;
pub mod event_detector;
pub mod execution;
pub mod market_scanner;
pub mod metrics;
pub mod paper_trading;
pub mod polymarket;
pub mod polymarket_clob;
pub mod position_tracker;
pub mod types;
