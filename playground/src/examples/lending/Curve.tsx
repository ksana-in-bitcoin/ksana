/**
 * The borrow rate against utilization: the market's kinked line with the kink and the current utilization marked, and a readout
 * of both rates at the utilization under the pointer or the arrow keys.
 */
import { useRef, useState, type KeyboardEvent, type PointerEvent } from 'react';
import { BPS, borrowRateBps, percentText, rateCorners, supplyRateBps, type Params } from './math';
import { useWidth } from './parts';

const HEIGHT = 150;
const PAD_TOP = 10;
const PAD_BOTTOM = 24;
const PAD_LEFT = 40;
const PAD_RIGHT = 12;
/** Room a label of the utilization axis takes on each side of its anchor, roughly, at 11px */
const TICK_HALF = 16;

export interface CurveText {
  /** What the drawing shows, for screen readers */
  label: string;
  /** The rates at the market's utilization now */
  now: (utilization: string, borrow: string, supply: string) => string;
  /** The rates at a utilization under the pointer */
  at: (utilization: string, borrow: string, supply: string) => string;
  rate: string;
  current: string;
  kink: (utilization: string) => string;
}

/** Clean steps of the rate axis, in basis points: about four from 0 to at least the top rate */
function rateTicks(top: bigint): bigint[] {
  const most = top > 0n ? top : 100n;
  const rough = most / 4n > 0n ? most / 4n : 1n;
  let magnitude = 1n;
  while (magnitude * 10n <= rough) magnitude *= 10n;
  const step = [1n, 2n, 5n, 10n].map((f) => f * magnitude).find((s) => s >= rough) ?? magnitude * 10n;
  const ticks: bigint[] = [];
  for (let v = 0n; ticks.length < 8; v += step) {
    ticks.push(v);
    if (v >= most) break;
  }
  return ticks;
}

export function RateCurve({ params, utilization, borrowRate, supplyRate, text }: {
  params: Params; utilization: bigint; borrowRate: bigint; supplyRate: bigint; text: CurveText;
}) {
  const box = useRef<HTMLDivElement>(null);
  const width = Math.max(useWidth(box), 220);
  const [cursor, setCursor] = useState<bigint | null>(null);
  const corners = rateCorners(params);
  const ticks = rateTicks(corners[corners.length - 1]?.rate ?? 0n);
  const scale = Number(ticks[ticks.length - 1] ?? 1n) || 1;
  const plotWidth = width - PAD_LEFT - PAD_RIGHT;
  const plotHeight = HEIGHT - PAD_TOP - PAD_BOTTOM;
  const x = (u: bigint) => PAD_LEFT + (Number(u) / Number(BPS)) * plotWidth;
  const y = (rate: bigint) => PAD_TOP + (1 - Number(rate) / scale) * plotHeight;
  const point = (u: bigint, rate: bigint) => `${x(u).toFixed(1)},${y(rate).toFixed(1)}`;
  const rateAt = (u: bigint) => borrowRateBps(params, u) ?? params.baseRateBps;
  const line = corners.map((c, i) => `${i === 0 ? 'M' : 'L'}${point(c.utilization, c.rate)}`).join(' ');
  const area = `${line} L${point(BPS, 0n)} L${point(0n, 0n)} Z`;
  const kink = params.kinkBps < BPS ? params.kinkBps : null;
  const kinkLabel = kink !== null && x(kink) - x(0n) > TICK_HALF * 3 && x(BPS) - x(kink) > TICK_HALF * 3.5;

  const readout = cursor === null
    ? text.now(percentText(utilization), percentText(borrowRate), percentText(supplyRate))
    : (() => {
      const borrow = rateAt(cursor);
      return text.at(percentText(cursor), percentText(borrow), percentText(supplyRateBps(borrow, cursor, params.reserveFactorBps)));
    })();

  const follow = (e: PointerEvent<HTMLDivElement>) => {
    const rect = e.currentTarget.getBoundingClientRect();
    const span = rect.width - PAD_LEFT - PAD_RIGHT;
    if (span <= 0) return;
    const share = Math.min(1, Math.max(0, (e.clientX - rect.left - PAD_LEFT) / span));
    setCursor(BigInt(Math.round(share * 100)) * 100n);
  };
  const step = (e: KeyboardEvent<HTMLDivElement>) => {
    const moves: Record<string, (v: bigint) => bigint> = {
      ArrowLeft: (v) => v - 100n, ArrowDown: (v) => v - 100n, ArrowRight: (v) => v + 100n, ArrowUp: (v) => v + 100n,
      PageDown: (v) => v - 1_000n, PageUp: (v) => v + 1_000n, Home: () => 0n, End: () => BPS,
    };
    if (e.key === 'Escape') setCursor(null);
    const move = moves[e.key];
    if (!move) return;
    e.preventDefault();
    setCursor((v) => {
      const next = move(v ?? (utilization / 100n) * 100n);
      return next < 0n ? 0n : next > BPS ? BPS : next;
    });
  };

  return <figure className="lending-curve">
    <div className="lending-curve-readout" aria-live="polite">{readout}</div>
    <div ref={box} className="lending-curve-plot" tabIndex={0} role="group" aria-label={text.label}
      onPointerMove={follow} onPointerDown={follow} onPointerLeave={() => setCursor(null)} onKeyDown={step} onBlur={() => setCursor(null)}>
      <svg viewBox={`0 0 ${width} ${HEIGHT}`} width={width} height={HEIGHT} aria-hidden="true" focusable="false">
        {ticks.map((tick) => <g key={tick.toString()}>
          <line className={tick === 0n ? 'lending-curve-axis' : 'lending-curve-grid'} x1={PAD_LEFT} x2={width - PAD_RIGHT} y1={y(tick)} y2={y(tick)} />
          <text className="lending-curve-text" x={PAD_LEFT - 6} y={y(tick) + 4} textAnchor="end">{percentText(tick)}</text>
        </g>)}
        <path className="lending-curve-area" d={area} />
        {kink !== null && <line className="lending-curve-kink" x1={x(kink)} x2={x(kink)} y1={y(0n)} y2={PAD_TOP} />}
        <path className="lending-curve-line" d={line} />
        <line className="lending-curve-now-line" x1={x(utilization)} x2={x(utilization)} y1={y(0n)} y2={y(borrowRate)} />
        <circle className="lending-curve-now" cx={x(utilization)} cy={y(borrowRate)} r={5} />
        {cursor !== null && <>
          <line className="lending-curve-cross" x1={x(cursor)} x2={x(cursor)} y1={y(0n)} y2={PAD_TOP} />
          <circle className="lending-curve-dot" cx={x(cursor)} cy={y(rateAt(cursor))} r={4} />
        </>}
        <text className="lending-curve-text" x={x(0n)} y={HEIGHT - 6} textAnchor="start">0%</text>
        {kink !== null && kinkLabel && <text className="lending-curve-text" x={x(kink)} y={HEIGHT - 6} textAnchor="middle">{percentText(kink)}</text>}
        <text className="lending-curve-text" x={x(BPS)} y={HEIGHT - 6} textAnchor="end">100%</text>
      </svg>
    </div>
    <figcaption className="lending-curve-key">
      <span><i className="lending-key-line" aria-hidden="true" />{text.rate}</span>
      <span><i className="lending-key-dot" aria-hidden="true" />{text.current}</span>
      {kink !== null && <span><i className="lending-key-kink" aria-hidden="true" />{text.kink(percentText(kink))}</span>}
    </figcaption>
  </figure>;
}
