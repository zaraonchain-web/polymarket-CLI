//! Polymarket Farmer + Full Telegram Bot  (Chainlink-Signal Edition)
//!
//! ── SETUP ────────────────────────────────────────────────────────────────────
//! 1. Copy this file to:  polymarket-cli-main/src/farmer.rs
//!
//! 2. Edit polymarket-cli-main/Cargo.toml:
//!
//!    a) Add below the existing [[bin]] block:
//!       [[bin]]
//!       name = "farmer"
//!       path = "src/farmer.rs"
//!
//!    b) Add to [dependencies]:
//!       teloxide  = { version = "0.13", features = ["macros"] }
//!       tokio-tungstenite = { version = "0.24", features = ["rustls-tls-webpki-roots"] }
//!       futures-util = "0.3"
//!
//!    c) Change the existing tokio line to:
//!       tokio = { version = "1", features = ["rt-multi-thread", "macros", "sync", "time"] }
//!
//! 3. Set environment variables (Railway dashboard → Variables):
//!    POLYMARKET_PRIVATE_KEY   = 0x...
//!    TELEGRAM_BOT_TOKEN       = 123456:ABC-...
//!    TELEGRAM_CHAT_ID         = 123456789
//!
//! ── STRATEGY ─────────────────────────────────────────────────────────────────
//!
//! This farmer targets Polymarket's 15-minute BTC Up/Down markets, which
//! resolve using the Chainlink BTC/USD data stream on Polygon. The edge is:
//!
//!   1. Connect to Polymarket's real-time Chainlink websocket feed
//!   2. Track the BTC/USD price at the START of each 15-min window
//!   3. Measure price drift % vs start price as time passes
//!   4. Only enter when momentum is STRONG and time remaining is SHORT enough
//!      that a reversal is unlikely (e.g. BTC is +0.6% up with 3 min left)
//!   5. The market odds often haven't fully adjusted yet → edge exists
//!   6. Position sizing scales with confidence (drift magnitude)
//!
//! ── TELEGRAM COMMANDS ────────────────────────────────────────────────────────
//! FARMER
//!   /start                    Start the farming loop
//!   /stop                     Pause the farming loop
//!   /status                   Show if running or paused + current BTC signal
//!
//! PORTFOLIO
//!   /balance                  USDC collateral balance
//!   /positions                Your open bot positions
//!   /mypositions              On-chain open positions for your wallet
//!   /closedpositions          On-chain closed positions
//!   /trades                   Recent on-chain trade history
//!   /value                    Total portfolio value
//!
//! ORDERS
//!   /orders                   List open CLOB orders
//!   /cancelall                Cancel all open orders
//!   /cancel <orderID>         Cancel a specific order
//!   /buy <tokenID> <amount>   Market buy (USDC amount)
//!   /sell <tokenID> <shares>  Market sell (shares)
//!   /limit <tokenID> <side> <price> <size>   Limit order
//!
//! MARKETS
//!   /search <query>           Search markets
//!   /market <slug>            Get market details + price
//!   /book <tokenID>           Order book for a token
//!   /price <tokenID>          Midpoint price for a token
//!   /top                      Top 5 crypto markets by volume
//!   /signal                   Current BTC Chainlink signal + drift
//!
//! DATA
//!   /leaderboard              Top 10 traders
//!   /rewards                  Your reward earnings today
//!
//! CTF
//!   /redeem <conditionID>     Redeem winning tokens after resolution
//!
//! BRIDGE
//!   /deposit                  Get your deposit addresses (EVM/Solana/BTC)

#![allow(clippy::too_many_lines)]

use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{Local, Timelike, Utc};
use futures_util::StreamExt;
use teloxide::prelude::*;
use teloxide::types::{ChatId, ParseMode};
use teloxide::utils::command::BotCommands;
use tokio::sync::RwLock;
use tokio_tungstenite::connect_async;

use polymarket_client_sdk::{
    POLYGON,
    auth::{LocalSigner, Signer as _},
    bridge::{self, types::DepositRequest},
    clob::{
        self,
        types::{
            Amount, AssetType, OrderType, Side,
            SignatureType,
            request::{
                BalanceAllowanceRequest, LastTradePriceRequest, MidpointRequest,
                OrderBookSummaryRequest, OrdersRequest,
            },
        },
    },
    ctf::{self, types::RedeemPositionsRequest},
    data::{
        self,
        types::request::{
            ClosedPositionsRequest, PositionsRequest, TradesRequest,
            TraderLeaderboardRequest, ValueRequest,
        },
    },
    gamma::{self, types::request::{EventsRequest, MarketBySlugRequest, SearchRequest}},
    types::{Address, B256, Decimal, U256},
};

// ── Constants ─────────────────────────────────────────────────────────────────

/// Farmer cycle interval. 10 seconds so we can catch the last 10-second
/// snipe window inside each 5-min candle without missing it.
const POLL_SECS: u64 = 10;

/// Base USDC per trade.
const BASE_POSITION_USD: &str = "1";

/// Maximum USDC per single trade.
const MAX_POSITION_USD: &str = "5";

/// Minimum BTC drift % from the 5-min window open to consider a trade.
/// 0.04% on $83,000 BTC = ~$33 move — realistic for a 5-min choppy candle.
const MIN_DRIFT_PCT: f64 = 0.04;

/// We only enter in the LAST 10 SECONDS of the window.
/// This is tracked in seconds, not minutes (see seconds_remaining()).
/// The market-maker spread blows out right before resolution — that gap
/// is what we are buying into cheaply.
const MAX_SECONDS_REMAINING: u64 = 10;

/// Don't enter if fewer than 3 seconds remain — FOK needs time to fill.
const MIN_SECONDS_REMAINING: u64 = 3;

/// Take-profit on the GTC sell order: 8 cents per share.
/// At entry ≤ 38¢, a fill at 46¢ is very achievable right after resolution
/// when the losing side collapses and the winning side rockets toward $1.
const PROFIT_TARGET: &str = "0.08";

/// Maximum entry price. Only buy if midpoint is AT OR BELOW 38¢.
/// This ensures we always have at least 8¢ of room to our TP
/// and we are buying the "underdog" side that the spread is hiding.
const MAX_ENTRY_PRICE: &str = "0.38";

/// Minimum order-book spread (YES ask - NO ask, or best_ask - best_bid)
/// that must exist before we enter. 30¢ minimum, ideally 40¢.
/// A wide spread means market makers have pulled liquidity — the true
/// probability is clearer than the price suggests, giving us edge.
const MIN_SPREAD_FOR_ENTRY: &str = "0.30";

/// General market filters (for the fallback scanner).
const MIN_LIQUIDITY: &str = "500";
const MIN_VOLUME: &str = "1000";
const MAX_POSITIONS: usize = 5;

