// Prices open positions every exits.pollMs and sells on take profit, stop loss, or max hold time.
// Each exit goes through Trader.exit, like a delete exit from pipeline.ts, and stays on the position
// until a sell confirms.

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
      // An exit is kept on the position until a sell confirms; the trade book allows one sell at a time.
      for (const [mint, p] of this.book.positions()) {
        if (p.pendingExits.length) continue; // already exiting; the reconciler retries the sell
        this.check(mint, p)
          .then((exit) => {
            if (!exit) return;
            log("exit", `${short(mint)}: ${exit.detail}`);
            return this.trader.exit(mint, exit.code);
          })
          .catch((error: Error) => log("exits", `${short(mint)}: ${error.message}`));
      }
    }, this.config.exits.pollMs);
  }

  stop(): void {
    if (this.timer) clearInterval(this.timer);
  }

  private async check(mint: string, p: Position): Promise<{ code: string; detail: string } | null> {
    const { takeProfitPct, stopLossPct, maxHoldSec } = this.config.exits;
    if (maxHoldSec !== null && Date.now() - p.openedAt >= maxHoldSec * 1000) return { code: "max_hold", detail: `held ${maxHoldSec} s` };
    if (p.tokenAmount === null || (takeProfitPct === null && stopLossPct === null)) return null;
    const value = await this.valueLamports(mint, BigInt(p.tokenAmount));
    if (value === null) return null; // no price: wait for the next poll or the time limit
    const pnlPct = (Number(value) / Number(p.costLamports) - 1) * 100;
    if (takeProfitPct !== null && pnlPct >= takeProfitPct) return { code: "take_profit", detail: `take profit at ${pnlPct.toFixed(1)}%` };
    if (stopLossPct !== null && pnlPct <= -stopLossPct) return { code: "stop_loss", detail: `stop loss at ${pnlPct.toFixed(1)}%` };
    return null;
  }

  // A position on a live pump.fun curve is priced from one account read, after pump.fun's fee;
  // anything else from a Jupiter quote to SOL.
  private async valueLamports(mint: string, tokens: bigint): Promise<bigint | null> {
    const tiers = this.cache.feeTiers();
    const [account] = await this.network.getAccounts([bondingCurveAddress(mint).toBase58()]).catch(() => [null]);
    const curve = account ? parseCurve(account.data) : null;
    if (tiers && curve && !curve.complete && curve.solPaired) return lamportsForTokens(curve, tokens, curveFeeBps(tiers, curve));
    return this.network.quoteSellLamports(mint, tokens).catch(() => null);
  }
}
