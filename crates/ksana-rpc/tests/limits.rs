//! The server's limits (F-29, D-132): a batch of more than `MAX_BATCH_LEN` calls is refused as a whole, a request body
//! longer than `MAX_REQUEST_BODY_BYTES` is not read, and a WebSocket connection holds at most
//! `MAX_SUBSCRIPTIONS_PER_CONNECTION` subscriptions.

mod common;

use std::net::SocketAddr;
use std::sync::Arc;

use common::{exchange, Empty};
use jsonrpsee::core::client::SubscriptionClientT;
use jsonrpsee::rpc_params;
use jsonrpsee::server::ServerHandle;
use jsonrpsee::ws_client::WsClientBuilder;
use ksana_rpc::{
    start_server, BlockSummary, Cors, MAX_BATCH_LEN, MAX_REQUEST_BODY_BYTES,
    MAX_SUBSCRIPTIONS_PER_CONNECTION,
};
use serde_json::Value;
use tokio::sync::broadcast;

fn start(rt: &tokio::runtime::Runtime) -> (SocketAddr, ServerHandle) {
    let backend = Arc::new(Empty(broadcast::channel(1).0));
    rt.block_on(start_server(
        "127.0.0.1:0".parse().unwrap(),
        backend,
        &Cors::Disabled,
    ))
    .unwrap()
}

fn post(addr: SocketAddr, body: &str) -> (String, Value) {
    let (head, body) = exchange(
        addr,
        "POST / HTTP/1.1\r\nContent-Type: application/json",
        body,
    );
    (head, serde_json::from_str(&body).unwrap_or(Value::Null))
}

fn batch(len: u32) -> String {
    let calls: Vec<String> = (0..len)
        .map(|id| {
            format!(
                r#"{{"jsonrpc":"2.0","id":{id},"method":"ksana_getBlock","params":["latest"]}}"#
            )
        })
        .collect();
    format!("[{}]", calls.join(","))
}

#[test]
fn a_batch_of_more_than_the_limit_is_refused_as_a_whole() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let (addr, _handle) = start(&rt);
    // At the limit every call is answered (here: not found)
    let (head, answers) = post(addr, &batch(MAX_BATCH_LEN));
    assert!(head.starts_with("http/1.1 200"), "{head}");
    let answers = answers.as_array().expect("a list of answers");
    assert_eq!(answers.len(), MAX_BATCH_LEN as usize);
    assert!(answers.iter().all(|a| a["error"]["code"] == -32004));
    // One call more: one error for the whole batch, jsonrpsee's -32010 with id null, and no call is made
    let (_, refused) = post(addr, &batch(MAX_BATCH_LEN + 1));
    assert_eq!(refused["error"]["code"], -32010, "{refused}");
    assert_eq!(refused["id"], Value::Null);
}

#[test]
fn a_request_body_over_the_limit_is_not_read() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let (addr, _handle) = start(&rt);
    let call = |len: usize| {
        let empty = r#"{"jsonrpc":"2.0","id":1,"method":"ksana_getBlock","params":[""]}"#;
        let padding = "0".repeat(len - empty.len());
        format!(r#"{{"jsonrpc":"2.0","id":1,"method":"ksana_getBlock","params":["{padding}"]}}"#)
    };
    let limit = MAX_REQUEST_BODY_BYTES as usize;
    // A body exactly at the limit is read: the call is answered (a block ID that long is malformed)
    let body = call(limit);
    assert_eq!(body.len(), limit);
    let (head, answer) = post(addr, &body);
    assert!(head.starts_with("http/1.1 200"), "{head}");
    assert_eq!(answer["error"]["code"], -32602, "{answer}");
    // One byte over: refused before it is read, HTTP 413 and -32007
    let (head, answer) = post(addr, &call(limit + 1));
    assert!(head.starts_with("http/1.1 413"), "{head}");
    assert_eq!(answer["error"]["code"], -32007, "{answer}");
}

#[test]
fn a_websocket_connection_holds_a_limited_number_of_subscriptions() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let (addr, _handle) = start(&rt);
    rt.block_on(async {
        let client = WsClientBuilder::default()
            .build(format!("ws://{addr}"))
            .await
            .unwrap();
        let mut held = Vec::new();
        for _ in 0..MAX_SUBSCRIPTIONS_PER_CONNECTION {
            let subscription = client
                .subscribe::<BlockSummary, _>(
                    "ksana_subscribeBlocks",
                    rpc_params![],
                    "ksana_unsubscribeBlocks",
                )
                .await
                .unwrap();
            held.push(subscription);
        }
        let refused = client
            .subscribe::<BlockSummary, _>(
                "ksana_subscribeBlocks",
                rpc_params![],
                "ksana_unsubscribeBlocks",
            )
            .await;
        match refused {
            Err(jsonrpsee::core::client::Error::Call(e)) => assert_eq!(e.code(), -32006, "{e}"),
            other => panic!("a subscription over the limit: {other:?}"),
        }
        // Another connection has its own
        let other = WsClientBuilder::default()
            .build(format!("ws://{addr}"))
            .await
            .unwrap();
        other
            .subscribe::<BlockSummary, _>(
                "ksana_subscribeBlocks",
                rpc_params![],
                "ksana_unsubscribeBlocks",
            )
            .await
            .unwrap();
    });
}
