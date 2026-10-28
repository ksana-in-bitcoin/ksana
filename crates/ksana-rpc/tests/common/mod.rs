//! The parts the server tests share: a backend that finds nothing, and raw HTTP requests.

#![allow(dead_code)]

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

use jsonrpsee::core::RpcResult;
use ksana_rpc::{
    codes, error, AccountJson, AssetJson, AssetStatusJson, Backend, BlockId, BlockJson,
    BlockSummary, CallRequestJson, ChainInfoJson, EstimateFeeJson, OpListJson, OpPosition,
    OpStatusJson, ReceiptJson, SettlementJson, SubmitResult,
};
use tokio::sync::broadcast;

/// A backend whose every query returns "not found": only used to check how the server answers.
pub struct Empty(pub broadcast::Sender<BlockSummary>);

pub fn absent<T>() -> RpcResult<T> {
    Err(error(codes::NOT_FOUND, "not found"))
}

impl Backend for Empty {
    fn submit_op(&self, _: Vec<u8>) -> RpcResult<SubmitResult> {
        absent()
    }
    fn op_status(&self, _: [u8; 32]) -> RpcResult<OpStatusJson> {
        absent()
    }
    fn block(&self, _: BlockId) -> RpcResult<BlockJson> {
        absent()
    }
    fn account(&self, _: [u8; 32]) -> RpcResult<AccountJson> {
        absent()
    }
    fn asset(&self, _: [u8; 32]) -> RpcResult<AssetJson> {
        absent()
    }
    fn asset_status(&self, _: [u8; 32], _: [u8; 32]) -> RpcResult<AssetStatusJson> {
        absent()
    }
    fn call(&self, _: CallRequestJson) -> RpcResult<ReceiptJson> {
        absent()
    }
    fn settlement(&self, _: BlockId) -> RpcResult<SettlementJson> {
        absent()
    }
    fn estimate_fee(&self) -> RpcResult<EstimateFeeJson> {
        absent()
    }
    fn chain_info(&self) -> RpcResult<ChainInfoJson> {
        absent()
    }
    fn account_ops(&self, _: [u8; 32], _: Option<OpPosition>, _: usize) -> RpcResult<OpListJson> {
        absent()
    }
    fn latest_ops(&self, _: Option<OpPosition>, _: usize) -> RpcResult<OpListJson> {
        absent()
    }
    fn subscribe_blocks(&self) -> broadcast::Receiver<BlockSummary> {
        self.0.subscribe()
    }
}

/// Send a raw HTTP request and return the response head in lowercase.
pub fn request(addr: SocketAddr, head: &str, body: &str) -> String {
    exchange(addr, head, body).0
}

/// Send a raw HTTP request and return the response head in lowercase and the body as it came.
pub fn exchange(addr: SocketAddr, head: &str, body: &str) -> (String, String) {
    let mut stream = TcpStream::connect(addr).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let text = format!(
        "{head}\r\nHost: {addr}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(text.as_bytes()).unwrap();
    let mut response = Vec::new();
    // The server may close the connection before reading all of a body it refuses
    let _ = stream.read_to_end(&mut response);
    let response = String::from_utf8_lossy(&response).into_owned();
    let (head, body) = response.split_once("\r\n\r\n").unwrap_or((&response, ""));
    (head.to_lowercase(), body.to_string())
}
