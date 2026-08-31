//! UDP port allocation for instances on one node.
//!
//! Small, but the failure it prevents is not: two instances handed the same port
//! means the second container fails to bind, or worse, binds and silently steals
//! the first one's traffic. Allocation is therefore explicit and single-owner —
//! the agent holds one allocator and nothing else picks ports.
//!
//! Ports are **not reused immediately**. A freed port goes to the back of the
//! queue rather than straight back to the front, because a game client that was
//! mid-session will keep sending to the old port for a while, and handing it
//! instantly to a different instance would deliver one server's players to
//! another server's socket.

use std::collections::{BTreeSet, VecDeque};

use crate::config::PortRange;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PortError {
    /// Every port in the range is in use.
    Exhausted { range: PortRange },
    /// Asked to reserve a specific port that is outside the configured range.
    OutOfRange { port: u16, range: PortRange },
    /// Asked to reserve a specific port already held.
    AlreadyHeld(u16),
}

impl core::fmt::Display for PortError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Exhausted { range } => write!(
                f,
                "no free port in {}-{}: every one is allocated",
                range.start, range.end
            ),
            Self::OutOfRange { port, range } => write!(
                f,
                "port {port} is outside this node's range {}-{}",
                range.start, range.end
            ),
            Self::AlreadyHeld(p) => write!(f, "port {p} is already allocated"),
        }
    }
}

impl std::error::Error for PortError {}

#[derive(Debug)]
pub struct PortAllocator {
    range: PortRange,
    free: VecDeque<u16>,
    held: BTreeSet<u16>,
}

impl PortAllocator {
    pub fn new(range: PortRange) -> Self {
        let free = (range.start..=range.end).collect::<VecDeque<_>>();
        Self { range, free, held: BTreeSet::new() }
    }

    /// Rebuild an allocator that already knows about ports in use — after a
    /// restart, from the ports of containers still running. Without this an
    /// agent that restarted would hand out ports its own live instances hold.
    pub fn with_reserved(range: PortRange, reserved: &[u16]) -> Self {
        let mut alloc = Self::new(range);
        for &port in reserved {
            // A reserved port outside the range is not an error here: the range
            // may have been narrowed since that instance started, and the port
            // is still genuinely taken. Record it so nothing else gets it.
            alloc.free.retain(|&p| p != port);
            alloc.held.insert(port);
        }
        alloc
    }

    pub fn range(&self) -> PortRange {
        self.range
    }

    /// Take a whole port set at once — one host port per port the game
    /// declares (`GAMES.md` §3: a Source server is a game port *and* an RCON
    /// port *and* maybe SourceTV).
    ///
    /// A `Some(p)` slot asks for that exact port, a `None` slot takes whatever
    /// is next. **All or nothing**: a set that cannot be satisfied in full
    /// gives every port back before returning, because a half-allocated
    /// instance is one that never starts and leaks the rest of its set on every
    /// retry.
    pub fn acquire(&mut self, requested: &[Option<u16>]) -> Result<Vec<u16>, PortError> {
        let mut taken = Vec::with_capacity(requested.len());
        for slot in requested {
            let got = match slot {
                Some(p) => self.reserve(*p).map(|()| *p),
                None => self.allocate(),
            };
            match got {
                Ok(p) => taken.push(p),
                Err(e) => {
                    self.release_all(&taken);
                    return Err(e);
                }
            }
        }
        Ok(taken)
    }

    pub fn allocate(&mut self) -> Result<u16, PortError> {
        match self.free.pop_front() {
            Some(port) => {
                self.held.insert(port);
                Ok(port)
            }
            None => Err(PortError::Exhausted { range: self.range }),
        }
    }

    /// Take one specific port, for an instance that must keep the port it had.
    pub fn reserve(&mut self, port: u16) -> Result<(), PortError> {
        if !self.range.contains(port) {
            return Err(PortError::OutOfRange { port, range: self.range });
        }
        if self.held.contains(&port) {
            return Err(PortError::AlreadyHeld(port));
        }
        self.free.retain(|&p| p != port);
        self.held.insert(port);
        Ok(())
    }

