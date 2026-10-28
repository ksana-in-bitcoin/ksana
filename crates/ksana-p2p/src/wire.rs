//! Wire format (D-040).
//!
//! - gossipsub: a `/ft/1/blocks` message is Borsh(`Block`), length limit [`MAX_GOSSIP_BYTES`]; a `/ft/1/ops` message
//!   is Borsh([`OpMessage`]), length limit the network's `MAX_OP_BYTES` (no longer op can be valid, D-132).
//! - `/ksana/sync/2`: each request / response is one frame: 4-byte little-endian length ‖ Borsh([`Request`] /
//!   [`Response`]), length limit [`MAX_REQUEST_BYTES`] for a request and [`MAX_SYNC_MESSAGE_BYTES`] for a response,
//!   less for the responses to `Status` and `GetMempool` ([`response_limit`], D-132). The length is checked before the
//!   content is read, and reading does not pre-allocate memory according to the length the peer declares.
//!
//! All received bytes are untrusted: check the length limit before decoding and require the input to be fully consumed
//! (`ksana_primitives::codec::decode`).
//! These limits are local policy, not consensus.

use std::cmp::Ordering;
use std::io;

use borsh::{BorshDeserialize, BorshSerialize};
use ksana_primitives::codec::{self, CodecError};
use ksana_primitives::Block;
use libp2p::futures::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use libp2p::{request_response, StreamProtocol};

use crate::{OpMessage, Status};

/// The gossipsub topic for new blocks.
pub const BLOCKS_TOPIC: &str = "/ft/1/blocks";
/// The gossipsub topic for new ops.
pub const OPS_TOPIC: &str = "/ft/1/ops";
/// The sync protocol (request-response). The `Status` and `GetBlocks` of the block DAG (D-063) are incompatible with
/// `/ksana/sync/1`.
pub const SYNC_PROTOCOL: StreamProtocol = StreamProtocol::new("/ksana/sync/2");

/// Length limit of a gossip message (a Borsh-encoded block / op): 16 MiB.
pub const MAX_GOSSIP_BYTES: usize = 16 * 1024 * 1024;
/// Length limit of a sync response (excluding the 4-byte length prefix): 32 MiB.
pub const MAX_SYNC_MESSAGE_BYTES: usize = 32 * 1024 * 1024;
/// Length limit of a sync request (excluding the 4-byte length prefix): 64 KiB (D-132). The longest request is a
/// `GetBlocks`, whose locator has at most 10 + 64 items (the server uses only the first [`MAX_LOCATOR_LEN`]).
pub const MAX_REQUEST_BYTES: usize = 64 * 1024;
/// Length limit of a response to `Status`: 64 KiB (D-132), far more than [`MAX_STATUS_TIPS`] tips take.
pub const MAX_STATUS_RESPONSE_BYTES: usize = 64 * 1024;
/// Length limit of a response to `GetMempool`: 4 MiB (D-132). The server sends the leading ops that fit, at most
/// [`MAX_MEMPOOL_OPS`]; ops are broadcast by gossip anyway, the response only starts a newly connected peer's pool.
pub const MAX_MEMPOOL_RESPONSE_BYTES: usize = 4 * 1024 * 1024;
/// Maximum number of blocks returned by one GetBlocks (the server limit, and also the number we request).
pub const MAX_BLOCKS_PER_REQUEST: u32 = 500;
/// Maximum number of ops returned by one GetMempool (the server limit, and also the number we request).
pub const MAX_MEMPOOL_OPS: u32 = 1000;
/// The server uses only the first this many items of a locator (a normal locator has at most 10 + 64 items).
pub const MAX_LOCATOR_LEN: usize = 128;
/// Maximum number of DAG tips carried by a `Status` (the receiver ignores any extra ones).
pub const MAX_STATUS_TIPS: usize = 64;

/// A `/ksana/sync/2` request.
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub enum Request {
    /// Ask for the peer's sync status.
    Status,
    /// Fetch, by locator and cursor, the blocks in the peer's DAG we may lack (see [`crate::Handler::blocks_after`]).
    GetBlocks {
        /// The locator of our main chain.
        locator: Vec<[u8; 32]>,
        /// Cursor: the last block of the previous batch (`None` for the first batch).
        after: Option<[u8; 32]>,
        /// Maximum number of blocks to return (the server caps it at [`MAX_BLOCKS_PER_REQUEST`]).
        max: u32,
    },
    /// Fetch one block by hash.
    GetBlock {
        /// The block hash.
        hash: [u8; 32],
    },
    /// Fetch the pending ops in the peer's entry pool.
    GetMempool {
        /// Maximum number of ops to return (the server caps it at [`MAX_MEMPOOL_OPS`]).
        max: u32,
    },
}

