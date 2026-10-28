//! Rate limits and byte budgets per source (D-059, D-132, local policy).
//!
//! A source is where a peer connects from: the IP address of its connection, an IPv6 address counted by its /64 prefix
//! (one host or one site's network), or the peer itself for a connection from a loopback or private address (the
//! node's own machine, its local network, devnet's containers; [`IpGroup::of`]). Every identity connecting from one
//! address shares that address's limits, and the token buckets outlive the connections: a source that disconnects and
//! comes back, under the same identity or a new one, finds its buckets as it left them. A bucket that has refilled to
//! its capacity is the same as a new one and is dropped ([`Limiter::prune`]).
//!
//! - Three token buckets per source: gossip blocks, gossip ops, inbound sync requests. Gossip messages over the quota
//!   are reported as Ignore before decoding, requests over the quota are dropped outright; neither costs score (honest
//!   nodes also forward in bursts). Integer arithmetic: tokens are counted in thousandths.
//! - Byte budgets ([`Budget`]): what the sources may hold in the gossip validation queues and in the sync responses
//!   being sent, in all and each.
//! - The inbound sync requests being served ([`Serving`]): how many one source may have in flight, and the bytes of
//!   their responses until they have been sent.

use std::collections::BTreeMap;
use std::fmt;
use std::net::{Ipv4Addr, Ipv6Addr};
use std::time::Duration;

use libp2p::multiaddr::Protocol;
use libp2p::{Multiaddr, PeerId};
use tokio::time::Instant;

use crate::wire::Request;
use crate::{PeerLimits, Rate};

/// One thousandth of a token.
const MILLI: u64 = 1_000;
/// When the same source is being rate limited, log at most once per this interval.
const LOG_INTERVAL: Duration = Duration::from_secs(10);

/// Token count of `GetBlocks` and `GetMempool` (their responses can reach the sync message limit); `Status` and
/// `GetBlock` count 1.
pub const HEAVY_REQUEST_COST: u32 = 10;

/// Tokens consumed by an inbound request.
pub(crate) fn request_cost(request: &Request) -> u32 {
    match request {
        Request::Status | Request::GetBlock { .. } => 1,
        Request::GetBlocks { .. } | Request::GetMempool { .. } => HEAVY_REQUEST_COST,
    }
}

/// An IP address as a source: an IPv4 address, or the /64 prefix of an IPv6 address.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum IpGroup {
    V4([u8; 4]),
    V6([u8; 8]),
}

impl IpGroup {
    /// The group of the IP address a multiaddr starts with (an IPv4-mapped IPv6 address counts as the IPv4 address).
    /// `None` for an address that is not an IP address, and for a loopback, private, link-local or unspecified one:
    /// such a connection comes from the node's own machine or its local network, is counted by its peer, and is never
    /// refused or banned by address (D-132).
    pub(crate) fn of(addr: &Multiaddr) -> Option<IpGroup> {
        match addr.iter().next()? {
            Protocol::Ip4(ip) => Self::v4(ip),
            Protocol::Ip6(ip) => match ip.to_ipv4_mapped() {
                Some(ip) => Self::v4(ip),
                None => Self::v6(ip),
            },
            _ => None,
        }
    }

    fn v4(ip: Ipv4Addr) -> Option<IpGroup> {
        let local = ip.is_loopback()
            || ip.is_private()
            || ip.is_link_local()
            || ip.is_unspecified()
            || ip.is_broadcast();
        (!local).then(|| IpGroup::V4(ip.octets()))
    }

    fn v6(ip: Ipv6Addr) -> Option<IpGroup> {
        let local = ip.is_loopback()
            || ip.is_unspecified()
            || ip.is_unique_local()
            || ip.is_unicast_link_local();
        if local {
            return None;
        }
        let octets = ip.octets();
        let prefix = octets.get(..8)?;
        <[u8; 8]>::try_from(prefix).ok().map(IpGroup::V6)
    }
}

