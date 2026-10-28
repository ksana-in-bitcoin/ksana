//! Inbound connections by address (D-132, local policy): at most `max_per_ip` inbound connections from one IP group
//! ([`IpGroup`]), those still in the handshake counted too, and none from a group banned after a peer's misbehavior
//! (the event loop bans and lifts the groups, [`Gate::ban`] / [`Gate::unban`]). Both refusals happen before the Noise
//! handshake. Connections without an IP group (loopback, private and link-local addresses) are not counted, and
//! outbound connections are never refused here: this node dials only its bootstrap peers and explicit dials.
//!
//! The totals (inbound connections established and in the handshake, connections per peer) are libp2p's
//! `connection_limits` behaviour beside this one.

use std::collections::{BTreeMap, BTreeSet};
use std::convert::Infallible;
use std::fmt;
use std::task::{Context, Poll};

use libp2p::core::transport::PortUse;
use libp2p::core::{ConnectedPoint, Endpoint};
use libp2p::swarm::behaviour::ConnectionEstablished;
use libp2p::swarm::{
    dummy, ConnectionClosed, ConnectionDenied, ConnectionId, FromSwarm, ListenFailure,
    NetworkBehaviour, THandler, THandlerInEvent, THandlerOutEvent, ToSwarm,
};
use libp2p::{Multiaddr, PeerId};

use crate::limit::IpGroup;

/// Why the gate refused an inbound connection.
#[derive(Debug)]
pub(crate) enum Refused {
    /// Its group already has the most inbound connections.
    Full { group: IpGroup, max: usize },
    /// Its group is banned.
    Banned(IpGroup),
}

impl fmt::Display for Refused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Refused::Full { group, max } => {
                write!(f, "address {group} already has {max} inbound connections")
            }
            Refused::Banned(group) => write!(f, "address {group} is banned"),
        }
    }
}

impl std::error::Error for Refused {}

/// The inbound connections by IP group and the banned groups.
pub(crate) struct Gate {
    max_per_ip: usize,
    /// Inbound connections still in the handshake, by the group of their remote address.
    pending: BTreeMap<ConnectionId, IpGroup>,
    /// Established inbound connections, by group.
    established: BTreeMap<ConnectionId, IpGroup>,
    banned: BTreeSet<IpGroup>,
}

impl Gate {
    pub(crate) fn new(max_per_ip: usize) -> Self {
        Gate {
            max_per_ip,
            pending: BTreeMap::new(),
            established: BTreeMap::new(),
            banned: BTreeSet::new(),
        }
    }

    /// Refuse new inbound connections from `group` until [`Gate::unban`]; those already open stay.
    pub(crate) fn ban(&mut self, group: IpGroup) {
        self.banned.insert(group);
    }

    pub(crate) fn unban(&mut self, group: &IpGroup) {
        self.banned.remove(group);
    }

    /// An inbound connection from `remote` starts its handshake: refused when its group is banned or already has
    /// `max_per_ip` inbound connections.
    fn admit(&mut self, connection: ConnectionId, remote: &Multiaddr) -> Result<(), Refused> {
        let Some(group) = IpGroup::of(remote) else {
            return Ok(());
        };
        if self.banned.contains(&group) {
            return Err(Refused::Banned(group));
        }
        let open = self
            .pending
            .values()
            .chain(self.established.values())
            .filter(|open| **open == group)
            .count();
        if open >= self.max_per_ip {
            return Err(Refused::Full {
                group,
                max: self.max_per_ip,
            });
        }
        self.pending.insert(connection, group);
        Ok(())
    }

    /// An inbound connection passed the handshake and every behaviour took it.
    fn established(&mut self, connection: ConnectionId) {
        if let Some(group) = self.pending.remove(&connection) {
            self.established.insert(connection, group);
        }
    }

    /// A connection closed, failed in the handshake or was refused by another behaviour.
    fn closed(&mut self, connection: &ConnectionId) {
        self.pending.remove(connection);
        self.established.remove(connection);
    }
}

impl NetworkBehaviour for Gate {
    type ConnectionHandler = dummy::ConnectionHandler;
    type ToSwarm = Infallible;

    fn handle_pending_inbound_connection(
        &mut self,
        connection_id: ConnectionId,
        _: &Multiaddr,
        remote_addr: &Multiaddr,
    ) -> Result<(), ConnectionDenied> {
        self.admit(connection_id, remote_addr)
            .map_err(ConnectionDenied::new)
    }

    fn handle_established_inbound_connection(
        &mut self,
        _: ConnectionId,
        _: PeerId,
        _: &Multiaddr,
        _: &Multiaddr,
    ) -> Result<THandler<Self>, ConnectionDenied> {
        Ok(dummy::ConnectionHandler)
    }

    fn handle_established_outbound_connection(
        &mut self,
        _: ConnectionId,
        _: PeerId,
        _: &Multiaddr,
        _: Endpoint,
        _: PortUse,
    ) -> Result<THandler<Self>, ConnectionDenied> {
        Ok(dummy::ConnectionHandler)
    }

    fn on_swarm_event(&mut self, event: FromSwarm) {
        match event {
            FromSwarm::ConnectionEstablished(ConnectionEstablished {
                connection_id,
                endpoint: ConnectedPoint::Listener { .. },
                ..
            }) => self.established(connection_id),
            FromSwarm::ConnectionClosed(ConnectionClosed { connection_id, .. })
            | FromSwarm::ListenFailure(ListenFailure { connection_id, .. }) => {
                self.closed(&connection_id)
            }
            _ => {}
        }
    }

    fn on_connection_handler_event(
        &mut self,
        _: PeerId,
        _: ConnectionId,
        event: THandlerOutEvent<Self>,
    ) {
        match event {}
    }

