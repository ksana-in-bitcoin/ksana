/** The hosted signet's KSA faucet (signet/services/faucet.mjs): `POST {"to": <account>}` sends test KSA to the account. */
import { resolveUrl } from './deployment';

export async function claimFromFaucet(url: string, account: string): Promise<string> {
  const res = await fetch(resolveUrl(url), { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify({ to: account }) });
  let body: unknown = null;
  try {
    body = await res.json();
  } catch {
    // not JSON: reported by the status below
  }
  const reply = (typeof body === 'object' && body !== null ? body : {}) as { op_id?: unknown; error?: unknown };
  if (!res.ok || typeof reply.op_id !== 'string') throw new Error(typeof reply.error === 'string' ? reply.error : `HTTP ${res.status}`);
  return reply.op_id;
}
