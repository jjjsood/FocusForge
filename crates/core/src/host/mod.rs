// SPDX-License-Identifier: GPL-3.0-or-later

//! Host resource facts: memory totals and availability, the applicable
//! container/cgroup limit, and the logical core count -- gathered once per
//! process through [`snapshot`], and re-readable on demand through
//! [`HostProbe`] for a consumer that needs a fresh read during a run rather
//! than the cached snapshot.
//!
//! This module answers "what does the host have"; it never decides a
//! budget's number -- every CPU or GPU budget reads these facts directly
//! rather than through [`crate::params`]. A resource budget is not an
//! algorithm parameter.
//!
//! Every probe failure -- an unsupported platform, or a total memory of
//! zero -- degrades to [`HostFacts::fallback`] rather than failing a run.
//! Nothing here returns `Result`, and nothing panics.

use std::sync::{Mutex, OnceLock};

use sysinfo::{MemoryRefreshKind, ProcessRefreshKind, ProcessesToUpdate, RefreshKind, System};

/// Where a [`HostFacts`] value came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FactsOrigin {
    /// Read from a live probe.
    Probed,
    /// The probe was unavailable or failed; every memory field is zeroed
    /// and `logical_cores` is 1. A consumer that sees `Fallback` is
    /// expected to fall back to its own stated constant.
    Fallback,
}

/// A snapshot of what the host has: total and currently available RAM, the
/// applicable container/cgroup limit (when one applies), and the logical
/// core count. Bytes throughout (sysinfo 0.39 reports bytes).
///
/// Neither `available_memory()` nor `usable_available()` is a safety
/// guarantee: "available" is a moving number, delivered here as a starting
/// estimate only. A container or cgroup limit can be far
/// below the host total -- `usable_memory()`/`usable_available()` clamp to
/// it, since reading the host total inside a container would size a budget
/// the process cannot honour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostFacts {
    total_memory: u64,
    available_memory: u64,
    cgroup_limit: Option<u64>,
    cgroup_available: Option<u64>,
    logical_cores: usize,
    origin: FactsOrigin,
    process_rss: Option<u64>,
}

impl HostFacts {
    /// Builds a `HostFacts` with `origin: Probed`, applying the "no
    /// effective limit" normalization: sysinfo already clamps a cgroup's
    /// reported total to `min(MemTotal, every ancestor limit)` and maps an
    /// unlimited `memory.max` to `u64::MAX`, so a `cgroup_limit` that is
    /// equal to or above `total_memory` is not a real ceiling and is
    /// dropped to `None` here -- the one place that rule is applied,
    /// whether the caller is the real probe or a test's fabricated values.
    /// `cgroup_available` is only meaningful alongside an effective
    /// `cgroup_limit`, so it is dropped to `None` too when the limit is.
    pub fn new(
        total_memory: u64,
        available_memory: u64,
        cgroup_limit: Option<u64>,
        cgroup_available: Option<u64>,
        logical_cores: usize,
    ) -> Self {
        let cgroup_limit = cgroup_limit.filter(|&limit| limit < total_memory);
        let cgroup_available = cgroup_available.filter(|_| cgroup_limit.is_some());
        HostFacts {
            total_memory,
            available_memory,
            cgroup_limit,
            cgroup_available,
            logical_cores,
            origin: FactsOrigin::Probed,
            process_rss: None,
        }
    }

    /// The degraded snapshot used when a probe is unavailable or fails:
    /// every memory field zeroed, one logical core, `origin: Fallback`.
    pub const fn fallback() -> Self {
        HostFacts {
            total_memory: 0,
            available_memory: 0,
            cgroup_limit: None,
            cgroup_available: None,
            logical_cores: 1,
            origin: FactsOrigin::Fallback,
            process_rss: None,
        }
    }

    /// Consuming builder: attaches this process's resident set size, in
    /// bytes, as read by [`SysinfoProbe`]. `HostFacts::new`'s five-argument
    /// shape has call sites in this module's own tests and in five
    /// `resolve_in` tests in `crate::params`, so the RSS a back-off signal
    /// needs is attached here instead of widening that constructor.
    pub fn with_process_rss(mut self, rss: Option<u64>) -> Self {
        self.process_rss = rss;
        self
    }

    pub fn total_memory(&self) -> u64 {
        self.total_memory
    }

    pub fn available_memory(&self) -> u64 {
        self.available_memory
    }

