//! Optional announce indexer: a cache of the mesh with a front door.
//!
//! `DESIGN.md` §2.4. Not the authority — see `registry` for why that is a fact
//! about the code and not just an intention.

pub mod client;
pub mod hosting;
pub mod http;
pub mod node;
pub mod quota;
pub mod wire;
pub mod registry;
