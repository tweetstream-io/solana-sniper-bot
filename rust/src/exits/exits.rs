//! Watches open positions every `exits.pollMs`: carries out pending exits, then records take
//! profit, stop loss, or max hold time as a pending exit and sells. An exit stays on the position
//! until a sell of it confirms.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use solana_pubkey::Pubkey;
use tokio::task::JoinHandle;

use crate::execute::cache::ChainCache;
use crate::execute::pump::{bonding_curve_address, curve_fee_bps, lamports_for_tokens, parse_curve};
use crate::execute::trade::Trader;
use crate::execute::trades::{ExitReason, PendingExit, Position, TradeBook};
use crate::log::{log, short, unix_ms};
use crate::network::Network;
use crate::setup::config::Config;

/// Polls open positions and sells the ones that reached an exit.
pub struct ExitWatcher<N> {
    config: Arc<Config>,
    book: Arc<TradeBook>,
    network: Arc<N>,
    cache: Arc<ChainCache>,
    trader: Arc<Trader<N>>,
    unpriced: Mutex<HashSet<String>>,
}

impl<N: Network> ExitWatcher<N> {
    /// Creates the watcher.
    pub fn new(
        config: Arc<Config>,
        book: Arc<TradeBook>,
        network: Arc<N>,
        cache: Arc<ChainCache>,
        trader: Arc<Trader<N>>,
    ) -> ExitWatcher<N> {
        ExitWatcher { config, book, network, cache, trader, unpriced: Mutex::default() }
    }

    /// Starts polling; abort the returned handle to stop. The trade book allows one trade per mint
    /// at a time, so overlapping polls cannot sell twice.
    pub fn start(self: Arc<Self>) -> JoinHandle<()> {
        tokio::spawn(async move {
            let period = Duration::from_millis(self.config.exits.poll_ms.max(1));
            let mut ticker = tokio::time::interval_at(tokio::time::Instant::now() + period, period);
            loop {
                ticker.tick().await;
                for position in self.book.positions() {
                    if self.book.is_busy(&position.mint) {
                        continue;
                    }
                    let watcher = Arc::clone(&self);
                    drop(tokio::spawn(async move {
                        if !position.pending_exits.is_empty() {
                            return watcher.trader.run_exits(&position.mint).await;
                        }
                        if let Some((code, why)) = watcher.check(&position).await {
                            log("exit", &format!("{}: {why}", short(&position.mint)));
                            watcher.trader.exit(&position.mint, PendingExit { code, at: unix_ms() }).await;
                        }
                    }));
                }
            }
        })
    }

    async fn check(&self, position: &Position) -> Option<(ExitReason, String)> {
        let exits = &self.config.exits;
        if let Some(max_hold) = exits.max_hold_sec
            && (unix_ms() - position.opened_at) as f64 >= max_hold * 1000.0
        {
            return Some((ExitReason::MaxHold, format!("held {max_hold} s")));
        }
        let tokens = position.token_amount?;
        if exits.take_profit_pct.is_none() && exits.stop_loss_pct.is_none() {
            return None;
        }
        let Some(value) = self.value_lamports(position, tokens).await else {
            let mut unpriced = self.unpriced.lock().unwrap_or_else(|p| p.into_inner());
            if unpriced.insert(position.mint.clone()) {
                log(
                    "exits",
                    &format!("{}: no price, so take profit and stop loss are skipped; the time limit still applies", short(&position.mint)),
                );
            }
            return None;
        };
        self.unpriced.lock().unwrap_or_else(|p| p.into_inner()).remove(&position.mint);
        let pnl_pct = (value as f64 / position.cost_lamports as f64 - 1.0) * 100.0;
        if exits.take_profit_pct.is_some_and(|tp| pnl_pct >= tp) {
            return Some((ExitReason::TakeProfit, format!("take profit at {pnl_pct:.1}%")));
        }
        if exits.stop_loss_pct.is_some_and(|sl| pnl_pct <= -sl) {
            return Some((ExitReason::StopLoss, format!("stop loss at {pnl_pct:.1}%")));
        }
        None
    }

    /// A position on a live curve is priced from one account read, after pump.fun's fee; a
    /// graduated one from a Jupiter quote to SOL.
    async fn value_lamports(&self, position: &Position, tokens: u64) -> Option<u64> {
        if let (Some(tiers), Ok(mint)) = (self.cache.fee_tiers(), position.mint.parse::<Pubkey>()) {
            let accounts = self.network.get_accounts(&[bonding_curve_address(&mint).to_string()]).await.unwrap_or_default();
            if let Some(curve) = accounts.into_iter().next().flatten().and_then(|a| parse_curve(&a.data)).filter(|c| !c.complete) {
                return Some(lamports_for_tokens(&curve, tokens, curve_fee_bps(&tiers, &curve)));
            }
        }
        self.network.quote_sell_lamports(&position.mint, tokens).await.ok().flatten()
    }
}
