// Prices open positions every exits.pollMs and sells on take profit, stop loss, or max hold time.
// Delete, caller, and author exits come from pipeline.ts through Trader.exit instead.

import type { Config } from "../setup/config.ts";
import type { Position, TradeBook } from "../execute/trades.ts";
import type { Network } from "../network.ts";
import type { Trader } from "../execute/trade.ts";
import type { ChainCache } from "../execute/cache.ts";
import { bondingCurveAddress, curveFeeBps, lamportsForTokens, parseCurve } from "../execute/pump.ts";
import { log, short } from "../log.ts";

export class ExitWatcher {
  private timer: NodeJS.Timeout | null = null;
  private readonly config: Config;
  private readonly book: TradeBook;
  private readonly network: Network;
  private readonly cache: ChainCache;
  private readonly trader: Trader;

  constructor(config: Config, book: TradeBook, network: Network, cache: ChainCache, trader: Trader) {
    this.config = config;
    this.book = book;
    this.network = network;
    this.cache = cache;
    this.trader = trader;
  }

  start(): void {
    this.timer = setInterval(() => {
      // The trade book allows one sell per position, so overlapping polls cannot double-sell.
      for (const p of this.book.positions().filter((p) => !p.sellTradeId)) {
        this.check(p)
          .then((reason) => (reason ? this.trader.sell(p.mint, reason) : undefined))
          .catch((error: Error) => log("exits", `${short(p.mint)}: ${error.message}`));
      }
    }, this.config.exits.pollMs);
  }

  stop(): void {
    if (this.timer) clearInterval(this.timer);
  }

  private async check(p: Position): Promise<string | null> {
    const { takeProfitPct, stopLossPct, maxHoldSec } = this.config.exits;
    if (maxHoldSec !== null && Date.now() - p.openedAt >= maxHoldSec * 1000) return `held ${maxHoldSec} s`;
    if (p.tokenAmount === null || (takeProfitPct === null && stopLossPct === null)) return null;
    const value = await this.valueLamports(p, BigInt(p.tokenAmount));
    if (value === null) return null; // no price: wait for the next poll or the time limit
    const pnlPct = (Number(value) / Number(p.costLamports) - 1) * 100;
    if (takeProfitPct !== null && pnlPct >= takeProfitPct) return `take profit at ${pnlPct.toFixed(1)}%`;
    if (stopLossPct !== null && pnlPct <= -stopLossPct) return `stop loss at ${pnlPct.toFixed(1)}%`;
    return null;
  }

  // A curve position is priced from one account read, after pump.fun's fee; anything else from a
  // Jupiter quote.
  private async valueLamports(p: Position, tokens: bigint): Promise<bigint | null> {
    if (p.route === "pump" && this.cache.feeTiers) {
      const [account] = await this.network.getAccounts([bondingCurveAddress(p.mint).toBase58()]);
      const curve = account ? parseCurve(account.data) : null;
      if (curve && !curve.complete) return lamportsForTokens(curve, tokens, curveFeeBps(this.cache.feeTiers, curve));
    }
    return this.network.quoteSellLamports(p.mint, tokens);
  }
}