    pub fn cgroup_limit(&self) -> Option<u64> {
        self.cgroup_limit
    }

    pub fn logical_cores(&self) -> usize {
        self.logical_cores
    }

    pub fn origin(&self) -> FactsOrigin {
        self.origin
    }

    /// This process's anonymous resident memory, in bytes, as of the read
    /// that produced this snapshot: heap and stacks, without file-backed
    /// pages such as mapped frames. Where the platform does not separate the
    /// two, the whole resident set. `None` when the process lookup failed or
    /// on [`HostFacts::fallback`]. A `None` here disarms a consumer's
    /// back-off logic rather than failing anything.
    pub fn process_rss(&self) -> Option<u64> {
        self.process_rss
    }

    /// The memory this process can actually use: the host total, clamped to
    /// the applicable cgroup limit when one applies.
    pub fn usable_memory(&self) -> u64 {
        match self.cgroup_limit {
            Some(limit) => self.total_memory.min(limit),
            None => self.total_memory,
        }
    }

    /// The memory currently available to this process: the host's own
    /// `available_memory`, clamped to the cgroup's own free-memory
    /// accounting when a cgroup limit applies. The two are different
    /// quantities -- the host's `MemAvailable` versus a cgroup's
    /// `limit - current` -- so the minimum is the conservative answer, and
    /// neither is a guarantee.
    pub fn usable_available(&self) -> u64 {
        match self.cgroup_available {
            Some(free) => self.available_memory.min(free),
            None => self.available_memory,
        }
    }
}

/// The substitution seam: anything that can answer "what does the host
/// have right now". [`SysinfoProbe`] is the real, re-readable probe;
/// [`FixedFacts`] is a fabricated-values test double, reached the same way
/// by every consumer's own tests.
pub trait HostProbe: Send + Sync {
    fn facts(&self) -> HostFacts;
}

/// A [`HostProbe`] that always answers the same fabricated [`HostFacts`],
/// for tests.
pub struct FixedFacts(pub HostFacts);

impl HostProbe for FixedFacts {
    fn facts(&self) -> HostFacts {
        self.0
    }
}

/// The real, `sysinfo`-backed probe. Re-readable: every call to `facts()`
/// refreshes memory (and, on Linux, the process's own cgroup limits) live,
/// rather than caching a single reading for the probe's lifetime -- a
/// back-off signal reads this process's RSS through a live probe during a
/// run, at a cadence its own consumer picks.
///
/// `Mutex`, not `&mut self`: the trait is `Send + Sync` because a back-off
/// consumer reads it from scheduler worker threads.
pub struct SysinfoProbe {
    sys: Mutex<System>,
}

impl SysinfoProbe {
    pub fn new() -> Self {
        let sys = System::new_with_specifics(
            RefreshKind::nothing().with_memory(MemoryRefreshKind::nothing().with_ram()),
        );
        SysinfoProbe {
            sys: Mutex::new(sys),
        }
    }
}

impl Default for SysinfoProbe {
    fn default() -> Self {
        Self::new()
    }
}

impl HostProbe for SysinfoProbe {
    fn facts(&self) -> HostFacts {
        if !sysinfo::IS_SUPPORTED_SYSTEM {
            return HostFacts::fallback();
        }

        let mut sys = match self.sys.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        sys.refresh_memory_specifics(MemoryRefreshKind::nothing().with_ram());
        let total_memory = sys.total_memory();
        if total_memory == 0 {
            return HostFacts::fallback();
        }
        let available_memory = sys.available_memory();
        let (cgroup_limit, cgroup_available, process_rss) = read_process_facts(&mut sys);

        // Not sysinfo's CPU list: `available_parallelism` already honours a
        // cgroup CPU quota, so re-deriving the same number from sysinfo
        // would be pure duplication.
        let logical_cores = std::thread::available_parallelism()
            .map(std::num::NonZeroUsize::get)
            .unwrap_or(1);

        HostFacts::new(
            total_memory,
            available_memory,
            cgroup_limit,
            cgroup_available,
            logical_cores,
        )
        .with_process_rss(process_rss)
    }
}

/// The `RssAnon` figure of a process status text, in bytes: anonymous
/// resident memory, without file-backed pages such as mapped frames. `None`
/// when the line is absent or malformed.
fn anonymous_rss(status: &str) -> Option<u64> {
    let line = status.lines().find_map(|l| l.strip_prefix("RssAnon:"))?;
    let kilobytes: u64 = line.trim().strip_suffix("kB")?.trim().parse().ok()?;
    kilobytes.checked_mul(1024)
}

