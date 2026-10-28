//! Wire format: frames and codec of the sync protocol, decoding of gossip messages, the server's count / length
//! limits, locator, work comparison, node key.

use std::io;
use std::sync::Mutex;

use ksana_p2p::wire::{
    self, Request, Response, SyncCodec, MAX_BLOCKS_PER_REQUEST, MAX_GOSSIP_BYTES, MAX_LOCATOR_LEN,
    MAX_MEMPOOL_OPS, MAX_MEMPOOL_RESPONSE_BYTES, MAX_REQUEST_BYTES, MAX_STATUS_RESPONSE_BYTES,
    MAX_SYNC_MESSAGE_BYTES,
};
use ksana_p2p::{load_or_create_key, Handler, OpMessage, P2pError, PeerId, Status, Verdict};
use ksana_primitives::codec::CodecError;
use ksana_primitives::{
    Action, Block, BlockHeader, BtcAnchor, FeeMode, SettlementTxs, SigScheme, VmOp,
};
use libp2p::futures::executor::block_on;
use libp2p::futures::io::Cursor;
use libp2p::request_response::Codec;

/// The devnet's `MAX_OP_BYTES` (every network has the same).
const MAX_OP_BYTES: usize = ksana_primitives::params::DEVNET.max_op_bytes;

fn block(height: u64, settlement_len: usize) -> Block {
    Block {
        header: BlockHeader {
            version: 1,
            height,
            parents: vec![[height as u8; 32]],
            btc_anchor: BtcAnchor {
                height: 7,
                hash: [8u8; 32],
            },
            entries_root: [1u8; 32],
            state_root: [2u8; 32],
            producer: [3u8; 32],
            timestamp: 1_700_000_000 + height,
            bits: 0x207f_ffff,
            nonce: height,
        },
        entries: Vec::new(),
        // The reveal transaction takes most of the length (D-081); the lengths of the two transactions add up to
        // settlement_len
        settlement: Some(SettlementTxs {
            commit: vec![0x5a; settlement_len.min(64)],
            reveal: vec![0x5a; settlement_len.saturating_sub(64)],
        }),
    }
}

fn op_message(nonce: u64) -> OpMessage {
    op_with_args(nonce, vec![1, 2, 3])
}

fn op_with_args(nonce: u64, args: Vec<u8>) -> OpMessage {
    OpMessage {
        op: VmOp {
            version: 1,
            chain_id: 2,
            account: [5u8; 32],
            nonce,
            fee: FeeMode::Token { amount: 2_000 },
            max_gas: 50_000,
            action: Action::Call {
                contract: [7u8; 32],
                method: "transfer".to_string(),
                args,
                funds: Vec::new(),
            },
            sig: SigScheme::Bip340([4u8; 64]),
        },
    }
}

fn status() -> Status {
    Status {
        genesis: [9u8; 32],
        tip_height: 42,
        tip_hash: [10u8; 32],
        tip_work: [11u8; 32],
        tips: vec![[10u8; 32], [12u8; 32]],
    }
}

fn write_request(request: Request) -> Vec<u8> {
    let mut io = Cursor::new(Vec::new());
    block_on(SyncCodec::default().write_request(&wire::SYNC_PROTOCOL, &mut io, request)).unwrap();
    io.into_inner()
}

fn write_response(response: Response) -> Vec<u8> {
    let mut io = Cursor::new(Vec::new());
    block_on(SyncCodec::default().write_response(&wire::SYNC_PROTOCOL, &mut io, response)).unwrap();
    io.into_inner()
}

fn read_request(bytes: Vec<u8>) -> (io::Result<Request>, u64) {
    let mut io = Cursor::new(bytes);
    let result = block_on(SyncCodec::default().read_request(&wire::SYNC_PROTOCOL, &mut io));
    (result, io.position())
}

fn read_response(bytes: Vec<u8>) -> (io::Result<Response>, u64) {
    read_response_to(None, bytes)
}

/// Read a response with the codec that sent `request` (as request-response does, one codec per stream), or with a
/// fresh codec.
fn read_response_to(request: Option<Request>, bytes: Vec<u8>) -> (io::Result<Response>, u64) {
    let mut codec = SyncCodec::default();
    if let Some(request) = request {
        let mut sink = Cursor::new(Vec::new());
        block_on(codec.write_request(&wire::SYNC_PROTOCOL, &mut sink, request)).unwrap();
    }
    let mut io = Cursor::new(bytes);
    let result = block_on(codec.read_response(&wire::SYNC_PROTOCOL, &mut io));
    (result, io.position())
}

