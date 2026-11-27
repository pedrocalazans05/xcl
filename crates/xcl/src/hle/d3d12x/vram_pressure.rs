//! Post-OOM heap creation backoff and host RAM/commit pressure hysteresis (`RamBand`).
//!
//! On 16 GiB RAM / 8 GiB VRAM systems (e.g. RX 580), D3D12 `CreateHeap` failures with
//! `E_OUTOFMEMORY` (`0x8007000E`) tend to arrive in tight bursts as texture streaming retries
//! placements. `OomBackoff` short-circuits repeated `CreateHeap` calls for 500 ms after an
//! `E_OUTOFMEMORY` failure so the driver is not thrashed, while counting avoided attempts.
//!
//! `RamBandTracker` tracks physical RAM load and virtual commit headroom with hysteresis
//! (`Normal` / `Elevated` / `Critical`) so empty-pool reclamation and speculative pool
//! throttling react smoothly without oscillating on every sample.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use windows::Win32::Graphics::Direct3D12::{ID3D12Device, ID3D12Heap, D3D12_HEAP_DESC};

use crate::util::E_OUTOFMEMORY;

/// Post-OOM backoff window after a `CreateHeap` `E_OUTOFMEMORY` failure.
pub(super) const OOM_BACKOFF_WINDOW: Duration = Duration::from_millis(500);

/// Minimum interval between live `GlobalMemoryStatusEx` queries in `current_ram_band()`.
const RAM_SAMPLE_INTERVAL: Duration = Duration::from_millis(250);

// Hysteresis thresholds tuned for 16 GiB RAM + 8 GiB discrete GPU hosts.
const ELEVATED_ENTER_LOAD_PCT: u32 = 82;
const ELEVATED_EXIT_LOAD_PCT: u32 = 75;
const ELEVATED_ENTER_COMMIT_MB: u64 = 3072;
const ELEVATED_EXIT_COMMIT_MB: u64 = 4096;

const CRITICAL_ENTER_LOAD_PCT: u32 = 92;
const CRITICAL_EXIT_LOAD_PCT: u32 = 86;
const CRITICAL_ENTER_COMMIT_MB: u64 = 1024;
const CRITICAL_EXIT_COMMIT_MB: u64 = 2048;

/// Deterministic post-OOM backoff state machine for D3D12 `CreateHeap`.
pub(super) struct OomBackoff {
    window: Duration,
    until: Option<Instant>,
    avoided: u64,
}

impl OomBackoff {
    pub(super) const fn new(window: Duration) -> Self {
        Self { window, until: None, avoided: 0 }
    }

    /// Returns `true` if `now` is within an active backoff window, incrementing `avoided`.
    pub(super) fn should_skip(&mut self, now: Instant) -> bool {
        if self.is_active(now) {
            self.avoided += 1;
            true
        } else {
            self.until = None;
            false
        }
    }

    /// Activate (or extend) the backoff window starting at `now`.
    pub(super) fn record_oom(&mut self, now: Instant) {
        self.until = Some(now + self.window);
    }

    /// Clear any active backoff window (e.g. after a successful allocation or pool reclaim).
    pub(super) fn clear(&mut self) {
        self.until = None;
    }

    /// Number of `CreateHeap` attempts short-circuited during backoff windows.
    pub(super) fn avoided(&self) -> u64 {
        self.avoided
    }

    /// Whether the backoff window is active at `now` without mutating counters.
    pub(super) fn is_active(&self, now: Instant) -> bool {
        self.until.is_some_and(|u| now < u)
    }

    /// Execute `create` unless currently in an OOM backoff window.
    ///
    /// On `Err(E_OUTOFMEMORY)`, activates the backoff window for `self.window`.
    /// On `Ok(_)`, clears any active backoff window.
    /// Non-OOM errors are returned without activating backoff.
    #[cfg(test)]
    pub(super) fn try_create_heap<T, F>(&mut self, now: Instant, create: F) -> Result<T, i32>
    where
        F: FnOnce() -> Result<T, i32>,
    {
        if self.should_skip(now) {
            return Err(E_OUTOFMEMORY);
        }
        match create() {
            Ok(v) => {
                self.clear();
                Ok(v)
            }
            Err(hr) => {
                if hr == E_OUTOFMEMORY {
                    self.record_oom(now);
                }
                Err(hr)
            }
        }
    }
}

static HEAP_OOM_BACKOFF: Mutex<OomBackoff> = Mutex::new(OomBackoff::new(OOM_BACKOFF_WINDOW));

