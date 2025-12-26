# Polymarket Arbitrage Bot v2.0

A high-performance arbitrage trading system for Polymarket prediction markets. Supports **binary (YES/NO)** and **multi-outcome** markets with mandatory paper trading mode for safe strategy validation.

## Strategy

Exploits the fundamental property of prediction markets: **sum of all outcome prices = $1.00**.

When you can buy ALL outcomes for less than $1.00 total, you lock in guaranteed profit:
- Exactly one outcome will win and pay $1.00
- You spent less than $1.00 to buy all outcomes
- **Profit = $1.00 - total cost**

### Example: Binary Market
```
YES ask: $0.48
NO ask:  $0.50
Total:   $0.98
Profit:  $0.02 per contract (2%)
```

### Example: Multi-Outcome Market (4 candidates)
```
Candidate A: $0.30
Candidate B: $0.25
Candidate C: $0.22
Candidate D: $0.18
Total:       $0.95
Profit:      $0.05 per set (5%)
```

## Features

- **Paper Trading Mode** - Mandatory 14-day simulation before live trading
- **Binary Markets** - YES/NO arbitrage detection
- **Multi-Outcome Markets** - 2-8 outcome support
- **Real-time WebSocket** - Live price feeds from Polymarket
- **Capital Management** - Conservative $100 account settings
- **Circuit Breaker** - Automatic halt on losses/errors
- **Metrics & Reporting** - Daily performance reports

## Quick Start

### 1. Install Rust
```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

### 2. Set Up Credentials
Create a `.env` file:
```bash
# Polymarket credentials
POLY_PRIVATE_KEY=0xYOUR_WALLET_PRIVATE_KEY
POLY_FUNDER=0xYOUR_WALLET_ADDRESS

# Mode settings (defaults to paper trading)
PAPER_MODE=1
DRY_RUN=1
RUST_LOG=info
```

### 3. Build & Run
```bash
# Build
cargo build --release

# Run (defaults to paper trading mode)
cargo run --release
```

## Environment Variables

| Variable | Default | Description |
|----------|---------|-------------|
| `POLY_PRIVATE_KEY` | Required | Ethereum private key (0x prefix) |
| `POLY_FUNDER` | Required | Your wallet address |
| `PAPER_MODE` | `1` | Paper trading mode (1=enabled) |
| `DRY_RUN` | `1` | Dry run mode (1=no real orders) |
| `FORCE_LIVE` | `0` | Force live mode even if paper ready |
| `RUST_LOG` | `info` | Log level |

### Circuit Breaker Settings
| Variable | Default | Description |
|----------|---------|-------------|
| `CB_MAX_POSITION_PER_MARKET` | `100` | Max contracts per market |
| `CB_MAX_TOTAL_POSITION` | `200` | Max total contracts |
| `CB_MAX_DAILY_LOSS` | `20` | Max daily loss ($) |
| `CB_MAX_CONSECUTIVE_ERRORS` | `5` | Errors before halt |

## Paper Trading Mode

Paper trading is **mandatory** for the first 14 days. The bot simulates:
- Realistic execution delays (100-500ms)
- Partial fills (70-100% fill rate)
- Price slippage (0-2¢)

### Graduation Requirements
To switch to live trading, you must meet ALL criteria:
- [ ] 14+ days of paper trading
- [ ] 10+ simulated trades
- [ ] 60%+ win rate
- [ ] $10+ simulated profit

### Check Status
The bot prints paper trading status every minute:
```
[PAPER] Day 7/14 | Balance: $102.50 | P&L: +$2.50 | Win rate: 72.5%
```

## Architecture

```
src/
├── main.rs              # Entry point, orchestration
├── config.rs            # Configuration constants
├── types.rs             # Core data structures
├── market_scanner.rs    # Gamma API market discovery
├── polymarket.rs        # WebSocket price feeds
├── polymarket_clob.rs   # Order execution client
├── execution.rs         # Arb execution engine
├── paper_trading.rs     # Paper trading simulation
├── metrics.rs           # Performance tracking
├── circuit_breaker.rs   # Risk management
├── position_tracker.rs  # Position & P&L tracking
└── event_detector.rs    # New market/volume detection
```

## Market Filters

The bot only tracks markets that meet these criteria:
- **Volume**: > $50,000 USD
- **Outcomes**: 2-8 outcomes
- **Expiry**: < 30 days
- **Categories**: Sports, Politics, Crypto, Entertainment

Disabled categories: Weather, local events, long-term futures (2026+)

## Realistic Expectations

With $100 capital:

| Scenario | Monthly Profit |
|----------|----------------|
| Pessimistic | $0-10 |
| Realistic | $20-50 |
| Optimistic | $50-100 |

**Important**: 
- Arb opportunities are rare and fleeting
- Competition from faster bots exists
- Server costs may eat into profits

## License

MIT OR Apache-2.0
