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
//! Targets Polymarket's 5-minute BTC Up/Down markets, resolved via Chainlink.
//!
//!   1. Connect to Polymarket's real-time Chainlink BTC/USD websocket feed
//!   2. Track the BTC price at the START of each 5-min window
//!   3. In the LAST 3–15 SECONDS of the window, check the dollar move
//!   4. If BTC has moved ≥$30 from the window open, enter:
//!        UP  ($30+) → buy YES
//!        DOWN($30+) → buy NO
//!   5. After the window closes, wait 90s for Polymarket to resolve
//!   6. Auto-redeem winning tokens on-chain for full $1 per share
//!
//! ── TELEGRAM COMMANDS ────────────────────────────────────────────────────────
//! FARMER
//!   /start                    Start the farming loop
//!   /stop                     Pause the farming loop
//!   /status                   Running state + current BTC move
//!   /signal                   Full Chainlink signal breakdown + tier
//!
//! PORTFOLIO
//!   /balance                  USDC collateral balance
//!   /positions                Open bot positions (live or awaiting redeem)
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
//!
//! DATA
//!   /leaderboard              Top 10 traders
//!   /rewards                  Your reward earnings today
//!
//! CTF
//!   /redeem <conditionID>     Manually redeem winning tokens after resolution
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

/// Farmer cycle interval — 5 seconds to reliably catch the 15-second
/// entry window before each 5-min candle closes.
const POLL_SECS: u64 = 5;

/// Flat USDC size per trade.
const POSITION_USD: &str = "1";

/// Enter only in the LAST 15 SECONDS of the 5-min window.
const MAX_SECONDS_REMAINING: u64 = 15;

/// Don't enter if fewer than 3 seconds remain — FOK needs time to fill.
const MIN_SECONDS_REMAINING: u64 = 3;

/// Dollar move tiers from the window open price.
/// Tier 1: $30 minimum move  — enter
/// Tier 2: $40+ move         — still $1, but logged as stronger signal
const DOLLAR_TIER_1: f64 = 30.0;
const DOLLAR_TIER_2: f64 = 40.0;

/// How long to wait after window close before attempting redeem (seconds).
/// Polymarket typically resolves within 60-120s of window close.
const REDEEM_DELAY_SECS: u64 = 90;

const MAX_POSITIONS: usize = 5;

const USDC_DECIMALS: u32 = 6;
const USDC_ADDRESS_STR: &str = "0x2791Bca1f2de4661ED88A30C99A7a9449Aa84174";

/// Polymarket real-time Chainlink websocket
const POLYMARKET_WS: &str = "wss://ws-subscriptions-clob.polymarket.com/ws/";

// ── BTC price tracker ─────────────────────────────────────────────────────────

/// Stores the Chainlink BTC/USD price received from the websocket.
#[derive(Debug, Clone, Default)]
struct BtcSignal {
    /// Price at the start of the current 5-min window (resets every 5 min).
    window_start_price: f64,
    /// Which 5-min window slot we are in (0, 5, 10 … 55).
    window_start_minute: u32,
    /// Latest streamed price.
    latest_price: f64,
    /// Raw dollar move from the window open (positive = BTC up).
    dollar_move: f64,
    /// Timestamp of latest update.
    last_update: chrono::DateTime<Utc>,
}

impl BtcSignal {
    /// Called every time a new Chainlink price arrives from the websocket.
    fn update(&mut self, price: f64) {
        let now          = Utc::now();
        let minute       = now.minute();
        let window_start = (minute / 5) * 5;

        if self.window_start_price == 0.0 || window_start != self.window_start_minute {
            // New 5-min window — reset the open price.
            self.window_start_price  = price;
            self.window_start_minute = window_start;
        }

        self.latest_price = price;
        self.last_update  = now;
        self.dollar_move  = price - self.window_start_price;
    }

    /// Seconds remaining in the current 5-min window.
    fn seconds_remaining(&self) -> u64 {
        let now          = Utc::now();
        let minute       = now.minute();
        let second       = now.second();
        let window       = (minute / 5) * 5;
        let elapsed_secs = ((minute - window) * 60 + second) as u64;
        (5 * 60u64).saturating_sub(elapsed_secs)
    }

    /// Minutes remaining in the current 5-min window (rounded down).
    fn minutes_remaining(&self) -> u64 {
        self.seconds_remaining() / 60
    }

    /// Returns Some((Side, label, dollar_move)) when ALL conditions are met:
    ///   1. Live price data is available.
    ///   2. We are in the last MIN..MAX seconds of the window.
    ///   3. BTC has moved at least DOLLAR_TIER_1 ($30) from the window open.
    ///
    /// Direction: positive move → buy YES,  negative move → buy NO.
    fn trade_signal(&self) -> Option<(Side, &'static str, f64)> {
        if self.latest_price == 0.0 { return None; }

        let secs = self.seconds_remaining();
        if secs > MAX_SECONDS_REMAINING || secs < MIN_SECONDS_REMAINING {
            return None;
        }

        let abs_move = self.dollar_move.abs();
        if abs_move < DOLLAR_TIER_1 { return None; }

        if self.dollar_move > 0.0 {
            Some((Side::Buy, "YES", abs_move))
        } else {
            Some((Side::Buy, "NO", abs_move))
        }
    }

