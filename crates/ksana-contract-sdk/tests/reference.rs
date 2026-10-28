//! The contract reference for AI assistants, `playground/src/ai/ksana-contracts.md` (D-124), lists the public API of
//! this crate's modules as Rust stubs, and this test holds it to the source: every public function and method,
//! constant, type alias, struct with its public fields, enum with its variants and trait with its methods, declared as
//! the source declares it, and nothing else. A change to the SDK fails here until the reference shows it.
//!
//! The stubs are the declarations alone, one per line (a struct or an enum may spread over several lines, and an
//! `impl` or a `trait` block holds one member per line): no bodies, attributes or `#[doc(hidden)]` items, `..` for
//! private fields, and the leading `_` of an unused parameter dropped. Each block of stubs is a ```rust block whose
//! first line names its module (`// ksana_contract_sdk`, `// ksana_contract_sdk::storage`, ...); `///` lines document
//! the stubs.

use std::collections::{BTreeMap, BTreeSet};

use ksana_contract_sdk::{
    asset_id, conformance, encode, mock, CAP_FREEZE, CAP_MINT, STATUS_FROZEN,
};

#[path = "reference/points.rs"]
mod points;
#[path = "reference/token.rs"]
mod token;

/// The reference, as the playground ships it and the site publishes it.
const REFERENCE: &str = include_str!("../../../playground/src/ai/ksana-contracts.md");

/// The files the reference shows whole, which the repository builds and runs: the tutorial's poll with one of its tests
/// (`contracts/examples/poll`), and the token and the asset below, which this file's tests run.
const EXAMPLES: [(&str, &str); 4] = [
    (
        "contracts/examples/poll/src/lib.rs",
        include_str!("../../../contracts/examples/poll/src/lib.rs"),
    ),
    (
        "contracts/examples/poll/tests/one_vote.rs",
        include_str!("../../../contracts/examples/poll/tests/one_vote.rs"),
    ),
    (
        "crates/ksana-contract-sdk/tests/reference/token.rs",
        include_str!("reference/token.rs"),
    ),
    (
        "crates/ksana-contract-sdk/tests/reference/points.rs",
        include_str!("reference/points.rs"),
    ),
];

/// The modules the reference lists, with the files that define them (the crate root re-exports `api` and `codec`).
const MODULES: [(&str, &[&str]); 8] = [
    (
        "ksana_contract_sdk",
        &[
            include_str!("../src/api.rs"),
            include_str!("../src/codec.rs"),
        ],
    ),
    (
        "ksana_contract_sdk::storage",
        &[include_str!("../src/storage.rs")],
    ),
    ("ksana_contract_sdk::btc", &[include_str!("../src/btc.rs")]),
    (
        "ksana_contract_sdk::standard",
        &[include_str!("../src/standard.rs")],
    ),
    (
        "ksana_contract_sdk::ks20",
        &[include_str!("../src/ks20.rs")],
    ),
    (
        "ksana_contract_sdk::ks721",
        &[include_str!("../src/ks721.rs")],
    ),
    (
        "ksana_contract_sdk::mock",
        &[include_str!("../src/mock.rs")],
    ),
    (
        "ksana_contract_sdk::conformance",
        &[include_str!("../src/conformance.rs")],
    ),
];

/// A declaration: the `impl` or `trait` header it belongs to, if any, and the declaration itself.
type Item = (Option<String>, String);

/// One declaration on one line: whitespace collapsed, no space inside brackets, no trailing comma, `;` or `{`, and no
/// leading `_` on a parameter.
fn norm(text: &str) -> String {
    let mut s = text.split_whitespace().collect::<Vec<_>>().join(" ");
    for (from, to) in [
        ("( ", "("),
        ("[ ", "["),
        (" )", ")"),
        (" ]", "]"),
        (",)", ")"),
        (",]", "]"),
        (", }", " }"),
        ("(_", "("),
        (", _", ", "),
    ] {
        while s.contains(from) {
            s = s.replace(from, to);
        }
    }
    s.trim_end_matches(['{', ';', ' '])
        .trim_end_matches(',')
        .to_string()
}

fn indent(line: &str) -> usize {
    line.len() - line.trim_start().len()
}

/// The index after the line that closes the block opened at `start`: the next line at `depth` that starts with `}`.
fn after_block(lines: &[&str], start: usize, depth: usize) -> usize {
    let mut i = start + 1;
    while i < lines.len() && !(indent(lines[i]) == depth && lines[i].trim().starts_with('}')) {
        i += 1;
    }
    i + 1
}

