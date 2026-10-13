// One TweetStream WebSocket connection: authenticate, keep alive, reconnect with jittered backoff,
// and stamp each frame on receipt. Run one per API key (standard, and Ultra Speed if you have it).
// Next: feed/dedupe.ts drops the second copy, then pipeline.ts routes the event.

import WebSocket from "ws";
import type { Envelope } from "../protocol.ts";
import { log } from "../log.ts";

export type Receipt = {
  connection: string;
  wallMs: number; // Date.now(): comparable with post publish times
  monoNs: bigint; // process.hrtime.bigint(): for timing our own stages
};

export type FeedOptions = {
  name: string; // "standard" or "ultra"
  apiKey: string;
  url: string;
  heartbeatMs: number;
  onEvent: (event: Envelope, receipt: Receipt) => void;
  onFatal: (reason: string) => void;
};

const DASHBOARD = "https://tweetstream.io/dashboard?tab=api";

// Exponential backoff with jitter, so many clients do not reconnect in lockstep after an outage.
export function backoffMs(attempt: number, random = Math.random()): number {
  const base = Math.min(30_000, 500 * 2 ** attempt);
  return Math.floor(base / 2 + random * (base / 2));
}

export class FeedClient {
  private attempt = 0;
  private stopped = false;
  private socket: WebSocket | null = null;
  private heartbeat: NodeJS.Timeout | null = null;
  private retry: NodeJS.Timeout | null = null;
  private lastFrameAt = 0;

  private readonly opts: FeedOptions;

  constructor(opts: FeedOptions) {
    this.opts = opts;
  }

  start(): void {
    this.connect();
  }

  stop(): void {
    this.stopped = true;
    if (this.heartbeat) clearInterval(this.heartbeat);
    if (this.retry) clearTimeout(this.retry);
    this.socket?.close(1000, "client shutdown");
  }

  private connect(): void {
    const { name, apiKey, url } = this.opts;
    // The key travels as a subprotocol, so it never appears in a URL or a proxy log.
    // Compression is off: inflating every frame costs CPU on the hot path for small JSON.
    const socket = new WebSocket(url, ["tweetstream.v1", `tweetstream.auth.token.${apiKey}`], { perMessageDeflate: false });
    this.socket = socket;
    let openedAt: number | null = null;
    let settled = false;

    // Nagle's algorithm would hold our small auth_ping frames; ws sets TCP_NODELAY too, this
    // makes it explicit.
    socket.on("upgrade", (response) => response.socket.setNoDelay(true));
    socket.on("open", () => {
      openedAt = Date.now();
      this.lastFrameAt = openedAt;
      log("connected", `${name} ${url}`);
      socket.send('{"op":"auth_ping"}'); // answered by control/auth_pong
      this.startHeartbeat(socket);
    });

    socket.on("message", (data) => {
      // Stamp before parsing, so publish -> receipt excludes our own work.
      const receipt: Receipt = { connection: name, wallMs: Date.now(), monoNs: process.hrtime.bigint() };
      this.lastFrameAt = receipt.wallMs;
      let event: Partial<Envelope> | null;
      try {
        event = JSON.parse(data.toString());
      } catch {
        log("skip", `${name}: frame is not JSON`);
        return;
      }
      if (event?.v !== 1 || typeof event.t !== "string" || typeof event.op !== "string") return;
      this.opts.onEvent(event as Envelope, receipt);
    });

    socket.on("unexpected-response", (_request, response) => {
      openedAt = null;
      log("refused", `${name}: handshake answered HTTP ${response.statusCode}`);
      if (response.statusCode === 401) return this.opts.onFatal(`${name} API key rejected (401). Copy a current key from ${DASHBOARD}`);
      if (response.statusCode === 403) return this.opts.onFatal(`${name} key cannot stream (403). Start a trial or renew your plan.`);
      if (response.statusCode === 429) log("limit", `${name}: every connection allowed for this key is in use; close another client`);
      reconnect(`HTTP ${response.statusCode}`);
    });

    const reconnect = (reason: string) => {
      if (settled || this.stopped) return;
      settled = true;
      if (this.heartbeat) clearInterval(this.heartbeat);
      if (openedAt !== null && Date.now() - openedAt >= 30_000) this.attempt = 0; // a healthy session resets backoff
      const delay = backoffMs(this.attempt);
      this.attempt = Math.min(this.attempt + 1, 6);
      log("closed", `${name}: ${reason}; reconnecting in ${(delay / 1000).toFixed(1)} s`);
      this.retry = setTimeout(() => this.connect(), delay);
    };
    socket.on("error", (error) => reconnect(error.message));
    socket.on("close", (code) => reconnect(`code ${code}`));
  }

  // auth_ping on a timer doubles as a liveness check: a half-open TCP connection after a network
  // change goes silent, and waiting for the OS to notice can take minutes.
  private startHeartbeat(socket: WebSocket): void {
    const every = this.opts.heartbeatMs;
    if (this.heartbeat) clearInterval(this.heartbeat);
    this.heartbeat = setInterval(() => {
      if (Date.now() - this.lastFrameAt > every * 2 + 5_000) {
        log("stale", `${this.opts.name}: no frames for ${Math.round((every * 2 + 5_000) / 1000)} s`);
        socket.terminate();
        return;
      }
      if (socket.readyState === WebSocket.OPEN) socket.send('{"op":"auth_ping"}');
    }, every);
  }
}