    /// Give a port back. To the **back** of the queue — see the module docs.
    pub fn release(&mut self, port: u16) {
        if self.held.remove(&port) && self.range.contains(port) {
            self.free.push_back(port);
        }
    }

    /// Give a whole set back, for an instance that is going away.
    pub fn release_all(&mut self, ports: &[u16]) {
        for &port in ports {
            self.release(port);
        }
    }

    pub fn held(&self) -> Vec<u16> {
        self.held.iter().copied().collect()
    }

    pub fn available(&self) -> usize {
        self.free.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn range(start: u16, end: u16) -> PortRange {
        PortRange { start, end }
    }

    #[test]
    fn allocates_in_order_and_tracks_what_is_held() {
        let mut a = PortAllocator::new(range(27100, 27102));
        assert_eq!(a.allocate().unwrap(), 27100);
        assert_eq!(a.allocate().unwrap(), 27101);
        assert_eq!(a.held(), vec![27100, 27101]);
        assert_eq!(a.available(), 1);
    }

    /// The node-side half of `GAMES.md` §7 step 2: a shipped multi-port pack
    /// gets a whole port set on a real allocator, or none of it.
    ///
    /// Found by the property, never by game id — the same reason
    /// `second_game.rs` never names its game. A node's range is the operator's,
    /// so what is checked here is the shape of the request, not the numbers.
    #[test]
    fn every_shipped_pack_gets_its_whole_port_set_or_nothing() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packs");
        let packs = game_bridge::GamePack::load_dir(&dir).expect("shipped packs read").packs;
        assert!(!packs.is_empty(), "expected the shipped packs to be there");

        for pack in &packs {
            let profile = pack.to_profile().expect("a shipped pack is usable");
            let wanted = profile.ports();
            // Every port unallocated: the pack's own numbers are what the game
            // binds inside its container, and what they are reachable as
            // outside comes from the node.
            let request: Vec<Option<u16>> = wanted.iter().map(|_| None).collect();

            let mut a = PortAllocator::new(range(27100, 27199));
            let got = a.acquire(&request).unwrap_or_else(|e| panic!("{}: {e}", pack.id));
            assert_eq!(got.len(), wanted.len(), "{}", pack.id);
            let distinct: std::collections::BTreeSet<u16> = got.iter().copied().collect();
            assert_eq!(distinct.len(), got.len(), "{} was given a port twice", pack.id);
            assert_eq!(a.available(), 100 - got.len(), "{}", pack.id);

            // A range one port short of the set must leave the allocator
            // untouched, not half-filled — otherwise a retry loop leaks the
            // rest of the set on every attempt.
            let short = 27100 + (wanted.len() as u16) - 2;
            let mut tight = PortAllocator::new(range(27100, short));
            assert!(tight.acquire(&request).is_err(), "{} fit in too small a range", pack.id);
            assert_eq!(tight.held().len(), 0, "{} kept part of a set it could not fill", pack.id);
        }
    }

    #[test]
    fn exhaustion_is_an_error_not_a_wrap_around() {
        let mut a = PortAllocator::new(range(27100, 27101));
        a.allocate().unwrap();
        a.allocate().unwrap();
        assert_eq!(
            a.allocate(),
            Err(PortError::Exhausted { range: range(27100, 27101) })
        );
    }

    /// The rule from the module docs, as a test: a released port must not be
    /// the very next one handed out, or a client still sending to the old
    /// instance lands on a new one.
    #[test]
    fn a_released_port_goes_to_the_back_of_the_queue() {
        let mut a = PortAllocator::new(range(27100, 27102));
        let first = a.allocate().unwrap();
        a.allocate().unwrap();
        a.release(first);
        assert_eq!(a.allocate().unwrap(), 27102, "the freed port must not jump the queue");
        assert_eq!(a.allocate().unwrap(), first, "and it comes back only once nothing else is free");
    }

