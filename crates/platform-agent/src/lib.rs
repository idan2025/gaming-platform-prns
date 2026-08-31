//! Per-node daemon: many game servers on one host, one copy of the content.
//!
//! `PLAN.md` §8 phase 3. `agent::Agent` is the orchestration; `store` holds the
//! layout and its security boundary; `config` holds the split between what a
//! game pack describes and what a node's operator decides.

pub mod agent;
pub mod api;
pub mod config;
pub mod content;
pub mod docker;
pub mod instance;
pub mod ports;
pub mod store;
pub mod uplink;
pub mod uplink_wire;