const USDC_DECIMALS: u32 = 6;
const USDC_ADDRESS_STR: &str = "0x2791Bca1f2de4661ED88A30C99A7a9449Aa84174";

/// Polymarket real-time Chainlink websocket
const POLYMARKET_WS: &str = "wss://ws-subscriptions-clob.polymarket.com/ws/";

// ── BTC price tracker ─────────────────────────────────────────────────────────

/// Stores the Chainlink BTC/USD price we received from the websocket.
#[derive(Debug, Clone, Default)]
struct BtcSignal {
    /// Price at the start of the current 5-min window (resets every 5 min).
    window_start_price: f64,
    /// The window start minute (0, 5, 10, 15, 20, 25, 30, 35, 40, 45, 50, 55).
    window_start_minute: u32,
    /// Latest streamed price.
    latest_price: f64,
    /// Drift % from window open (positive = BTC going up).
    drift_pct: f64,
    /// Timestamp of latest update.
    last_update: chrono::DateTime<Utc>,
}

impl BtcSignal {
    /// Called every time a new Chainlink price arrives from the websocket.
    fn update(&mut self, price: f64) {
        let now    = Utc::now();
        let minute = now.minute();
        // 5-min windows: :00, :05, :10, :15, :20, :25, :30, :35, :40, :45, :50, :55
        let window_start = (minute / 5) * 5;

        if self.window_start_price == 0.0 || window_start != self.window_start_minute {
            // New 5-min window — reset start price
            self.window_start_price  = price;
            self.window_start_minute = window_start;
        }

        self.latest_price = price;
        self.last_update  = now;

        if self.window_start_price > 0.0 {
            self.drift_pct = (price - self.window_start_price) / self.window_start_price * 100.0;
        }
    }

    /// Seconds remaining in the current 5-min window.
    /// e.g. if it is 12:04:52, window ends at 12:05:00, so 8 seconds remain.
    fn seconds_remaining(&self) -> u64 {
        let now     = Utc::now();
        let minute  = now.minute();
        let second  = now.second();
        let window  = (minute / 5) * 5;
        let elapsed_secs = ((minute - window) * 60 + second) as u64;
        (5 * 60u64).saturating_sub(elapsed_secs)
    }

    /// Returns Some((Side, label, abs_drift)) when ALL conditions are met:
    ///   1. We have live price data
    ///   2. We are in the last MAX_SECONDS_REMAINING seconds of the window
    ///   3. BTC has drifted at least MIN_DRIFT_PCT from the window open
    ///
    /// The direction of the drift tells us which side to buy:
    ///   UP drift   → buy YES (BTC ends higher than open → YES wins)
    ///   DOWN drift → buy NO  (BTC ends lower than open  → NO wins)
    fn trade_signal(&self) -> Option<(Side, &'static str, f64)> {
        if self.latest_price == 0.0 { return None; }

        let secs = self.seconds_remaining();
        if secs > MAX_SECONDS_REMAINING || secs < MIN_SECONDS_REMAINING {
            return None;
        }

        let abs_drift = self.drift_pct.abs();
        if abs_drift < MIN_DRIFT_PCT { return None; }

        if self.drift_pct > 0.0 {
            Some((Side::Buy, "YES", abs_drift))
        } else {
            Some((Side::Buy, "NO", abs_drift))
        }
    }

    /// Scale position size $1–$5 based on how strong the drift is.
    ///   0.04% (~$33) → $1   — minimum signal, minimum bet
    ///   0.10% (~$83) → ~$3  — moderate
    ///   0.18%+ (~$149)→ $5  — strong drift, full bet
    fn confidence_size(&self, abs_drift: f64) -> Decimal {
        let base: f64 = BASE_POSITION_USD.parse().unwrap();
        let max:  f64 = MAX_POSITION_USD.parse().unwrap();
        let scale_top = 0.18_f64; // hits max at 0.18% drift in a 5-min window
        let t = ((abs_drift - MIN_DRIFT_PCT) / (scale_top - MIN_DRIFT_PCT)).clamp(0.0, 1.0);
        let size = base + t * (max - base);
        Decimal::from_str(&format!("{:.2}", size)).unwrap()
    }
}

// ── Position tracker ──────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct Position {
    slug:          String,
    side_label:    String,
    entry_price:   Decimal,
    sell_order_id: String,
}

struct State {
    running:    bool,
    positions:  HashMap<String, Position>,
    btc_signal: BtcSignal,
}

impl State {
    fn new() -> Self {
        Self {
            running:    false,
            positions:  HashMap::new(),
            btc_signal: BtcSignal::default(),
        }
    }
}

type Shared = Arc<RwLock<State>>;

// ── Key resolution ────────────────────────────────────────────────────────────

fn resolve_key() -> Result<String> {
    if let Ok(k) = std::env::var("POLYMARKET_PRIVATE_KEY") {
        if !k.is_empty() { return Ok(k); }
    }
    if let Some(home) = dirs::home_dir() {
        let path = home.join(".config").join("polymarket").join("config.json");
        if path.exists() {
            let raw = std::fs::read_to_string(&path).context("read config")?;
            let v: serde_json::Value = serde_json::from_str(&raw).context("parse config")?;
            if let Some(k) = v["private_key"].as_str() { return Ok(k.to_string()); }
        }
    }
    anyhow::bail!("Set POLYMARKET_PRIVATE_KEY env var.")
}

fn my_address() -> Result<Address> {
    let key    = resolve_key()?;
    let signer = LocalSigner::from_str(&key).context("bad key")?;
    Ok(signer.address())
}

fn telegram_token() -> String {
    std::env::var("TELEGRAM_BOT_TOKEN").expect("TELEGRAM_BOT_TOKEN not set")
}

fn allowed_chat() -> ChatId {
    let id: i64 = std::env::var("TELEGRAM_CHAT_ID")
        .expect("TELEGRAM_CHAT_ID not set")
        .parse()
        .expect("TELEGRAM_CHAT_ID must be a number");
    ChatId(id)
}

// ── Auth ──────────────────────────────────────────────────────────────────────

async fn auth_clob()
    -> Result<clob::Client<polymarket_client_sdk::auth::state::Authenticated<polymarket_client_sdk::auth::Normal>>>
{
    let key    = resolve_key()?;
    let signer = LocalSigner::from_str(&key).context("bad key")?.with_chain_id(Some(POLYGON));
    clob::Client::default()
        .authentication_builder(&signer)
        .signature_type(SignatureType::Proxy)
        .authenticate()
        .await
        .context("CLOB auth failed")
}

// ── Notify helper ─────────────────────────────────────────────────────────────

async fn notify(bot: &Bot, text: &str) {
    if let Err(e) = bot.send_message(allowed_chat(), text)
        .parse_mode(ParseMode::Html).await
    {
        eprintln!("[WARN] Telegram send: {e}");
    }
}