fn frame(body: &[u8]) -> Vec<u8> {
    [&(body.len() as u32).to_le_bytes()[..], body].concat()
}

#[test]
fn requests_and_responses_round_trip() {
    let requests = vec![
        Request::Status,
        Request::GetBlocks {
            locator: vec![[1u8; 32], [2u8; 32]],
            after: None,
            max: 500,
        },
        Request::GetBlocks {
            locator: vec![[1u8; 32]],
            after: Some([4u8; 32]),
            max: 7,
        },
        Request::GetBlock { hash: [3u8; 32] },
        Request::GetMempool { max: 1000 },
    ];
    for request in requests {
        let bytes = write_request(request.clone());
        // Frame = 4-byte little-endian length ‖ Borsh.
        let body = borsh::to_vec(&request).unwrap();
        assert_eq!(bytes, frame(&body));
        let (decoded, consumed) = read_request(bytes.clone());
        assert_eq!(decoded.unwrap(), request);
        assert_eq!(consumed, bytes.len() as u64);
    }
    let responses = vec![
        Response::Status(status()),
        Response::Blocks(vec![block(1, 10), block(2, 0)]),
        Response::Block(Some(block(3, 5))),
        Response::Block(None),
        Response::Mempool(vec![op_message(1), op_message(2)]),
    ];
    for response in responses {
        let bytes = write_response(response.clone());
        assert_eq!(bytes, frame(&borsh::to_vec(&response).unwrap()));
        assert_eq!(read_response(bytes).0.unwrap(), response);
    }
}

#[test]
fn request_encoding_is_fixed() {
    // The variant index is the format: Status = 0, GetBlocks = 1, GetBlock = 2, GetMempool = 3.
    assert_eq!(write_request(Request::Status), vec![1, 0, 0, 0, 0]);
    assert_eq!(
        write_request(Request::GetMempool { max: 1000 }),
        vec![5, 0, 0, 0, 3, 0xe8, 0x03, 0, 0]
    );
    // GetBlocks: variant ‖ locator (4-byte count ‖ items) ‖ after (1-byte tag ‖ hash) ‖ max
    let get_blocks = write_request(Request::GetBlocks {
        locator: vec![[0xaa; 32]],
        after: None,
        max: 500,
    });
    assert_eq!(&get_blocks[..9], &[42, 0, 0, 0, 1, 1, 0, 0, 0]);
    assert_eq!(get_blocks[41], 0);
    assert_eq!(&get_blocks[42..], &500u32.to_le_bytes());
    let get_blocks = write_request(Request::GetBlocks {
        locator: vec![],
        after: Some([0xbb; 32]),
        max: 1,
    });
    assert_eq!(&get_blocks[..10], &[42, 0, 0, 0, 1, 0, 0, 0, 0, 1]);
    assert_eq!(&get_blocks[10..42], &[0xbb; 32]);
    assert_eq!(&get_blocks[42..], &1u32.to_le_bytes());
}

#[test]
fn status_encoding_is_fixed() {
    // Status: genesis ‖ tip_height ‖ tip_hash ‖ tip_work ‖ tips (4-byte count ‖ items)
    let bytes = borsh::to_vec(&status()).unwrap();
    assert_eq!(bytes.len(), 32 + 8 + 32 + 32 + 4 + 2 * 32);
    assert_eq!(&bytes[..32], &[9u8; 32]);
    assert_eq!(&bytes[32..40], &42u64.to_le_bytes());
    assert_eq!(&bytes[104..108], &2u32.to_le_bytes());
    assert_eq!(&bytes[140..], &[12u8; 32]);
}