    fn tier_label(&self) -> &'static str {
        let abs = self.dollar_move.abs();
        if abs >= DOLLAR_TIER_2      { "Tier 2 ($40+)" }
        else if abs >= DOLLAR_TIER_1 { "Tier 1 ($30+)" }
        else                         { "No tier" }
    }
}

// ── Position tracker ──────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct Position {
    slug:         String,
    side_label:   String,
    token_id:     U256,
    shares:       Decimal,
    /// The CTF condition ID needed to redeem after resolution.
    condition_id: Option<B256>,
    /// Timestamp when the window closed (so we know when to attempt redeem).
    window_closed_at: Option<chrono::DateTime<Utc>>,
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
    let (ws_stream, _) = connect_async(&url).await.context("WS connect failed")?;
    let (mut write, mut read2) = ws_stream.split();
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
                        println!(
                            "[Chainlink] BTC/USD ${:.2} | move ${:+.2} | {}m rem",
                            price,
                            s.btc_signal.dollar_move,
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
                Ok("▶️ Farmer started. Watching Chainlink BTC/USD for 5-min window signals.".into())
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
                    "BTC: <b>${:.2}</b> | Move: <b>${:+.2}</b> | Window rem: <b>{}s</b>",
                    sig.latest_price, sig.dollar_move, sig.seconds_remaining()
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
            let direction = if sig.dollar_move > 0.0 { "📈 UP" } else { "📉 DOWN" };
            let secs = sig.seconds_remaining();
            let tier = sig.tier_label();
            let trade = match sig.trade_signal() {
                Some((_, label, mv)) => format!(
                    "\n🎯 <b>SIGNAL ACTIVE</b>: Buy <b>{label}</b> (${mv:+.2}, {tier}, {secs}s rem)"
                ),
                None => format!(
                    "\n⏸ No signal (need ≥${DOLLAR_TIER_1} move + {MIN_SECONDS_REMAINING}–{MAX_SECONDS_REMAINING}s remaining)"
                ),
            };
            Ok(format!(
                "📡 <b>Chainlink BTC/USD (5-min)</b>\n\
                 Price: <b>${:.2}</b>\n\
                 Window open: <b>${:.2}</b>\n\
                 Move: <b>${:+.2}</b> {direction}\n\
                 Tier: <b>{tier}</b>\n\
                 Seconds remaining: <b>{secs}s</b>\
                 {trade}",
                sig.latest_price,
                sig.window_start_price,
                sig.dollar_move,
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
                let status = if p.window_closed_at.is_some() { "⏳ awaiting redeem" } else { "🔴 live" };
                lines.push(format!(
                    "• <b>{}</b> | {} | shares: {} | {}",
                    slug, p.side_label, p.shares, status,
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
    redeem_ready(bot, state).await;

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
        s.btc_signal.trade_signal().map(|(side, label, dollar_move)| {
            (side, label, dollar_move, s.btc_signal.seconds_remaining())
        })
    };

    if let Some((_side, label, dollar_move, rem)) = signal {
        match try_chainlink_trade(bot, state, label, dollar_move, rem).await {
            Ok(true)  => return Ok(()),
            Ok(false) => {}
            Err(e)    => eprintln!("  [Chainlink trade] {e:#}"),
        }
    }

    Ok(())
}

/// Finds the active BTC 5-min market matching the signal direction,
/// then enters with a flat $1 FOK buy. Returns Ok(true) if a trade was placed.
async fn try_chainlink_trade(
    bot:         &Bot,
    state:       &Shared,
    label:       &str,
    dollar_move: f64,
    rem:         u64,
) -> Result<bool> {
    let size_usd = Decimal::from_str(POSITION_USD).unwrap();
    let tier     = if dollar_move >= DOLLAR_TIER_2 { 2 } else { 1 };

    let gamma = gamma::Client::default();
    let req = EventsRequest::builder()
        .limit(20)
        .maybe_closed(Some(false))
        .maybe_tag_slug(Some("crypto".to_string()))
        .ascending(false)
        .build();
    let events = gamma.events(&req).await?;

    for event in &events {
        let Some(markets) = &event.markets else { continue };
        for market in markets {
            let q    = market.question.as_deref().unwrap_or("").to_lowercase();
            let slug = match &market.slug { Some(s) if !s.is_empty() => s.clone(), _ => continue };

            // Must be a 5-min BTC up/down market
            let is_5min_btc = (q.contains("btc") || q.contains("bitcoin"))
                && (q.contains("up") || q.contains("down") || q.contains("higher") || q.contains("lower"))
                && (q.contains("5m") || q.contains("5-min") || q.contains("5 min") || q.contains("5 minute"));
            if !is_5min_btc { continue; }

            { if state.read().await.positions.contains_key(&slug) { continue; } }

            let tokens = match &market.clob_token_ids {
                Some(t) if t.len() >= 2 => t,
                _ => continue,
            };

            // YES = index 0, NO = index 1
            let (buy_token, buy_label) = if label == "YES" {
                (tokens[0], "YES")
            } else {
                (tokens[1], "NO")
            };

            // Grab condition ID from market for later redeem
            let condition_id = market.condition_id
                .as_deref()
                .and_then(|s| B256::from_str(s).ok());

            let msg = format!(
                "🎯 <b>5-MIN BTC SNIPE</b>\n\
                 Market: <b>{slug}</b>\n\
                 Direction: <b>{buy_label}</b> | Move: <b>${dollar_move:+.2}</b> (Tier {tier})\n\
                 Window: <b>{rem}s</b> remaining | Size: <b>${size_usd}</b>",
            );
            println!("  [SNIPE] {slug} | {buy_label} | ${dollar_move:+.2} tier {tier} | {rem}s rem");
            notify(bot, &msg).await;

            match enter_position(buy_token, size_usd).await {
                Ok(shares) => {
                    notify(bot, &format!(
                        "✅ Sniped: <b>{slug}</b> ({buy_label})\nShares: {shares}"
                    )).await;
                    let pos = Position {
                        slug:             slug.clone(),
                        side_label:       buy_label.to_string(),
                        token_id:         buy_token,
                        shares,
                        condition_id,
                        window_closed_at: None,
                    };
                    state.write().await.positions.insert(slug, pos);
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

/// FOK market buy only. Returns shares received.
async fn enter_position(token_id: U256, size_usd: Decimal) -> Result<Decimal> {
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

    if buy_res.taking_amount <= Decimal::ZERO {
        anyhow::bail!("buy filled but taking_amount is zero");
    }

    Ok(buy_res.taking_amount)
}

/// Marks positions as closed when their window ends, then redeems after
/// REDEEM_DELAY_SECS to give Polymarket time to resolve the market.
async fn redeem_ready(bot: &Bot, state: &Shared) {
    let now  = Utc::now();
    let secs = state.read().await.btc_signal.seconds_remaining();

    // Mark positions as window-closed when a new window just started (secs > 250)
    if secs > 250 {
        let mut s = state.write().await;
        for pos in s.positions.values_mut() {
            if pos.window_closed_at.is_none() {
                pos.window_closed_at = Some(now);
            }
        }
    }

    // Collect positions ready to redeem
    let ready: Vec<String> = {
        let s = state.read().await;
        s.positions.iter()
            .filter(|(_, p)| {
                p.condition_id.is_some() &&
                p.window_closed_at.map(|t| {
                    (now - t).num_seconds() as u64 >= REDEEM_DELAY_SECS
                }).unwrap_or(false)
            })
            .map(|(slug, _)| slug.clone())
            .collect()
    };

    for slug in ready {
        let pos = {
            let mut s = state.write().await;
            s.positions.remove(&slug)
        };
        let Some(pos) = pos else { continue };
        let Some(condition_id) = pos.condition_id else { continue };

        println!("  [REDEEM] Attempting redeem for {slug}");

        match do_redeem(condition_id).await {
            Ok(tx) => {
                notify(bot, &format!(
                    "💰 <b>REDEEMED</b>: <b>{slug}</b> ({})\nTx: <code>{tx}</code>",
                    pos.side_label
                )).await;
            }
            Err(e) => {
                eprintln!("  [REDEEM] Failed for {slug}: {e:#}");
                notify(bot, &format!("⚠️ Redeem failed: <b>{slug}</b>\n{e:#}")).await;
                // Put it back so we retry next cycle
                let mut s = state.write().await;
                s.positions.insert(slug, pos);
            }
        }
    }
}

/// Calls CTF redeem on-chain for a given condition ID.
async fn do_redeem(condition_id: B256) -> Result<String> {
    let key        = resolve_key()?;
    let signer     = LocalSigner::from_str(&key)?.with_chain_id(Some(POLYGON));
    let collateral = Address::from_str(USDC_ADDRESS_STR).unwrap();
    let provider   = alloy::providers::ProviderBuilder::new()
        .wallet(signer)
        .connect("https://polygon.drpc.org")
        .await?;
    let client = ctf::Client::new(provider, POLYGON)?;
    let req = RedeemPositionsRequest::builder()
        .collateral_token(collateral)
        .parent_collection_id(B256::default())
        .condition_id(condition_id)
        .index_sets(vec![U256::from(1u64), U256::from(2u64)])
        .build();
    let resp = client.redeem_positions(&req).await.context("Redeem failed")?;
    Ok(format!("{}", resp.transaction_hash))
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
