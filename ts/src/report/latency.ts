// Per-signal stage timings and the summary printed on exit. Stages, in order:
//   publish->receipt   post publish time (from the X id) to WebSocket receipt; wall clocks, so it
//                      includes clock skew between X and this machine
//   receipt->decision  parse, dedupe, detect, rule match, reservation; pure CPU
//   decision->signed   safety read and filters, transaction build, signing
//   build+sign         the part of decision->signed after the safety read returned
//   signed->first ack  first sender that accepted the transaction
//   send->confirmed    first send to confirmation over the RPC WebSocket
// Every stage except the first uses process.hrtime, which never jumps.

import { log } from "../log.ts";

export type Trace = {
  mint: string;
  rule: string;
  connection: string;
  eventRef: string;
  publishedAtWall: number | null;
  receivedWall: number;
  receivedNs: bigint;
  decidedNs?: bigint;
  gateMs?: number;
  gateDoneNs?: bigint;
  signedNs?: bigint;
  sentNs?: bigint;
  firstAckNs?: bigint;
  firstAckBy?: string;
  confirmedNs?: bigint;
};

const STAGES = ["publish->receipt", "receipt->decision", "safety read", "build+sign", "decision->signed", "signed->first ack", "send->confirmed"] as const;
type Stage = (typeof STAGES)[number];

const ms = (from: bigint, to: bigint) => Number(to - from) / 1e6;
const round = (value: number) => Math.round(value * 1000) / 1000;

export function percentile(sorted: number[], p: number): number | undefined {
  return sorted.length ? sorted[Math.max(0, Math.ceil(p * sorted.length) - 1)] : undefined;
}

export class LatencyReport {
  private readonly stages = new Map<Stage, number[]>();
  private readonly feed = new Map<string, number[]>(); // connection -> publish->receipt of new posts
  private readonly firstCopies = new Map<string, number>(); // connection -> events it delivered first
  private readonly firstAcks = new Map<string, number>(); // sender -> times it acked first

  recordPost(connection: string, publishToReceiptMs: number): void {
    const list = this.feed.get(connection) ?? [];
    list.push(publishToReceiptMs);
    if (list.length > 10_000) list.shift();
    this.feed.set(connection, list);
  }

  recordFirstCopy(connection: string): void {
    this.firstCopies.set(connection, (this.firstCopies.get(connection) ?? 0) + 1);
  }

  finish(t: Trace, outcome: string): void {
    const values: Partial<Record<Stage, number>> = {};
    if (t.publishedAtWall !== null) values["publish->receipt"] = t.receivedWall - t.publishedAtWall;
    if (t.decidedNs !== undefined) values["receipt->decision"] = round(ms(t.receivedNs, t.decidedNs));
    if (t.gateMs !== undefined) values["safety read"] = round(t.gateMs);
    if (t.signedNs !== undefined && t.gateDoneNs !== undefined) values["build+sign"] = round(ms(t.gateDoneNs, t.signedNs));
    if (t.signedNs !== undefined && t.decidedNs !== undefined) values["decision->signed"] = round(ms(t.decidedNs, t.signedNs));
    if (t.firstAckNs !== undefined && t.signedNs !== undefined) values["signed->first ack"] = round(ms(t.signedNs, t.firstAckNs));
    if (t.confirmedNs !== undefined && t.sentNs !== undefined) values["send->confirmed"] = round(ms(t.sentNs, t.confirmedNs));
    for (const stage of STAGES) {
      const v = values[stage];
      if (v === undefined) continue;
      const list = this.stages.get(stage) ?? [];
      list.push(v);
      this.stages.set(stage, list);
    }
    if (t.firstAckBy) this.firstAcks.set(t.firstAckBy, (this.firstAcks.get(t.firstAckBy) ?? 0) + 1);
    const parts = STAGES.filter((s) => values[s] !== undefined).map((s) => `${s} ${values[s]} ms`);
    log("latency", `${t.rule} ${t.mint.slice(0, 6)} via ${t.connection}: ${parts.join(", ")}${t.firstAckBy ? `, first ack ${t.firstAckBy}` : ""} => ${outcome}`);
  }

  summary(): string[] {
    const fmt = (list: number[]) => {
      const sorted = [...list].sort((a, b) => a - b);
      return `p50 ${percentile(sorted, 0.5)} ms, p95 ${percentile(sorted, 0.95)} ms, n=${sorted.length}`;
    };
    const lines: string[] = [];
    for (const [connection, list] of this.feed) lines.push(`feed ${connection} publish->receipt: ${fmt(list)}`);
    for (const [connection, n] of this.firstCopies) lines.push(`first copy of an event came from ${connection}: ${n} times`);
    for (const stage of STAGES) {
      const list = this.stages.get(stage);
      if (list?.length) lines.push(`${stage}: ${fmt(list)}`);
    }
    for (const [sender, n] of this.firstAcks) lines.push(`first sender to accept: ${sender} (${n})`);
    return lines.length ? lines : ["no events yet"];
  }
}
