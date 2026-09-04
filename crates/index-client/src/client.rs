//! Querying an index over Reticulum, from a launcher's side.
//!
//! The mirror of `node.rs`. A launcher that has heard an index announce can ask
//! it for a list without ever touching the internet — and, crucially, can decide
//! not to: an index is one more source alongside what the launcher hears
//! directly, never a replacement for it.

use anyhow::{anyhow, Result};
use personal_rns::prelude::{DestinationHash, PrnsNodeHandle};
use prns_core::routing::request_handlers::RequestPathHash;
use tracing::debug;

use crate::wire::{decode_result, IndexQuery, QueryResult, QUERY_ENDPOINT_ID};

/// Ask one index for a list.
///
/// Opens a link, asks, closes. An index query is a question, not a session — a
/// launcher polling several indexes should not be holding links open to all of
/// them.
pub async fn query_index(
    handle: &PrnsNodeHandle,
    index: DestinationHash,
    query: &IndexQuery,
) -> Result<QueryResult> {
    let link_id = match handle.establish_link(index).await {
        Ok(id) => id,
        Err(e) => {
            debug!(error = ?e, "no route to the index; requesting a path first");
            handle
                .request_path(index)
                .await
                .map_err(|pe| anyhow!("no path to the index: {pe:?}"))?;
            handle
                .establish_link(index)
                .await
                .map_err(|e2| anyhow!("link to the index failed: {e2:?}"))?
        }
    };

    let outcome = handle
        .request(link_id, RequestPathHash::of(QUERY_ENDPOINT_ID), &query.encode())
        .await;
    let _ = handle.close_link(link_id);

    let (response, _rtt) = outcome.map_err(|e| anyhow!("index query failed: {e:?}"))?;
    decode_result(&response).map_err(|e| anyhow!("index sent a result we cannot read: {e}"))
}
