//! Quotas and idle reaping.
//!
//! # Why this is load-bearing and not cosmetic
//!
//! An account here is a keypair somebody generated. There is no email, no
//! payment, no human review — that is the point of `platform-auth`, and it is
//! also why `DESIGN.md` §4 says public deploy plus anonymous identities is free
//! compute. Without quotas, one script makes unlimited accounts and fills a node.
//! `PLAN.md` §8 phase 4 therefore asks for per-account quotas and idle reaping
//! **from day one** rather than after the first incident.
//!
//! # The clock is an argument, never a call
//!
//! Nothing here reads `SystemTime::now()`. A policy engine that reads the clock
//! cannot be tested against the moments that matter — the second before a
//! timeout, a restored record with a timestamp from last week, a clock that went
//! backwards during an NTP correction. All three are handled below, and all
//! three have tests, which would be impossible if `now` were ambient.
//!
//! Every duration is a saturating subtraction. A timestamp in the future means
//! a clock this process cannot reason about, and the safe reading of "I do not
//! know how old this is" is **brand new** — never "infinitely idle", which would
//! reap a running server because someone's clock drifted.

use std::collections::BTreeMap;
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuotaPolicy {
    pub max_instances_per_account: u32,
    pub max_total_instances: u32,
    /// Stop an instance with no players for this long. `None` disables reaping,
    /// which is only reasonable on a node whose instances are all deliberate.
    pub idle_timeout: Option<Duration>,
    /// Never reap an instance younger than this, however idle.
    ///
    /// A server that started thirty seconds ago has no players *by definition* —
    /// nobody has had time to join. Without this, reaping and starting fight
    /// each other and no server ever survives its first minute.
    pub min_lifetime: Duration,
    /// Refuse a new instance from an account that created one within this long.
    pub create_cooldown: Option<Duration>,
    /// Accounts that differ from the rules above. Anyone not listed gets the
    /// rules as written.
    #[serde(default)]
    pub overrides: BTreeMap<AccountId, AccountLimits>,
    /// Remove a server that has been stopped for this long. `None` keeps
    /// stopped servers for ever, which is the default.
    ///
    /// Removing takes the container only. The server's files stay on the node
    /// until the operator removes them, the same as any server removed by hand.
    #[serde(default)]
    pub stopped_expiry: Option<Duration>,
}

/// What one account is allowed beyond (or below) the shared rules. Every field
/// left out means "as everyone else".
///
/// Nothing here can lift the node-wide cap: `max_total_instances` is checked
/// for every account, so an override can reserve room for one account but never
/// make a full node take another server.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AccountLimits {
    /// Instances this account may hold, in place of `max_instances_per_account`.
    #[serde(default)]
    pub max_instances: Option<u32>,
    /// The outer `Some` is "this account has its own rule"; the inner `None` is
    /// "no cooldown at all".
    #[serde(default)]
    pub create_cooldown: Option<Option<Duration>>,
    /// Never stop this account's servers for being idle, and never remove them
    /// for having been stopped too long.
    #[serde(default)]
    pub never_reap: bool,
    /// The node classes this account may ask for. An account that lists none
    /// may ask for none, so naming a class is never self-service.
    #[serde(default)]
    pub classes: Vec<String>,
}

impl Default for QuotaPolicy {
    /// Conservative on purpose: an operator who never touches this file should
    /// end up with a node that cannot be trivially exhausted.
    fn default() -> Self {
        Self {
            max_instances_per_account: 3,
            max_total_instances: 20,
            idle_timeout: Some(Duration::from_secs(30 * 60)),
            min_lifetime: Duration::from_secs(5 * 60),
            create_cooldown: Some(Duration::from_secs(30)),
            overrides: BTreeMap::new(),
            stopped_expiry: None,
        }
    }
}

/// An account, which is an identity hash in hex. Opaque here.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct AccountId(pub String);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstanceRecord {
    pub instance_id: String,
    pub account: AccountId,
    pub created_at: SystemTime,
    /// `None` means it has never had a player at all.
    pub last_player_seen: Option<SystemTime>,
    pub players_now: u32,
    /// An operator pinned it. Outranks every reap rule.
    pub exempt_from_reaping: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DenyReason {
    AccountInstanceLimit { limit: u32, held: u32 },
    NodeInstanceLimit { limit: u32, held: u32 },
    Cooldown { retry_after: Duration },
}

