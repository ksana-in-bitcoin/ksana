/**
 * The ABI block of a contract's source: a fenced block with the info string `abi` in the `//!` module documentation, one
 * declaration per line, `//` after a declaration being its one-line description:
 *
 *     //! ```abi
 *     //! struct Funds { asset: AssetId, amount: u128 }
 *     //! enum Action { Transfer(AssetId, Account, u128), SetThreshold(u8) }
 *     //! init(name: String, decimals: u8)                   // issues the supply to the deployer
 *     //! payable call deposit(min_shares: u128) -> u128
 *     //! view balance_of(account: Account) -> u128
 *     //! event transfer(from: Account, to: Account, amount: u128)
 *     //! ```
 *
 * Types: the integers, `bool`, `String`, `Bytes` (`Vec<u8>`), `[u8; N]`, `Account`, `ContractId`, `AssetId`, `Hash`, `TokenId`
 * (32 bytes each), `Option<T>`, `Vec<T>`, tuples, `()` and the structs and enums declared in the block. An enum's Borsh tag is its
 * variants' declaration order. The block is documentation for people as well: rustdoc shows it as a code block.
 */
import type { Abi, AbiType, EnumDef, EventDef, Field, IdName, IntName, MethodDef, StructDef } from './types';

export class AbiError extends Error {
  constructor(message: string, readonly line: number) {
    super(line > 0 ? `line ${line}: ${message}` : message);
    this.name = 'AbiError';
  }
}

const INTS = new Set<string>(['u8', 'u16', 'u32', 'u64', 'u128', 'i8', 'i16', 'i32', 'i64', 'i128']);
const IDS = new Set<string>(['Account', 'ContractId', 'AssetId', 'Hash', 'TokenId']);
const RESERVED = new Set<string>([...INTS, ...IDS, 'bool', 'String', 'Bytes', 'Option', 'Vec', 'u8']);
const MAX_FIXED = 4096;
const OPEN_RE = /^\s*\/\/!\s?```abi\s*$/;
const CLOSE_RE = /^\s*\/\/!\s?```\s*$/;

/** The lines of the ABI block (with their 1-based line numbers), or null when the source has none */
export function abiLines(source: string): { text: string; line: number }[] | null {
  const lines = source.split(/\r?\n/);
  const start = lines.findIndex((l) => OPEN_RE.test(l));
  if (start < 0) return null;
  const out: { text: string; line: number }[] = [];
  for (let i = start + 1; i < lines.length; i++) {
    const raw = lines[i]!;
    if (CLOSE_RE.test(raw)) return out;
    const m = /^\s*\/\/!\s?(.*)$/.exec(raw);
    if (!m) throw new AbiError('the abi block must end with ``` inside the //! documentation', i + 1);
    out.push({ text: m[1]!, line: i + 1 });
  }
  throw new AbiError('the abi block is not closed with ```', start + 1);
}

/** Parses the ABI block of a contract's source; null when the source declares none */
export function parseAbi(source: string): Abi | null {
  const lines = abiLines(source);
  if (lines === null) return null;
  const abi: Abi = { structs: new Map(), enums: new Map(), init: null, methods: [], events: [] };
  const names = new Set<string>();
  const eventNames = new Set<string>();
  for (const { text, line } of lines) {
    const cut = text.indexOf('//');
    const decl = (cut >= 0 ? text.slice(0, cut) : text).trim();
    const doc = cut >= 0 ? text.slice(cut + 2).trim() : '';
    if (decl === '') continue;
    const p = new Parser(decl, line);
    const head = p.ident();
    if (head === 'struct') {
      const def = { ...p.struct(), line };
      if (abi.structs.has(def.name) || abi.enums.has(def.name)) throw new AbiError(`type ${def.name} is declared twice`, line);
      abi.structs.set(def.name, def);
    } else if (head === 'enum') {
      const def = { ...p.enumDef(), line };
      if (abi.structs.has(def.name) || abi.enums.has(def.name)) throw new AbiError(`type ${def.name} is declared twice`, line);
      abi.enums.set(def.name, def);
    } else if (head === 'event') {
      const name = p.ident();
      const fields = p.params();
      p.end();
      if (eventNames.has(name)) throw new AbiError(`event ${name} is declared twice`, line);
      eventNames.add(name);
      abi.events.push({ name, fields, doc, line } satisfies EventDef);
    } else {
      let payable = false;
      let kind = head;
      if (kind === 'payable') {
        payable = true;
        kind = p.ident();
      }
      let method: MethodDef;
      if (kind === 'init') {
        const params = p.params();
        if (p.peek('->')) throw new AbiError('init returns nothing', line);
        p.end();
        if (abi.init) throw new AbiError('init is declared twice', line);
        method = { kind: 'init', name: 'init', payable, params, returns: null, doc, line };
        abi.init = method;
      } else if (kind === 'call' || kind === 'view') {
        const name = p.ident();
        if (name === 'init') throw new AbiError('declare init without call or view', line);
        const params = p.params();
        let returns: AbiType | null = null;
        if (p.peek('->')) {
          p.expect('->');
          returns = p.type();
          if (returns.kind === 'tuple' && returns.items.length === 0) returns = null;
        }
        p.end();
        if (kind === 'view' && payable) throw new AbiError('a view takes no funds', line);
        if (kind === 'view' && returns === null) throw new AbiError(`view ${name} must return a value`, line);
        method = { kind, name, payable, params, returns, doc, line };
        abi.methods.push(method);
      } else {
        throw new AbiError(`unknown declaration "${head}": expected struct, enum, init, call, view, payable or event`, line);
      }
      if (names.has(method.name)) throw new AbiError(`method ${method.name} is declared twice`, line);
      names.add(method.name);
    }
  }
  check(abi, lines[0]?.line ?? 0);
  return abi;
}