/// Reads the applicable cgroup memory limit, preferring the **process's
/// own** cgroup scope over the root cgroup: `System::cgroup_limits` sees
/// only the root cgroup and therefore misses a `systemd-run`/container
/// scope (GuillaumeGomez/sysinfo#1610); the process form
/// (`Process::cgroup_limits`, new in sysinfo 0.39.0) is used first and the
/// root form only as a fallback when it returns `None`. The same single
/// process refresh also yields this process's RSS for a back-off signal, so
/// cgroup limits and RSS cost one `/proc/<pid>` round trip, not two.
///
/// On a hybrid cgroup v1+v2 host, 0.39.6 can still miss the limit
/// (GuillaumeGomez/sysinfo#1723, open), which yields the *host* total
/// instead of the container's -- an over-estimate, never an under-estimate.
fn read_process_facts(sys: &mut System) -> (Option<u64>, Option<u64>, Option<u64>) {
    let mut rss = None;
    let mut process_cgroup_limits = None;
    if let Ok(pid) = sysinfo::get_current_pid() {
        sys.refresh_processes_specifics(
            ProcessesToUpdate::Some(std::slice::from_ref(&pid)),
            false,
            ProcessRefreshKind::nothing().with_memory(),
        );
        if let Some(process) = sys.process(pid) {
            rss = Some(process.memory());
            process_cgroup_limits = process.cgroup_limits();
        }
    }
    // Mapped frame pages are file-backed and reclaimable; only anonymous
    // memory is pressure this run caused, so it replaces the whole set
    // wherever the platform reports it.
    if let Some(anonymous) = std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|status| anonymous_rss(&status))
    {
        rss = Some(anonymous);
    }
    // The process's own cgroup scope wins when it has one; the root scope
    // (`System::cgroup_limits`) is the fallback used both when the process
    // form returned `None` and when the pid lookup itself failed.
    match process_cgroup_limits.or_else(|| sys.cgroup_limits()) {
        Some(limits) => (Some(limits.total_memory), Some(limits.free_memory), rss),
        None => (None, None, rss),
    }
}

/// Gathers host facts once per process, cheap to read thereafter. A cached
/// convenience layer over [`SysinfoProbe`], not the only access path -- a
/// consumer that needs a fresh read during a run (a back-off signal)
/// constructs its own [`SysinfoProbe`] instead.
pub fn snapshot() -> HostFacts {
    static SNAPSHOT: OnceLock<HostFacts> = OnceLock::new();
    *SNAPSHOT.get_or_init(|| SysinfoProbe::new().facts())
}

/// What operating system this is, as far as the platform will say. Each
/// field is `None` when the platform does not answer.
///
/// The host name is deliberately absent: it routinely carries the user's
/// own name, and nothing here may leak it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OsDescription {
    /// The distribution or operating system name, e.g. `Ubuntu`.
    pub name: Option<String>,
    /// The operating system version, e.g. `24.04`.
    pub os_version: Option<String>,
    /// The kernel version, e.g. `6.8.0-45-generic`.
    pub kernel_version: Option<String>,
}

