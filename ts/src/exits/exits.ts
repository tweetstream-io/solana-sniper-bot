// Prices open positions every exits.pollMs and sells on take profit, stop loss, or max hold time.
// Delete, caller, and author exits come from pipeline.ts instead and sell at once.

import type { Config } from "../setup/config.ts";
import type { Ledger, Position } from "../rules/ledger.ts";
import type { Network } from "../network.ts";
import type { Trader } from "../execute/trade.ts";
import { bondingCurveAddress, lamportsForTokens, parseCurve } from "../execute/pump.ts";
import { log, short } from "../log.ts";

export class ExitWatcher {
  private timer: NodeJS.Timeout | null = null;

  private readonly config: Config;
  private readonly ledger: Ledger;
  private readonly network: Network;
  private readonly trader: Trader;

  constructor(config: Config, ledger: Ledger, network: Network, trader: Trader) {
    this.config = config;
    this.ledger = ledger;
    this.network = network;
    this.trader = trader;
  }

  start(): void {
    this.timer = setInterval(() => {
      for (const p of this.ledger.positions()) {
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

  // A curve position is priced from one account read; anything else from a Jupiter quote.
  private async valueLamports(p: Position, tokens: bigint): Promise<bigint | null> {
    if (p.route === "pump") {
      const [account] = await this.network.getAccounts([bondingCurveAddress(p.mint).toBase58()]);
      const curve = account ? parseCurve(account.data) : null;
      if (curve && !curve.complete) return lamportsForTokens(curve, tokens);
    }
    return this.network.quoteSellLamports(p.mint, tokens);
  }
}