#[test]
fn oversized_frames_are_rejected_before_reading_the_body() {
    for len in [MAX_SYNC_MESSAGE_BYTES as u32 + 1, u32::MAX] {
        // Only the length prefix, no content: trying to read the content would give UnexpectedEof instead of
        // InvalidData.
        let (result, consumed) = read_request(len.to_le_bytes().to_vec());
        let err = result.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData, "{err}");
        assert_eq!(consumed, 4);
        let (result, consumed) = read_response(len.to_le_bytes().to_vec());
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::InvalidData);
        assert_eq!(consumed, 4);
    }
    // A request has a limit of its own (D-132): one byte over it is malformed, which costs the peer score
    let (result, consumed) = read_request((MAX_REQUEST_BYTES as u32 + 1).to_le_bytes().to_vec());
    assert_eq!(result.unwrap_err().kind(), io::ErrorKind::InvalidData);
    assert_eq!(consumed, 4);
    // A length exactly at the limit is not rejected by the length check (the content is incomplete here, hence
    // UnexpectedEof).
    let (result, _) = read_request((MAX_REQUEST_BYTES as u32).to_le_bytes().to_vec());
    assert_eq!(result.unwrap_err().kind(), io::ErrorKind::UnexpectedEof);
    let (result, _) = read_response((MAX_SYNC_MESSAGE_BYTES as u32).to_le_bytes().to_vec());
    assert_eq!(result.unwrap_err().kind(), io::ErrorKind::UnexpectedEof);
}

#[test]
fn responses_are_limited_by_the_request_they_answer() {
    // The limits of the responses to Status and GetMempool are smaller (D-132)
    let limits = [
        (Request::Status, MAX_STATUS_RESPONSE_BYTES),
        (
            Request::GetMempool { max: 1000 },
            MAX_MEMPOOL_RESPONSE_BYTES,
        ),
        (
            Request::GetBlock { hash: [1u8; 32] },
            MAX_SYNC_MESSAGE_BYTES,
        ),
        (
            Request::GetBlocks {
                locator: Vec::new(),
                after: None,
                max: 1,
            },
            MAX_SYNC_MESSAGE_BYTES,
        ),
    ];
    for (request, limit) in limits {
        assert_eq!(wire::response_limit(&request), limit);
        // At the limit the length passes (the content is incomplete here)
        let (result, _) =
            read_response_to(Some(request.clone()), (limit as u32).to_le_bytes().to_vec());
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::UnexpectedEof);
        // One byte over is refused before the content is read; within MAX_SYNC_MESSAGE_BYTES the refusal is no
        // malformed response (a peer of an earlier version sends one) and costs no score
        let over = (limit as u32 + 1).to_le_bytes().to_vec();
        let (result, consumed) = read_response_to(Some(request.clone()), over);
        let err = result.unwrap_err();
        let expected = if limit < MAX_SYNC_MESSAGE_BYTES {
            io::ErrorKind::Other
        } else {
            io::ErrorKind::InvalidData
        };
        assert_eq!(err.kind(), expected, "{request:?}: {err}");
        assert_eq!(consumed, 4);
    }
    // A response within its limit is read as before
    let bytes = write_response(Response::Status(status()));
    let (result, _) = read_response_to(Some(Request::Status), bytes);
    assert_eq!(result.unwrap(), Response::Status(status()));
}

#[test]
fn oversized_requests_are_not_written() {
    // A locator of 2 100 hashes is longer than any request may be
    let request = Request::GetBlocks {
        locator: vec![[7u8; 32]; 2100],
        after: None,
        max: 1,
    };
    assert!(borsh::to_vec(&request).unwrap().len() > MAX_REQUEST_BYTES);
    let mut io = Cursor::new(Vec::new());
    let err = block_on(SyncCodec::default().write_request(&wire::SYNC_PROTOCOL, &mut io, request))
        .unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    assert!(io.into_inner().is_empty());
    // The longest locator a node sends fits easily
    let request = Request::GetBlocks {
        locator: vec![[7u8; 32]; wire::locator_heights(u64::MAX).len()],
        after: Some([8u8; 32]),
        max: MAX_BLOCKS_PER_REQUEST,
    };
    assert!(borsh::to_vec(&request).unwrap().len() < MAX_REQUEST_BYTES / 16);
}

#[test]
fn oversized_messages_are_not_written() {
    let mut io = Cursor::new(Vec::new());
    let payload = vec![0u8; MAX_SYNC_MESSAGE_BYTES + 1];
    let err = block_on(wire::write_frame(&mut io, &payload, MAX_SYNC_MESSAGE_BYTES)).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    assert!(io.into_inner().is_empty());
    // A response whose encoding exceeds the limit is not written either.
    let response = Response::Blocks(vec![block(1, MAX_SYNC_MESSAGE_BYTES)]);
    let mut io = Cursor::new(Vec::new());
    let err =
        block_on(SyncCodec::default().write_response(&wire::SYNC_PROTOCOL, &mut io, response))
            .unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    assert!(io.into_inner().is_empty());
}