/// Clear the process-wide `CreateHeap` OOM backoff window (e.g. after reclaiming VRAM pools).
pub(super) fn clear_heap_oom_backoff() {
    HEAP_OOM_BACKOFF.lock().unwrap().clear();
}

/// Total `CreateHeap` calls avoided due to active post-OOM backoff.
#[allow(dead_code)]
pub(super) fn heap_oom_avoided() -> u64 {
    HEAP_OOM_BACKOFF.lock().unwrap().avoided()
}

/// Call `dev.CreateHeap(&desc, &mut heap)` guarded by the 500 ms post-OOM backoff window.
///
/// The mutex is never held across `dev.CreateHeap`. During an active backoff window this
/// immediately returns `Err(E_OUTOFMEMORY)` without invoking the driver.
pub(super) unsafe fn create_heap_with_backoff(
    dev: &ID3D12Device,
    desc: &D3D12_HEAP_DESC,
    heap: &mut Option<ID3D12Heap>,
) -> windows::core::Result<()> {
    let now = Instant::now();
    {
        let mut g = HEAP_OOM_BACKOFF.lock().unwrap();
        if g.should_skip(now) {
            return Err(windows::core::Error::from_hresult(windows::core::HRESULT(E_OUTOFMEMORY)));
        }
    }
    let res = dev.CreateHeap(desc, heap);
    match &res {
        Ok(()) => {
            HEAP_OOM_BACKOFF.lock().unwrap().clear();
        }
        Err(e) if e.code().0 == E_OUTOFMEMORY => {
            let avoided = {
                let mut g = HEAP_OOM_BACKOFF.lock().unwrap();
                g.record_oom(Instant::now());
                g.avoided()
            };
            crate::xlog_warn!(
                "D3D12.X: CreateHeap E_OUTOFMEMORY ({} MiB); backing off {} ms (avoided={avoided})",
                desc.SizeInBytes >> 20,
                OOM_BACKOFF_WINDOW.as_millis(),
            );
        }
        Err(_) => {}
    }
    res
}

/// Host RAM and virtual commit pressure band with hysteresis.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum RamBand {
    Normal,
    Elevated,
    Critical,
}

/// Point-in-time host memory metrics used to evaluate `RamBand`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct MemorySnapshot {
    /// Physical RAM load percentage (`0..=100`, from `MEMORYSTATUSEX::dwMemoryLoad`).
    pub memory_load_pct: u32,
    /// Available virtual commit headroom in MiB (`MEMORYSTATUSEX::ullAvailPageFile >> 20`).
    pub avail_commit_mb: u64,
}

impl MemorySnapshot {
    fn enters_critical(self) -> bool {
        self.memory_load_pct >= CRITICAL_ENTER_LOAD_PCT
            || self.avail_commit_mb <= CRITICAL_ENTER_COMMIT_MB
    }

    fn exits_critical(self) -> bool {
        self.memory_load_pct <= CRITICAL_EXIT_LOAD_PCT
            && self.avail_commit_mb >= CRITICAL_EXIT_COMMIT_MB
    }

    fn enters_elevated(self) -> bool {
        self.memory_load_pct >= ELEVATED_ENTER_LOAD_PCT
            || self.avail_commit_mb <= ELEVATED_ENTER_COMMIT_MB
    }

    fn exits_elevated(self) -> bool {
        self.memory_load_pct <= ELEVATED_EXIT_LOAD_PCT
            && self.avail_commit_mb >= ELEVATED_EXIT_COMMIT_MB
    }
}

/// Hysteresis tracker for `RamBand`.
pub(super) struct RamBandTracker {
    band: RamBand,
    last_sample: Option<Instant>,
}

impl RamBandTracker {
    pub(super) const fn new() -> Self {
        Self { band: RamBand::Normal, last_sample: None }
    }

    pub(super) fn band(&self) -> RamBand {
        self.band
    }

    /// Update the current band from `snap` using hysteresis thresholds and return the new band.
    pub(super) fn update(&mut self, snap: MemorySnapshot) -> RamBand {
        self.band = match self.band {
            RamBand::Normal => {
                if snap.enters_critical() {
                    RamBand::Critical
                } else if snap.enters_elevated() {
                    RamBand::Elevated
                } else {
                    RamBand::Normal
                }
            }
            RamBand::Elevated => {
                if snap.enters_critical() {
                    RamBand::Critical
                } else if snap.exits_elevated() {
                    RamBand::Normal
                } else {
                    RamBand::Elevated
                }
            }
            RamBand::Critical => {
                if !snap.exits_critical() {
                    RamBand::Critical
                } else if snap.exits_elevated() {
                    RamBand::Normal
                } else {
                    RamBand::Elevated
                }
            }
        };
        self.band
    }

