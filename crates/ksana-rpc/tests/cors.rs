//! Cross-origin access to the RPC (D-055): `Cors::Any` and an origin list answer the browser's preflight request and
//! allow the origin in the call's response; disallowed by default.

mod common;

use std::net::SocketAddr;
use std::sync::Arc;

use common::{request, Empty};
use ksana_rpc::{start_server, Cors};
use tokio::sync::broadcast;

fn preflight(addr: SocketAddr, origin: &str) -> String {
    request(
        addr,
        &format!(
            "OPTIONS / HTTP/1.1\r\nOrigin: {origin}\r\nAccess-Control-Request-Method: POST\r\n\
             Access-Control-Request-Headers: content-type"
        ),
        "",
    )
}

fn call(addr: SocketAddr, origin: &str) -> String {
    request(
        addr,
        &format!("POST / HTTP/1.1\r\nOrigin: {origin}\r\nContent-Type: application/json"),
        r#"{"jsonrpc":"2.0","id":1,"method":"ksana_getBlock","params":["latest"]}"#,
    )
}

fn allowed_origin(headers: &str) -> Option<String> {
    headers
        .lines()
        .find_map(|l| l.strip_prefix("access-control-allow-origin:"))
        .map(|v| v.trim().to_string())
}

#[test]
fn cors_follows_the_configuration() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let start = |cors: Cors| {
        let backend = Arc::new(Empty(broadcast::channel(1).0));
        rt.block_on(start_server("127.0.0.1:0".parse().unwrap(), backend, &cors))
            .unwrap()
    };

    let (any, _h1) = start(Cors::Any);
    let headers = preflight(any, "https://wallet.example");
    assert!(headers.starts_with("http/1.1 200"), "{headers}");
    assert_eq!(allowed_origin(&headers).as_deref(), Some("*"));
    assert!(
        headers.contains("access-control-allow-methods"),
        "{headers}"
    );
    assert_eq!(
        allowed_origin(&call(any, "https://wallet.example")).as_deref(),
        Some("*")
    );

    let (listed, _h2) = start(Cors::parse("https://a.example, https://b.example").unwrap());
    assert_eq!(
        allowed_origin(&preflight(listed, "https://b.example")).as_deref(),
        Some("https://b.example")
    );
    assert_eq!(allowed_origin(&call(listed, "https://evil.example")), None);

    let (disabled, _h3) = start(Cors::Disabled);
    assert_eq!(
        allowed_origin(&preflight(disabled, "https://wallet.example")),
        None
    );
    assert_eq!(
        allowed_origin(&call(disabled, "https://wallet.example")),
        None
    );
    // Calls without Origin (command-line tools, server programs) work as usual
    let plain = request(
        disabled,
        "POST / HTTP/1.1\r\nContent-Type: application/json",
        r#"{"jsonrpc":"2.0","id":1,"method":"ksana_getBlock","params":["latest"]}"#,
    );
    assert!(plain.starts_with("http/1.1 200"), "{plain}");
}

#[test]
fn cors_values_are_parsed() {
    assert_eq!(Cors::parse("").unwrap(), Cors::Disabled);
    assert_eq!(Cors::parse(" * ").unwrap(), Cors::Any);
    assert_eq!(
        Cors::parse("https://a.example,https://b.example").unwrap(),
        Cors::Origins(vec!["https://a.example".into(), "https://b.example".into()])
    );
    assert!(Cors::parse("https://a.example\n").is_ok());
    assert!(Cors::parse("bad\u{7f}origin").is_err());
}