impl fmt::Display for IpGroup {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            IpGroup::V4(octets) => write!(f, "{}", Ipv4Addr::from(*octets)),
            IpGroup::V6(prefix) => {
                let mut octets = [0u8; 16];
                for (to, from) in octets.iter_mut().zip(prefix) {
                    *to = *from;
                }
                write!(f, "{}/64", Ipv6Addr::from(octets))
            }
        }
    }
}

/// Where a peer's messages, requests and connections are counted (D-132): the IP group of its connection, or the peer
/// itself for a connection without one ([`IpGroup::of`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Source {
    Ip(IpGroup),
    Peer(PeerId),
}

impl Source {
    /// The source of a connection with `peer` whose remote address is `addr`.
    pub(crate) fn of(peer: PeerId, addr: &Multiaddr) -> Source {
        IpGroup::of(addr).map_or(Source::Peer(peer), Source::Ip)
    }

    /// The IP group, for a source counted by address.
    pub(crate) fn ip(&self) -> Option<IpGroup> {
        match self {
            Source::Ip(group) => Some(*group),
            Source::Peer(_) => None,
        }
    }
}

impl fmt::Display for Source {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Source::Ip(group) => write!(f, "address {group}"),
            Source::Peer(peer) => write!(f, "peer {peer}"),
        }
    }
}

/// The kinds of rate-limited messages.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Limited {
    /// Gossip blocks.
    Block,
    /// Gossip ops.
    Op,
    /// Inbound sync requests.
    Request,
}

impl Limited {
    fn describe(self) -> &'static str {
        match self {
            Limited::Block => "gossip block",
            Limited::Op => "gossip op",
            Limited::Request => "sync request",
        }
    }
}

/// A token bucket.
#[derive(Debug)]
struct Bucket {
    /// Current token count (thousandths of a token).
    milli: u64,
    /// The instant up to which refills have been counted.
    at: Instant,
}

impl Bucket {
    fn full(rate: Rate, now: Instant) -> Self {
        Bucket {
            milli: capacity(rate),
            at: now,
        }
    }

    /// Count the refills up to `now`: per_sec tokens per second = per_sec thousandths per millisecond; only whole
    /// milliseconds are counted, the sub-millisecond remainder is left for next time.
    fn refill(&mut self, rate: Rate, now: Instant) {
        let elapsed = now.saturating_duration_since(self.at);
        let millis = u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX);
        if millis > 0 {
            self.milli = self
                .milli
                .saturating_add(millis.saturating_mul(u64::from(rate.per_sec)))
                .min(capacity(rate));
            self.at = self
                .at
                .checked_add(Duration::from_millis(millis))
                .unwrap_or(now);
        }
    }

    /// Take `cost` tokens (capped at the capacity, i.e. a full bucket is required); when there are not enough, take
    /// none and return false.
    fn take(&mut self, rate: Rate, cost: u32, now: Instant) -> bool {
        self.refill(rate, now);
        let need = u64::from(cost.min(rate.burst)).saturating_mul(MILLI);
        match self.milli.checked_sub(need) {
            Some(rest) => {
                self.milli = rest;
                true
            }
            None => false,
        }
    }

    /// Whether the bucket has refilled to its capacity by `now` (it is then the same as a new one).
    fn is_full(&mut self, rate: Rate, now: Instant) -> bool {
        self.refill(rate, now);
        self.milli >= capacity(rate)
    }
}

fn capacity(rate: Rate) -> u64 {
    u64::from(rate.burst).saturating_mul(MILLI)
}

/// One source's token buckets and the number of dropped messages (for logging).
#[derive(Debug)]
struct Buckets {
    blocks: Bucket,
    ops: Bucket,
    requests: Bucket,
    dropped: u64,
    logged: Option<Instant>,
}

/// The token buckets of all sources.
#[derive(Debug)]
pub(crate) struct Limiter {
    limits: PeerLimits,
    sources: BTreeMap<Source, Buckets>,
}

impl Limiter {
    pub(crate) fn new(limits: PeerLimits) -> Self {
        Limiter {
            limits,
            sources: BTreeMap::new(),
        }
    }

