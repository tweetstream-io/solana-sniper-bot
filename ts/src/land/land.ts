// Sends one signed transaction to every sender at once, rebroadcasts it until it confirms or its
// blockhash expires, and learns the outcome from a signatureSubscribe WebSocket instead of polling.
// Live mode only. Next: report/latency.ts records the stage times.

import WebSocket from "ws";
import bs58 from "bs58";
import type { VersionedTransaction } from "@solana/web3.js";
import { rpcCall } from "../network.ts";
import { log } from "../log.ts";
import { backoffMs } from "../feed/client.ts";

// Jito's /api/v1/transactions takes only the encoding option and always skips preflight.
// RPCs get skipPreflight (saves a simulation) and maxRetries 0, because we rebroadcast ourselves.
// needsTip: a sender that only accepts transactions paying it a tip (configured tipAccounts).
export type SenderTarget = { name: string; url: string; kind: "rpc" | "jito"; needsTip?: boolean };

export type Landing = {
  signature: string;
  status: "confirmed" | "failed" | "unknown";
  error?: string;
  sentNs: bigint;
  firstAckNs?: bigint;
  firstAckBy?: string;
  confirmedNs?: bigint;
};

type LanderOptions = { rebroadcastMs: number; confirmTimeoutMs: number; currentBlockHeight: () => number | null; statusRpcUrl: string | null };

export class Lander {
  private readonly watcher: SignatureWatcher;
  private readonly opts: LanderOptions;

  constructor(watcher: SignatureWatcher, opts: LanderOptions) {
    this.watcher = watcher;
    this.opts = opts;
  }

  // Sends to `targets`, which execute/trade.ts picks per transaction (antiMev, tips).
  async land(tx: VersionedTransaction, lastValidBlockHeight: number, targets: SenderTarget[]): Promise<Landing> {
    const signature = bs58.encode(tx.signatures[0]!);
    const base64 = Buffer.from(tx.serialize()).toString("base64");
    // Subscribe before sending, so a fast confirmation cannot arrive before we listen.
    const outcome = this.watcher.wait(signature, this.opts.confirmTimeoutMs);
    const sentNs = process.hrtime.bigint();
    const result: Landing = { signature, status: "unknown", sentNs };
    const first = this.fanOut(base64, targets).then((by) => {
      result.firstAckNs = process.hrtime.bigint();
      result.firstAckBy = by;
    });
    first.catch((error: Error) => log("send", `no sender accepted ${signature.slice(0, 8)}: ${error.message}`));

    const rebroadcast = setInterval(() => {
      const height = this.opts.currentBlockHeight();
      if (height !== null && height > lastValidBlockHeight) return clearInterval(rebroadcast);
      this.fanOut(base64, targets).catch(() => {});
    }, this.opts.rebroadcastMs);
    try {
      const final = await outcome;
      Object.assign(result, final.status === "unknown" ? await this.pollOnce(signature, final) : final);
      if (result.status !== "unknown") result.confirmedNs = process.hrtime.bigint();
    } finally {
      clearInterval(rebroadcast);
    }
    return result;
  }

  // The reconciler sends a stored transaction again while its blockhash is valid.
  async rebroadcast(base64: string, targets: SenderTarget[]): Promise<void> {
    if (targets.length) await this.fanOut(base64, targets).catch(() => {});
  }

  // If the subscription never answered (the RPC WebSocket was down), ask once over HTTP.
  private async pollOnce(signature: string, fallback: Outcome): Promise<Outcome> {
    const rpc = this.opts.statusRpcUrl;
    if (!rpc) return fallback;
    type Status = { value: Array<{ confirmationStatus?: string; err: unknown } | null> };
    const status = await rpcCall<Status>(rpc, "getSignatureStatuses", [[signature]]).catch(() => null);
    const value = status?.value[0];
    if (!value?.confirmationStatus || value.confirmationStatus === "processed") return fallback;
    return value.err ? { status: "failed", error: JSON.stringify(value.err) } : { status: "confirmed" };
  }