/** Every referenced type is declared, no type contains itself, field and parameter names are unique */
function check(abi: Abi, firstLine: number) {
  const lineOf = new Map<string, number>();
  for (const d of [...abi.structs.values(), ...abi.enums.values()]) lineOf.set(d.name, d.line ?? firstLine);
  const visit = (type: AbiType, line: number, stack: string[]) => {
    switch (type.kind) {
      case 'option':
      case 'vec':
        visit(type.inner, line, stack);
        return;
      case 'tuple':
        type.items.forEach((t) => visit(t, line, stack));
        return;
      case 'struct':
      case 'enum': {
        const known = abi.structs.has(type.name) || abi.enums.has(type.name);
        if (!known) throw new AbiError(`unknown type ${type.name}`, line);
        // the parser cannot tell a struct from an enum by name: fix the kind now
        (type as { kind: 'struct' | 'enum' }).kind = abi.structs.has(type.name) ? 'struct' : 'enum';
        if (stack.includes(type.name)) throw new AbiError(`type ${type.name} contains itself`, lineOf.get(type.name) ?? line);
        const next = [...stack, type.name];
        const s = abi.structs.get(type.name);
        if (s) s.fields.forEach((f) => visit(f.type, lineOf.get(type.name) ?? line, next));
        const e = abi.enums.get(type.name);
        if (e) e.variants.forEach((v) => v.fields.forEach((t) => visit(t, lineOf.get(type.name) ?? line, next)));
        return;
      }
      default:
        return;
    }
  };
  const unique = (fields: Field[], what: string, line: number) => {
    const seen = new Set<string>();
    for (const f of fields) {
      if (seen.has(f.name)) throw new AbiError(`${what} has two fields named ${f.name}`, line);
      seen.add(f.name);
    }
  };
  for (const s of abi.structs.values()) {
    const line = s.line ?? firstLine;
    unique(s.fields, `struct ${s.name}`, line);
    s.fields.forEach((f) => visit(f.type, line, [s.name]));
  }
  for (const e of abi.enums.values()) {
    const line = e.line ?? firstLine;
    const seen = new Set<string>();
    for (const v of e.variants) {
      if (seen.has(v.name)) throw new AbiError(`enum ${e.name} has two variants named ${v.name}`, line);
      seen.add(v.name);
      v.fields.forEach((t) => visit(t, line, [e.name]));
    }
    if (e.variants.length > 256) throw new AbiError(`enum ${e.name} has more than 256 variants`, line);
  }
  for (const m of [...(abi.init ? [abi.init] : []), ...abi.methods]) {
    unique(m.params, m.name, m.line);
    m.params.forEach((f) => visit(f.type, m.line, []));
    if (m.returns) visit(m.returns, m.line, []);
  }
  for (const ev of abi.events) {
    unique(ev.fields, `event ${ev.name}`, ev.line);
    ev.fields.forEach((f) => visit(f.type, ev.line, []));
  }
}

class Parser {
  private pos = 0;
  private readonly tokens: string[];