    /// Whether a `kind` message (`cost` tokens) from `source` is within the quota; when over, count it as dropped and
    /// log at the interval.
    pub(crate) fn allow(&mut self, source: Source, kind: Limited, cost: u32, now: Instant) -> bool {
        let limits = &self.limits;
        let buckets = self.sources.entry(source).or_insert_with(|| Buckets {
            blocks: Bucket::full(limits.blocks, now),
            ops: Bucket::full(limits.ops, now),
            requests: Bucket::full(limits.requests, now),
            dropped: 0,
            logged: None,
        });
        let (bucket, rate) = match kind {
            Limited::Block => (&mut buckets.blocks, limits.blocks),
            Limited::Op => (&mut buckets.ops, limits.ops),
            Limited::Request => (&mut buckets.requests, limits.requests),
        };
        if bucket.take(rate, cost, now) {
            return true;
        }
        buckets.dropped = buckets.dropped.saturating_add(1);
        let due = buckets
            .logged
            .is_none_or(|at| now.saturating_duration_since(at) >= LOG_INTERVAL);
        if due {
            eprintln!(
                "[ksana-p2p] {source} exceeded its {} quota, dropping ({} messages, D-059)",
                kind.describe(),
                buckets.dropped
            );
            buckets.dropped = 0;
            buckets.logged = Some(now);
        }
        false
    }

    /// Drop the buckets of the sources whose three buckets have refilled to their capacity by `now`: the source would
    /// start again with full buckets, the same thing (D-132). Called periodically, connected or not, so that the
    /// buckets kept are those of the sources that used their quotas lately.
    pub(crate) fn prune(&mut self, now: Instant) {
        let limits = &self.limits;
        self.sources.retain(|_, buckets| {
            let full = buckets.blocks.is_full(limits.blocks, now)
                && buckets.ops.is_full(limits.ops, now)
                && buckets.requests.is_full(limits.requests, now);
            !full
        });
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.sources.len()
    }
}

/// Bytes held for sources, within a total budget and a budget for each source (D-132).
#[derive(Debug)]
pub(crate) struct Budget {
    total: usize,
    per_source: usize,
    used: usize,
    by_source: BTreeMap<Source, usize>,
}

impl Budget {
    pub(crate) fn new(total: usize, per_source: usize) -> Self {
        Budget {
            total,
            per_source,
            used: 0,
            by_source: BTreeMap::new(),
        }
    }

    /// Take `bytes` for `source` when they fit both the total and the source's budget; otherwise take nothing and
    /// return false.
    pub(crate) fn take(&mut self, source: Source, bytes: usize) -> bool {
        let held = self.by_source.get(&source).copied().unwrap_or(0);
        let (Some(used), Some(held)) = (self.used.checked_add(bytes), held.checked_add(bytes))
        else {
            return false;
        };
        if used > self.total || held > self.per_source {
            return false;
        }
        self.used = used;
        self.by_source.insert(source, held);
        true
    }

    /// Give back `bytes` taken for `source` (at most what it holds).
    pub(crate) fn give(&mut self, source: &Source, bytes: usize) {
        let Some(held) = self.by_source.get_mut(source) else {
            return;
        };
        let given = bytes.min(*held);
        *held = held.saturating_sub(given);
        self.used = self.used.saturating_sub(given);
        if *held == 0 {
            self.by_source.remove(source);
        }
    }

    /// The bytes held in all.
    pub(crate) fn used(&self) -> usize {
        self.used
    }
}

/// What [`Serving::reserve`] found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Reserve {
    /// The response's bytes are counted: send it.
    Fits,
    /// The request has already failed (timed out, its connection closed): drop the response.
    Gone,
    /// The response does not fit the budgets: drop it (the peer sees a failure and asks again later).
    Full,
}

/// One request being served.
#[derive(Debug)]
struct InFlight {
    source: Source,
    /// The bytes reserved for its response (0 until the response is ready).
    bytes: usize,
}

/// The inbound sync requests being served (D-132), by request: from the moment one is taken in until its response has
/// been sent or it failed. A source may have at most `max_per_source` of them in flight, and the responses' bytes,
/// from the moment one is ready until it has been sent, count against a [`Budget`].
#[derive(Debug)]
pub(crate) struct Serving<K> {
    max_per_source: usize,
    requests: BTreeMap<K, InFlight>,
    per_source: BTreeMap<Source, usize>,
    bytes: Budget,
}

