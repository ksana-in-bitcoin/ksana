/** What the example apps share: building calls from the ABI, sending them through the review, and the state of a send. */
import { useCallback, useEffect, useRef, useState } from 'react';
import { fromHex, type Action, type Funds } from '@ksana/sdk';
import { encodeArgs } from '../abi/codec';
import type { Abi, MethodDef, Value } from '../abi/types';
import { sortFunds } from '../chain/ops';
import { CancelledError, runOp, type Activity, type OpRequest } from '../chain/run';
import { contractChanged } from '../chain/views';
import { t as messages } from '../i18n';
import { errorText } from '../ui/errors';
import { toast } from '../ui/Toasts';

export interface AppProps {
  /** The contract's ID (hex) */
  contract: string;
  /** The ABI it was deployed with */
  abi: Abi;
}

export function methodOf(abi: Abi, name: string): MethodDef {
  const method = abi.methods.find((m) => m.name === name);
  if (!method) throw new Error(`the ABI has no method ${name}`);
  return method;
}

/** A call of `method` with its arguments in the ABI's order, encoded as the contract decodes them */
export function callAction(abi: Abi, contract: string, method: string, args: Value[], funds: Funds[] = []): Action {
  const def = methodOf(abi, method);
  return {
    kind: 'Call',
    contract: fromHex(contract, 32),
    method,
    args: encodeArgs(abi, def.params.map((p) => p.type), args),
    funds: sortFunds(funds),
  };
}

export interface SendState {
  /** Opens the review; resolves with the operation once a block includes it, or null when the person cancels or it fails */
  send: (request: OpRequest) => Promise<Activity | null>;
  busy: boolean;
  error: string | null;
  clear: () => void;
}

/** The state of one button's operations: busy while the review is open and until a block includes it */
export function useSend(): SendState {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const alive = useRef(true);
  useEffect(() => {
    alive.current = true;
    return () => { alive.current = false; };
  }, []);
  const send = useCallback(async (request: OpRequest) => {
    setBusy(true);
    setError(null);
    try {
      const activity = await runOp(request);
      contractChanged();
      toast({ tone: 'ok', title: request.title, body: messages().core.activity.states.fast_confirmed, opId: activity.opId });
      return activity;
    } catch (e) {
      if (!(e instanceof CancelledError)) {
        const text = errorText(e, messages());
        if (alive.current) setError(text);
        toast({ tone: 'danger', title: request.title, body: text });
        contractChanged();
      }
      return null;
    } finally {
      if (alive.current) setBusy(false);
    }
  }, []);
  const clear = useCallback(() => setError(null), []);
  return { send, busy, error, clear };
}
