import { describe, expect, it } from 'vitest';
import { EXAMPLES, exampleAbi } from './index';
import { codeStart, methodLine } from './outline';

describe('codeStart', () => {
  it('is the first line after the module documentation', () => {
    expect(codeStart('//! a\n//! b\n#![no_std]\nfn f() {}')).toBe(3);
    expect(codeStart('//! a\n\nuse x;')).toBe(3);
    expect(codeStart('fn f() {}\n')).toBe(1);
    expect(codeStart('//! only docs')).toBe(1);
    for (const e of EXAMPLES) expect(e.source.split('\n')[codeStart(e.source) - 1]?.startsWith('//!'), e.id).toBe(false);
  });
});

describe('methodLine', () => {
  it('finds the arm of the dispatch, not the ABI block or a comment', () => {
    const source = [
      '//! call add(amount: u64) -> u64',
      'match method {',
      '    // "add" => old',
      '    ("add", Some(value)) => change(value),',
      '    ("get" | "add", None) => revert(),',
      '}',
    ].join('\n');
    expect(methodLine(source, 'add')).toBe(4);
    expect(methodLine(source, 'get')).toBe(5);
  });

  it('falls back to the function, and to null', () => {
    const source = 'pub fn swap(amount: u128) {}\nfn swap_all() {}';
    expect(methodLine(source, 'swap')).toBe(1);
    expect(methodLine(source, 'swap_all')).toBe(2);
    expect(methodLine(source, 'burn')).toBeNull();
    expect(methodLine(source, 'a|b')).toBeNull();
  });

  it('finds where every example handles init', () => {
    for (const e of EXAMPLES) {
      if (exampleAbi(e)?.init) expect(methodLine(e.source, 'init'), e.id).not.toBeNull();
    }
  });
});
