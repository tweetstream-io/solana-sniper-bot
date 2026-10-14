//! Solana sniper bot for X/Twitter signals on the TweetStream real-time feed. Modules follow the
//! path of one event: `feed` -> `pipeline` (`detect`, `rules`) -> `filters` -> `execute` -> `land`
//! -> `report`. The same layout as the TypeScript bot in `ts/src`, file for file.

pub mod demo;
pub mod log;
pub mod network;
pub mod pipeline;
pub mod protocol;
pub mod scenario;

/// Detectors: contract addresses and account activity.
pub mod detect {
    pub mod account;
    pub mod addresses;
    pub mod post;
    pub mod store;
    pub mod types;
}

/// Trades: the state machine, the trader, background values, and pump.fun building.
pub mod execute {
    pub mod cache;
    pub mod pump;
    pub mod trade;
    pub mod trades;
}

/// Exits on take profit, stop loss, and hold time.
pub mod exits {
    #[allow(clippy::module_inception)]
    pub mod exits;
}

/// The TweetStream WebSocket and cross-connection dedupe.
pub mod feed {
    pub mod client;
    pub mod dedupe;
}

/// Safety checks and rule filters.
pub mod filters {
    pub mod gate;
}

/// Sending and confirming transactions.
pub mod land {
    #[allow(clippy::module_inception)]
    pub mod land;
}

/// Stage timings.
pub mod report {
    pub mod latency;
}

/// Rule matching and the limits that need memory.
pub mod rules {
    pub mod engine;
    pub mod index;
    pub mod modes;
}

/// Environment and config loading.
pub mod setup {
    pub mod config;
    pub mod env;
}