  constructor(text: string, private readonly line: number) {
    this.tokens = [];
    const re = /\s*(->|[A-Za-z_][A-Za-z0-9_]*|[0-9]+|[(){}<>[\],:;])/y;
    let at = 0;
    while (at < text.length) {
      re.lastIndex = at;
      const m = re.exec(text);
      if (!m) {
        if (/^\s*$/.test(text.slice(at))) break;
        throw new AbiError(`unexpected "${text.slice(at).trim().slice(0, 12)}"`, line);
      }
      this.tokens.push(m[1]!);
      at = re.lastIndex;
    }
  }

  private fail(what: string): never {
    const got = this.tokens[this.pos];
    throw new AbiError(`expected ${what}${got === undefined ? ' at the end' : `, found "${got}"`}`, this.line);
  }

  peek(token: string): boolean {
    return this.tokens[this.pos] === token;
  }

  expect(token: string) {
    if (this.tokens[this.pos] !== token) this.fail(`"${token}"`);
    this.pos++;
  }

  end() {
    if (this.pos !== this.tokens.length) this.fail('the end of the declaration');
  }

  ident(): string {
    const t = this.tokens[this.pos];
    if (t === undefined || !/^[A-Za-z_][A-Za-z0-9_]*$/.test(t)) this.fail('a name');
    this.pos++;
    return t;
  }

  private typeIdent(): string {
    const name = this.ident();
    if (RESERVED.has(name)) throw new AbiError(`${name} is a built-in type and cannot be declared`, this.line);
    return name;
  }

  struct(): StructDef {
    const name = this.typeIdent();
    this.expect('{');
    const fields: Field[] = [];
    while (!this.peek('}')) {
      const field = this.ident();
      this.expect(':');
      fields.push({ name: field, type: this.type() });
      if (!this.peek('}')) this.expect(',');
    }
    this.expect('}');
    this.end();
    return { name, fields };
  }

  enumDef(): EnumDef {
    const name = this.typeIdent();
    this.expect('{');
    const variants: EnumDef['variants'] = [];
    while (!this.peek('}')) {
      const variant = this.ident();
      const fields: AbiType[] = [];
      if (this.peek('(')) {
        this.expect('(');
        while (!this.peek(')')) {
          fields.push(this.type());
          if (!this.peek(')')) this.expect(',');
        }
        this.expect(')');
      }
      variants.push({ name: variant, fields });
      if (!this.peek('}')) this.expect(',');
    }
    this.expect('}');
    this.end();
    if (variants.length === 0) throw new AbiError(`enum ${name} has no variants`, this.line);
    return { name, variants };
  }

  params(): Field[] {
    this.expect('(');
    const out: Field[] = [];
    while (!this.peek(')')) {
      const name = this.ident();
      this.expect(':');
      out.push({ name, type: this.type() });
      if (!this.peek(')')) this.expect(',');
    }
    this.expect(')');
    return out;
  }

  type(): AbiType {
    if (this.peek('(')) {
      this.expect('(');
      const items: AbiType[] = [];
      while (!this.peek(')')) {
        items.push(this.type());
        if (!this.peek(')')) this.expect(',');
      }
      this.expect(')');
      return { kind: 'tuple', items };
    }
    if (this.peek('[')) {
      this.expect('[');
      if (this.ident() !== 'u8') throw new AbiError('only byte arrays [u8; N] are supported', this.line);
      this.expect(';');
      const n = this.tokens[this.pos];
      if (n === undefined || !/^[0-9]+$/.test(n)) this.fail('the array length');
      this.pos++;
      this.expect(']');
      const len = Number(n);
      if (len < 1 || len > MAX_FIXED) throw new AbiError(`array length must be 1 to ${MAX_FIXED}`, this.line);
      return { kind: 'fixed', len };
    }
    const name = this.ident();
    if (INTS.has(name)) return { kind: 'int', name: name as IntName };
    if (IDS.has(name)) return { kind: 'id', name: name as IdName };
    if (name === 'bool') return { kind: 'bool' };
    if (name === 'String') return { kind: 'string' };
    if (name === 'Bytes') return { kind: 'bytes' };
    if (name === 'Option' || name === 'Vec') {
      this.expect('<');
      const inner = this.type();
      this.expect('>');
      if (name === 'Option') {
        // A form's value is null for both None and Some(None): one would be sent, and shown, as the other (D-133)
        if (inner.kind === 'option') throw new AbiError('Option<Option<…>> is not supported: its Some(None) would read as None', this.line);
        return { kind: 'option', inner };
      }
      return inner.kind === 'int' && inner.name === 'u8' ? { kind: 'bytes' } : { kind: 'vec', inner };
    }
    // struct or enum: resolved by check() once every declaration is read
    return { kind: 'struct', name };
  }
}