/// A `/ksana/sync/2` response; the variants correspond one-to-one to [`Request`].
#[derive(Clone, Debug, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub enum Response {
    /// Response to `Status`.
    Status(Status),
    /// Response to `GetBlocks`: ascending by `(blue_work, hash)`; truncated when the total length exceeds the limit
    /// (only the leading blocks that fit are returned).
    Blocks(Vec<Block>),
    /// Response to `GetBlock`.
    Block(Option<Block>),
    /// Response to `GetMempool`.
    Mempool(Vec<OpMessage>),
}

/// Decode a `/ft/1/blocks` message: check [`MAX_GOSSIP_BYTES`] first, require full consumption.
pub fn decode_block(data: &[u8]) -> Result<Block, CodecError> {
    codec::decode(data, MAX_GOSSIP_BYTES)
}

/// Decode a `/ft/1/ops` message: check the network's `MAX_OP_BYTES` (`max_op_bytes`) first, require full consumption.
/// The message is the encoding of the op, and an op whose encoding is longer is invalid (D-132).
pub fn decode_op(data: &[u8], max_op_bytes: usize) -> Result<OpMessage, CodecError> {
    codec::decode(data, max_op_bytes)
}

/// The length limit of the response to `request` (D-132): [`MAX_STATUS_RESPONSE_BYTES`] for `Status`,
/// [`MAX_MEMPOOL_RESPONSE_BYTES`] for `GetMempool`, [`MAX_SYNC_MESSAGE_BYTES`] for the others.
pub fn response_limit(request: &Request) -> usize {
    match request {
        Request::Status => MAX_STATUS_RESPONSE_BYTES,
        Request::GetMempool { .. } => MAX_MEMPOOL_RESPONSE_BYTES,
        Request::GetBlocks { .. } | Request::GetBlock { .. } => MAX_SYNC_MESSAGE_BYTES,
    }
}

/// Compare two cumulative works (256-bit unsigned integers, 32 little-endian bytes): `a > b`.
pub fn work_gt(a: &[u8; 32], b: &[u8; 32]) -> bool {
    a.iter().rev().cmp(b.iter().rev()) == Ordering::Greater
}

/// The height sequence of a locator: starting from `tip`, the first 10 consecutive, then at doubling intervals, the
/// last one being 0 (the genesis block).
pub fn locator_heights(tip: u64) -> Vec<u64> {
    let mut heights = Vec::new();
    let mut height = tip;
    let mut step: u64 = 1;
    loop {
        heights.push(height);
        if height == 0 {
            return heights;
        }
        if heights.len() >= 10 {
            step = step.saturating_mul(2);
        }
        height = height.saturating_sub(step);
    }
}

/// Read one frame: 4-byte little-endian length ‖ content. When the length exceeds `max`, the content is not read and
/// `InvalidData` is returned right away.
pub async fn read_frame<T>(io: &mut T, max: usize) -> io::Result<Vec<u8>>
where
    T: AsyncRead + Unpin,
{
    read_frame_within(io, max, max).await
}

/// Read one frame of at most `max` bytes that is expected to be at most `expected` long: a longer frame is not read
/// either, but within `max` it is refused with `io::ErrorKind::Other` rather than `InvalidData` (D-132: a response
/// longer than its request's limit costs the peer no score, a peer of an earlier version sends one).
async fn read_frame_within<T>(io: &mut T, expected: usize, max: usize) -> io::Result<Vec<u8>>
where
    T: AsyncRead + Unpin,
{
    let mut prefix = [0u8; 4];
    io.read_exact(&mut prefix).await?;
    let len = u32::from_le_bytes(prefix);
    let len_usize = usize::try_from(len)
        .map_err(|_| invalid_data(format!("frame length {len} is not representable")))?;
    if len_usize > max {
        return Err(invalid_data(format!(
            "frame length {len} exceeds the limit {max}"
        )));
    }
    if len_usize > expected {
        return Err(io::Error::other(format!(
            "frame length {len} exceeds the {expected} bytes expected"
        )));
    }
    // Grow the buffer as data arrives: a peer that declares a length but does not send does not tie up that much
    // memory.
    let mut buf = Vec::new();
    io.take(u64::from(len)).read_to_end(&mut buf).await?;
    if buf.len() != len_usize {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            format!(
                "incomplete frame: {len} bytes declared, {} bytes received",
                buf.len()
            ),
        ));
    }
    Ok(buf)
}

