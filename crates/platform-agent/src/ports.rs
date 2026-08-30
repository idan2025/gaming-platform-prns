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