    /// Rate-limited update using a caller-supplied sampler so tests can inject deterministic clocks.
    pub(super) fn sample_if_due<F>(&mut self, now: Instant, sample: F) -> RamBand
    where
        F: FnOnce() -> Option<MemorySnapshot>,
    {
        if self.last_sample.is_some_and(|t| now.duration_since(t) < RAM_SAMPLE_INTERVAL) {
            return self.band;
        }
        self.last_sample = Some(now);
        if let Some(snap) = sample() {
            self.update(snap);
        }
        self.band
    }
}

/// Query host RAM load and available commit headroom via `GlobalMemoryStatusEx`.
pub(super) fn sample_host_memory() -> Option<MemorySnapshot> {
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    let mut m: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
    m.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
    if unsafe { GlobalMemoryStatusEx(&mut m) } == 0 {
        return None;
    }
    Some(MemorySnapshot {
        memory_load_pct: m.dwMemoryLoad,
        avail_commit_mb: m.ullAvailPageFile >> 20,
    })
}

static RAM_BAND: Mutex<RamBandTracker> = Mutex::new(RamBandTracker::new());

/// Current host `RamBand`, refreshed at most once every `RAM_SAMPLE_INTERVAL`.
pub(super) fn current_ram_band(now: Instant) -> RamBand {
    let mut g = RAM_BAND.lock().unwrap();
    let prev = g.band();
    let next = g.sample_if_due(now, sample_host_memory);
    if next != prev {
        crate::xlog_info!("D3D12.X: host RAM pressure band {prev:?} -> {next:?}");
    }
    next
}

/// Effective warm spare pool count under `band`: trims spare pools to 0 under `Elevated`/`Critical`.
pub(super) const fn effective_keep_spare_pools(base_spare: usize, band: RamBand) -> usize {
    match band {
        RamBand::Normal => base_spare,
        RamBand::Elevated | RamBand::Critical => 0,
    }
}

/// Periodic empty-pool reclaim interval under `band`.
pub(super) const fn reclaim_interval(band: RamBand) -> Duration {
    match band {
        RamBand::Normal => Duration::from_secs(1),
        RamBand::Elevated => Duration::from_millis(500),
        RamBand::Critical => Duration::from_millis(250),
    }
}