/// Write one frame. When the content exceeds `max`, no byte is written and `InvalidInput` is returned.
pub async fn write_frame<T>(io: &mut T, payload: &[u8], max: usize) -> io::Result<()>
where
    T: AsyncWrite + Unpin,
{
    if payload.len() > max {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("frame length {} exceeds the limit {max}", payload.len()),
        ));
    }
    let len = u32::try_from(payload.len()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "frame length exceeds u32".to_string(),
        )
    })?;
    io.write_all(&len.to_le_bytes()).await?;
    io.write_all(payload).await?;
    Ok(())
}

/// Read one frame of at most `expected` bytes (within `max`, see [`read_frame_within`]) and decode it (check the length
/// limit first, require full consumption).
async fn read_message<T, M>(io: &mut T, expected: usize, max: usize) -> io::Result<M>
where
    T: AsyncRead + Unpin,
    M: BorshDeserialize,
{
    let bytes = read_frame_within(io, expected, max).await?;
    codec::decode(&bytes, max).map_err(|e| invalid_data(e.to_string()))
}

/// Encode and write one frame of at most `max` bytes.
async fn write_message<T, M>(io: &mut T, message: &M, max: usize) -> io::Result<()>
where
    T: AsyncWrite + Unpin,
    M: BorshSerialize,
{
    let bytes = codec::encode(message)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e.to_string()))?;
    write_frame(io, &bytes, max).await
}

fn invalid_data(message: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

/// The `/ksana/sync/2` codec: length prefix + Borsh, with [`MAX_REQUEST_BYTES`] as the limit for requests and
/// [`MAX_SYNC_MESSAGE_BYTES`] for responses. A codec that wrote a request reads its response within the request's
/// [`response_limit`] (libp2p uses one codec per stream).
///
/// Oversized frames and undecodable content both return `io::ErrorKind::InvalidData` (the network layer penalizes the
/// peer on that basis); a response longer than its request's limit but within [`MAX_SYNC_MESSAGE_BYTES`] returns
/// `io::ErrorKind::Other` without being read (no penalty).
#[derive(Clone, Copy, Debug)]
pub struct SyncCodec {
    /// The length limit of the response to the request written last.
    response_limit: usize,
}

impl Default for SyncCodec {
    fn default() -> Self {
        SyncCodec {
            response_limit: MAX_SYNC_MESSAGE_BYTES,
        }
    }
}

impl request_response::Codec for SyncCodec {
    type Protocol = StreamProtocol;
    type Request = Request;
    type Response = Response;

    async fn read_request<T>(&mut self, _: &StreamProtocol, io: &mut T) -> io::Result<Request>
    where
        T: AsyncRead + Unpin + Send,
    {
        read_message(io, MAX_REQUEST_BYTES, MAX_REQUEST_BYTES).await
    }

    async fn read_response<T>(&mut self, _: &StreamProtocol, io: &mut T) -> io::Result<Response>
    where
        T: AsyncRead + Unpin + Send,
    {
        read_message(io, self.response_limit, MAX_SYNC_MESSAGE_BYTES).await
    }

    async fn write_request<T>(
        &mut self,
        _: &StreamProtocol,
        io: &mut T,
        request: Request,
    ) -> io::Result<()>
    where
        T: AsyncWrite + Unpin + Send,
    {
        self.response_limit = response_limit(&request);
        write_message(io, &request, MAX_REQUEST_BYTES).await
    }

    async fn write_response<T>(
        &mut self,
        _: &StreamProtocol,
        io: &mut T,
        response: Response,
    ) -> io::Result<()>
    where
        T: AsyncWrite + Unpin + Send,
    {
        write_message(io, &response, MAX_SYNC_MESSAGE_BYTES).await
    }
}

/// The request type used inside the network layer: an undecodable inbound request cannot be returned as an IO error
/// (request-response silently drops `read_request` errors and the event loop would have no way to know which peer sent
/// it), so it is passed up as [`RequestFrame::Invalid`] to be penalized.
#[derive(Debug)]
pub(crate) enum RequestFrame {
    /// A successfully decoded request.
    Valid(Request),
    /// An oversized or undecodable request (the reason). Only occurs inbound.
    Invalid(String),
}

/// The codec used inside the network layer: the same format as [`SyncCodec`] (which it wraps), except that an
/// `InvalidData` error on an inbound request becomes [`RequestFrame::Invalid`].
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct ScoringCodec(SyncCodec);

impl request_response::Codec for ScoringCodec {
    type Protocol = StreamProtocol;
    type Request = RequestFrame;
    type Response = Response;

    async fn read_request<T>(
        &mut self,
        protocol: &StreamProtocol,
        io: &mut T,
    ) -> io::Result<RequestFrame>
    where
        T: AsyncRead + Unpin + Send,
    {
        match self.0.read_request(protocol, io).await {
            Ok(request) => Ok(RequestFrame::Valid(request)),
            Err(e) if e.kind() == io::ErrorKind::InvalidData => {
                Ok(RequestFrame::Invalid(e.to_string()))
            }
            Err(e) => Err(e),
        }
    }

    async fn read_response<T>(
        &mut self,
        protocol: &StreamProtocol,
        io: &mut T,
    ) -> io::Result<Response>
    where
        T: AsyncRead + Unpin + Send,
    {
        self.0.read_response(protocol, io).await
    }

    async fn write_request<T>(
        &mut self,
        protocol: &StreamProtocol,
        io: &mut T,
        request: RequestFrame,
    ) -> io::Result<()>
    where
        T: AsyncWrite + Unpin + Send,
    {
        match request {
            RequestFrame::Valid(request) => self.0.write_request(protocol, io, request).await,
            RequestFrame::Invalid(_) => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid requests are not sent".to_string(),
            )),
        }
    }

    async fn write_response<T>(
        &mut self,
        protocol: &StreamProtocol,
        io: &mut T,
        response: Response,
    ) -> io::Result<()>
    where
        T: AsyncWrite + Unpin + Send,
    {
        self.0.write_response(protocol, io, response).await
    }
}