impl<K: Ord + Copy> Serving<K> {
    pub(crate) fn new(max_per_source: usize, bytes: Budget) -> Self {
        Serving {
            max_per_source,
            requests: BTreeMap::new(),
            per_source: BTreeMap::new(),
            bytes,
        }
    }

    /// Take in request `request` from `source`; false when the source already has `max_per_source` requests in flight
    /// (the request is then dropped).
    pub(crate) fn admit(&mut self, request: K, source: Source) -> bool {
        let count = self.per_source.get(&source).copied().unwrap_or(0);
        if count >= self.max_per_source || self.requests.contains_key(&request) {
            return false;
        }
        self.per_source.insert(source, count.saturating_add(1));
        self.requests.insert(request, InFlight { source, bytes: 0 });
        true
    }

    /// The response to `request` is ready and `bytes` long: count its bytes until it has been sent.
    pub(crate) fn reserve(&mut self, request: K, bytes: usize) -> Reserve {
        let Some(in_flight) = self.requests.get_mut(&request) else {
            return Reserve::Gone;
        };
        if in_flight.bytes > 0 || !self.bytes.take(in_flight.source, bytes) {
            return Reserve::Full;
        }
        in_flight.bytes = bytes;
        Reserve::Fits
    }

    /// The response to `request` has been sent, or the request failed: give back its place and its bytes. Nothing
    /// happens for a request not in flight.
    pub(crate) fn finish(&mut self, request: &K) {
        let Some(InFlight { source, bytes }) = self.requests.remove(request) else {
            return;
        };
        self.bytes.give(&source, bytes);
        if let Some(count) = self.per_source.get_mut(&source) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                self.per_source.remove(&source);
            }
        }
    }

    /// The source of a request in flight.
    pub(crate) fn source(&self, request: &K) -> Option<Source> {
        self.requests.get(request).map(|in_flight| in_flight.source)
    }

    /// The number of requests in flight.
    pub(crate) fn len(&self) -> usize {
        self.requests.len()
    }

    /// The bytes of the responses being sent.
    pub(crate) fn bytes(&self) -> usize {
        self.bytes.used()
    }
}

#[cfg(test)]
#[allow(clippy::arithmetic_side_effects)]
mod tests {
    use super::*;

    const RATE: Rate = Rate {
        per_sec: 10,
        burst: 3,
    };

    fn addr(text: &str) -> Multiaddr {
        text.parse().unwrap()
    }

    fn ip(text: &str) -> Source {
        Source::Ip(IpGroup::of(&addr(text)).unwrap())
    }

    #[test]
    fn bucket_allows_the_burst_then_refills_at_the_rate() {
        let start = Instant::now();
        let mut bucket = Bucket::full(RATE, start);
        assert!((0..3).all(|_| bucket.take(RATE, 1, start)));
        assert!(!bucket.take(RATE, 1, start));
        // 10 per second: 99 ms is not yet one, 100 ms is exactly one
        assert!(!bucket.take(RATE, 1, start + Duration::from_millis(99)));
        assert!(bucket.take(RATE, 1, start + Duration::from_millis(100)));
        assert!(!bucket.take(RATE, 1, start + Duration::from_millis(100)));
        // Refills do not exceed the capacity
        let later = start + Duration::from_secs(60);
        assert!((0..3).all(|_| bucket.take(RATE, 1, later)));
        assert!(!bucket.take(RATE, 1, later));
    }

    #[test]
    fn sub_millisecond_calls_still_refill() {
        // Once every 0.5 ms: the sub-millisecond remainder is left for next time, so frequent calls never starve the
        // refill
        let start = Instant::now();
        let mut bucket = Bucket::full(RATE, start);
        assert!((0..3).all(|_| bucket.take(RATE, 1, start)));
        let allowed = (1..=400u64)
            .filter(|i| bucket.take(RATE, 1, start + Duration::from_micros(500 * i)))
            .count();
        // 200 ms refills 2
        assert_eq!(allowed, 2);
    }