// ── Main ──────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() {
    println!("Starting Polymarket Farmer + Telegram Bot (Chainlink Edition)…");
    resolve_key().expect("POLYMARKET_PRIVATE_KEY missing");

    let shared: Shared = Arc::new(RwLock::new(State::new()));
    let bot = Bot::new(telegram_token());

    // Spawn Chainlink BTC price websocket listener
    {
        let s = Arc::clone(&shared);
        tokio::spawn(async move { chainlink_listener(s).await; });
    }

    // Spawn farmer loop
    {
        let b = bot.clone();
        let s = Arc::clone(&shared);
        tokio::spawn(async move { farmer_loop(b, s).await; });
    }

    let help = Command::descriptions().to_string();
    notify(&bot, &format!("🤖 Polymarket Farmer (Chainlink Edition) online!\n\n{help}")).await;

    let handler = Update::filter_message()
        .branch(dptree::entry().filter_command::<Command>().endpoint(handle));

    Dispatcher::builder(bot, handler)
        .dependencies(dptree::deps![shared])
        .enable_ctrlc_handler()
        .build()
        .dispatch()
        .await;
}

// ── Chainlink websocket listener ──────────────────────────────────────────────

/// Connects to Polymarket's real-time Chainlink feed and updates BtcSignal
/// in shared state whenever a new BTC/USD price arrives.
async fn chainlink_listener(state: Shared) {
    loop {
        println!("[Chainlink] Connecting to websocket…");
        match connect_and_stream(&state).await {
            Ok(_)  => println!("[Chainlink] Stream ended, reconnecting…"),
            Err(e) => eprintln!("[Chainlink] Error: {e:#}. Reconnecting in 5s…"),
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

async fn connect_and_stream(state: &Shared) -> Result<()> {
    let url = format!("{POLYMARKET_WS}");
    let (ws_stream, _) = connect_async(&url).await.context("WS connect failed")?;
    let (mut _write, mut read) = ws_stream.split();

    // Subscribe to Chainlink BTC/USD
    // Polymarket's websocket subscription message format:
    let subscribe_msg = serde_json::json!({
        "action": "subscribe",
        "subscriptions": [{
            "topic": "crypto_prices_chainlink",
            "type": "*",
            "filters": "{\"symbol\":\"btc/usd\"}"
        }]
    }).to_string();

    use futures_util::SinkExt;
    let (mut write, mut read2) = {
        let url2 = format!("{POLYMARKET_WS}");
        let (ws2, _) = connect_async(&url2).await.context("WS connect2 failed")?;
        ws2.split()
    };
    write.send(tokio_tungstenite::tungstenite::Message::Text(subscribe_msg.into())).await?;

    // Keep alive ping every 5 seconds
    let ping_handle = {
        let mut w = write;
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(5)).await;
                if w.send(tokio_tungstenite::tungstenite::Message::Ping(vec![].into())).await.is_err() {
                    break;
                }
            }
        })
    };

    while let Some(msg) = read2.next().await {
        let msg = msg?;
        if let tokio_tungstenite::tungstenite::Message::Text(text) = msg {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                // Expected: {"topic":"crypto_prices_chainlink","type":"update","payload":{"symbol":"btc/usd","value":83500.0,...}}
                if v["topic"] == "crypto_prices_chainlink" {
                    if let Some(price) = v["payload"]["value"].as_f64() {
                        let mut s = state.write().await;
                        s.btc_signal.update(price);
                        // Log every price update for debugging
                        println!(
                            "[Chainlink] BTC/USD ${:.2} | drift {:.3}% | {}m rem",
                            price,
                            s.btc_signal.drift_pct,
                            s.btc_signal.minutes_remaining(),
                        );
                    }
                }
            }
        }
    }

    ping_handle.abort();
    Ok(())
}

// ── Commands ──────────────────────────────────────────────────────────────────

#[derive(BotCommands, Clone)]
#[command(rename_rule = "lowercase", description = "Polymarket Farmer (Chainlink Edition)")]
enum Command {
    #[command(description = "Start farming loop")]
    Start,
    #[command(description = "Pause farming loop")]
    Stop,
    #[command(description = "Farmer status + current BTC signal")]
    Status,
    #[command(description = "Current Chainlink BTC/USD signal details")]
    Signal,
    #[command(description = "USDC balance")]
    Balance,
    #[command(description = "Bot open positions")]
    Positions,
    #[command(description = "On-chain open positions")]
    Mypositions,
    #[command(description = "On-chain closed positions")]
    Closedpositions,
    #[command(description = "Recent trade history")]
    Trades,
    #[command(description = "Portfolio value")]
    Value,
    #[command(description = "Open CLOB orders")]
    Orders,
    #[command(description = "Cancel all orders")]
    Cancelall,
    #[command(description = "Cancel order: /cancel <orderID>")]
    Cancel(String),
    #[command(description = "Market buy: /buy <tokenID> <usdcAmount>")]
    Buy(String),
    #[command(description = "Market sell: /sell <tokenID> <shares>")]
    Sell(String),
    #[command(description = "Limit order: /limit <tokenID> <buy|sell> <price> <size>")]
    Limit(String),
    #[command(description = "Search markets: /search <query>")]
    Search(String),
    #[command(description = "Market info: /market <slug>")]
    Market(String),
    #[command(description = "Order book: /book <tokenID>")]
    Book(String),
    #[command(description = "Token price: /price <tokenID>")]
    Price(String),
    #[command(description = "Top 5 crypto markets by volume")]
    Top,
    #[command(description = "Top 10 trader leaderboard")]
    Leaderboard,
    #[command(description = "Your reward earnings")]
    Rewards,
    #[command(description = "Redeem winning tokens: /redeem <conditionID>")]
    Redeem(String),
    #[command(description = "Get your deposit addresses")]
    Deposit,
}

async fn handle(
    bot:   Bot,
    msg:   Message,
    cmd:   Command,
    state: Shared,
) -> ResponseResult<()> {
    if msg.chat.id != allowed_chat() {
        bot.send_message(msg.chat.id, "⛔ Unauthorized.").await?;
        return Ok(());
    }

    let reply = dispatch(&bot, &msg, cmd, &state).await
        .unwrap_or_else(|e| format!("❌ Error: {e:#}"));

    bot.send_message(msg.chat.id, &reply)
        .parse_mode(ParseMode::Html)
        .await?;

    Ok(())
}

