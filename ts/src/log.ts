// One line per event, label first, so logs stay readable when grepped or tailed.
export function log(label: string, text: string): void {
  console.log(`${new Date().toISOString().slice(11, 23)} ${label.padEnd(9)} ${text}`);
}

export const short = (address: string) => (address.length > 12 ? `${address.slice(0, 4)}…${address.slice(-4)}` : address);