/// Server side: call the [`crate::Handler`] according to the request and build the response. `max` is capped at
/// [`MAX_BLOCKS_PER_REQUEST`] / [`MAX_MEMPOOL_OPS`], only the first [`MAX_LOCATOR_LEN`] items of the locator are used,
/// and the encoded response stays within its [`response_limit`] (blocks / ops that do not fit are cut off; a single
/// block that does not fit is treated as absent).
pub fn serve(handler: &dyn crate::Handler, request: Request) -> Response {
    match request {
        Request::Status => {
            let mut status = handler.status();
            status.tips.truncate(MAX_STATUS_TIPS);
            Response::Status(status)
        }
        Request::GetBlocks {
            mut locator,
            after,
            max,
        } => {
            let max = max.min(MAX_BLOCKS_PER_REQUEST);
            locator.truncate(MAX_LOCATOR_LEN);
            let blocks = handler.blocks_after(&locator, after.as_ref(), max);
            Response::Blocks(fit(blocks, max, MAX_SYNC_MESSAGE_BYTES))
        }
        Request::GetBlock { hash } => {
            let block = handler.block(&hash).filter(|b| {
                encoded_len(b).is_some_and(|len| len <= response_budget(MAX_SYNC_MESSAGE_BYTES, 2))
            });
            Response::Block(block)
        }
        Request::GetMempool { max } => {
            let max = max.min(MAX_MEMPOOL_OPS);
            Response::Mempool(fit(handler.mempool(max), max, MAX_MEMPOOL_RESPONSE_BYTES))
        }
    }
}

/// Keep at most `max` items, truncated so that the encoded response stays within `limit` bytes.
fn fit<T: BorshSerialize>(items: Vec<T>, max: u32, limit: usize) -> Vec<T> {
    let max = usize::try_from(max).unwrap_or(usize::MAX);
    // Response = 1-byte variant index + 4-byte Vec length + the items.
    let budget = response_budget(limit, 5);
    let mut used: usize = 0;
    let mut out = Vec::new();
    for item in items.into_iter().take(max) {
        let Some(len) = encoded_len(&item) else { break };
        match used.checked_add(len) {
            Some(total) if total <= budget => used = total,
            _ => break,
        }
        out.push(item);
    }
    out
}

fn response_budget(limit: usize, overhead: usize) -> usize {
    limit.saturating_sub(overhead)
}

fn encoded_len<T: BorshSerialize>(item: &T) -> Option<usize> {
    borsh::object_length(item).ok()
}