  // Resolves with the name of the first sender that accepted the transaction.
  private fanOut(base64: string, targets: SenderTarget[]): Promise<string> {
    const errors: string[] = [];
    return Promise.any(
      targets.map(async (s) => {
        const options = s.kind === "jito" ? { encoding: "base64" } : { encoding: "base64", skipPreflight: true, maxRetries: 0 };
        try {
          await rpcCall<string>(s.url, "sendTransaction", [base64, options], AbortSignal.timeout(5_000));
          return s.name;
        } catch (error) {
          errors.push(`${s.name}: ${(error as Error).message}`);
          throw error;
        }
      }),
    ).catch(() => {
      throw new Error(errors.join("; "));
    });
  }
}

type Outcome = { status: "confirmed" | "failed" | "unknown"; error?: string };

// One long-lived Solana PubSub connection. A confirmation arrives as a push the moment the RPC
// sees it, where polling getSignatureStatuses would add up to one poll interval.
export class SignatureWatcher {
  private socket: WebSocket | null = null;
  private attempt = 0;
  private nextId = 1;
  private readonly pending = new Map<string, (o: Outcome) => void>(); // signature -> resolver
  private readonly requests = new Map<number, string>(); // request id -> signature
  private readonly subscriptions = new Map<number, string>(); // subscription id -> signature

  private stopped = false;

  private readonly url: string;

  constructor(url: string) {
    this.url = url;
  }

  stop(): void {
    this.stopped = true;
    this.socket?.close();
  }

  start(): void {
    if (this.stopped) return;
    const socket = new WebSocket(this.url, { perMessageDeflate: false });
    this.socket = socket;
    socket.on("open", () => {
      this.attempt = 0;
      for (const signature of this.pending.keys()) this.subscribe(signature);
    });
    socket.on("message", (data) => this.onMessage(data.toString()));
    socket.on("error", () => {});
    socket.on("close", () => {
      this.subscriptions.clear();
      this.requests.clear();
      setTimeout(() => this.start(), backoffMs(this.attempt++));
    });
  }

  wait(signature: string, timeoutMs: number): Promise<Outcome> {
    return new Promise((resolve) => {
      const timer = setTimeout(() => finish({ status: "unknown", error: `not confirmed within ${timeoutMs} ms` }), timeoutMs);
      const finish = (o: Outcome) => {
        clearTimeout(timer);
        this.pending.delete(signature);
        resolve(o);
      };
      this.pending.set(signature, finish);
      this.subscribe(signature);
    });
  }

  private subscribe(signature: string): void {
    if (this.socket?.readyState !== WebSocket.OPEN) return; // subscribed on (re)connect
    const id = this.nextId++;
    this.requests.set(id, signature);
    this.socket.send(JSON.stringify({ jsonrpc: "2.0", id, method: "signatureSubscribe", params: [signature, { commitment: "confirmed" }] }));
  }

  private onMessage(text: string): void {
    let msg: {
      id?: number;
      result?: number;
      method?: string;
      params?: { subscription: number; result: { value: { err: unknown } } };
    };
    try {
      msg = JSON.parse(text);
    } catch {
      return log("confirm", "RPC WebSocket sent a frame that is not JSON");
    }
    if (msg.id !== undefined && typeof msg.result === "number") {
      const signature = this.requests.get(msg.id);
      if (signature) this.subscriptions.set(msg.result, signature);
      this.requests.delete(msg.id);
      return;
    }
    if (msg.method !== "signatureNotification" || !msg.params) return;
    const signature = this.subscriptions.get(msg.params.subscription);
    this.subscriptions.delete(msg.params.subscription);
    const err = msg.params.result.value.err;
    if (signature) this.pending.get(signature)?.(err ? { status: "failed", error: JSON.stringify(err) } : { status: "confirmed" });
  }
}