async fn dispatch(
    _bot:  &Bot,
    _msg:  &Message,
    cmd:   Command,
    state: &Shared,
) -> Result<String> {
    match cmd {
        // ── Farmer controls ───────────────────────────────────────────────
        Command::Start => {
            let mut s = state.write().await;
            if s.running {
                Ok("✅ Already running.".into())
            } else {
                s.running = true;
                Ok("▶️ Farmer started. Watching Chainlink BTC/USD for 15-min window signals.".into())
            }
        }

        Command::Stop => {
            state.write().await.running = false;
            Ok("⏸ Farmer paused. Open positions remain on exchange.".into())
        }

        Command::Status => {
            let s = state.read().await;
            let sig = &s.btc_signal;
            let signal_line = if sig.latest_price == 0.0 {
                "⏳ Waiting for Chainlink feed…".to_string()
            } else {
                format!(
                    "BTC: <b>${:.2}</b> | Drift: <b>{:+.3}%</b> | Window rem: <b>{}s</b>",
                    sig.latest_price, sig.drift_pct, sig.seconds_remaining()
                )
            };
            Ok(format!(
                "{}\nBot positions: {}\n{}",
                if s.running { "▶️ Running" } else { "⏸ Paused" },
                s.positions.len(),
                signal_line,
            ))
        }

        Command::Signal => {
            let s = state.read().await;
            let sig = &s.btc_signal;
            if sig.latest_price == 0.0 {
                return Ok("⏳ No Chainlink data yet. Feed may be connecting…".into());
            }
            let direction = if sig.drift_pct > 0.0 { "📈 UP" } else { "📉 DOWN" };
            let secs = sig.seconds_remaining();
            let trade = match sig.trade_signal() {
                Some((_, label, drift)) => format!(
                    "\n🎯 <b>SNIPE SIGNAL ACTIVE</b>: Buy <b>{label}</b> (drift {drift:.3}%, {secs}s rem)"
                ),
                None => format!(
                    "\n⏸ No signal (need ≥{MIN_DRIFT_PCT}% drift + {MIN_SECONDS_REMAINING}–{MAX_SECONDS_REMAINING}s remaining + ≥30¢ spread gap)"
                ),
            };
            Ok(format!(
                "📡 <b>Chainlink BTC/USD (5-min)</b>\n\
                 Price: <b>${:.2}</b>\n\
                 Window open: <b>${:.2}</b>\n\
                 Drift: <b>{:+.3}%</b> {direction}\n\
                 Seconds remaining: <b>{secs}s</b>\
                 {trade}",
                sig.latest_price,
                sig.window_start_price,
                sig.drift_pct,
            ))
        }

        // ── Portfolio ─────────────────────────────────────────────────────
        Command::Balance => {
            let bal = fetch_balance().await?;
            Ok(format!("💰 USDC Balance: <b>${:.2}</b>", bal))
        }

        Command::Positions => {
            let s = state.read().await;
            if s.positions.is_empty() {
                return Ok("📭 No open bot positions.".into());
            }
            let mut lines = vec!["📊 <b>Bot Positions:</b>".to_string()];
            for (slug, p) in &s.positions {
                let target = p.entry_price + Decimal::from_str(PROFIT_TARGET).unwrap();
                lines.push(format!(
                    "• <b>{}</b> | {} @ {:.2}¢ → {:.2}¢\n  sell: <code>{}</code>",
                    slug,
                    p.side_label,
                    p.entry_price * Decimal::from(100),
                    target       * Decimal::from(100),
                    p.sell_order_id,
                ));
            }
            Ok(lines.join("\n"))
        }

        Command::Mypositions => {
            let addr   = my_address()?;
            let client = data::Client::default();
            let req    = PositionsRequest::builder().user(addr).limit(10)?.build();
            let positions = client.positions(&req).await?;
            if positions.is_empty() {
                return Ok("📭 No on-chain positions found.".into());
            }
            let mut lines = vec!["📊 <b>On-Chain Positions:</b>".to_string()];
            for p in positions.iter().take(10) {
                lines.push(format!(
                    "• <b>{}</b> | {} | size: {:.2} | avg: {:.4} | pnl: {:.2}",
                    truncate(&p.title, 35), p.outcome, p.size, p.avg_price, p.cash_pnl,
                ));
            }
            Ok(lines.join("\n"))
        }

        Command::Closedpositions => {
            let addr   = my_address()?;
            let client = data::Client::default();
            let req    = ClosedPositionsRequest::builder().user(addr).limit(10)?.build();
            let positions = client.closed_positions(&req).await?;
            if positions.is_empty() {
                return Ok("📭 No closed positions.".into());
            }
            let mut lines = vec!["📋 <b>Closed Positions:</b>".to_string()];
            for p in positions.iter().take(10) {
                lines.push(format!(
                    "• <b>{}</b> | {} | avg: {:.4} | realized pnl: {:.2}",
                    truncate(&p.title, 35), p.outcome, p.avg_price, p.realized_pnl,
                ));
            }
            Ok(lines.join("\n"))
        }

        Command::Trades => {
            let addr   = my_address()?;
            let client = data::Client::default();
            let req    = TradesRequest::builder().user(addr).limit(10)?.build();
            let trades = client.trades(&req).await?;
            if trades.is_empty() {
                return Ok("📭 No trades found.".into());
            }
            let mut lines = vec!["📜 <b>Recent Trades:</b>".to_string()];
            for t in trades.iter().take(10) {
                lines.push(format!(
                    "• <b>{}</b> | {} {} | {:.2} @ {:.4}",
                    truncate(&t.title, 30), t.side, t.outcome, t.size, t.price,
                ));
            }
            Ok(lines.join("\n"))
        }

        Command::Value => {
            let addr   = my_address()?;
            let client = data::Client::default();
            let req    = ValueRequest::builder().user(addr).build();
            let values = client.value(&req).await?;
            if values.is_empty() {
                return Ok("No value data.".into());
            }
            let total: Decimal = values.iter().map(|v| v.value).sum();
            Ok(format!("💼 Portfolio Value: <b>${:.2}</b>", total))
        }

        // ── Orders ────────────────────────────────────────────────────────
        Command::Orders => {
            let client = auth_clob().await?;
            let req    = OrdersRequest::builder().build();
            let page   = client.orders(&req, None).await?;
            if page.data.is_empty() {
                return Ok("📭 No open orders.".into());
            }
            let mut lines = vec![format!("📋 <b>Open Orders ({})</b>:", page.data.len())];
            for o in page.data.iter().take(10) {
                lines.push(format!(
                    "• <code>{}</code> | {} | {} @ {} | matched: {}",
                    truncate(&o.id, 12), o.side, o.price, o.original_size, o.size_matched,
                ));
            }
            Ok(lines.join("\n"))
        }

        Command::Cancelall => {
            let client = auth_clob().await?;
            let req    = OrdersRequest::builder().build();
            let page   = client.orders(&req, None).await?;
            let count  = page.data.len();
            client.cancel_all_orders().await?;
            state.write().await.positions.clear();
            Ok(format!("🚫 Cancelled {count} order(s). Bot positions cleared."))
        }

        Command::Cancel(order_id) => {
            let order_id = order_id.trim().to_string();
            if order_id.is_empty() {
                return Ok("Usage: /cancel &lt;orderID&gt;".into());
            }
            let client = auth_clob().await?;
            client.cancel_order(&order_id).await?;
            Ok(format!("✅ Cancelled order <code>{}</code>", truncate(&order_id, 16)))
        }

        Command::Buy(args) => {
            let parts: Vec<&str> = args.split_whitespace().collect();
            if parts.len() != 2 {
                return Ok("Usage: /buy &lt;tokenID&gt; &lt;usdcAmount&gt;".into());
            }
            let token_id = U256::from_str(parts[0]).map_err(|_| anyhow::anyhow!("Invalid token ID"))?;
            let amount   = Decimal::from_str(parts[1]).map_err(|_| anyhow::anyhow!("Invalid amount"))?;
            let (client, signer) = make_clob_client().await?;
            let order = client.market_order()
                .token_id(token_id).side(Side::Buy)
                .amount(Amount::usdc(amount)?).order_type(OrderType::FOK)
                .build().await?;
            let order = client.sign(&signer, order).await?;
            let res   = client.post_order(order).await?;
            if res.success {
                Ok(format!("✅ Buy filled!\nOrder: <code>{}</code>\nShares: {}", res.order_id, res.taking_amount))
            } else {
                Ok(format!("❌ Buy failed: {}", res.error_msg.unwrap_or_default()))
            }
        }

        Command::Sell(args) => {
            let parts: Vec<&str> = args.split_whitespace().collect();
            if parts.len() != 2 {
                return Ok("Usage: /sell &lt;tokenID&gt; &lt;shares&gt;".into());
            }
            let token_id = U256::from_str(parts[0]).map_err(|_| anyhow::anyhow!("Invalid token ID"))?;
            let shares   = Decimal::from_str(parts[1]).map_err(|_| anyhow::anyhow!("Invalid shares"))?;
            let (client, signer) = make_clob_client().await?;
            let order = client.market_order()
                .token_id(token_id).side(Side::Sell)
                .amount(Amount::shares(shares)?).order_type(OrderType::FOK)
                .build().await?;
            let order = client.sign(&signer, order).await?;
            let res   = client.post_order(order).await?;
            if res.success {
                Ok(format!("✅ Sell filled!\nOrder: <code>{}</code>\nUSDC: {}", res.order_id, res.making_amount))
            } else {
                Ok(format!("❌ Sell failed: {}", res.error_msg.unwrap_or_default()))
            }
        }

        Command::Limit(args) => {
            let parts: Vec<&str> = args.split_whitespace().collect();
            if parts.len() != 4 {
                return Ok("Usage: /limit &lt;tokenID&gt; &lt;buy|sell&gt; &lt;price&gt; &lt;size&gt;".into());
            }
            let token_id = U256::from_str(parts[0]).map_err(|_| anyhow::anyhow!("Invalid token ID"))?;
            let side = match parts[1].to_lowercase().as_str() {
                "buy"  => Side::Buy,
                "sell" => Side::Sell,
                _      => anyhow::bail!("Side must be buy or sell"),
            };
            let price = Decimal::from_str(parts[2]).map_err(|_| anyhow::anyhow!("Invalid price"))?;
            let size  = Decimal::from_str(parts[3]).map_err(|_| anyhow::anyhow!("Invalid size"))?;
            let (client, signer) = make_clob_client().await?;
            let order = client.limit_order()
                .token_id(token_id).side(side).price(price).size(size)
                .order_type(OrderType::GTC).build().await?;
            let order = client.sign(&signer, order).await?;
            let res   = client.post_order(order).await?;
            Ok(format!("✅ Limit placed!\nID: <code>{}</code>\nStatus: {}", res.order_id, res.status))
        }

        // ── Markets ───────────────────────────────────────────────────────
        Command::Search(query) => {
            if query.trim().is_empty() {
                return Ok("Usage: /search &lt;query&gt;".into());
            }
            let gamma = gamma::Client::default();
            let req = SearchRequest::builder()
                .q(query.trim().to_string())
                .limit_per_type(5)
                .build();
            let results = gamma.search(&req).await?;
            let markets: Vec<_> = results.events
                .unwrap_or_default()
                .into_iter()
                .flat_map(|e| e.markets.unwrap_or_default())
                .take(5)
                .collect();
            if markets.is_empty() {
                return Ok("No markets found.".into());
            }
            let mut lines = vec!["🔍 <b>Search Results:</b>".to_string()];
            for m in &markets {
                let q    = m.question.as_deref().unwrap_or("—");
                let slug = m.slug.as_deref().unwrap_or("—");
                let price = m.outcome_prices.as_ref()
                    .and_then(|p| p.first())
                    .map(|p| format!("{:.2}¢", p * Decimal::from(100)))
                    .unwrap_or_else(|| "—".into());
                lines.push(format!("• <b>{}</b>\n  slug: <code>{slug}</code> | YES: {price}", truncate(q, 50)));
            }
            Ok(lines.join("\n"))
        }

        Command::Market(slug) => {
            let slug = slug.trim().to_string();
            if slug.is_empty() {
                return Ok("Usage: /market &lt;slug&gt;".into());
            }
            let gamma = gamma::Client::default();
            let req   = MarketBySlugRequest::builder().slug(slug).build();
            let m     = gamma.market_by_slug(&req).await?;
            let q     = m.question.as_deref().unwrap_or("—");
            let yes   = m.outcome_prices.as_ref().and_then(|p| p.first())
                .map(|p| format!("{:.2}¢", p * Decimal::from(100))).unwrap_or_else(|| "—".into());
            let no    = m.outcome_prices.as_ref().and_then(|p| p.get(1))
                .map(|p| format!("{:.2}¢", p * Decimal::from(100))).unwrap_or_else(|| "—".into());
            let vol   = m.volume_num.map(|v| format!("${:.0}", v)).unwrap_or_else(|| "—".into());
            let liq   = m.liquidity_num.map(|v| format!("${:.0}", v)).unwrap_or_else(|| "—".into());
            let status = match (m.closed, m.active) {
                (Some(true), _) => "Closed",
                (_, Some(true)) => "Active",
                _               => "Inactive",
            };
            Ok(format!(
                "📊 <b>{}</b>\n\nYES: {yes} | NO: {no}\nVol: {vol} | Liq: {liq}\nStatus: {status}",
                truncate(q, 80)
            ))
        }

        Command::Book(token_arg) => {
            let token_id = parse_token(&token_arg)?;
            let unauth   = clob::Client::default();
            let req      = OrderBookSummaryRequest::builder().token_id(token_id).build();
            let book     = unauth.order_book(&req).await?;
            let mut lines = vec![format!("📖 <b>Order Book</b>\nMarket: <code>{}</code>", book.market)];
            if let Some(ltp) = book.last_trade_price {
                lines.push(format!("Last trade: {:.4}", ltp));
            }
            lines.push("<b>Top Bids:</b>".into());
            for b in book.bids.iter().take(3) {
                lines.push(format!("  {} @ {}", b.size, b.price));
            }
            lines.push("<b>Top Asks:</b>".into());
            for a in book.asks.iter().take(3) {
                lines.push(format!("  {} @ {}", a.size, a.price));
            }
            Ok(lines.join("\n"))
        }

        Command::Price(token_arg) => {
            let token_id = parse_token(&token_arg)?;
            let unauth   = clob::Client::default();
            let mid_req  = MidpointRequest::builder().token_id(token_id).build();
            let mid      = unauth.midpoint(&mid_req).await?;
            let lt_req   = LastTradePriceRequest::builder().token_id(token_id).build();
            let last     = unauth.last_trade_price(&lt_req).await.ok();
            let mut msg  = format!("💹 Midpoint: <b>{:.4}</b> ({:.2}¢)", mid.mid, mid.mid * Decimal::from(100));
            if let Some(lt) = last {
                msg.push_str(&format!("\nLast trade: {:.4} ({})", lt.price, lt.side));
            }
            Ok(msg)
        }

        Command::Top => {
            let gamma = gamma::Client::default();
            let req   = EventsRequest::builder()
                .limit(5)
                .maybe_closed(Some(false))
                .maybe_tag_slug(Some("crypto".to_string()))
                .ascending(false)
                .build();
            let events = gamma.events(&req).await?;
            let mut lines = vec!["🏆 <b>Top Crypto Events:</b>".to_string()];
            for e in events.iter().take(5) {
                let title = e.title.as_deref().unwrap_or("—");
                let vol   = e.volume.map(|v| format!("${:.0}", v)).unwrap_or_else(|| "—".into());
                let liq   = e.liquidity.map(|v| format!("${:.0}", v)).unwrap_or_else(|| "—".into());
                lines.push(format!("• <b>{}</b>\n  Vol: {vol} | Liq: {liq}", truncate(title, 50)));
            }
            Ok(lines.join("\n"))
        }

        // ── Data ──────────────────────────────────────────────────────────
        Command::Leaderboard => {
            let client  = data::Client::default();
            let req     = TraderLeaderboardRequest::builder().limit(10)?.build();
            let entries = client.leaderboard(&req).await?;
            if entries.is_empty() {
                return Ok("No leaderboard data.".into());
            }
            let mut lines = vec!["🏆 <b>Top Traders:</b>".to_string()];
            for e in entries.iter().take(10) {
                let name = e.user_name.as_deref().unwrap_or("Anonymous");
                lines.push(format!("{}. <b>{}</b> | PnL: ${:.0} | Vol: ${:.0}", e.rank, truncate(name, 20), e.pnl, e.vol));
            }
            Ok(lines.join("\n"))
        }

        Command::Rewards => {
            let client   = auth_clob().await?;
            let today    = chrono::Local::now().date_naive();
            let earnings = client.total_earnings_for_user_for_day(today).await?;
            if earnings.is_empty() {
                return Ok("No reward earnings today.".into());
            }
            let mut lines = vec!["🎁 <b>Today's Rewards:</b>".to_string()];
            for e in &earnings {
                lines.push(format!("• ${:.4} (rate: {})", e.earnings, e.asset_rate));
            }
            Ok(lines.join("\n"))
        }

        // ── CTF ───────────────────────────────────────────────────────────
        Command::Redeem(condition_str) => {
            let condition_str = condition_str.trim().to_string();
            if condition_str.is_empty() {
                return Ok("Usage: /redeem &lt;conditionID&gt;".into());
            }
            let condition = B256::from_str(&condition_str)
                .map_err(|_| anyhow::anyhow!("Invalid condition ID"))?;
            let collateral = Address::from_str(USDC_ADDRESS_STR).unwrap();
            let provider = {
                let key    = resolve_key()?;
                let signer = LocalSigner::from_str(&key)?.with_chain_id(Some(POLYGON));
                alloy::providers::ProviderBuilder::new()
                    .wallet(signer)
                    .connect("https://polygon.drpc.org")
                    .await?
            };
            let client = ctf::Client::new(provider, POLYGON)?;
            let req = RedeemPositionsRequest::builder()
                .collateral_token(collateral)
                .parent_collection_id(B256::default())
                .condition_id(condition)
                .index_sets(vec![U256::from(1u64), U256::from(2u64)])
                .build();
            let resp = client.redeem_positions(&req).await.context("Redeem failed")?;
            Ok(format!("✅ Redeemed!\nTx: <code>{}</code>\nBlock: {}", resp.transaction_hash, resp.block_number))
        }

        // ── Bridge ────────────────────────────────────────────────────────
        Command::Deposit => {
            let addr   = my_address()?;
            let client = bridge::Client::default();
            let req    = DepositRequest::builder().address(addr).build();
            let resp   = client.deposit(&req).await?;
            let mut lines = vec!["🌉 <b>Deposit Addresses:</b>".to_string()];
            lines.push(format!("EVM:    <code>{}</code>", resp.address.evm));
            lines.push(format!("Solana: <code>{}</code>", resp.address.svm));
            lines.push(format!("BTC:    <code>{}</code>", resp.address.btc));
            if let Some(note) = &resp.note {
                lines.push(format!("\n📝 {}", note));
            }
            Ok(lines.join("\n"))
        }
    }
}