    #[test]
    fn releasing_a_port_that_was_never_held_changes_nothing() {
        let mut a = PortAllocator::new(range(27100, 27101));
        a.release(27100);
        a.release(9999);
        assert_eq!(a.available(), 2, "a stray release must not duplicate a free port");
        assert_eq!(a.allocate().unwrap(), 27100);
        assert_eq!(a.allocate().unwrap(), 27101);
        assert!(a.allocate().is_err());
    }

    #[test]
    fn reserve_takes_a_specific_port_and_refuses_a_taken_one() {
        let mut a = PortAllocator::new(range(27100, 27102));
        a.reserve(27101).unwrap();
        assert_eq!(a.reserve(27101), Err(PortError::AlreadyHeld(27101)));
        // The reserved port is gone from the rotation.
        assert_eq!(a.allocate().unwrap(), 27100);
        assert_eq!(a.allocate().unwrap(), 27102);
        assert!(a.allocate().is_err());
    }

    #[test]
    fn reserve_refuses_a_port_outside_the_range() {
        let mut a = PortAllocator::new(range(27100, 27102));
        assert_eq!(
            a.reserve(80),
            Err(PortError::OutOfRange { port: 80, range: range(27100, 27102) })
        );
    }

    /// A game with an RCON port and a TV port needs three host ports, and it
    /// needs all three or none: a container that starts with two of them is a
    /// server missing a port nobody can reach.
    #[test]
    fn a_port_set_is_taken_whole() {
        let mut a = PortAllocator::new(range(27100, 27102));
        let set = a.acquire(&[None, None, None]).unwrap();
        assert_eq!(set, vec![27100, 27101, 27102]);
        assert_eq!(a.available(), 0);
    }

    /// The all-or-nothing half, which is the one that matters: a failed acquire
    /// must leave the pool exactly as it found it, or every retry burns ports.
    #[test]
    fn a_set_that_does_not_fit_gives_back_everything_it_took() {
        let mut a = PortAllocator::new(range(27100, 27101));
        assert_eq!(
            a.acquire(&[None, None, None]),
            Err(PortError::Exhausted { range: range(27100, 27101) })
        );
        assert_eq!(a.available(), 2, "a failed set must not hold anything");
        assert!(a.held().is_empty());
        assert_eq!(a.acquire(&[None, None]).unwrap(), vec![27100, 27101]);
    }

    /// A fixed slot is for an instance that must keep the port it had — an
    /// RCON port somebody wrote into a firewall rule, say. A taken one fails
    /// the whole set rather than quietly handing out a different port.
    #[test]
    fn a_set_can_pin_some_ports_and_let_the_rest_float() {
        let mut a = PortAllocator::new(range(27100, 27103));
        let set = a.acquire(&[Some(27102), None]).unwrap();
        assert_eq!(set, vec![27102, 27100]);

        let mut b = PortAllocator::new(range(27100, 27103));
        b.reserve(27101).unwrap();
        assert_eq!(b.acquire(&[None, Some(27101)]), Err(PortError::AlreadyHeld(27101)));
        assert_eq!(b.available(), 3, "the floating port taken first must come back");
    }

    /// After a restart the agent rebuilds from what is actually running. If it
    /// did not, it would hand a live instance's port to a new one.
    #[test]
    fn reserved_ports_survive_a_rebuild() {
        let mut a = PortAllocator::with_reserved(range(27100, 27102), &[27101]);
        assert_eq!(a.available(), 2);
        assert_eq!(a.allocate().unwrap(), 27100);
        assert_eq!(a.allocate().unwrap(), 27102);
        assert!(a.allocate().is_err(), "27101 belongs to a running instance");
    }

    /// The range may have been narrowed while an instance kept running. That
    /// port is still taken, and the allocator has to know it even though it can
    /// never hand it out again.
    #[test]
    fn a_reserved_port_outside_a_narrowed_range_is_still_respected() {
        let a = PortAllocator::with_reserved(range(27100, 27102), &[29999]);
        assert!(a.held().contains(&29999));
        assert_eq!(a.available(), 3);
    }
}
