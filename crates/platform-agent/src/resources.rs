//! Whether a server's memory and CPU fit this host (`MINECRAFT.md` §9).
//!
//! One judgement, asked twice: by the page while a person picks a size, and by
//! `create`, which refuses what this refuses. It reads the host's own figures
//! (`MemAvailable`, the CPU count), never a guess, and a host that reports no
//! memory figure is a warning, not a pass dressed as one.

use serde::Serialize;

use crate::instance::MemorySpec;

const MIB: u64 = 1024 * 1024;
/// Memory always left to the host: this, or a tenth of it if that is more.
pub const MIN_RESERVE_BYTES: u64 = 1024 * MIB;

/// What the host has, as its kernel reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostResources {
    pub total_bytes: u64,
    pub available_bytes: u64,
    pub cpus: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ResourceLevel {
    Ok,
    Warn,
    Refuse,
}

/// The answer, with the numbers a page shows beside it. Its keys are the
/// page's contract.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ResourceVerdict {
    pub level: ResourceLevel,
    /// Plain sentences, worst first.
    pub messages: Vec<String>,
    pub total_mb: Option<u64>,
    pub available_mb: Option<u64>,
    pub reserve_mb: Option<u64>,
    /// The container's hard limit this request sets.
    pub limit_mb: Option<u64>,
    pub host_cpus: u32,
}

/// The memory a host keeps for itself.
pub fn reserve_bytes(total: u64) -> u64 {
    MIN_RESERVE_BYTES.max(total / 10)
}

/// Judge a request. `operator_limit_bytes` is the node operator's own ceiling
/// for this game, if they set one; a request past it is refused.
pub fn judge(
    host: Option<HostResources>,
    memory: Option<MemorySpec>,
    cpus: Option<u16>,
    operator_limit_bytes: Option<i64>,
) -> ResourceVerdict {
    let mut refuse = Vec::new();
    let mut warn = Vec::new();
    let limit = memory.map(|m| m.container_limit_bytes());
    let host_cpus = host.map(|h| h.cpus).unwrap_or(0);

    if let (Some(limit), Some(ceiling)) = (limit, operator_limit_bytes) {
        if limit > ceiling.max(0) as u64 {
            refuse.push(format!(
                "This node's operator allows this game at most {} MB; this server would need {} MB with the Java overhead.",
                ceiling.max(0) as u64 / MIB,
                limit / MIB
            ));
        }
    }
    match (host, limit) {
        (Some(h), Some(limit)) => {
            let reserve = reserve_bytes(h.total_bytes);
            if limit > h.available_bytes.saturating_sub(reserve) {
                refuse.push(format!(
                    "Not enough free memory: this server would need {} MB, and the host has {} MB free \
                     and must keep {} MB for itself. Pick a smaller size or stop another server.",
                    limit / MIB,
                    h.available_bytes / MIB,
                    reserve / MIB
                ));
            } else if limit > h.available_bytes / 2 {
                warn.push(format!(
                    "This server would take more than half of the host's free memory ({} of {} MB). \
                     Other servers may run short.",
                    limit / MIB,
                    h.available_bytes / MIB
                ));
            }
        }
        (None, Some(_)) => warn.push("This host does not report its free memory, so the size could not be checked.".to_string()),
        _ => {}
    }
    if let (Some(c), Some(h)) = (cpus, host) {
        if u32::from(c) > h.cpus {
            refuse.push(format!("This host has {} CPU cores; a server cannot have {c}.", h.cpus));
        } else if u32::from(c) * 2 > h.cpus {
            warn.push(format!("{c} of the host's {} CPU cores is more than half of them.", h.cpus));
        }
    }

    let level = if !refuse.is_empty() {
        ResourceLevel::Refuse
    } else if !warn.is_empty() {
        ResourceLevel::Warn
    } else {
        ResourceLevel::Ok
    };
    refuse.extend(warn);
    ResourceVerdict {
        level,
        messages: refuse,
        total_mb: host.map(|h| h.total_bytes / MIB),
        available_mb: host.map(|h| h.available_bytes / MIB),
        reserve_mb: host.map(|h| reserve_bytes(h.total_bytes) / MIB),
        limit_mb: limit.map(|l| l / MIB),
        host_cpus,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host(total_gb: u64, free_gb: u64, cpus: u32) -> Option<HostResources> {
        Some(HostResources { total_bytes: total_gb * 1024 * MIB, available_bytes: free_gb * 1024 * MIB, cpus })
    }

    fn heap(min_mb: u32, max_mb: u32) -> Option<MemorySpec> {
        Some(MemorySpec { min_mb, max_mb })
    }

    #[test]
    fn a_size_that_fits_easily_is_fine() {
        let v = judge(host(32, 24, 8), heap(2048, 4096), Some(2), None);
        assert_eq!(v.level, ResourceLevel::Ok, "{:?}", v.messages);
        assert_eq!(v.limit_mb, Some(4096 + 1024 + 512));
    }

    #[test]
    fn more_than_half_of_free_memory_is_a_warning() {
        // 6 GB heap: limit 8192 MB, over half of 12 GB free, under free less reserve.
        let v = judge(host(16, 12, 8), heap(4096, 6144), None, None);
        assert_eq!(v.level, ResourceLevel::Warn, "{:?}", v.messages);
    }

    #[test]
    fn leaving_less_than_the_reserve_is_refused() {
        // 16 GB host keeps 1.6 GB; 8 GB free; a 6 GB heap needs 8 GB.
        let v = judge(host(16, 8, 8), heap(4096, 6144), None, None);
        assert_eq!(v.level, ResourceLevel::Refuse);
        assert!(v.messages[0].contains("Not enough free memory"), "{:?}", v.messages);
        // A small host keeps at least 1 GB.
        assert_eq!(reserve_bytes(4 * 1024 * MIB), MIN_RESERVE_BYTES);
    }

    #[test]
    fn the_operators_ceiling_and_the_cpu_count_hold() {
        let v = judge(host(64, 60, 8), heap(2048, 4096), None, Some(4 * 1024 * MIB as i64));
        assert_eq!(v.level, ResourceLevel::Refuse, "past the operator's ceiling");
        assert_eq!(judge(host(64, 60, 4), None, Some(5), None).level, ResourceLevel::Refuse);
        assert_eq!(judge(host(64, 60, 4), None, Some(3), None).level, ResourceLevel::Warn);
        assert_eq!(judge(host(64, 60, 4), None, Some(2), None).level, ResourceLevel::Ok);
    }

    #[test]
    fn a_host_that_reports_nothing_is_a_warning_not_a_pass() {
        assert_eq!(judge(None, heap(1024, 2048), None, None).level, ResourceLevel::Warn);
        assert_eq!(judge(None, None, None, None).level, ResourceLevel::Ok, "nothing asked, nothing to check");
    }
}