// ── Farmer loop ───────────────────────────────────────────────────────────────

async fn farmer_loop(bot: Bot, state: Shared) {
    loop {
        let running = state.read().await.running;
        if running {
            println!("[{}] Farmer cycle…", Local::now().format("%H:%M:%S"));
            if let Err(e) = cycle(&bot, &state).await {
                let msg = format!("⚠️ Cycle error: {e:#}");
                eprintln!("{msg}");
                notify(&bot, &msg).await;
            }
        }
        tokio::time::sleep(Duration::from_secs(POLL_SECS)).await;
    }
}

async fn cycle(bot: &Bot, state: &Shared) -> Result<()> {
    check_exits(bot, state).await;

    {
        let s = state.read().await;
        if s.positions.len() >= MAX_POSITIONS {
            println!("  At max positions ({})", MAX_POSITIONS);
            return Ok(());
        }
    }

    // ── Primary strategy: Chainlink 5-min BTC snipe ───────────────────────
    let signal = {
        let s = state.read().await;
        s.btc_signal.trade_signal().map(|(side, label, drift)| {
            (side, label, drift, s.btc_signal.confidence_size(drift), s.btc_signal.seconds_remaining())
        })
    };

    if let Some((_side, label, drift, size_usd, rem)) = signal {
        let result = try_chainlink_trade(bot, state, label, drift, size_usd, rem).await;
        match result {
            Ok(true)  => return Ok(()), // traded, no need for fallback
            Ok(false) => {}             // no suitable 15-min market found, fall through
            Err(e)    => eprintln!("  [Chainlink trade] {e:#}"),
        }
    }

    // ── Fallback: general crypto market scanner (original strategy) ───────
    fallback_scanner(bot, state).await
}