/// Joins the lines from `start` up to the first one `last` accepts; returns the text and the index after it.
fn join(lines: &[&str], start: usize, last: impl Fn(&str) -> bool) -> (String, usize) {
    let mut text = String::new();
    let mut i = start;
    while i < lines.len() {
        let line = lines[i].trim();
        text.push(' ');
        text.push_str(line);
        i += 1;
        if last(line) {
            break;
        }
    }
    (text, i)
}

/// Whether a line ends a function's declaration: its body opens (`{`, or `{ ... }` on the same line) or it has none.
fn ends_declaration(line: &str) -> bool {
    line.ends_with('{') || line.ends_with(';') || line.ends_with('}')
}

/// A function's declaration without its body.
fn signature(decl: &str) -> &str {
    decl.find(" {").map_or(decl, |end| &decl[..end])
}

fn is_doc_or_blank(text: &str) -> bool {
    text.is_empty() || text.starts_with("//")
}

/// The name of the type an `impl` header implements for: `impl<P: Policy> Ks20<P>` gives `Ks20`.
fn self_type(header: &str) -> &str {
    let rest = header.trim_start_matches("impl");
    let rest = match rest.strip_prefix('<') {
        Some(generics) => {
            let mut depth = 1usize;
            let end = generics
                .char_indices()
                .find(|&(_, c)| {
                    match c {
                        '<' => depth += 1,
                        '>' => depth -= 1,
                        _ => {}
                    }
                    depth == 0
                })
                .map_or(generics.len(), |(i, _)| i + 1);
            &generics[end..]
        }
        None => rest,
    };
    let rest = rest.trim_start();
    let end = rest.find(['<', ' ', '{']).unwrap_or(rest.len());
    &rest[..end]
}

/// The fields of a struct (or a struct variant) whose body runs from `start` + 1 to the line closing it at `depth`;
/// private fields are `..`, once, after the public ones. Returns the fields and the index after the closing line.
fn fields(lines: &[&str], start: usize, depth: usize, public: bool) -> (Vec<String>, usize) {
    let mut out = Vec::new();
    let mut private = false;
    let mut i = start + 1;
    while i < lines.len() {
        let text = lines[i].trim();
        if indent(lines[i]) == depth && text.starts_with('}') {
            break;
        }
        if is_doc_or_blank(text) || text.starts_with("#[") {
            i += 1;
            continue;
        }
        let (field, next) = join(lines, i, |l| l.ends_with(','));
        if !public || field.trim_start().starts_with("pub ") {
            out.push(norm(&field));
        } else {
            private = true;
        }
        i = next;
    }
    if private {
        out.push("..".to_string());
    }
    (out, i + 1)
}

/// The variants of an enum whose body runs from `start` + 1 to its closing line at column 0.
fn variants(lines: &[&str], start: usize) -> (Vec<String>, usize) {
    let mut out = Vec::new();
    let mut i = start + 1;
    while i < lines.len() {
        let text = lines[i].trim();
        if indent(lines[i]) == 0 && text.starts_with('}') {
            break;
        }
        if is_doc_or_blank(text) || text.starts_with("#[") {
            i += 1;
            continue;
        }
        if text.ends_with('{') {
            let (named, next) = fields(lines, i, indent(lines[i]), false);
            out.push(format!("{} {{ {} }}", norm(text), named.join(", ")));
            i = next;
        } else {
            let (variant, next) = join(lines, i, |l| l.ends_with(','));
            out.push(norm(&variant));
            i = next;
        }
    }
    (out, i + 1)
}

/// The members of an `impl` or `trait` block opened at `start`, under `header`: the public ones of an `impl`, every
/// one of a trait. Returns the index after the block.
fn members(
    lines: &[&str],
    start: usize,
    header: &str,
    every: bool,
    out: &mut BTreeSet<Item>,
) -> usize {
    let mut i = start + 1;
    let mut hidden = false;
    while i < lines.len() {
        let line = lines[i];
        let text = line.trim();
        if indent(line) == 0 && text.starts_with('}') {
            return i + 1;
        }
        if is_doc_or_blank(text) || indent(line) != 4 {
            i += 1;
            continue;
        }
        if text.starts_with("#[") {
            hidden |= text == "#[doc(hidden)]";
            i += 1;
            continue;
        }
        let (decl, next) = join(lines, i, ends_declaration);
        if (every || text.starts_with("pub ")) && !hidden {
            out.insert((Some(header.to_string()), norm(signature(&decl))));
        }
        hidden = false;
        i = if decl.ends_with('{') {
            after_block(lines, next - 1, 4)
        } else {
            next
        };
    }
    i
}