#[test]
fn malformed_borsh_is_rejected() {
    let cases: Vec<Vec<u8>> = vec![
        // Nonexistent variant.
        vec![9],
        // Trailing byte after Status.
        vec![0, 0],
        // GetBlocks locator declares 1 item but has no content.
        vec![1, 1, 0, 0, 0],
        // GetMempool max has only 2 bytes.
        vec![3, 1, 2],
        // Empty content.
        vec![],
    ];
    for body in cases {
        let (result, _) = read_request(frame(&body));
        assert_eq!(
            result.unwrap_err().kind(),
            io::ErrorKind::InvalidData,
            "{body:?}"
        );
    }
    // The response declares 2^32 - 1 blocks: decoding fails without allocating memory for the declared count.
    let (result, _) = read_response(frame(&[1, 0xff, 0xff, 0xff, 0xff]));
    assert_eq!(result.unwrap_err().kind(), io::ErrorKind::InvalidData);
    // Status response missing fields.
    let (result, _) = read_response(frame(&[0, 1, 2, 3]));
    assert_eq!(result.unwrap_err().kind(), io::ErrorKind::InvalidData);
}

#[test]
fn truncated_frames_are_rejected() {
    let mut bytes = 10u32.to_le_bytes().to_vec();
    bytes.extend_from_slice(&[0, 1, 2]);
    let (result, _) = read_request(bytes);
    assert_eq!(result.unwrap_err().kind(), io::ErrorKind::UnexpectedEof);
    let (result, _) = read_request(vec![1, 0]);
    assert_eq!(result.unwrap_err().kind(), io::ErrorKind::UnexpectedEof);
}

#[test]
fn gossip_messages_check_the_length_limit_first() {
    let b = block(5, 100);
    let bytes = borsh::to_vec(&b).unwrap();
    assert_eq!(wire::decode_block(&bytes).unwrap(), b);
    let m = op_message(3);
    let bytes = borsh::to_vec(&m).unwrap();
    assert_eq!(wire::decode_op(&bytes, MAX_OP_BYTES).unwrap(), m);

    let oversized = vec![0u8; MAX_GOSSIP_BYTES + 1];
    assert!(matches!(
        wire::decode_block(&oversized),
        Err(CodecError::TooLong { .. })
    ));
    assert!(matches!(
        wire::decode_op(&oversized, MAX_OP_BYTES),
        Err(CodecError::TooLong { .. })
    ));
    // An op is limited by the network's MAX_OP_BYTES, not by the gossip limit (D-132): an op exactly that long
    // decodes, one byte longer is refused before decoding
    let short = borsh::to_vec(&op_with_args(4, Vec::new())).unwrap().len();
    let longest = op_with_args(4, vec![0x33; MAX_OP_BYTES - short]);
    let bytes = borsh::to_vec(&longest).unwrap();
    assert_eq!(bytes.len(), MAX_OP_BYTES);
    assert_eq!(wire::decode_op(&bytes, MAX_OP_BYTES).unwrap(), longest);
    let longer = borsh::to_vec(&op_with_args(4, vec![0x33; MAX_OP_BYTES - short + 1])).unwrap();
    assert!(matches!(
        wire::decode_op(&longer, MAX_OP_BYTES),
        Err(CodecError::TooLong { .. })
    ));
    // Undecodable, and trailing bytes.
    assert!(matches!(
        wire::decode_block(&[1, 2, 3]),
        Err(CodecError::Decode(_))
    ));
    let mut trailing = borsh::to_vec(&b).unwrap();
    trailing.push(0);
    assert!(matches!(
        wire::decode_block(&trailing),
        Err(CodecError::Decode(_))
    ));
    let mut bad_fee = borsh::to_vec(&m).unwrap();
    // The fee mode variant index can only be 0 or 1 (after 1 + 4 + 32 + 8 bytes).
    bad_fee[45] = 2;
    assert!(matches!(
        wire::decode_op(&bad_fee, MAX_OP_BYTES),
        Err(CodecError::Decode(_))
    ));
}

#[test]
fn op_message_encoding_is_the_op() {
    let m = op_message(1);
    assert_eq!(borsh::to_vec(&m).unwrap(), borsh::to_vec(&m.op).unwrap());
}