    fn poll(&mut self, _: &mut Context<'_>) -> Poll<ToSwarm<Self::ToSwarm, THandlerInEvent<Self>>> {
        Poll::Pending
    }
}

#[cfg(test)]
mod tests {
    use libp2p::swarm::ListenError;

    use super::*;

    fn addr(text: &str) -> Multiaddr {
        text.parse().unwrap()
    }

    fn group(text: &str) -> IpGroup {
        IpGroup::of(&addr(text)).unwrap()
    }

    #[test]
    fn inbound_connections_are_capped_per_address() {
        let mut gate = Gate::new(2);
        let (a, b) = ("/ip4/203.0.113.9/tcp/50000", "/ip4/203.0.113.10/tcp/50000");
        let id = ConnectionId::new_unchecked;
        assert!(gate.admit(id(1), &addr(a)).is_ok());
        gate.established(id(1));
        // In the handshake counts too
        assert!(gate
            .admit(id(2), &addr("/ip4/203.0.113.9/tcp/50001"))
            .is_ok());
        assert!(matches!(
            gate.admit(id(3), &addr(a)),
            Err(Refused::Full { max: 2, .. })
        ));
        assert!(gate.admit(id(4), &addr(b)).is_ok(), "another address");
        // A handshake that failed, and a connection that closed, give their places back
        gate.closed(&id(2));
        assert!(gate.admit(id(5), &addr(a)).is_ok());
        assert!(gate.admit(id(6), &addr(a)).is_err());
        gate.closed(&id(1));
        assert!(gate.admit(id(7), &addr(a)).is_ok());
        // An IPv6 /64 counts as one address
        assert!(gate.admit(id(8), &addr("/ip6/2001:db8::1/tcp/1")).is_ok());
        assert!(gate.admit(id(9), &addr("/ip6/2001:db8::2/tcp/1")).is_ok());
        assert!(gate.admit(id(10), &addr("/ip6/2001:db8::3/tcp/1")).is_err());
        assert!(gate
            .admit(id(11), &addr("/ip6/2001:db8:0:1::3/tcp/1"))
            .is_ok());
    }

    #[test]
    fn local_addresses_are_neither_counted_nor_banned() {
        let mut gate = Gate::new(1);
        for n in 0..10 {
            for local in [
                "/ip4/127.0.0.1/tcp/1",
                "/ip4/172.20.0.3/tcp/1",
                "/ip6/::1/tcp/1",
            ] {
                assert!(gate
                    .admit(ConnectionId::new_unchecked(n), &addr(local))
                    .is_ok());
            }
        }
        assert!(gate.pending.is_empty());
    }

    /// The places follow the swarm's events: taken when a handshake starts, kept once the connection is established,
    /// given back when the handshake fails, another behaviour refuses the connection, or it closes; outbound
    /// connections are not counted.
    #[test]
    fn the_swarm_events_track_the_places() {
        let mut gate = Gate::new(1);
        let local = addr("/ip4/0.0.0.0/tcp/9000");
        let remote = addr("/ip4/203.0.113.9/tcp/50000");
        let peer = PeerId::random();
        let id = ConnectionId::new_unchecked;
        let pending =
            |gate: &mut Gate, n| gate.handle_pending_inbound_connection(id(n), &local, &remote);
        let listener = ConnectedPoint::Listener {
            local_addr: local.clone(),
            send_back_addr: remote.clone(),
        };
        let established = |gate: &mut Gate, n, endpoint: &ConnectedPoint| {
            gate.on_swarm_event(FromSwarm::ConnectionEstablished(ConnectionEstablished {
                peer_id: peer,
                connection_id: id(n),
                endpoint,
                failed_addresses: &[],
                other_established: 0,
            }))
        };
        let failed = |gate: &mut Gate, n| {
            gate.on_swarm_event(FromSwarm::ListenFailure(ListenFailure {
                local_addr: &local,
                send_back_addr: &remote,
                error: &ListenError::Aborted,
                connection_id: id(n),
                peer_id: None,
            }))
        };
        // A handshake that fails gives its place back
        assert!(pending(&mut gate, 1).is_ok());
        assert!(pending(&mut gate, 2).is_err());
        failed(&mut gate, 1);
        // An established connection keeps it until it closes
        assert!(pending(&mut gate, 3).is_ok());
        established(&mut gate, 3, &listener);
        assert!(pending(&mut gate, 4).is_err());
        // A connection this node dialed is not counted, even to the same address
        let dialer = ConnectedPoint::Dialer {
            address: remote.clone(),
            role_override: Endpoint::Dialer,
            port_use: PortUse::Reuse,
        };
        established(&mut gate, 5, &dialer);
        gate.on_swarm_event(FromSwarm::ConnectionClosed(ConnectionClosed {
            peer_id: peer,
            connection_id: id(3),
            endpoint: &listener,
            cause: None,
            remaining_established: 0,
        }));
        assert!(pending(&mut gate, 6).is_ok());
        assert_eq!((gate.pending.len(), gate.established.len()), (1, 0));
    }

    #[test]
    fn a_banned_address_is_refused_until_lifted() {
        let mut gate = Gate::new(4);
        let a = "/ip4/198.51.100.20/tcp/7000";
        gate.ban(group(a));
        assert!(matches!(
            gate.admit(ConnectionId::new_unchecked(1), &addr(a)),
            Err(Refused::Banned(_))
        ));
        assert!(gate
            .admit(
                ConnectionId::new_unchecked(2),
                &addr("/ip4/198.51.100.21/tcp/7000")
            )
            .is_ok());
        gate.unban(&group(a));
        assert!(gate.admit(ConnectionId::new_unchecked(3), &addr(a)).is_ok());
    }
}