/// Reads this host's [`OsDescription`]. No `System` instance is built: all
/// three are associated functions.
pub fn os_description() -> OsDescription {
    OsDescription {
        name: System::name(),
        os_version: System::os_version(),
        kernel_version: System::kernel_version(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anonymous_rss_reads_the_anonymous_line_in_kilobytes() {
        let status =
            "Name:\tfocusforge\nVmRSS:\t  9000 kB\nRssAnon:\t  2048 kB\nRssFile:\t  6952 kB\n";
        assert_eq!(anonymous_rss(status), Some(2048 * 1024));
    }

    #[test]
    fn anonymous_rss_is_none_without_the_line_or_a_number() {
        assert_eq!(anonymous_rss("Name:\tfocusforge\nVmRSS:\t9000 kB\n"), None);
        assert_eq!(anonymous_rss("RssAnon:\tlots kB\n"), None);
        assert_eq!(anonymous_rss(""), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn this_process_reports_anonymous_resident_memory_on_linux() {
        let facts = SysinfoProbe::new().facts();
        let anonymous = facts.process_rss().expect("Linux exposes RssAnon");
        assert!(anonymous > 0);
        assert!(anonymous <= facts.total_memory());
    }

    #[test]
    fn probing_this_machine_returns_a_nonzero_total() {
        let facts = SysinfoProbe::new().facts();
        assert_eq!(facts.origin(), FactsOrigin::Probed);
        assert!(facts.total_memory() > 0, "facts: {facts:?}");
        assert!(facts.logical_cores() >= 1);
    }

    #[test]
    fn the_os_description_carries_at_least_a_name_or_a_kernel_version() {
        let os = os_description();
        assert!(os.name.is_some() || os.kernel_version.is_some(), "{os:?}");
        for field in [&os.name, &os.os_version, &os.kernel_version] {
            assert!(field.as_deref() != Some(""), "{os:?}");
        }
    }

    #[test]
    fn fixed_facts_round_trips_every_accessor() {
        let facts = HostFacts::new(
            16_000_000_000,
            8_000_000_000,
            Some(4_000_000_000),
            Some(1_000_000_000),
            12,
        );
        let probe = FixedFacts(facts);
        let read = probe.facts();
        assert_eq!(read.total_memory(), 16_000_000_000);
        assert_eq!(read.available_memory(), 8_000_000_000);
        assert_eq!(read.cgroup_limit(), Some(4_000_000_000));
        assert_eq!(read.logical_cores(), 12);
        assert_eq!(read.origin(), FactsOrigin::Probed);
        assert_eq!(read.usable_memory(), 4_000_000_000);
        assert_eq!(read.usable_available(), 1_000_000_000);
    }

    #[test]
    fn cgroup_limit_at_or_above_total_reports_as_none() {
        let at_total = HostFacts::new(
            16_000_000_000,
            8_000_000_000,
            Some(16_000_000_000),
            Some(1_000_000_000),
            4,
        );
        assert_eq!(at_total.cgroup_limit(), None);
        assert_eq!(at_total.usable_memory(), 16_000_000_000);
        // cgroup_available is dropped with the limit, so usable_available
        // falls back to plain available_memory.
        assert_eq!(at_total.usable_available(), 8_000_000_000);

        let above_total = HostFacts::new(16_000_000_000, 8_000_000_000, Some(u64::MAX), None, 4);
        assert_eq!(above_total.cgroup_limit(), None);
    }

    #[test]
    fn below_total_cgroup_limit_is_kept() {
        let facts = HostFacts::new(
            16_000_000_000,
            8_000_000_000,
            Some(4_000_000_000),
            Some(500_000_000),
            4,
        );
        assert_eq!(facts.cgroup_limit(), Some(4_000_000_000));
        assert_eq!(facts.usable_memory(), 4_000_000_000);
        assert_eq!(facts.usable_available(), 500_000_000);
    }

    #[test]
    fn fallback_reports_fallback_origin_and_never_panics() {
        let facts = HostFacts::fallback();
        assert_eq!(facts.origin(), FactsOrigin::Fallback);
        assert_eq!(facts.total_memory(), 0);
        assert_eq!(facts.available_memory(), 0);
        assert_eq!(facts.cgroup_limit(), None);
        assert_eq!(facts.logical_cores(), 1);
        assert_eq!(facts.usable_memory(), 0);
        assert_eq!(facts.usable_available(), 0);
    }

    #[test]
    fn snapshot_is_cached_across_calls() {
        let first = snapshot();
        let second = snapshot();
        assert_eq!(first, second);
    }

    #[test]
    fn with_process_rss_round_trips() {
        let facts = HostFacts::new(16_000_000_000, 8_000_000_000, None, None, 4)
            .with_process_rss(Some(1_234));
        assert_eq!(facts.process_rss(), Some(1_234));
    }

    #[test]
    fn fallback_reports_no_process_rss() {
        assert_eq!(HostFacts::fallback().process_rss(), None);
    }

    #[test]
    fn live_sysinfo_probe_reports_a_nonzero_process_rss() {
        let facts = SysinfoProbe::new().facts();
        // Every platform sysinfo supports (Linux/macOS/Windows) reports the
        // current process form; a `None` here would be a platform finding
        // worth recording, not a failure to paper over.
        match facts.process_rss() {
            Some(rss) => assert!(rss > 0, "facts: {facts:?}"),
            None => {
                panic!("SysinfoProbe reported no process RSS on this platform; facts: {facts:?}")
            }
        }
    }
}
