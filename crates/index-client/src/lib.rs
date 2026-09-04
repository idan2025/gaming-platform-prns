//! Asking an index for a server list, and the wire format both ends speak.
//!
//! # Why this is its own crate
//!
//! A launcher must be able to *use* an index without becoming one. The index
//! server pulls in an HTTP front door, a quota engine and — through
//! `platform-agent` — a Docker client; none of that belongs in a desktop game
//! launcher, and a dependency that dragged it in would be a launcher carrying
//! bollard so it could read a list.
//!
//! So the half a client needs lives here: the codec, which both ends must agree
//! on byte for byte, and the query call. `platform-index` depends on this and
//! re-exports it, so the server keeps one definition of the format rather than
//! a copy that can drift.
//!
//! # What this does not change
//!
//! An index remains **a cache of the mesh, never the source of truth**
//! (`DESIGN.md` §0). This crate is one more place a launcher may look, beside
//! what it hears directly and what it remembers; the zero-infrastructure path
//! must keep working with none of it.
pub mod client;
pub mod wire;