#[test]
fn work_is_compared_as_little_endian_256_bit_integers() {
    let mut a = [0u8; 32];
    let mut b = [0u8; 32];
    assert!(!wire::work_gt(&a, &b));
    a[0] = 1;
    assert!(wire::work_gt(&a, &b));
    assert!(!wire::work_gt(&b, &a));
    // The high byte decides the ordering.
    b[31] = 1;
    assert!(wire::work_gt(&b, &a));
    a[31] = 1;
    assert!(wire::work_gt(&a, &b));
    a = [0xff; 32];
    a[31] = 0;
    b = [0u8; 32];
    b[31] = 1;
    assert!(wire::work_gt(&b, &a));
}

#[test]
fn locator_heights_are_dense_then_exponential() {
    assert_eq!(wire::locator_heights(0), vec![0]);
    assert_eq!(wire::locator_heights(5), vec![5, 4, 3, 2, 1, 0]);
    assert_eq!(wire::locator_heights(9), vec![9, 8, 7, 6, 5, 4, 3, 2, 1, 0]);
    assert_eq!(
        wire::locator_heights(100),
        vec![100, 99, 98, 97, 96, 95, 94, 93, 92, 91, 89, 85, 77, 61, 29, 0]
    );
    let long = wire::locator_heights(u64::MAX);
    assert_eq!(long.first(), Some(&u64::MAX));
    assert_eq!(long.last(), Some(&0));
    assert!(long.len() <= 10 + 64);
    assert!(long.windows(2).all(|w| w[0] > w[1]));
}

/// Request parameters recorded by Canned: (locator length, after, max).
type Seen = (usize, Option<[u8; 32]>, u32);

/// A Handler for server-side tests: records the parameters received and returns preset data (ignoring `max`).
#[derive(Default)]
struct Canned {
    blocks: Vec<Block>,
    ops: Vec<OpMessage>,
    /// When non-empty, Status carries these tips.
    tips: Vec<[u8; 32]>,
    seen: Mutex<Vec<Seen>>,
}

impl Handler for Canned {
    fn status(&self) -> Status {
        let mut status = status();
        if !self.tips.is_empty() {
            status.tips = self.tips.clone();
        }
        status
    }
    fn locator(&self) -> Vec<[u8; 32]> {
        Vec::new()
    }
    fn blocks_after(&self, locator: &[[u8; 32]], after: Option<&[u8; 32]>, max: u32) -> Vec<Block> {
        self.seen
            .lock()
            .unwrap()
            .push((locator.len(), after.copied(), max));
        self.blocks.clone()
    }
    fn unknown(&self, hashes: &[[u8; 32]]) -> Vec<[u8; 32]> {
        hashes.to_vec()
    }
    fn missing_parents(&self) -> Vec<[u8; 32]> {
        Vec::new()
    }
    fn block(&self, hash: &[u8; 32]) -> Option<Block> {
        self.blocks
            .iter()
            .find(|b| b.header.hash() == *hash)
            .cloned()
    }
    fn mempool(&self, max: u32) -> Vec<OpMessage> {
        self.seen.lock().unwrap().push((0, None, max));
        self.ops.clone()
    }
    fn on_block(&self, _: PeerId, _: Block, _: bool) -> Verdict {
        Verdict::Ignore
    }
    fn on_op(&self, _: PeerId, _: OpMessage) -> Verdict {
        Verdict::Ignore
    }
}