/// Finds the active BTC Up/Down 5-min market matching the signal direction,
/// checks the order-book spread is wide enough (30-40¢), then enters.
/// Returns Ok(true) if a trade was placed.
async fn try_chainlink_trade(
    bot:      &Bot,
    state:    &Shared,
    label:    &str,   // "YES" or "NO"
    drift:    f64,
    size_usd: Decimal,
    rem:      u64,    // seconds remaining
) -> Result<bool> {
    let gamma = gamma::Client::default();
    let req = EventsRequest::builder()
        .limit(20)
        .maybe_closed(Some(false))
        .maybe_tag_slug(Some("crypto".to_string()))
        .ascending(false)
        .build();
    let events = gamma.events(&req).await?;

    let unauth     = clob::Client::default();
    let max_entry  = Decimal::from_str(MAX_ENTRY_PRICE).unwrap();  // 0.38
    let profit     = Decimal::from_str(PROFIT_TARGET).unwrap();    // 0.08
    let min_spread = Decimal::from_str(MIN_SPREAD_FOR_ENTRY).unwrap(); // 0.30

    for event in &events {
        let Some(markets) = &event.markets else { continue };
        for market in markets {
            let q    = market.question.as_deref().unwrap_or("").to_lowercase();
            let slug = match &market.slug { Some(s) if !s.is_empty() => s.clone(), _ => continue };

            // Must be a 5-min BTC up/down market specifically
            let is_5min_btc = (q.contains("btc") || q.contains("bitcoin"))
                && (q.contains("up") || q.contains("down") || q.contains("higher") || q.contains("lower"))
                && (q.contains("5") || q.contains("5m") || q.contains("5-min") || q.contains("5 min"));
            if !is_5min_btc { continue; }

            // Skip already open positions
            { if state.read().await.positions.contains_key(&slug) { continue; } }

            let tokens = match &market.clob_token_ids {
                Some(t) if t.len() >= 2 => t,
                _ => continue,
            };

            // YES token = index 0, NO token = index 1
            let (buy_token, buy_label) = if label == "YES" {
                (tokens[0], "YES")
            } else {
                (tokens[1], "NO")
            };

            // ── Fetch the order book to check spread width ────────────────
            // The spread = best_ask(YES) + best_ask(NO) should be wide
            // (i.e. YES + NO prices don't add up to 1.00) when market makers
            // have pulled out. We check: YES_mid + NO_mid ≤ 0.70 (30¢ gap)
            // which means the combined "insurance" spread is at least 30¢.
            let yes_mid_req = MidpointRequest::builder().token_id(tokens[0]).build();
            let no_mid_req  = MidpointRequest::builder().token_id(tokens[1]).build();

            let yes_mid = match unauth.midpoint(&yes_mid_req).await {
                Ok(m) => m.mid,
                Err(_) => continue,
            };
            let no_mid = match unauth.midpoint(&no_mid_req).await {
                Ok(m) => m.mid,
                Err(_) => continue,
            };

            // Spread check: YES + NO midpoints should leave a gap of ≥ 30¢
            // e.g. YES=0.35, NO=0.35 → sum=0.70, gap=0.30 ✓
            // e.g. YES=0.50, NO=0.48 → sum=0.98, gap=0.02 ✗ (too tight)
            let spread_gap = Decimal::from(1) - yes_mid - no_mid;
            if spread_gap < min_spread {
                println!(
                    "  [SNIPE] {slug} spread gap {:.2}¢ < {:.0}¢ min — skipping",
                    spread_gap * Decimal::from(100),
                    min_spread * Decimal::from(100),
                );
                continue;
            }

            // Entry price check: must be ≤ 38¢
            let buy_mid = if label == "YES" { yes_mid } else { no_mid };
            if buy_mid > max_entry {
                println!(
                    "  [SNIPE] {slug} {buy_label} mid {:.2}¢ > 38¢ max — skipping",
                    buy_mid * Decimal::from(100),
                );
                continue;
            }

            // TP: entry + 8¢, capped at 97¢ to avoid post-resolution weirdness
            let sell_price = (buy_mid + profit).min(Decimal::from_str("0.97").unwrap());

            let msg = format!(
                "🎯 <b>5-MIN BTC SNIPE</b>\n\
                 Market: <b>{slug}</b>\n\
                 Direction: <b>{buy_label}</b> | Drift: <b>{drift:+.3}%</b>\n\
                 Entry: <b>{:.2}¢</b> → TP: <b>{:.2}¢</b> (+8¢)\n\
                 Spread gap: <b>{:.2}¢</b> | <b>{rem}s</b> remaining\n\
                 Size: <b>${size_usd}</b>",
                buy_mid    * Decimal::from(100),
                sell_price * Decimal::from(100),
                spread_gap * Decimal::from(100),
            );
            println!("  [SNIPE] {slug} | {buy_label} | drift {drift:.3}% | gap {:.2}¢ | {rem}s rem",
                spread_gap * Decimal::from(100));
            notify(bot, &msg).await;

            match enter_position(&slug, buy_token, buy_label, buy_mid, sell_price, size_usd).await {
                Ok(pos) => {
                    notify(bot, &format!(
                        "✅ Sniped: <b>{slug}</b> ({buy_label})\nSell order: <code>{}</code>",
                        pos.sell_order_id
                    )).await;
                    let mut s = state.write().await;
                    s.positions.insert(slug, pos);
                    return Ok(true);
                }
                Err(e) => {
                    eprintln!("  [SNIPE] Entry failed: {e:#}");
                    notify(bot, &format!("❌ Snipe failed: {slug}\n{e:#}")).await;
                }
            }
        }
    }

    Ok(false)
}