    #[test]
    fn cost_above_the_burst_needs_a_full_bucket() {
        let start = Instant::now();
        let mut bucket = Bucket::full(RATE, start);
        assert!(bucket.take(RATE, HEAVY_REQUEST_COST, start));
        assert!(!bucket.take(RATE, 1, start));
        assert!(!bucket.take(RATE, HEAVY_REQUEST_COST, start + Duration::from_millis(200)));
        assert!(bucket.take(RATE, HEAVY_REQUEST_COST, start + Duration::from_millis(300)));
    }

    fn tight() -> PeerLimits {
        PeerLimits {
            blocks: Rate {
                per_sec: 1,
                burst: 1,
            },
            ops: Rate {
                per_sec: 1,
                burst: 2,
            },
            requests: Rate {
                per_sec: 1,
                burst: 1,
            },
        }
    }

    #[test]
    fn sources_have_separate_buckets_per_kind() {
        let now = Instant::now();
        let (a, b) = (
            Source::Peer(PeerId::random()),
            Source::Peer(PeerId::random()),
        );
        let mut limiter = Limiter::new(tight());
        assert!(limiter.allow(a, Limited::Block, 1, now));
        assert!(!limiter.allow(a, Limited::Block, 1, now));
        // The same source's other kinds and another source are unaffected
        assert!(limiter.allow(a, Limited::Op, 1, now));
        assert!(limiter.allow(a, Limited::Op, 1, now));
        assert!(!limiter.allow(a, Limited::Op, 1, now));
        assert!(limiter.allow(a, Limited::Request, 1, now));
        assert!(limiter.allow(b, Limited::Block, 1, now));
    }

    /// The audit finding (F-28): the buckets were the peer's and were deleted when it disconnected, so a peer that
    /// reconnected, or connected again under a new identity, started over with full buckets. They are now the
    /// address's, and stay until they have refilled.
    #[test]
    fn buckets_are_the_addresses_and_outlive_the_connections() {
        let start = Instant::now();
        let mut limiter = Limiter::new(tight());
        let (first, second) = (PeerId::random(), PeerId::random());
        let remote = addr("/ip4/203.0.113.7/tcp/4001");
        assert_eq!(Source::of(first, &remote), Source::of(second, &remote));
        assert!(limiter.allow(Source::of(first, &remote), Limited::Block, 1, start));
        // Another identity from the same address, or the same one back after a disconnect: still empty
        assert!(!limiter.allow(Source::of(second, &remote), Limited::Block, 1, start));
        limiter.prune(start + Duration::from_millis(500));
        assert!(!limiter.allow(
            Source::of(first, &remote),
            Limited::Block,
            1,
            start + Duration::from_millis(500)
        ));
        // Another address has its own buckets
        let other = addr("/ip4/203.0.113.8/tcp/4001");
        assert!(limiter.allow(Source::of(second, &other), Limited::Block, 1, start));
        assert!(limiter.allow(Source::of(second, &other), Limited::Op, 1, start));
        assert!(limiter.allow(Source::of(second, &other), Limited::Op, 1, start));
        // Refilled buckets are dropped: a source then starts with full ones, the same thing
        assert_eq!(limiter.len(), 2);
        limiter.prune(start + Duration::from_millis(1_500));
        assert_eq!(limiter.len(), 1, "two ops take two seconds to refill");
        limiter.prune(start + Duration::from_secs(2));
        assert_eq!(limiter.len(), 0);
        assert!(limiter.allow(
            Source::of(first, &remote),
            Limited::Block,
            1,
            start + Duration::from_secs(3)
        ));
    }

