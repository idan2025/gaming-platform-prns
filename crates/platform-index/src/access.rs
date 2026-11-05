//! Which accounts may see, or run, a server through this index.
//!
//! Today's rule is the owner's, and [`OwnerOnly`] is it. A build that lets
//! more people in (shares, invitations, public servers) implements [`Access`]
//! and sets it with `Hosting::with_access`. Nothing else changes: the index's
//! other checks still read the owner.
//!
//! What this trait covers is what an account may **see in its list** and
//! **run** (stop, start, read the log, change the map), and which heard
//! servers the index **lists** to everyone. What it never covers,
//! whatever a policy says, is destroying a server, moving it, its backups, a
//! region request, or erasing an account: those stay the owner's, and they go
//! through `Hosting::owned`, which is owner-only by name. A policy that wants
//! more for one of those has to change that code, in public, where it can be
//! read.

use std::sync::Arc;

use crate::hosting::HostedInstance;
use crate::quota::AccountId;

pub trait Access: Send + Sync {
    /// Whether this server is listed in the account's own list.
    fn may_see(&self, account: &AccountId, server: &HostedInstance) -> bool;

    /// Whether this account may stop, start, read the log of or change the map
    /// of this server.
    fn may_operate(&self, account: &AccountId, server: &HostedInstance) -> bool;

    /// Whether this index lists a server it heard, by its destination, to
    /// anyone who browses it (`/servers` and the Reticulum query alike). Every
    /// server, unless a policy keeps the directory to some. It changes what
    /// this cache shows, never what the mesh carries: a launcher still hears
    /// every announce itself.
    /// `config` is the hosting file in force, so a policy can follow it.
    fn listed(&self, _config: &crate::hosting::HostingConfig, _destination: &[u8; 16]) -> bool {
        true
    }
}

/// The owner, and nobody else. What the index does when nothing else is set.
#[derive(Debug, Default, Clone, Copy)]
pub struct OwnerOnly;

impl Access for OwnerOnly {
    fn may_see(&self, account: &AccountId, server: &HostedInstance) -> bool {
        is_owner(account, server)
    }

    fn may_operate(&self, account: &AccountId, server: &HostedInstance) -> bool {
        is_owner(account, server)
    }
}

/// The owner check every destructive or moving operation keeps.
pub fn is_owner(account: &AccountId, server: &HostedInstance) -> bool {
    server.owner.as_deref() == Some(account.0.as_str())
}

/// The policy used when none is set.
pub fn owner_only() -> Arc<dyn Access> {
    Arc::new(OwnerOnly)
}