/// The public declarations of one source file.
fn declared(source: &str) -> BTreeSet<Item> {
    let lines: Vec<&str> = source.lines().collect();
    let public_types: BTreeSet<&str> = lines
        .iter()
        .filter_map(|l| {
            ["pub struct ", "pub enum ", "pub type ", "pub trait "]
                .iter()
                .find_map(|p| l.strip_prefix(p))
        })
        .map(|rest| {
            &rest[..rest
                .find(|c: char| !c.is_alphanumeric() && c != '_')
                .unwrap_or(rest.len())]
        })
        .collect();
    let mut out = BTreeSet::new();
    let mut hidden = false;
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        let text = line.trim();
        if indent(line) > 0 || is_doc_or_blank(text) {
            i += 1;
            continue;
        }
        if text.starts_with("#[") || text.starts_with("#!") {
            hidden |= text == "#[doc(hidden)]";
            i += 1;
            continue;
        }
        let skip = std::mem::take(&mut hidden);
        let public = text.starts_with("pub ");
        if public && (text.starts_with("pub fn ") || text.starts_with("pub const fn ")) {
            let (decl, next) = join(&lines, i, ends_declaration);
            if !skip {
                out.insert((None, norm(signature(&decl))));
            }
            i = if decl.ends_with('{') {
                after_block(&lines, next - 1, 0)
            } else {
                next
            };
        } else if public
            && ["pub const ", "pub static ", "pub type "]
                .iter()
                .any(|p| text.starts_with(p))
        {
            let (decl, next) = join(&lines, i, |l| l.ends_with(';'));
            if !skip {
                out.insert((None, norm(&decl)));
            }
            i = next;
        } else if public && text.starts_with("pub struct ") && text.ends_with('{') {
            let (named, next) = fields(&lines, i, 0, true);
            if !skip {
                out.insert((None, format!("{} {{ {} }}", norm(text), named.join(", "))));
            }
            i = next;
        } else if public && text.starts_with("pub enum ") {
            let (listed, next) = variants(&lines, i);
            if !skip {
                out.insert((None, format!("{} {{ {} }}", norm(text), listed.join(", "))));
            }
            i = next;
        } else if public && text.starts_with("pub trait ") {
            let header = norm(text);
            let mut own = BTreeSet::new();
            i = members(&lines, i, &header, true, &mut own);
            if !skip {
                out.insert((None, header));
                out.extend(own);
            }
        } else if text.starts_with("impl")
            && !text.contains(" for ")
            && public_types.contains(self_type(text))
        {
            let header = norm(text);
            let mut own = BTreeSet::new();
            i = members(&lines, i, &header, false, &mut own);
            if !skip {
                out.extend(own);
            }
        } else if public && text.ends_with(';') {
            // a unit or tuple struct
            if !skip {
                out.insert((None, norm(text)));
            }
            i += 1;
        } else if text.ends_with('{') {
            i = after_block(&lines, i, 0);
        } else {
            i += 1;
        }
    }
    out
}

/// The stubs of the reference, by module.
fn listed(reference: &str) -> BTreeMap<String, BTreeSet<Item>> {
    let mut modules: BTreeMap<String, BTreeSet<Item>> = BTreeMap::new();
    let mut lines = reference.lines();
    while let Some(line) = lines.next() {
        if line.trim() != "```rust" {
            continue;
        }
        let block: Vec<&str> = lines.by_ref().take_while(|l| l.trim() != "```").collect();
        let Some(module) = block
            .first()
            .and_then(|l| l.trim().strip_prefix("// "))
            .filter(|m| m.starts_with("ksana_contract_sdk"))
        else {
            continue;
        };
        assert!(
            MODULES.iter().any(|(name, _)| *name == module),
            "the reference lists a module this test does not know: {module}"
        );
        let items = modules.entry(module.to_string()).or_default();
        let mut header: Option<String> = None;
        let mut pending = String::new();
        for line in &block[1..] {
            let text = line.trim();
            if is_doc_or_blank(text) {
                continue;
            }
            if !pending.is_empty() {
                pending.push(' ');
                pending.push_str(text);
                if text == "}" {
                    items.insert((None, norm(&std::mem::take(&mut pending))));
                }
                continue;
            }
            match &header {
                Some(_) if text == "}" => header = None,
                Some(h) => {
                    items.insert((Some(h.clone()), norm(text)));
                }
                None if (text.starts_with("impl") || text.starts_with("pub trait "))
                    && text.ends_with('{') =>
                {
                    let h = norm(text);
                    if text.starts_with("pub trait ") {
                        items.insert((None, h.clone()));
                    }
                    header = Some(h);
                }
                None if text.ends_with('{') => pending = text.to_string(),
                None => {
                    items.insert((None, norm(text)));
                }
            }
        }
        assert!(
            header.is_none() && pending.is_empty(),
            "a block of {module} in the reference is not closed"
        );
    }
    modules
}

