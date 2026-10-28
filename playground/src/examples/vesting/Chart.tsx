/** A schedule drawn over time: the vested share from a little before the start to a little after the end, the cliff and now marked. */
import { useLayoutEffect, useRef, useState, type KeyboardEvent, type PointerEvent, type RefObject } from 'react';
import { curve, vestedAt } from './schedule';

const PLOT = 122;
/** Room over the top line for its label, where no mark reaches */
const PAD_TOP = 18;
const PAD_BOTTOM = 8;
const PAD_X = 6;
/** Room under the axis for the dates of the start and the end */
const TICKS = 18;
/** Narrower than this, the two dates could run into each other and are left out */
const TICKS_FROM = 240;

/** The element's width in CSS pixels, followed as it changes */
function useWidth(ref: RefObject<HTMLElement | null>): number {
  const [width, setWidth] = useState(0);
  useLayoutEffect(() => {
    const el = ref.current;
    if (!el) return;
    setWidth(el.clientWidth);
    if (typeof ResizeObserver === 'undefined') return;
    const observer = new ResizeObserver((entries) => {
      const w = entries[0]?.contentRect.width;
      if (w) setWidth(Math.round(w));
    });
    observer.observe(el);
    return () => observer.disconnect();
  }, [ref]);
  return width;
}

export interface ChartText {
  /** What the drawing shows, for screen readers */
  label: string;
  vested: string;
  cliff: string;
  now: string;
  /** The top of the scale, all of it vested */
  full: string;
  /** A date at an end of the axis */
  tick: (at: bigint) => string;
  /** The readout at a time under the pointer or the arrow keys */
  readout: (at: bigint) => string;
}

export function VestingChart({ start, cliff, duration, now, text }: {
  start: bigint; cliff: bigint; duration: bigint; now: bigint | null; text: ChartText;
}) {
  const box = useRef<HTMLDivElement>(null);
  const measured = useWidth(box);
  const [cursor, setCursor] = useState<number | null>(null);
  const c = curve({ start, cliff, duration });
  const span = Number(c.to - c.from) || 1;
  const width = Math.max(measured, 160);
  const plotWidth = width - PAD_X * 2;
  const ticks = width >= TICKS_FROM;
  const height = PLOT + (ticks ? TICKS : 0);
  const plotHeight = PLOT - PAD_TOP - PAD_BOTTOM;
  const x = (at: bigint) => PAD_X + (Number(at - c.from) / span) * plotWidth;
  const y = (s: number) => PAD_TOP + (1 - s) * plotHeight;
  const xy = (at: bigint, s: number) => `${x(at).toFixed(1)},${y(s).toFixed(1)}`;
  const line = c.points.map((p, i) => `${i === 0 ? 'M' : 'L'}${xy(p.at, p.share)}`).join(' ');
  const area = `${line} L${xy(c.to, 0)} L${xy(c.from, 0)} Z`;
  // The share at any time, by the contract's rule on a total of a million
  const shareAt = (at: bigint) => Number(vestedAt({ total: 1_000_000n, released: 0n, start, cliff, duration, revoked_at: null }, at)) / 1_000_000;
  const at = cursor === null ? null : c.from + BigInt(Math.round(cursor * span));
  const nowShown = now !== null && now >= c.from && now <= c.to;
  const cliffTime = start + cliff;
  const end = start + duration;

  const follow = (e: PointerEvent<HTMLDivElement>) => {
    const rect = e.currentTarget.getBoundingClientRect();
    if (rect.width <= PAD_X * 2) return;
    setCursor(Math.min(1, Math.max(0, (e.clientX - rect.left - PAD_X) / (rect.width - PAD_X * 2))));
  };
  const step = (e: KeyboardEvent<HTMLDivElement>) => {
    const moves: Record<string, (v: number) => number> = {
      ArrowLeft: (v) => v - 0.02, ArrowRight: (v) => v + 0.02, Home: () => 0, End: () => 1,
    };
    const move = moves[e.key];
    if (e.key === 'Escape') setCursor(null);
    if (!move) return;
    e.preventDefault();
    setCursor((v) => Math.min(1, Math.max(0, move(v ?? (nowShown && now !== null ? Number(now - c.from) / span : 0)))));
  };

  return <figure className="vesting-chart">
    <div className="vesting-readout" aria-live="polite">{at !== null ? text.readout(at) : ' '}</div>
    <div ref={box} className="vesting-plot" tabIndex={0} role="group" aria-label={text.label}
      onPointerMove={follow} onPointerDown={follow} onPointerLeave={() => setCursor(null)} onKeyDown={step} onBlur={() => setCursor(null)}>
      <svg viewBox={`0 0 ${width} ${height}`} width={width} height={height} aria-hidden="true" focusable="false">
        <line className="vesting-grid" x1={PAD_X} x2={width - PAD_X} y1={y(1)} y2={y(1)} />
        <text className="vesting-tick" x={PAD_X} y={PAD_TOP - 5}>{text.full}</text>
        {ticks && <>
          <text className="vesting-tick" x={x(start)} y={PLOT + 10} textAnchor="start">{text.tick(start)}</text>
          <text className="vesting-tick" x={x(end)} y={PLOT + 10} textAnchor="end">{text.tick(end)}</text>
        </>}
        <path className="vesting-area" d={area} />
        <line className="vesting-axis" x1={PAD_X} x2={width - PAD_X} y1={y(0)} y2={y(0)} />
        {cliff > 0n && <line className="vesting-mark-cliff" x1={x(cliffTime)} x2={x(cliffTime)} y1={y(0)} y2={PAD_TOP} />}
        {nowShown && now !== null && <line className="vesting-mark-now" x1={x(now)} x2={x(now)} y1={y(0)} y2={PAD_TOP} />}
        <path className="vesting-line" d={line} />
        {cliff > 0n && cliff < duration && <circle className="vesting-dot" cx={x(cliffTime)} cy={y(shareAt(cliffTime))} r={4} />}
        <circle className="vesting-dot" cx={x(end)} cy={y(1)} r={4} />
        {at !== null && <>
          <line className="vesting-cross" x1={x(at)} x2={x(at)} y1={y(0)} y2={PAD_TOP} />
          <circle className="vesting-dot" cx={x(at)} cy={y(shareAt(at))} r={4} />
        </>}
      </svg>
    </div>
    <figcaption className="vesting-key">
      <span><i className="vesting-key-line" aria-hidden="true" />{text.vested}</span>
      {cliff > 0n && <span><i className="vesting-key-cliff" aria-hidden="true" />{text.cliff}</span>}
      {nowShown && <span><i className="vesting-key-now" aria-hidden="true" />{text.now}</span>}
    </figcaption>
  </figure>;
}