/// Whether speculative/eager pool growth (e.g. advertising unallocated spare pools for retries)
/// is permitted under `band`.
pub(super) const fn allow_speculative_pool_growth(band: RamBand) -> bool {
    matches!(band, RamBand::Normal)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oom_backoff_skips_within_500ms_counts_avoided_and_retries_after() {
        let t0 = Instant::now();
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        let mut bo = OomBackoff::new(OOM_BACKOFF_WINDOW);
        let mut calls = 0u32;

        // First call fails with E_OUTOFMEMORY -> activates 500 ms backoff.
        let r1: Result<u32, i32> = bo.try_create_heap(at(0), || {
            calls += 1;
            Err(E_OUTOFMEMORY)
        });
        assert_eq!(r1, Err(E_OUTOFMEMORY));
        assert_eq!(calls, 1);
        assert_eq!(bo.avoided(), 0);
        assert!(bo.is_active(at(0)));

        // Calls within 500 ms short-circuit without invoking the closure and increment avoided.
        for ms in [1, 100, 499] {
            let r: Result<u32, i32> = bo.try_create_heap(at(ms), || {
                calls += 1;
                Ok(42)
            });
            assert_eq!(r, Err(E_OUTOFMEMORY), "must short-circuit at +{ms}ms");
        }
        assert_eq!(calls, 1, "driver CreateHeap must not be called during backoff");
        assert_eq!(bo.avoided(), 3);

        // At exactly 500 ms, backoff expires and the call retries.
        let r_retry = bo.try_create_heap(at(500), || {
            calls += 1;
            Ok(99)
        });
        assert_eq!(r_retry, Ok(99));
        assert_eq!(calls, 2);
        assert_eq!(bo.avoided(), 3);
        assert!(!bo.is_active(at(500)));
    }

    #[test]
    fn oom_backoff_ignores_non_oom_errors_and_clear_resets_window() {
        let t0 = Instant::now();
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        let mut bo = OomBackoff::new(OOM_BACKOFF_WINDOW);
        let e_invalidarg = 0x80070057_u32 as i32;

        // Non-OOM error must not activate backoff.
        let r: Result<u32, i32> = bo.try_create_heap(at(0), || Err(e_invalidarg));
        assert_eq!(r, Err(e_invalidarg));
        assert!(!bo.is_active(at(0)));

        // Activate backoff with OOM, then clear() allows immediate retry.
        let _: Result<u32, i32> = bo.try_create_heap(at(10), || Err(E_OUTOFMEMORY));
        assert!(bo.is_active(at(100)));
        bo.clear();
        assert!(!bo.is_active(at(100)));
        let r2 = bo.try_create_heap(at(100), || Ok(7));
        assert_eq!(r2, Ok(7));
    }

    #[test]
    fn ram_band_hysteresis_prevents_oscillation_on_load_and_commit() {
        let mut tracker = RamBandTracker::new();
        assert_eq!(tracker.band(), RamBand::Normal);

        // Below Elevated enter threshold -> stays Normal.
        assert_eq!(
            tracker.update(MemorySnapshot { memory_load_pct: 81, avail_commit_mb: 5000 }),
            RamBand::Normal
        );

        // Crosses Elevated enter (82%) -> Elevated.
        assert_eq!(
            tracker.update(MemorySnapshot { memory_load_pct: 82, avail_commit_mb: 5000 }),
            RamBand::Elevated
        );

        // Drops to 78% (between 75% exit and 82% enter) -> stays Elevated (hysteresis).
        assert_eq!(
            tracker.update(MemorySnapshot { memory_load_pct: 78, avail_commit_mb: 5000 }),
            RamBand::Elevated
        );

        // Drops to 75% load, but commit is still 3500 MiB (< 4096 MiB exit) -> stays Elevated.
        assert_eq!(
            tracker.update(MemorySnapshot { memory_load_pct: 75, avail_commit_mb: 3500 }),
            RamBand::Elevated
        );

        // Both load <= 75% and commit >= 4096 MiB -> returns to Normal.
        assert_eq!(
            tracker.update(MemorySnapshot { memory_load_pct: 75, avail_commit_mb: 4096 }),
            RamBand::Normal
        );

        // Low commit headroom (<= 1024 MiB) jumps straight to Critical even at moderate RAM load.
        assert_eq!(
            tracker.update(MemorySnapshot { memory_load_pct: 60, avail_commit_mb: 1024 }),
            RamBand::Critical
        );

        // Commit recovers to 1500 MiB (< 2048 MiB exit) -> stays Critical.
        assert_eq!(
            tracker.update(MemorySnapshot { memory_load_pct: 60, avail_commit_mb: 1500 }),
            RamBand::Critical
        );

        // Commit recovers to 2500 MiB (>= 2048 MiB exit, < 4096 MiB Elevated exit) -> steps down to Elevated.
        assert_eq!(
            tracker.update(MemorySnapshot { memory_load_pct: 60, avail_commit_mb: 2500 }),
            RamBand::Elevated
        );

        // Load spikes to 92% -> Critical.
        assert_eq!(
            tracker.update(MemorySnapshot { memory_load_pct: 92, avail_commit_mb: 6000 }),
            RamBand::Critical
        );

        // Load drops to 88% (> 86% exit) -> stays Critical.
        assert_eq!(
            tracker.update(MemorySnapshot { memory_load_pct: 88, avail_commit_mb: 6000 }),
            RamBand::Critical
        );

        // Load drops to 70% and commit >= 4096 MiB -> drops straight to Normal.
        assert_eq!(
            tracker.update(MemorySnapshot { memory_load_pct: 70, avail_commit_mb: 6000 }),
            RamBand::Normal
        );
    }

    #[test]
    fn ram_band_policies_trim_spares_and_throttle_speculative_growth() {
        assert_eq!(effective_keep_spare_pools(2, RamBand::Normal), 2);
        assert_eq!(effective_keep_spare_pools(2, RamBand::Elevated), 0);
        assert_eq!(effective_keep_spare_pools(2, RamBand::Critical), 0);

        assert_eq!(reclaim_interval(RamBand::Normal), Duration::from_secs(1));
        assert_eq!(reclaim_interval(RamBand::Elevated), Duration::from_millis(500));
        assert_eq!(reclaim_interval(RamBand::Critical), Duration::from_millis(250));

        assert!(allow_speculative_pool_growth(RamBand::Normal));
        assert!(!allow_speculative_pool_growth(RamBand::Elevated));
        assert!(!allow_speculative_pool_growth(RamBand::Critical));
    }
}
