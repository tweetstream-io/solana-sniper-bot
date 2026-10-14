//! Prices open positions every `exits.pollMs` and sells on take profit, stop loss, or max hold
//! time. Delete, caller, and author exits come from `pipeline.rs` through `Trader::exit` instead.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use solana_pubkey::Pubkey;
use tokio::task::JoinHandle;

use crate::execute::cache::ChainCache;
use crate::execute::pump::{bonding_curve_address, curve_fee_bps, lamports_for_tokens, parse_curve};
use crate::execute::trade::Trader;
use crate::execute::trades::{Position, PositionRoute, TradeBook};
use crate::log::{log, short};
use crate::network::{NetResult, Network};
use crate::setup::config::Config;

/// Polls open positions and sells the ones that reached an exit.
pub struct ExitWatcher<N> {
    config: Arc<Config>,
    book: Arc<TradeBook>,
    network: Arc<N>,
    cache: Arc<ChainCache>,
    trader: Arc<Trader<N>>,
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
        ExitWatcher { config, book, network, cache, trader }
    }

    /// Starts polling; abort the returned handle to stop. The trade book allows one sell per
    /// position, so overlapping polls cannot sell twice.
    pub fn start(self: Arc<Self>) -> JoinHandle<()> {
        tokio::spawn(async move {
            let period = Duration::from_millis(self.config.exits.poll_ms.max(1));
            let mut ticker = tokio::time::interval_at(tokio::time::Instant::now() + period, period);
            loop {
                ticker.tick().await;
                for position in self.book.positions() {
                    if position.sell_trade_id.is_some() {
                        continue;
                    }
                    let watcher = Arc::clone(&self);
                    drop(tokio::spawn(async move {
                        match watcher.check(&position).await {
                            Ok(Some(reason)) => {
                                watcher.trader.sell(&position.mint, &reason).await;
                            }
                            Ok(None) => {}
                            Err(error) => log("exits", &format!("{}: {error}", short(&position.mint))),
                        }
                    }));
                }
            }
        })
    }

    async fn check(&self, position: &Position) -> NetResult<Option<String>> {
        let exits = &self.config.exits;
        let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64);
        if let Some(max_hold) = exits.max_hold_sec
            && (now - position.opened_at) as f64 >= max_hold * 1000.0
        {
            return Ok(Some(format!("held {max_hold} s")));
        }
        let Some(tokens) = position.token_amount.as_deref().and_then(|t| t.parse::<u64>().ok()) else {
            return Ok(None);
        };
        if exits.take_profit_pct.is_none() && exits.stop_loss_pct.is_none() {
            return Ok(None);
        }
        let Some(value) = self.value_lamports(position, tokens).await? else {
            return Ok(None);
        };
        let pnl_pct = (value as f64 / position.cost_lamports as f64 - 1.0) * 100.0;
        if exits.take_profit_pct.is_some_and(|tp| pnl_pct >= tp) {
            return Ok(Some(format!("take profit at {pnl_pct:.1}%")));
        }
        if exits.stop_loss_pct.is_some_and(|sl| pnl_pct <= -sl) {
            return Ok(Some(format!("stop loss at {pnl_pct:.1}%")));
        }
        Ok(None)
    }

    /// A curve position is priced from one account read, after pump.fun's fee; anything else from a
    /// Jupiter quote.
    async fn value_lamports(&self, position: &Position, tokens: u64) -> NetResult<Option<u64>> {
        if let (PositionRoute::Pump, Some(tiers), Ok(mint)) = (position.route, self.cache.fee_tiers(), position.mint.parse::<Pubkey>()) {
            let accounts = self.network.get_accounts(&[bonding_curve_address(&mint).to_string()]).await?;
            if let Some(curve) = accounts.into_iter().next().flatten().and_then(|a| parse_curve(&a.data)).filter(|c| !c.complete) {
                return Ok(Some(lamports_for_tokens(&curve, tokens, curve_fee_bps(&tiers, &curve))));
            }
        }
        self.network.quote_sell_lamports(&position.mint, tokens).await
    }
}