impl core::fmt::Display for DenyReason {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::AccountInstanceLimit { limit, held } => write!(
                f,
                "you already have {held} of your {limit} servers running; \
                 stop one before starting another"
            ),
            Self::NodeInstanceLimit { limit, held } => {
                write!(f, "this node is full: {held} of {limit} servers are running")
            }
            Self::Cooldown { retry_after } => write!(
                f,
                "too many servers started too quickly; try again in {} seconds",
                retry_after.as_secs().max(1)
            ),
        }
    }
}

impl std::error::Error for DenyReason {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReapReason {
    IdleTooLong { idle_for: Duration },
    NeverHadPlayers { age: Duration },
}

impl core::fmt::Display for ReapReason {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::IdleTooLong { idle_for } => {
                write!(f, "no players for {} seconds", idle_for.as_secs())
            }
            Self::NeverHadPlayers { age } => {
                write!(f, "never had a player in {} seconds", age.as_secs())
            }
        }
    }
}

/// A timestamp in the future reads as age zero, not as "infinitely old".
fn age_since(then: SystemTime, now: SystemTime) -> Duration {
    now.duration_since(then).unwrap_or(Duration::ZERO)
}

pub struct Quotas {
    policy: QuotaPolicy,
}

impl Quotas {
    pub fn new(policy: QuotaPolicy) -> Self {
        Self { policy }
    }

    pub fn policy(&self) -> &QuotaPolicy {
        &self.policy
    }

    /// The limit that applies to `account`: its override if it has one.
    pub fn max_instances_for(&self, account: &AccountId) -> u32 {
        self.policy
            .overrides
            .get(account)
            .and_then(|o| o.max_instances)
            .unwrap_or(self.policy.max_instances_per_account)
    }

    /// How long a stopped server of `account` is kept before it is removed, or
    /// `None` when it is kept for ever.
    pub fn stopped_expiry_for(&self, account: &AccountId) -> Option<Duration> {
        if self.policy.overrides.get(account).is_some_and(|o| o.never_reap) {
            return None;
        }
        self.policy.stopped_expiry
    }

    /// May `account` ask for servers on nodes of `class`?
    pub fn may_use_class(&self, account: &AccountId, class: &str) -> bool {
        self.policy.overrides.get(account).is_some_and(|o| o.classes.iter().any(|c| c == class))
    }

    /// Whether a server of `account` stopped for `stopped_for` is due removal.
    pub fn stopped_expired(&self, account: &AccountId, stopped_for: Duration) -> bool {
        self.stopped_expiry_for(account).is_some_and(|after| stopped_for >= after)
    }

    fn cooldown_for(&self, account: &AccountId) -> Option<Duration> {
        match self.policy.overrides.get(account).and_then(|o| o.create_cooldown) {
            Some(own) => own,
            None => self.policy.create_cooldown,
        }
    }

    pub fn held_by(&self, account: &AccountId, existing: &[InstanceRecord]) -> u32 {
        existing.iter().filter(|i| &i.account == account).count() as u32
    }

    /// May `account` create another instance?
    ///
    /// Counts `existing` rather than trusting a running total. A leaked
    /// increment would otherwise lock an account out forever and a leaked
    /// decrement would hand out free instances; counting reality cannot drift.
    pub fn admit(
        &self,
        account: &AccountId,
        existing: &[InstanceRecord],
        now: SystemTime,
    ) -> Result<(), DenyReason> {
        // Account limit first: "you already have three servers" is actionable,
        // where "the node is full" reads as somebody else's problem even when
        // the caller is the one filling it.
        let held = self.held_by(account, existing);
        let limit = self.max_instances_for(account);
        if held >= limit {
            return Err(DenyReason::AccountInstanceLimit { limit, held });
        }

        let total = existing.len() as u32;
        if total >= self.policy.max_total_instances {
            return Err(DenyReason::NodeInstanceLimit {
                limit: self.policy.max_total_instances,
                held: total,
            });
        }

        if let Some(cooldown) = self.cooldown_for(account) {
            let newest = existing
                .iter()
                .filter(|i| &i.account == account)
                .map(|i| age_since(i.created_at, now))
                .min();
            if let Some(since) = newest {
                if since < cooldown {
                    return Err(DenyReason::Cooldown {
                        retry_after: cooldown.saturating_sub(since),
                    });
                }
            }
        }
        Ok(())
    }