#[test]
fn serving_caps_counts_and_locator_length() {
    let handler = Canned {
        blocks: (1..=800).map(|h| block(h, 0)).collect(),
        ops: (0..1500).map(op_message).collect(),
        ..Canned::default()
    };
    let response = wire::serve(
        &handler,
        Request::GetBlocks {
            locator: vec![[0u8; 32]; 1000],
            after: None,
            max: 100_000,
        },
    );
    let Response::Blocks(blocks) = response else {
        panic!("wrong response type");
    };
    assert_eq!(blocks.len(), MAX_BLOCKS_PER_REQUEST as usize);
    assert_eq!(blocks[0].header.height, 1);
    let response = wire::serve(
        &handler,
        Request::GetBlocks {
            locator: vec![[0u8; 32]; 3],
            after: Some([5u8; 32]),
            max: 7,
        },
    );
    assert!(matches!(response, Response::Blocks(b) if b.len() == 7));
    let response = wire::serve(&handler, Request::GetMempool { max: u32::MAX });
    assert!(matches!(response, Response::Mempool(ops) if ops.len() == MAX_MEMPOOL_OPS as usize));
    assert_eq!(
        *handler.seen.lock().unwrap(),
        vec![
            (MAX_LOCATOR_LEN, None, MAX_BLOCKS_PER_REQUEST),
            (3, Some([5u8; 32]), 7),
            (0, None, MAX_MEMPOOL_OPS)
        ]
    );
    assert_eq!(
        wire::serve(&handler, Request::Status),
        Response::Status(status())
    );
    // Status carries at most MAX_STATUS_TIPS tips
    let many = Canned {
        tips: (0..200u8).map(|i| [i; 32]).collect(),
        ..Canned::default()
    };
    let Response::Status(served) = wire::serve(&many, Request::Status) else {
        panic!("wrong response type");
    };
    assert_eq!(served.tips.len(), wire::MAX_STATUS_TIPS);
    let wanted = block(5, 0);
    assert_eq!(
        wire::serve(
            &handler,
            Request::GetBlock {
                hash: wanted.header.hash()
            }
        ),
        Response::Block(Some(wanted))
    );
    assert_eq!(
        wire::serve(&handler, Request::GetBlock { hash: [0u8; 32] }),
        Response::Block(None)
    );
}

#[test]
fn serving_truncates_responses_to_the_size_limit() {
    // Each block is about 15 MiB: a 32 MiB response only fits the first two.
    let big = 15 * 1024 * 1024;
    let handler = Canned {
        blocks: (1..=3).map(|h| block(h, big)).collect(),
        ..Canned::default()
    };
    let response = wire::serve(
        &handler,
        Request::GetBlocks {
            locator: Vec::new(),
            after: None,
            max: 500,
        },
    );
    let encoded = borsh::to_vec(&response).unwrap();
    assert!(encoded.len() <= MAX_SYNC_MESSAGE_BYTES);
    let Response::Blocks(blocks) = response else {
        panic!("wrong response type");
    };
    assert_eq!(
        blocks.iter().map(|b| b.header.height).collect::<Vec<_>>(),
        vec![1, 2]
    );
    // A single block that does not fit is treated as absent.
    let huge = Canned {
        blocks: vec![block(1, MAX_SYNC_MESSAGE_BYTES)],
        ..Canned::default()
    };
    let hash = huge.blocks[0].header.hash();
    assert_eq!(
        wire::serve(&huge, Request::GetBlock { hash }),
        Response::Block(None)
    );
}

#[test]
fn serving_truncates_the_mempool_to_its_response_limit() {
    // 100 ops of about 60 KiB: a GetMempool response of 4 MiB only takes the leading ones that fit (D-132)
    let handler = Canned {
        ops: (0..100)
            .map(|n| op_with_args(n, vec![0x44; 60 * 1024]))
            .collect(),
        ..Canned::default()
    };
    let response = wire::serve(&handler, Request::GetMempool { max: 1000 });
    let encoded = borsh::to_vec(&response).unwrap();
    assert!(encoded.len() <= MAX_MEMPOOL_RESPONSE_BYTES);
    let Response::Mempool(ops) = response else {
        panic!("wrong response type");
    };
    let each = borsh::to_vec(&handler.ops[0]).unwrap().len();
    assert_eq!(ops.len(), (MAX_MEMPOOL_RESPONSE_BYTES - 5) / each);
    assert_eq!(ops[..], handler.ops[..ops.len()]);
}

#[test]
fn node_key_is_created_once_with_private_permissions() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("p2p.key");
    let first = load_or_create_key(&path).unwrap();
    let second = load_or_create_key(&path).unwrap();
    assert_eq!(first.public().to_peer_id(), second.public().to_peer_id());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
    // No temporary file is left behind.
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);

    let corrupt = dir.path().join("corrupt.key");
    std::fs::write(&corrupt, b"not a key").unwrap();
    assert!(matches!(
        load_or_create_key(&corrupt),
        Err(P2pError::Key(_))
    ));
    let missing_dir = dir.path().join("missing").join("p2p.key");
    assert!(matches!(
        load_or_create_key(&missing_dir),
        Err(P2pError::Io { .. })
    ));
}