    #[test]
    fn local_and_non_ip_addresses_are_counted_by_peer() {
        let peer = PeerId::random();
        for local in [
            "/ip4/127.0.0.1/tcp/1",
            "/ip4/10.1.2.3/tcp/1",
            "/ip4/172.18.0.2/tcp/1",
            "/ip4/192.168.1.20/tcp/1",
            "/ip4/169.254.0.9/tcp/1",
            "/ip4/0.0.0.0/tcp/1",
            "/ip6/::1/tcp/1",
            "/ip6/fd00::5/tcp/1",
            "/ip6/fe80::1/tcp/1",
            "/ip6/::ffff:127.0.0.1/tcp/1",
            "/ip6/::ffff:192.168.0.1/tcp/1",
            "/dns4/node2/tcp/9000",
        ] {
            assert_eq!(
                Source::of(peer, &addr(local)),
                Source::Peer(peer),
                "{local}"
            );
        }
        // An IPv4-mapped IPv6 address is the IPv4 address
        assert_eq!(
            ip("/ip6/::ffff:198.51.100.4/tcp/1"),
            ip("/ip4/198.51.100.4/tcp/9")
        );
        assert_eq!(
            ip("/ip4/198.51.100.4/tcp/1").to_string(),
            "address 198.51.100.4"
        );
    }

    #[test]
    fn an_ipv6_address_counts_by_its_64_bit_prefix() {
        let a = ip("/ip6/2001:db8:1:2::1/tcp/1");
        assert_eq!(a, ip("/ip6/2001:db8:1:2:ffff:ffff:ffff:ffff/tcp/1"));
        assert_ne!(a, ip("/ip6/2001:db8:1:3::1/tcp/1"));
        assert_eq!(a.to_string(), "address 2001:db8:1:2::/64");
    }

    #[test]
    fn a_budget_caps_the_total_and_each_source() {
        let (a, b) = (ip("/ip4/203.0.113.1/tcp/1"), ip("/ip4/203.0.113.2/tcp/1"));
        let mut budget = Budget::new(100, 60);
        assert!(budget.take(a, 50));
        assert!(!budget.take(a, 11), "over the source's 60");
        assert!(budget.take(a, 10));
        assert!(!budget.take(b, 41), "over the total 100");
        assert!(budget.take(b, 40));
        assert_eq!(budget.used(), 100);
        budget.give(&a, 60);
        assert_eq!(budget.used(), 40);
        assert!(budget.take(a, 60));
        assert!(!budget.take(b, usize::MAX));
        // Giving back more than a source holds, or for a source holding nothing, gives back what it holds
        budget.give(&b, 1_000);
        budget.give(&Source::Peer(PeerId::random()), 5);
        assert_eq!(budget.used(), 60);
        assert!(budget.take(b, 40));
        assert!(!budget.take(b, 1));
    }

    #[test]
    fn serving_caps_each_sources_requests_and_the_bytes_being_sent() {
        let (a, b) = (ip("/ip4/203.0.113.1/tcp/1"), ip("/ip4/203.0.113.2/tcp/1"));
        let mut serving = Serving::new(2, Budget::new(100, 70));
        assert!(serving.admit(1u64, a));
        assert!(serving.admit(2, a));
        assert!(!serving.admit(3, a), "a third request from the same source");
        assert!(!serving.admit(1, b), "a request already in flight");
        assert!(serving.admit(4, b));
        assert_eq!(serving.reserve(1, 60), Reserve::Fits);
        assert_eq!(serving.reserve(1, 1), Reserve::Full, "reserved once");
        assert_eq!(
            serving.reserve(2, 20),
            Reserve::Full,
            "over the source's 70"
        );
        assert_eq!(serving.reserve(4, 41), Reserve::Full, "over the total 100");
        assert_eq!(serving.reserve(4, 40), Reserve::Fits);
        assert_eq!(serving.reserve(9, 1), Reserve::Gone);
        assert_eq!((serving.len(), serving.bytes()), (3, 100));
        // A response sent gives back its place and its bytes; finishing twice changes nothing
        serving.finish(&1);
        serving.finish(&1);
        assert_eq!((serving.len(), serving.bytes()), (2, 40));
        assert!(serving.admit(3, a));
        assert_eq!(serving.reserve(2, 20), Reserve::Fits);
        // A request that failed before its response was ready: the response finds it gone
        serving.finish(&3);
        assert_eq!(serving.reserve(3, 1), Reserve::Gone);
        serving.finish(&2);
        serving.finish(&4);
        assert_eq!((serving.len(), serving.bytes()), (0, 0));
    }
}
