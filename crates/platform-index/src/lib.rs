//! Optional announce indexer: a cache of the mesh with a front door.
//!
//! `DESIGN.md` §2.4. Not the authority — see `registry` for why that is a fact
//! about the code and not just an intention.

pub mod agent_client;
pub mod hosting;
pub mod http;
pub mod node;
pub mod quota;
pub mod registry;

// The codec and the query call live in `index-client`, so a launcher can ask an
// index without depending on one. Re-exported rather than duplicated: both ends
// must agree on the format byte for byte, and two copies are two things to
// drift.
pub use index_client::{client, wire};
