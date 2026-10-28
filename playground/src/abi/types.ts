/**
 * The ABI a contract declares in its source: a fenced block with the info string `abi` inside its `//!` module documentation
 * (parse.ts), from which the playground builds its forms, encodes arguments and decodes results and events (codec.ts).
 */

export type IntName = 'u8' | 'u16' | 'u32' | 'u64' | 'u128' | 'i8' | 'i16' | 'i32' | 'i64' | 'i128';
/**
 * 32 bytes on the wire; the name says how to read and show them. `TokenId` is a KS-721 token ID (`docs/STANDARDS.md` §4):
 * `#n` for the ID `ks721::id(n)` gives the `n`-th token, the 64 hex digits of any other.
 */
export type IdName = 'Account' | 'ContractId' | 'AssetId' | 'Hash' | 'TokenId';

export type AbiType =
  | { kind: 'int'; name: IntName }
  | { kind: 'bool' }
  | { kind: 'string' }
  | { kind: 'bytes' }
  | { kind: 'fixed'; len: number }
  | { kind: 'id'; name: IdName }
  | { kind: 'option'; inner: AbiType }
  | { kind: 'vec'; inner: AbiType }
  | { kind: 'tuple'; items: AbiType[] }
  | { kind: 'struct'; name: string }
  | { kind: 'enum'; name: string };

export interface Field {
  name: string;
  type: AbiType;
}

export interface StructDef {
  name: string;
  fields: Field[];
  line?: number;
}

export interface Variant {
  name: string;
  fields: AbiType[];
}

export interface EnumDef {
  name: string;
  variants: Variant[];
  line?: number;
}

export type MethodKind = 'init' | 'call' | 'view';

export interface MethodDef {
  kind: MethodKind;
  name: string;
  /** Takes attached funds */
  payable: boolean;
  params: Field[];
  /** `null`: returns `()` */
  returns: AbiType | null;
  /** The `//` description after the declaration */
  doc: string;
  /** 1-based line in the source */
  line: number;
}

export interface EventDef {
  name: string;
  fields: Field[];
  doc: string;
  line: number;
}

export interface Abi {
  structs: Map<string, StructDef>;
  enums: Map<string, EnumDef>;
  init: MethodDef | null;
  /** `call` and `view` methods in declaration order */
  methods: MethodDef[];
  events: EventDef[];
}

/**
 * A decoded value:
 * integers are bigints, `bool` a boolean, `String` a string, `Bytes`, `[u8; N]` and the ID types a Uint8Array, `Option` null or
 * the inner value, `Vec` and tuples arrays, structs objects by field name, enums an {@link EnumValue}.
 */
export type Value = bigint | boolean | string | Uint8Array | null | Value[] | EnumValue | { [field: string]: Value };

export interface EnumValue {
  variant: string;
  values: Value[];
}

export function isEnumValue(value: Value): value is EnumValue {
  return typeof value === 'object' && value !== null && !Array.isArray(value) && !(value instanceof Uint8Array) &&
    typeof (value as { variant?: unknown }).variant === 'string' && Array.isArray((value as { values?: unknown }).values);
}

export const INT_BITS: Record<IntName, number> = {
  u8: 8, u16: 16, u32: 32, u64: 64, u128: 128, i8: 8, i16: 16, i32: 32, i64: 64, i128: 128,
};

export function intRange(name: IntName): { min: bigint; max: bigint } {
  const bits = BigInt(INT_BITS[name]);
  return name.startsWith('i') ? { min: -(1n << (bits - 1n)), max: (1n << (bits - 1n)) - 1n } : { min: 0n, max: (1n << bits) - 1n };
}

/** The type as the ABI block writes it */
export function typeName(type: AbiType): string {
  switch (type.kind) {
    case 'int': return type.name;
    case 'bool': return 'bool';
    case 'string': return 'String';
    case 'bytes': return 'Bytes';
    case 'fixed': return `[u8; ${type.len}]`;
    case 'id': return type.name;
    case 'option': return `Option<${typeName(type.inner)}>`;
    case 'vec': return `Vec<${typeName(type.inner)}>`;
    case 'tuple': return `(${type.items.map(typeName).join(', ')})`;
    case 'struct':
    case 'enum': return type.name;
  }
}