    /// Which instances should be stopped now, and why. Sorted by id so the
    /// answer is the same whatever order the caller collected them in.
    pub fn to_reap(
        &self,
        existing: &[InstanceRecord],
        now: SystemTime,
    ) -> Vec<(String, ReapReason)> {
        let Some(idle_timeout) = self.policy.idle_timeout else {
            return Vec::new();
        };

        let mut out: Vec<(String, ReapReason)> = existing
            .iter()
            .filter_map(|i| {
                // An operator pinned it.
                if i.exempt_from_reaping {
                    return None;
                }
                // The account's own rule: its servers are kept.
                if self.policy.overrides.get(&i.account).is_some_and(|o| o.never_reap) {
                    return None;
                }
                // Someone is playing. This ends the question even if
                // `last_player_seen` is stale or missing: kicking people out of
                // a running game because a stats field lagged is worse than not
                // reaping at all.
                if i.players_now > 0 {
                    return None;
                }
                let age = age_since(i.created_at, now);
                // Too young to judge — nobody has had time to join.
                if age < self.policy.min_lifetime {
                    return None;
                }
                match i.last_player_seen {
                    Some(seen) => {
                        let idle_for = age_since(seen, now);
                        (idle_for > idle_timeout)
                            .then_some((i.instance_id.clone(), ReapReason::IdleTooLong { idle_for }))
                    }
                    None => (age > self.policy.min_lifetime + idle_timeout)
                        .then_some((i.instance_id.clone(), ReapReason::NeverHadPlayers { age })),
                }
            })
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t0() -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000)
    }

    fn policy() -> QuotaPolicy {
        QuotaPolicy {
            max_instances_per_account: 2,
            max_total_instances: 3,
            idle_timeout: Some(Duration::from_secs(600)),
            min_lifetime: Duration::from_secs(300),
            create_cooldown: Some(Duration::from_secs(30)),
            overrides: BTreeMap::new(),
            stopped_expiry: None,
        }
    }

    #[test]
    fn an_account_may_ask_only_for_the_classes_it_was_given() {
        let q = with_override(
            AccountLimits { classes: vec!["gold".into()], ..Default::default() },
            1,
        );
        assert!(q.may_use_class(&account(1), "gold"));
        assert!(!q.may_use_class(&account(1), "silver"), "a class it was not given");
        assert!(!q.may_use_class(&account(2), "gold"), "an account with no entry has none");
    }

    #[test]
    fn a_stopped_server_is_removed_only_after_the_expiry() {
        let mut p = policy();
        p.stopped_expiry = Some(Duration::from_secs(86_400));
        let q = Quotas::new(p);
        assert!(!q.stopped_expired(&account(1), Duration::from_secs(86_399)));
        assert!(q.stopped_expired(&account(1), Duration::from_secs(86_400)));
    }

    #[test]
    fn with_no_expiry_set_a_stopped_server_is_kept_for_ever() {
        let q = Quotas::new(policy());
        assert!(!q.stopped_expired(&account(1), Duration::from_secs(10 * 365 * 86_400)));
    }

    #[test]
    fn a_never_reap_account_keeps_its_stopped_servers_too() {
        let mut p = policy();
        p.stopped_expiry = Some(Duration::from_secs(60));
        p.overrides.insert(account(1), AccountLimits { never_reap: true, ..Default::default() });
        let q = Quotas::new(p);
        assert!(!q.stopped_expired(&account(1), Duration::from_secs(1_000_000)));
        assert!(q.stopped_expired(&account(2), Duration::from_secs(1_000_000)));
    }

    fn with_override(limits: AccountLimits, who: u8) -> Quotas {
        let mut p = policy();
        p.overrides.insert(account(who), limits);
        Quotas::new(p)
    }

    fn account(n: u8) -> AccountId {
        AccountId(format!("acct{n}"))
    }

    fn rec(id: &str, acct: u8, created_ago: u64) -> InstanceRecord {
        InstanceRecord {
            instance_id: id.to_string(),
            account: account(acct),
            created_at: t0() - Duration::from_secs(created_ago),
            last_player_seen: None,
            players_now: 0,
            exempt_from_reaping: false,
        }
    }

    #[test]
    fn an_account_under_both_limits_is_admitted() {
        let q = Quotas::new(policy());
        assert_eq!(q.admit(&account(1), &[], t0()), Ok(()));
    }

    #[test]
    fn an_account_at_its_limit_is_denied_and_other_accounts_do_not_count() {
        let q = Quotas::new(policy());
        let mine = vec![rec("a", 1, 1000), rec("b", 1, 1000)];
        assert_eq!(
            q.admit(&account(1), &mine, t0()),
            Err(DenyReason::AccountInstanceLimit { limit: 2, held: 2 })
        );
        // Somebody else's two servers are not mine.
        let theirs = vec![rec("a", 2, 1000), rec("b", 2, 1000)];
        assert_eq!(q.admit(&account(1), &theirs, t0()), Ok(()));
    }

    #[test]
    fn a_full_node_denies_even_an_account_holding_nothing() {
        let q = Quotas::new(policy());
        let others = vec![rec("a", 2, 1000), rec("b", 3, 1000), rec("c", 4, 1000)];
        assert_eq!(
            q.admit(&account(1), &others, t0()),
            Err(DenyReason::NodeInstanceLimit { limit: 3, held: 3 })
        );
    }

    /// The caller shows the first reason, so it has to be the useful one.
    #[test]
    fn the_account_limit_is_reported_in_preference_to_the_node_limit() {
        let q = Quotas::new(policy());
        let existing = vec![rec("a", 1, 1000), rec("b", 1, 1000), rec("c", 2, 1000)];
        assert!(matches!(
            q.admit(&account(1), &existing, t0()),
            Err(DenyReason::AccountInstanceLimit { .. })
        ));
    }

    #[test]
    fn a_cooldown_delays_a_second_create_and_then_allows_it() {
        let q = Quotas::new(policy());
        let just_made = vec![rec("a", 1, 10)];
        assert_eq!(
            q.admit(&account(1), &just_made, t0()),
            Err(DenyReason::Cooldown { retry_after: Duration::from_secs(20) })
        );
        let older = vec![rec("a", 1, 31)];
        assert_eq!(q.admit(&account(1), &older, t0()), Ok(()));
    }

    /// The rule that outranks every other reap rule.
    #[test]
    fn an_instance_with_players_is_never_reaped() {
        let q = Quotas::new(policy());
        let mut r = rec("busy", 1, 100_000);
        r.players_now = 4;
        r.last_player_seen = Some(t0() - Duration::from_secs(99_999));
        assert!(q.to_reap(&[r], t0()).is_empty());
    }

    #[test]
    fn a_young_instance_is_never_reaped_however_empty() {
        let q = Quotas::new(policy());
        let r = rec("fresh", 1, 60);
        assert!(q.to_reap(&[r], t0()).is_empty(), "it has not had time to fill");
    }

    #[test]
    fn an_instance_that_never_had_a_player_is_eventually_reaped() {
        let q = Quotas::new(policy());
        // min_lifetime 300 + idle 600 = 900.
        assert!(q.to_reap(&[rec("empty", 1, 899)], t0()).is_empty());
        let reaped = q.to_reap(&[rec("empty", 1, 901)], t0());
        assert_eq!(reaped.len(), 1);
        assert!(matches!(reaped[0].1, ReapReason::NeverHadPlayers { .. }));
    }

    #[test]
    fn an_instance_idle_past_the_timeout_is_reaped() {
        let q = Quotas::new(policy());
        let mut r = rec("quiet", 1, 5000);
        r.last_player_seen = Some(t0() - Duration::from_secs(601));
        let reaped = q.to_reap(&[r.clone()], t0());
        assert!(matches!(reaped[0].1, ReapReason::IdleTooLong { .. }));

        r.last_player_seen = Some(t0() - Duration::from_secs(599));
        assert!(q.to_reap(&[r], t0()).is_empty());
    }

    #[test]
    fn an_exempt_instance_survives_every_reason_at_once() {
        let q = Quotas::new(policy());
        let mut r = rec("pinned", 1, 100_000);
        r.last_player_seen = Some(t0() - Duration::from_secs(99_999));
        r.exempt_from_reaping = true;
        assert!(q.to_reap(&[r], t0()).is_empty());
    }

    #[test]
    fn reaping_can_be_switched_off_entirely() {
        let q = Quotas::new(QuotaPolicy { idle_timeout: None, ..policy() });
        assert!(q.to_reap(&[rec("ancient", 1, 10_000_000)], t0()).is_empty());
    }

    /// Clocks move backwards. A record from "the future" must read as new, not
    /// as infinitely idle — the latter would reap a server somebody is about to
    /// join because NTP corrected during startup.
    #[test]
    fn a_creation_time_in_the_future_reaps_nothing() {
        let q = Quotas::new(policy());
        let mut r = rec("skewed", 1, 0);
        r.created_at = t0() + Duration::from_secs(10_000);
        assert!(q.to_reap(&[r], t0()).is_empty());
    }

    #[test]
    fn a_last_seen_in_the_future_reaps_nothing() {
        let q = Quotas::new(policy());
        let mut r = rec("skewed", 1, 100_000);
        r.last_player_seen = Some(t0() + Duration::from_secs(10_000));
        assert!(q.to_reap(&[r], t0()).is_empty());
    }

    /// And skew must not make a cooldown last forever either.
    #[test]
    fn a_creation_time_in_the_future_does_not_wedge_the_cooldown() {
        let q = Quotas::new(policy());
        let mut r = rec("skewed", 1, 0);
        r.created_at = t0() + Duration::from_secs(10_000);
        assert!(matches!(
            q.admit(&account(1), &[r], t0()),
            Err(DenyReason::Cooldown { .. })
        ));
    }

    #[test]
    fn to_reap_is_sorted_and_order_independent() {
        let q = Quotas::new(policy());
        let a = rec("aaa", 1, 100_000);
        let b = rec("bbb", 1, 100_000);
        let c = rec("ccc", 1, 100_000);
        let one = q.to_reap(&[c.clone(), a.clone(), b.clone()], t0());
        let two = q.to_reap(&[a, b, c], t0());
        assert_eq!(one, two);
        let ids: Vec<&str> = one.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(ids, vec!["aaa", "bbb", "ccc"]);
    }

    #[test]
    fn the_default_policy_is_not_wide_open() {
        let d = QuotaPolicy::default();
        assert!(d.max_instances_per_account > 0 && d.max_instances_per_account <= 10);
        assert!(d.max_total_instances > 0);
        assert!(d.idle_timeout.is_some(), "reaping off by default is free compute");
        assert!(d.min_lifetime > Duration::ZERO);
    }

    #[test]
    fn an_account_with_its_own_limit_gets_it_and_others_keep_the_shared_one() {
        let q = with_override(AccountLimits { max_instances: Some(3), ..Default::default() }, 1);
        let mine = vec![rec("a", 1, 1000), rec("b", 1, 1000)];
        assert_eq!(q.admit(&account(1), &mine, t0()), Ok(()), "two of its three");
        let theirs = vec![rec("a", 2, 1000), rec("b", 2, 1000)];
        assert_eq!(
            q.admit(&account(2), &theirs, t0()),
            Err(DenyReason::AccountInstanceLimit { limit: 2, held: 2 }),
            "another account is not widened"
        );
    }

    #[test]
    fn an_override_can_lower_a_limit_too() {
        let q = with_override(AccountLimits { max_instances: Some(1), ..Default::default() }, 1);
        assert_eq!(
            q.admit(&account(1), &[rec("a", 1, 1000)], t0()),
            Err(DenyReason::AccountInstanceLimit { limit: 1, held: 1 })
        );
    }

    /// An override reserves room for one account; it never makes a full node
    /// take another server.
    #[test]
    fn an_override_cannot_lift_the_node_wide_cap() {
        let q = with_override(AccountLimits { max_instances: Some(50), ..Default::default() }, 1);
        let others = vec![rec("a", 2, 1000), rec("b", 3, 1000), rec("c", 4, 1000)];
        assert_eq!(
            q.admit(&account(1), &others, t0()),
            Err(DenyReason::NodeInstanceLimit { limit: 3, held: 3 })
        );
    }

    #[test]
    fn an_account_can_have_its_own_cooldown_or_none() {
        let just_made = vec![rec("a", 1, 10)];
        let none = with_override(AccountLimits { create_cooldown: Some(None), ..Default::default() }, 1);
        assert_eq!(none.admit(&account(1), &just_made, t0()), Ok(()));
        let long = with_override(
            AccountLimits { create_cooldown: Some(Some(Duration::from_secs(100))), ..Default::default() },
            1,
        );
        assert_eq!(
            long.admit(&account(1), &just_made, t0()),
            Err(DenyReason::Cooldown { retry_after: Duration::from_secs(90) })
        );
        // Someone else still has the shared 30 s.
        let theirs = vec![rec("a", 2, 10)];
        assert_eq!(
            none.admit(&account(2), &theirs, t0()),
            Err(DenyReason::Cooldown { retry_after: Duration::from_secs(20) })
        );
    }

    #[test]
    fn a_never_reap_account_is_kept_and_its_neighbour_is_not() {
        let q = with_override(AccountLimits { never_reap: true, ..Default::default() }, 1);
        let kept = rec("kept", 1, 100_000);
        let gone = rec("gone", 2, 100_000);
        let reaped = q.to_reap(&[kept, gone], t0());
        let ids: Vec<&str> = reaped.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(ids, vec!["gone"]);
    }
}
