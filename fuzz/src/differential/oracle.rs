//! The Node oracle: `fuzz/differential/oracle.mjs` run as a child process, one JSON line per request and one per
//! answer.

use std::fmt;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use serde_json::{json, Value};

use super::abi::{abi_block, abi_defs, ABI_EVENTS, ABI_TYPES};

/// The oracle could not be started or stopped answering.
#[derive(Debug)]
pub enum OracleError {
    /// `node` could not be started.
    Spawn(std::io::Error),
    /// Reading from or writing to the oracle failed.
    Io(std::io::Error),
    /// The oracle exited or answered something that is not an answer.
    Protocol(String),
}

impl fmt::Display for OracleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            OracleError::Spawn(e) => write!(
                f,
                "cannot start node (set KSANA_DIFF_NODE to its path): {e}"
            ),
            OracleError::Io(e) => write!(f, "talking to the oracle failed: {e}"),
            OracleError::Protocol(m) => write!(f, "the oracle failed: {m}"),
        }
    }
}

impl std::error::Error for OracleError {}

/// A running oracle.
pub struct Oracle {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    /// What the oracle said when it started: the SDK and the ABI codec it loaded.
    pub hello: Value,
}

impl Oracle {
    /// The oracle's script.
    pub fn script() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("differential/oracle.mjs")
    }

    /// Starts the oracle with `node` (or the program `KSANA_DIFF_NODE` names). It loads the SDK from `sdk/ts/dist`
    /// (build it first: `npm ci && npm run build` in `sdk/ts`), or from the `index.js` that `KSANA_DIFF_SDK` names,
    /// and the playground's ABI codec through the SDK's esbuild.
    pub fn start() -> Result<Oracle, OracleError> {
        let node = std::env::var_os("KSANA_DIFF_NODE").unwrap_or_else(|| "node".into());
        let mut child = Command::new(node)
            .arg(Self::script())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(OracleError::Spawn)?;
        let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
            return Err(OracleError::Protocol(
                "the oracle's pipes are missing".into(),
            ));
        };
        let mut oracle = Oracle {
            child,
            stdin,
            stdout: BufReader::new(stdout),
            hello: Value::Null,
        };
        // The structs, enums and events the ABI requests use are this crate's (`super::abi`), and they must be what the
        // playground's parser makes of their declarations
        let events: Value = serde_json::from_str(ABI_EVENTS)
            .map_err(|e| OracleError::Protocol(format!("ABI_EVENTS: {e}")))?;
        let types: Vec<Value> = ABI_TYPES.iter().map(|t| t.abi_json()).collect();
        oracle.hello = oracle.ask(&json!({
            "kind": "hello",
            "abi_defs": abi_defs(),
            "abi_events": events,
            "abi_block": abi_block(),
            "abi_types": types,
        }))?;
        match oracle.hello.get("abi_mismatches").and_then(Value::as_array) {
            Some(mismatches) if mismatches.is_empty() => Ok(oracle),
            Some(mismatches) => Err(OracleError::Protocol(format!(
                "the playground's parser reads the test's ABI types differently: {}",
                Value::from(mismatches.clone())
            ))),
            None => Err(OracleError::Protocol(format!(
                "the oracle's hello has no ABI check: {}",
                oracle.hello
            ))),
        }
    }

    /// Asks one request ([`super::Request::to_json`]).
    pub fn ask(&mut self, request: &Value) -> Result<Value, OracleError> {
        self.send(request)?;
        self.receive()
    }

    /// Asks several requests in one line; the answers in the same order.
    pub fn ask_all(&mut self, requests: &[Value]) -> Result<Vec<Value>, OracleError> {
        if requests.is_empty() {
            return Ok(Vec::new());
        }
        match self.ask(&json!({ "kind": "batch", "requests": requests }))? {
            Value::Array(answers) if answers.len() == requests.len() => Ok(answers),
            other => Err(OracleError::Protocol(format!(
                "a batch of {} requests got {other}",
                requests.len()
            ))),
        }
    }

    fn send(&mut self, request: &Value) -> Result<(), OracleError> {
        let mut line =
            serde_json::to_string(request).map_err(|e| OracleError::Protocol(e.to_string()))?;
        line.push('\n');
        self.stdin
            .write_all(line.as_bytes())
            .map_err(OracleError::Io)?;
        self.stdin.flush().map_err(OracleError::Io)
    }

    fn receive(&mut self) -> Result<Value, OracleError> {
        let mut line = String::new();
        if self.stdout.read_line(&mut line).map_err(OracleError::Io)? == 0 {
            return Err(OracleError::Protocol("the oracle exited".into()));
        }
        serde_json::from_str(&line).map_err(|e| OracleError::Protocol(format!("{e}: {line}")))
    }
}

impl Drop for Oracle {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
