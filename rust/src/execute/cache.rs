//! Values the buy path needs, refreshed in the background so it never waits for them: the latest
//! blockhash, the current block height and epoch, priority fee and Jito tip levels, the SOL price,
//! and pump.fun's fee tiers. Read by `execute/trade.rs`, `filters/gate.rs`, and `land/land.rs`.

use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use tokio::task::JoinHandle;

use crate::execute::pump::{FeeTier, fee_config, parse_fee_config};
use crate::log::log;
use crate::network::{Blockhash, EpochInfo, NetResult, Network};

/// A blockhash older than this is not used: the cache has stopped refreshing, and a transaction
/// built on it may already be close to expiry.
const MAX_BLOCKHASH_AGE: Duration = Duration::from_secs(10);

#[derive(Debug, Default)]
struct Values {
    blockhash: Option<(Blockhash, Instant)>,
    height: Option<EpochInfo>,
    priority_micro_lamports: Option<u64>,
    tip_floor_sol: Option<f64>,
    sol_usd: Option<f64>,
    fee_tiers: Option<Vec<FeeTier>>,
}

/// The latest background values, readable from any task.
#[derive(Debug, Default)]
pub struct ChainCache {
    values: Mutex<Values>,
}

impl ChainCache {
    fn values(&self) -> MutexGuard<'_, Values> {
        self.values.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Returns the latest blockhash if it is at most 10 seconds old.
    pub fn fresh_blockhash(&self) -> Option<Blockhash> {
        let values = self.values();
        let (blockhash, fetched) = values.blockhash.as_ref()?;
        (fetched.elapsed() <= MAX_BLOCKHASH_AGE).then(|| blockhash.clone())
    }

    /// Returns the current block height.
    pub fn block_height(&self) -> Option<u64> {
        self.values().height.map(|h| h.block_height)
    }

    /// Returns the current epoch.
    pub fn epoch(&self) -> Option<u64> {
        self.values().height.map(|h| h.epoch)
    }

    /// Returns the 75th percentile of recent nonzero priority fees, per compute unit.
    pub fn priority_micro_lamports(&self) -> Option<u64> {
        self.values().priority_micro_lamports
    }

    /// Returns the recent landed Jito tip level in SOL.
    pub fn tip_floor_sol(&self) -> Option<f64> {
        self.values().tip_floor_sol
    }

    /// Returns the SOL price in USD.
    pub fn sol_usd(&self) -> Option<f64> {
        self.values().sol_usd
    }

    /// Returns pump.fun's fee tiers.
    pub fn fee_tiers(&self) -> Option<Vec<FeeTier>> {
        self.values().fee_tiers.clone()
    }

    /// Waits until the blockhash and the block height have had a chance to load (at most 5 s), so a
    /// fresh start does not build on nothing.
    pub async fn ready(&self) {
        for _ in 0..50 {
            if self.fresh_blockhash().is_some() && self.block_height().is_some() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// Starts the refresh loops; abort the returned handles to stop them.
    pub fn start<N: Network>(self: &Arc<Self>, network: Arc<N>, blockhash_refresh: Duration) -> Vec<JoinHandle<()>> {
        let refresh = |name: &'static str, period: Duration, task: fn(Arc<ChainCache>, Arc<N>) -> Refresh| {
            let (cache, net) = (Arc::clone(self), Arc::clone(&network));
            every(name, period, move || task(Arc::clone(&cache), Arc::clone(&net)))
        };
        vec![
            refresh("blockhash", blockhash_refresh, |cache, net| {
                Box::pin(async move {
                    let (hash, info) = tokio::join!(net.latest_blockhash(), net.epoch_info());
                    let mut values = cache.values();
                    values.blockhash = Some((hash?, Instant::now()));
                    values.height = Some(info?);
                    Ok(())
                })
            }),
            refresh("priority fees", Duration::from_secs(2), |cache, net| {
                Box::pin(async move {
                    let mut fees = Vec::new();
                    for fee in net.recent_priority_fees().await? {
                        if fee > 0 {
                            fees.push(fee);
                        }
                    }
                    fees.sort_unstable();
                    if let Some(fee) = fees.get(fees.len() * 3 / 4) {
                        cache.values().priority_micro_lamports = Some(*fee);
                    }
                    Ok(())
                })
            }),
            refresh("tip floor", Duration::from_secs(10), |cache, net| {
                Box::pin(async move {
                    let tip = net.jito_tip_floor_sol().await?;
                    cache.values().tip_floor_sol = tip;
                    Ok(())
                })
            }),
            refresh("sol price", Duration::from_secs(30), |cache, net| {
                Box::pin(async move {
                    let price = net.sol_usd_price().await?;
                    cache.values().sol_usd = price;
                    Ok(())
                })
            }),
            refresh("fee tiers", Duration::from_secs(60), |cache, net| {
                Box::pin(async move {
                    let accounts = net.get_accounts(&[fee_config().to_string()]).await?;
                    let tiers = accounts.into_iter().next().flatten().and_then(|a| parse_fee_config(&a.data));
                    cache.values().fee_tiers = tiers;
                    Ok(())
                })
            }),
        ]
    }
}

type Refresh = std::pin::Pin<Box<dyn Future<Output = NetResult<()>> + Send>>;

/// Runs `task` now and then every `period`, logging an outage once instead of every period.
fn every<F>(name: &'static str, period: Duration, task: F) -> JoinHandle<()>
where
    F: Fn() -> Refresh + Send + 'static,
{
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(period);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut last_error = String::new();
        loop {
            ticker.tick().await;
            match task().await {
                Ok(()) => last_error.clear(),
                Err(error) if error.0 != last_error => {
                    log("cache", &format!("{name}: {error}"));
                    last_error = error.0;
                }
                Err(_) => {}
            }
        }
    })
}