/// `items` as the reference writes them: top-level declarations, then each `impl` or `trait` block.
fn render(items: &BTreeSet<Item>) -> String {
    let mut out = String::new();
    let mut blocks: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for (header, decl) in items {
        match header {
            Some(h) => blocks.entry(h).or_default().push(decl),
            // a trait is written as its block
            None if decl.starts_with("pub trait ") => {
                blocks.entry(decl).or_default();
            }
            None if decl.ends_with('}') => out.push_str(&format!("{decl}\n")),
            None => out.push_str(&format!("{decl};\n")),
        }
    }
    for (header, decls) in blocks {
        out.push_str(&format!("{header} {{\n"));
        for decl in decls {
            out.push_str(&format!("    {decl};\n"));
        }
        out.push_str("}\n");
    }
    out
}

#[test]
fn the_reference_lists_the_public_api_of_every_module() {
    let listed = listed(REFERENCE);
    let mut problems = String::new();
    for (module, sources) in MODULES {
        let declared: BTreeSet<Item> = sources.iter().flat_map(|s| declared(s)).collect();
        assert!(
            declared.len() > 3,
            "{module}: the test found no declarations"
        );
        let empty = BTreeSet::new();
        let stubs = listed.get(module).unwrap_or(&empty);
        let missing: BTreeSet<Item> = declared.difference(stubs).cloned().collect();
        let stale: BTreeSet<Item> = stubs.difference(&declared).cloned().collect();
        if !missing.is_empty() {
            problems.push_str(&format!(
                "\n// {module}: in the source, not in the reference\n{}",
                render(&missing)
            ));
        }
        if !stale.is_empty() {
            problems.push_str(&format!(
                "\n// {module}: in the reference, not in the source\n{}",
                render(&stale)
            ));
        }
    }
    assert!(
        problems.is_empty(),
        "playground/src/ai/ksana-contracts.md differs from the SDK:\n{problems}"
    );
}

/// The extraction itself, on declarations of each kind.
#[test]
fn declarations_are_read_as_the_reference_writes_them() {
    let source = "\
//! Docs.
use crate::x::{
    a,
    b,
};

/// A constant.
pub const LIMIT: usize = 4 + 8;
const PRIVATE: u8 = 1;
pub type Pair = (u8, u8);

pub struct Point {
    /// X.
    pub x: i32,
    y: i32,
}

pub struct Unit;

pub enum Shape {
    Dot,
    Line(Point, Point),
    Box {
        corner: Point,
        size: (u32, u32),
    },
}

pub trait Measure: Sized {
    fn area(&self, _scale: u32) -> u64 {
        0
    }
    #[doc(hidden)]
    fn hidden(&self);
}

impl Point {
    pub const fn origin() -> Self {
        Self { x: 0, y: 0 }
    }

    fn private(&self) {}

    pub fn moved(
        &self,
        dx: i32,
        _dy: i32,
    ) -> Point {
        *self
    }
}

impl Measure for Point {}

struct Hidden;

impl Hidden {
    pub fn not_public(&self) {}
}

#[doc(hidden)]
pub fn secret() {}

pub fn run<F>(f: F) -> u8
where
    F: Fn() -> u8,
{
    f()
}

#[cfg(test)]
mod tests {
    pub fn test_only() {}
}
";
    let items: Vec<Item> = declared(source).into_iter().collect();
    let top = |s: &str| (None, s.to_string());
    let member = |h: &str, s: &str| (Some(h.to_string()), s.to_string());
    let mut expected =
        vec![
        top("pub const LIMIT: usize = 4 + 8"),
        top("pub type Pair = (u8, u8)"),
        top("pub struct Point { pub x: i32, .. }"),
        top("pub struct Unit"),
        top("pub enum Shape { Dot, Line(Point, Point), Box { corner: Point, size: (u32, u32) } }"),
        top("pub trait Measure: Sized"),
        member("pub trait Measure: Sized", "fn area(&self, scale: u32) -> u64"),
        member("impl Point", "pub const fn origin() -> Self"),
        member("impl Point", "pub fn moved(&self, dx: i32, dy: i32) -> Point"),
        top("pub fn run<F>(f: F) -> u8 where F: Fn() -> u8"),
    ];
    expected.sort();
    assert_eq!(items, expected);

    let reference = "\
```rust
// ksana_contract_sdk::storage
/// Docs.
pub const LIMIT: usize = 4 + 8;
pub struct Point {
    pub x: i32,
    ..
}
pub trait Measure: Sized {
    fn area(&self, scale: u32) -> u64;
}
impl Point {
    pub const fn origin() -> Self;
}
```
```rust
//! Not a block of stubs.
pub fn ignored() {}
```
";
    let listed = listed(reference);
    let stubs: Vec<&Item> = listed["ksana_contract_sdk::storage"].iter().collect();
    assert_eq!(
        stubs,
        vec![
            &top("pub const LIMIT: usize = 4 + 8"),
            &top("pub struct Point { pub x: i32, .. }"),
            &top("pub trait Measure: Sized"),
            &member("impl Point", "pub const fn origin() -> Self"),
            &member(
                "pub trait Measure: Sized",
                "fn area(&self, scale: u32) -> u64"
            ),
        ]
    );
    assert_eq!(listed.len(), 1);
}

/// The Rust the reference shows is either the SDK's stubs or one of [`EXAMPLES`], whole and as it is.
#[test]
fn the_reference_shows_the_examples_as_the_repository_builds_them() {
    let mut blocks = Vec::new();
    let mut lines = REFERENCE.lines();
    while let Some(line) = lines.next() {
        if line == "```rust" {
            let block: Vec<&str> = lines.by_ref().take_while(|l| *l != "```").collect();
            blocks.push(block.join("\n") + "\n");
        }
    }
    for block in &blocks {
        assert!(
            block.starts_with("// ksana_contract_sdk") || EXAMPLES.iter().any(|(_, file)| file == block),
            "a ```rust block of the reference is neither stubs nor one of the example files:\n{block}"
        );
    }
    for (path, file) in EXAMPLES {
        assert!(
            blocks.iter().any(|block| block == file),
            "the reference does not show {path} as it is"
        );
    }
}

