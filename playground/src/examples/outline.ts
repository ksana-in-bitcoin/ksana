/** Where an example's source starts and where it handles a method, for the workbench's view of the code and its jumps into it. */

/** The 1-based line where the code starts, after the module's documentation (the `//!` lines the source opens with) and the blank lines that follow it */
export function codeStart(source: string): number {
  const lines = source.split('\n');
  const docs = lines.findIndex((l) => !l.startsWith('//!'));
  if (docs <= 0) return 1;
  const at = lines.findIndex((l, i) => i >= docs && l.trim() !== '');
  return at < 0 ? 1 : at + 1;
}

/**
 * The 1-based line of `source` that handles `method`: its arm of the dispatch (`"swap" => …`, `("add", Some(value)) => …`), else
 * the first line that names it (`if method == "init"`), else the function of that name; null when the source has none of them
 * (a method a standard's library handles for the contract).
 */
export function methodLine(source: string, method: string): number | null {
  if (!/^[A-Za-z_][A-Za-z0-9_]*$/.test(method)) return null;
  const arm = new RegExp(`"${method}".*=>`);
  const named = new RegExp(`"${method}"`);
  const func = new RegExp(`\\bfn\\s+${method}\\s*[(<]`);
  const lines = source.split('\n');
  for (const pattern of [arm, named, func]) {
    const at = lines.findIndex((l) => !l.trimStart().startsWith('//') && pattern.test(l));
    if (at >= 0) return at + 1;
  }
  return null;
}
