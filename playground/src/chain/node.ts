/**
 * The page's client of the node. A node runs at most 4 read-only calls at a time for all its clients and answers any more with
 * `-32009`, busy (docs/RPC.md), which a view would show as its error until the next block: a page that reads a pool, its two
 * sides and the visitor's holdings at once got that answer from a node nobody else used. So the page keeps at most
 * {@link CALLS_IN_FLIGHT} of its own calls in flight, the others waiting their turn in order, and asks again, a moment later,
 * for a call the node was too busy to run.
 */
import { KsanaRpcClient, RpcError, type CallRequest, type Receipt } from '@ksana/sdk';

/** The page's read-only calls in flight at once, below the node's 4 so that its other clients keep a place */
export const CALLS_IN_FLIGHT = 3;
/** The waits before each new try of a call the node was too busy to run; after the last, its answer stands */
export const BUSY_RETRY_MS: readonly number[] = [250, 750, 2_000];
/** The node's answer to a call beyond the ones it runs at a time */
export const SERVER_IS_BUSY = -32009;

export class PageNode extends KsanaRpcClient {
  private running = 0;
  private readonly waiting: (() => void)[] = [];

  override async call(request: CallRequest): Promise<Receipt> {
    await this.turn();
    try {
      for (let attempt = 0; ; attempt++) {
        try {
          return await super.call(request);
        } catch (e) {
          const wait = BUSY_RETRY_MS[attempt];
          if (!(e instanceof RpcError && e.code === SERVER_IS_BUSY) || wait === undefined) throw e;
          await new Promise((resolve) => setTimeout(resolve, wait));
        }
      }
    } finally {
      this.done();
    }
  }

  /** Resolves when the call may go: at once while fewer than CALLS_IN_FLIGHT are in flight, else when one ahead of it is done */
  private turn(): Promise<void> {
    if (this.running < CALLS_IN_FLIGHT) {
      this.running += 1;
      return Promise.resolve();
    }
    return new Promise((resolve) => { this.waiting.push(resolve); });
  }

  /** A call is done: its place passes to the first one waiting */
  private done(): void {
    const next = this.waiting.shift();
    if (next) next();
    else this.running -= 1;
  }
}