/// Original blind scanner — runs when no Chainlink signal is active.
/// Buys cheap crypto prediction market outcomes and targets +7¢ profit.
async fn fallback_scanner(bot: &Bot, state: &Shared) -> Result<()> {
    let max_entry = Decimal::from_str(MAX_ENTRY_PRICE).unwrap();
    let profit    = Decimal::from_str(PROFIT_TARGET).unwrap();
    let size_usd  = Decimal::from_str(BASE_POSITION_USD).unwrap();
    let min_liq   = Decimal::from_str(MIN_LIQUIDITY).unwrap();
    let min_vol   = Decimal::from_str(MIN_VOLUME).unwrap();
    let unauth    = clob::Client::default();

    let gamma = gamma::Client::default();
    let req   = EventsRequest::builder()
        .limit(50)
        .maybe_closed(Some(false))
        .maybe_tag_slug(Some("crypto".to_string()))
        .ascending(false)
        .build();
    let events = gamma.events(&req).await.context("Gamma fetch failed")?;

    'outer: for event in &events {
        let Some(markets) = &event.markets else { continue };
        for market in markets {
            let slug = match &market.slug {
                Some(s) if !s.is_empty() => s.clone(),
                _ => continue,
            };
            { if state.read().await.positions.contains_key(&slug) { continue; } }

            let volume    = market.volume_num.unwrap_or(Decimal::ZERO);
            let liquidity = market.liquidity_num.unwrap_or(Decimal::ZERO);
            if volume < min_vol || liquidity < min_liq { continue; }

            let tokens = match &market.clob_token_ids {
                Some(t) if t.len() >= 2 => t,
                _ => continue,
            };
            let yes_mid = midpoint(&unauth, tokens[0]).await;
            let no_mid  = midpoint(&unauth, tokens[1]).await;

            let (side_label, buy_token, buy_price) = match (yes_mid, no_mid) {
                (Some(y), Some(n)) => {
                    if y <= n && y < max_entry     { ("YES", tokens[0], y) }
                    else if n < y && n < max_entry { ("NO",  tokens[1], n) }
                    else                           { continue; }
                }
                (Some(y), None) if y < max_entry => ("YES", tokens[0], y),
                (None, Some(n)) if n < max_entry => ("NO",  tokens[1], n),
                _ => continue,
            };

            let sell_price = buy_price + profit;
            if sell_price >= Decimal::from_str("0.99").unwrap() { continue; }

            let msg = format!(
                "📈 <b>FALLBACK ENTRY</b>\nMarket: <b>{slug}</b>\n\
                 Side: {side_label} @ {:.2}¢ → {:.2}¢\nVol: ${volume:.0} | Liq: ${liquidity:.0}",
                buy_price  * Decimal::from(100),
                sell_price * Decimal::from(100),
            );
            println!("  [FALLBACK] {slug} | {side_label}");
            notify(bot, &msg).await;

            match enter_position(&slug, buy_token, side_label, buy_price, sell_price, size_usd).await {
                Ok(pos) => {
                    notify(bot, &format!(
                        "✅ Opened: <b>{slug}</b> ({side_label})\nSell: <code>{}</code>",
                        pos.sell_order_id
                    )).await;
                    let mut s = state.write().await;
                    s.positions.insert(slug.clone(), pos);
                    if s.positions.len() >= MAX_POSITIONS { break 'outer; }
                }
                Err(e) => {
                    eprintln!("  [FALLBACK] {slug}: {e:#}");
                    notify(bot, &format!("❌ Entry failed: {slug}\n{e:#}")).await;
                }
            }
        }
    }
    Ok(())
}