/// The token the reference shows is a KS-20 token, by the standard's conformance kit.
#[test]
fn the_token_example_follows_ks20() {
    let holder = [7u8; 32];
    let outcome = conformance::ks20(|| {
        let init = encode(&("Token", "TKN", 8u8, 1_000_000u128));
        let token = mock::deploy(holder, 0, b"token", token::dispatch, &init).unwrap();
        conformance::Ks20Subject { token, holder }
    });
    assert_eq!(outcome, Ok(()));
}

/// The asset the reference shows: created with the capabilities to mint and to freeze, issued again, an account frozen.
#[test]
fn the_points_example_creates_issues_and_freezes() {
    mock::reset();
    let (deployer, contract, fan) = ([1; 32], [2; 32], [3; 32]);
    let id = mock::with_context(deployer, contract, || points::create_points(1_000));
    assert_eq!(id, asset_id(&contract, &[0; 32]));
    let again = mock::with_context(deployer, contract, || points::mint_points(&fan, 5));
    assert_eq!(again, id);
    mock::with_context(deployer, contract, || points::freeze_points(&fan, true));
    let info = mock::asset_info(&id).unwrap();
    assert_eq!(
        (info.name.as_str(), info.symbol.as_str(), info.decimals),
        ("Points", "PTS", 2)
    );
    assert_eq!(
        (info.total_supply, info.capabilities),
        (1_005, CAP_MINT | CAP_FREEZE)
    );
    assert_eq!(mock::balance(&id, &deployer), 1_000);
    assert_eq!(mock::balance(&id, &fan), 5);
    assert_eq!(mock::account_status(&id, &fan), STATUS_FROZEN);
}

/// Every method the token example's ABI block declares is one its `dispatch` answers, and it answers no other.
#[test]
fn the_token_example_answers_what_its_abi_block_declares() {
    mock::reset();
    let holder = [7u8; 32];
    let init = encode(&("Token", "TKN", 8u8, 1_000_000u128));
    let token = mock::deploy(holder, 0, b"token", token::dispatch, &init).unwrap();
    let declared: Vec<&str> = include_str!("reference/token.rs")
        .lines()
        .filter_map(|l| l.strip_prefix("//! "))
        .map(|l| l.trim_start_matches("payable "))
        .filter_map(|l| l.strip_prefix("call ").or_else(|| l.strip_prefix("view ")))
        .filter_map(|l| l.split('(').next())
        .collect();
    assert_eq!(declared.len(), 13);
    let unknown = |method: &str| {
        matches!(mock::call(holder, token, method, &[]),
            Err(mock::Failure::Revert { reason, .. }) if reason == b"unknown method")
    };
    for method in declared {
        assert!(!unknown(method), "{method}");
    }
    assert!(unknown("mint"));
}