async fn enter_position(
    slug:        &str,
    token_id:    U256,
    side_label:  &str,
    entry_price: Decimal,
    sell_price:  Decimal,
    size_usd:    Decimal,
) -> Result<Position> {
    let (client, signer) = make_clob_client().await?;

    let buy = client.market_order()
        .token_id(token_id).side(Side::Buy)
        .amount(Amount::usdc(size_usd)?).order_type(OrderType::FOK)
        .build().await?;
    let buy     = client.sign(&signer, buy).await?;
    let buy_res = client.post_order(buy).await.context("buy failed")?;

    if !buy_res.success {
        anyhow::bail!("FOK not filled: {}", buy_res.error_msg.unwrap_or_default());
    }

    let shares = if buy_res.taking_amount > Decimal::ZERO {
        buy_res.taking_amount
    } else {
        size_usd / entry_price
    };

    let sell = client.limit_order()
        .token_id(token_id).side(Side::Sell)
        .price(sell_price).size(shares).order_type(OrderType::GTC)
        .build().await?;
    let sell     = client.sign(&signer, sell).await?;
    let sell_res = client.post_order(sell).await.context("sell failed")?;

    Ok(Position {
        slug:          slug.to_string(),
        side_label:    side_label.to_string(),
        entry_price,
        sell_order_id: sell_res.order_id,
    })
}

async fn check_exits(bot: &Bot, state: &Shared) {
    let client = match auth_clob().await { Ok(c) => c, Err(_) => return };
    let req    = OrdersRequest::builder().build();
    let page   = match client.orders(&req, None).await { Ok(p) => p, Err(_) => return };

    let live: std::collections::HashSet<&str> = page.data.iter().map(|o| o.id.as_str()).collect();

    let gone: Vec<String> = {
        let s = state.read().await;
        s.positions.iter()
            .filter(|(_, p)| !live.contains(p.sell_order_id.as_str()))
            .map(|(slug, _)| slug.clone())
            .collect()
    };

    for slug in gone {
        let mut s = state.write().await;
        if let Some(p) = s.positions.remove(&slug) {
            let target = (p.entry_price + Decimal::from_str(PROFIT_TARGET).unwrap()) * Decimal::from(100);
            let msg = format!(
                "🎯 <b>EXIT</b>\nMarket: <b>{slug}</b> ({})\nSell filled — target was {:.2}¢",
                p.side_label, target
            );
            drop(s);
            notify(bot, &msg).await;
        }
    }
}

// ── Balance fetch ─────────────────────────────────────────────────────────────

async fn fetch_balance() -> Result<Decimal> {
    let client = auth_clob().await?;
    let req    = BalanceAllowanceRequest::builder()
        .asset_type(AssetType::Collateral)
        .build();
    let res = client.balance_allowance(req).await?;
    Ok(res.balance / Decimal::from(10u64.pow(USDC_DECIMALS)))
}

// ── Helpers ───────────────────────────────────────────────────────────────────

async fn midpoint(client: &clob::Client<clob::types::Unauthenticated>, token: U256) -> Option<Decimal> {
    let req = MidpointRequest::builder().token_id(token).build();
    client.midpoint(&req).await.ok().map(|r| r.mid)
}

/// Build an authenticated CLOB client + signer pair (avoids duplicating auth code).
async fn make_clob_client() -> Result<(
    clob::Client<polymarket_client_sdk::auth::state::Authenticated<polymarket_client_sdk::auth::Normal>>,
    impl polymarket_client_sdk::auth::Signer,
)> {
    let key    = resolve_key()?;
    let signer = LocalSigner::from_str(&key)?.with_chain_id(Some(POLYGON));
    let client = clob::Client::default()
        .authentication_builder(&signer)
        .signature_type(SignatureType::Proxy)
        .authenticate().await?;
    Ok((client, signer))
}

fn parse_token(s: &str) -> Result<U256> {
    U256::from_str(s.trim()).map_err(|_| anyhow::anyhow!("Invalid token ID"))
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max { s.to_string() }
    else { format!("{}…", s.chars().take(max - 1).collect::<String>()) }
}
