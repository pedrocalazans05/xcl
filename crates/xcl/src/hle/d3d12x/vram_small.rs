//! 4 KiB sub-allocated default heaps for small (< 64 KiB) sampled textures.
//!
//! A console title packs thousands of small mip-tails, icons, decals and LUTs at 256 B – 4 KiB
//! offsets inside its texture pools. Backing each one with a 64 KiB reserved resource in
//! `vram::Pages` wastes up to 16× video memory (one full 64 KiB `extra_pages` tile per small
//! texture) and submits a `CreateReservedResource` + `GetResourceTiling` + `UpdateTileMappings`
//! round trip for every placement.
//!
//! Direct3D 12 supports 4 KiB placement (`D3D12_SMALL_RESOURCE_PLACEMENT_ALIGNMENT`) for
//! single-sample non-RT/DS textures whose allocation info fits under 64 KiB. Placing those in
//! shared 16 MiB default heaps packs up to 4 096 textures into one heap with zero tile mappings,
//! shares the exact `(heap, offset)` across repeated placements at the same `(va, size)` so
//! same-address aliases see each other's uploads, and recycles released blocks by size class
//! after `IDLE` + `COOL`. `XCL_VRAM_SMALL=0` disables this path.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::ffi::c_void;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use windows::core::Interface;
use windows::Win32::Graphics::Direct3D12::*;

use crate::util::HRESULT;

/// Allocation unit: 4 KiB (`D3D12_SMALL_RESOURCE_PLACEMENT_ALIGNMENT`).
const UNIT: usize = 0x1000;
/// Largest small-resource size handled here: 60 KiB (`15 * 4 KiB < 64 KiB`).
const MAX_UNITS: u32 = 15;
/// Units per shared default heap: 16 MiB (`4096 * 4 KiB`).
const POOL_UNITS: u32 = 4096;
/// Keep an unreferenced `(va, units)` slot this long so immediate re-placements reuse it.
const IDLE: Duration = Duration::from_secs(2);
/// Wait before reusing a freed block for a different address (in-flight GPU work).
const COOL: Duration = Duration::from_secs(2);

/// Key identifying a shared small-texture backing slot: `(title_va, size_in_4k_units)`.
type SlotKey = (usize, u32);

/// Block location inside a device's small pools: `(pool_index, offset_in_4k_units, size_in_4k_units)`.
type Block = (u32, u32, u32);

struct Pool<H> {
    heap: H,
    /// Bump pointer in 4 KiB units (`0..=POOL_UNITS`).
    next_unit: u32,
    /// Units currently held by live/idle `slots` or `cooling` blocks (not yet in `free`).
    in_use_units: u32,
}

struct Slot {
    pool: u32,
    offset: u32,
    users: u32,
    idle_since: Option<Instant>,
    /// True once a resource has been successfully backed by this slot.
    had_resource: bool,
    /// Prior `idle_since` when an idle slot is re-acquired, preserved in case creation fails
    /// and rolls back so the block cannot bypass `IDLE` + `COOL`.
    prior_idle_since: Option<Instant>,
}

struct SmallPools<H> {
    pools: Vec<Option<Pool<H>>>,
    /// Per-size-class (`1..=MAX_UNITS`) reusable blocks `(pool, offset)`.
    free: [Vec<(u32, u32)>; (MAX_UNITS as usize) + 1],
    /// Blocks given back, reusable after `COOL`.
    cooling: VecDeque<(Block, Instant)>,
    /// Live or idle-kept `(va, units)` → backing block.
    slots: HashMap<SlotKey, Slot>,
    /// Idle slots, oldest first (`sweep_idle`).
    idle: VecDeque<(SlotKey, Instant)>,
    /// Live resource raw pointer → `(va, units)`.
    resources: HashMap<usize, SlotKey>,
    /// Total 4 KiB units currently held by `slots` (live + idle-kept).
    used_units: usize,
}

impl<H: Clone> SmallPools<H> {
    fn new() -> Self {
        SmallPools {
            pools: Vec::new(),
            free: std::array::from_fn(|_| Vec::new()),
            cooling: VecDeque::new(),
            slots: HashMap::new(),
            idle: VecDeque::new(),
            resources: HashMap::new(),
            used_units: 0,
        }
    }

    fn sweep_idle(&mut self, now: Instant) {
        while self.idle.front().is_some_and(|&(_, since)| now.duration_since(since) >= IDLE) {
            let Some((key, since)) = self.idle.pop_front() else { break };
            if self.slots.get(&key).is_some_and(|s| s.idle_since == Some(since)) {
                if let Some(s) = self.slots.remove(&key) {
                    self.used_units -= key.1 as usize;
                    self.cooling.push_back(((s.pool, s.offset, key.1), now));
                }
            }
        }
    }

    fn drain_cooling(&mut self, now: Instant) {
        while self.cooling.front().is_some_and(|&(_, at)| now.duration_since(at) >= COOL) {
            if let Some(((pool, offset, units), _)) = self.cooling.pop_front() {
                if let Some(p) = self.pools.get_mut(pool as usize).and_then(Option::as_mut) {
                    p.in_use_units -= units;
                    self.free[units as usize].push((pool, offset));
                }
            }
        }
    }

    /// Reclaim completely empty cooled heaps (`in_use_units == 0`), keeping up to `keep_spare`
    /// empty heaps warm so steady-state streaming does not thrash `CreateHeap`.
    /// Returns the reclaimed heaps so the caller can drop them outside `SMALL_POOLS.lock()`.
    fn reclaim_empty_pools(&mut self, keep_spare: usize) -> Vec<H> {
        let mut empty: Vec<usize> = self
            .pools
            .iter()
            .enumerate()
            .filter_map(|(idx, p)| p.as_ref().is_some_and(|p| p.in_use_units == 0).then_some(idx))
            .collect();
        if empty.len() <= keep_spare {
            return Vec::new();
        }
        let mut reclaimed = Vec::with_capacity(empty.len() - keep_spare);
        for idx in empty.drain(keep_spare..) {
            let target = idx as u32;
            for list in &mut self.free {
                list.retain(|&(pool, _)| pool != target);
            }
            if let Some(pool) = self.pools[idx].take() {
                reclaimed.push(pool.heap);
            }
        }
        reclaimed
    }

    fn sweep_and_reclaim(&mut self, now: Instant, keep_spare: usize) -> Vec<H> {
        self.sweep_idle(now);
        self.drain_cooling(now);
        self.reclaim_empty_pools(keep_spare)
    }

    /// Try to acquire an existing shared slot for `(va, units)` or allocate a block from existing
    /// pools / free lists without creating a new D3D12 heap.
    fn try_acquire(&mut self, key: SlotKey, now: Instant) -> Option<(H, u32)> {
        self.sweep_idle(now);
        if let Some(s) = self.slots.get_mut(&key) {
            s.users += 1;
            if let Some(prev) = s.idle_since.take() {
                s.prior_idle_since = Some(prev);
            }
            return Some((self.pools[s.pool as usize].as_ref().expect("live small pool").heap.clone(), s.offset));
        }
        let units = key.1;
        debug_assert!((1..=MAX_UNITS).contains(&units));
        self.drain_cooling(now);
        let (pool, offset) = if let Some(b) = self.free[units as usize].pop() {
            let p = self.pools[b.0 as usize].as_mut().expect("live small pool for free block");
            p.in_use_units += units;
            b
        } else if let Some((idx, Some(p))) = self.pools.iter_mut().enumerate().rev().find(|(_, p)| p.as_ref().is_some_and(|p| p.next_unit + units <= POOL_UNITS)) {
            let off = p.next_unit;
            p.next_unit += units;
            p.in_use_units += units;
            (idx as u32, off)
        } else {
            return None;
        };
        let heap = self.pools[pool as usize].as_ref().expect("live small pool").heap.clone();
        self.slots.insert(key, Slot { pool, offset, users: 1, idle_since: None, had_resource: false, prior_idle_since: None });
        self.used_units += units as usize;
        Some((heap, offset))
    }

    /// Register a newly created pool heap and acquire `(va, units)`.
    /// Re-checks `try_acquire` first under the lock so two threads racing to create a new 16 MiB
    /// pool do not push a redundant empty heap (the extra `heap` drops cleanly if another thread
    /// already added room or created the same slot).
    fn add_pool_and_acquire(&mut self, heap: H, key: SlotKey, now: Instant) -> (H, u32) {
        if let Some(got) = self.try_acquire(key, now) {
            return got;
        }
        let pool_idx = if let Some(idx) = self.pools.iter().position(Option::is_none) {
            self.pools[idx] = Some(Pool { heap, next_unit: 0, in_use_units: 0 });
            idx as u32
        } else {
            let idx = self.pools.len() as u32;
            self.pools.push(Some(Pool { heap, next_unit: 0, in_use_units: 0 }));
            idx
        };
        if let Some(got) = self.try_acquire(key, now) {
            return got;
        }
        // `pool_idx` has `next_unit == 0` and `units <= MAX_UNITS < POOL_UNITS`, so `try_acquire`
        // above always succeeds.
        unreachable!("fresh pool {pool_idx} must fit {} units", key.1);
    }

    fn note_resource(&mut self, res: usize, key: SlotKey) {
        if let Some(s) = self.slots.get_mut(&key) {
            s.had_resource = true;
            s.prior_idle_since = None;
        }
        self.resources.insert(res, key);
    }

    /// Release one user of `key`. When `rollback` is true (creation failed before the GPU ever saw
    /// the resource), a freshly allocated block returns directly to `free`; a slot that previously
    /// backed a resource preserves its idle/cooling window so another key cannot reuse it early.
    fn release_key(&mut self, key: SlotKey, now: Instant, rollback: bool) {
        let Some(s) = self.slots.get_mut(&key) else { return };
        s.users = s.users.saturating_sub(1);
        if s.users == 0 {
            if rollback && !s.had_resource && s.prior_idle_since.is_none() {
                if let Some(s) = self.slots.remove(&key) {
                    self.used_units -= key.1 as usize;
                    if let Some(p) = self.pools.get_mut(s.pool as usize).and_then(Option::as_mut) {
                        p.in_use_units -= key.1;
                        self.free[key.1 as usize].push((s.pool, s.offset));
                    }
                }
            } else if rollback {
                if let Some(prev) = s.prior_idle_since.take() {
                    if now.duration_since(prev) >= IDLE {
                        if let Some(s) = self.slots.remove(&key) {
                            self.used_units -= key.1 as usize;
                            self.cooling.push_back(((s.pool, s.offset, key.1), now));
                        }
                    } else {
                        s.idle_since = Some(prev);
                        if !self.idle.iter().any(|&(k, t)| k == key && t == prev) {
                            let pos = self.idle.partition_point(|&(_, t)| t <= prev);
                            self.idle.insert(pos, (key, prev));
                        }
                    }
                } else {
                    s.idle_since = Some(now);
                    self.idle.push_back((key, now));
                }
            } else {
                s.prior_idle_since = None;
                s.idle_since = Some(now);
                self.idle.push_back((key, now));
            }
        }
    }

    fn forget(&mut self, res: usize, now: Instant) -> bool {
        let Some(key) = self.resources.remove(&res) else {
            return false;
        };
        self.release_key(key, now, false);
        true
    }

    fn bytes(&self) -> (usize, usize) {
        let live_pools = self.pools.iter().filter(|p| p.is_some()).count();
        (self.used_units * UNIT, live_pools * POOL_UNITS as usize * UNIT)
    }

    #[cfg(test)]
    fn flush_idle(&mut self, now: Instant) {
        while let Some((key, since)) = self.idle.pop_front() {
            if self.slots.get(&key).is_some_and(|s| s.idle_since == Some(since)) {
                if let Some(s) = self.slots.remove(&key) {
                    self.used_units -= key.1 as usize;
                    self.cooling.push_back(((s.pool, s.offset, key.1), now));
                }
            }
        }
    }
}

// D3D12 heaps are free-threaded.
unsafe impl Send for SmallPools<ID3D12Heap> {}

static SMALL_POOLS: Mutex<BTreeMap<usize, SmallPools<ID3D12Heap>>> = Mutex::new(BTreeMap::new());

fn enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("XCL_VRAM_SMALL").map_or(true, |v| v.trim() != "0"))
}

/// Whether `desc` and `console_bytes` are eligible for 4 KiB small-resource sub-allocation.
fn candidate(desc: &D3D12_RESOURCE_DESC, console_bytes: Option<u64>) -> bool {
    const PAGE: u64 = 0x1_0000;
    desc.Layout == D3D12_TEXTURE_LAYOUT_UNKNOWN
        && desc.SampleDesc.Count <= 1
        && desc.Flags.0 & 0x7 == 0
        && matches!(desc.Dimension, D3D12_RESOURCE_DIMENSION_TEXTURE2D | D3D12_RESOURCE_DIMENSION_TEXTURE3D)
        && console_bytes.is_some_and(|b| b > 0 && b < PAGE)
}

/// Place a sub-64 KiB sampled texture at 4 KiB granularity in a shared default heap.
/// Returns `None` if the texture is not a small resource (the caller falls back to `vram::Pages`
/// or a committed resource).
#[allow(clippy::too_many_arguments)]
pub(super) unsafe fn place(
    dev: &ID3D12Device,
    this: *mut c_void,
    va: usize,
    desc: &D3D12_RESOURCE_DESC,
    state: u32,
    clear: *const c_void,
    console_bytes: Option<u64>,
    riid: *const windows_sys::core::GUID,
    ppv: *mut *mut c_void,
) -> Option<HRESULT> {
    if !enabled() || !candidate(desc, console_bytes) {
        return None;
    }
    let small_desc = D3D12_RESOURCE_DESC { Alignment: D3D12_SMALL_RESOURCE_PLACEMENT_ALIGNMENT as u64, ..*desc };
    let info = dev.GetResourceAllocationInfo(0, &[small_desc]);
    if info.SizeInBytes == u64::MAX || info.SizeInBytes == 0 || info.Alignment != UNIT as u64 || info.SizeInBytes >= 0x1_0000 {
        return None;
    }
    let units = (info.SizeInBytes as usize).div_ceil(UNIT) as u32;
    if !(1..=MAX_UNITS).contains(&units) {
        return None;
    }
    let dev_key = dev.as_raw() as usize;
    let key = (va, units);
    let now = Instant::now();

    let (got, dropped) = {
        let mut g = SMALL_POOLS.lock().unwrap();
        let pools = g.entry(dev_key).or_insert_with(SmallPools::new);
        let got = pools.try_acquire(key, now);
        let dropped = pools.reclaim_empty_pools(super::vram::keep_spare_pools());
        (got, dropped)
    };
    log_and_drop_reclaimed(dropped);
    let (heap, offset_units) = got.or_else(|| {
        // Create a new 16 MiB default heap outside `SMALL_POOLS.lock()`.
        let heap_desc = D3D12_HEAP_DESC {
            SizeInBytes: POOL_UNITS as u64 * UNIT as u64,
            Properties: D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_DEFAULT, ..Default::default() },
            // 64 KiB heap alignment is valid for 4 KiB-aligned small resources at any 4 KiB offset.
            Alignment: D3D12_DEFAULT_RESOURCE_PLACEMENT_ALIGNMENT as u64,
            Flags: D3D12_HEAP_FLAG_ALLOW_ONLY_NON_RT_DS_TEXTURES,
        };
        let mut new_heap: Option<ID3D12Heap> = None;
        if let Err(e) = super::title_textures::own_work(|| dev.CreateHeap(&heap_desc, &mut new_heap)) {
            crate::xlog_warn!("D3D12.X: small texture pool CreateHeap: {e}");
            return None;
        }
        let new_heap = new_heap?;
        let mut g = SMALL_POOLS.lock().unwrap();
        let pools = g.entry(dev_key).or_insert_with(SmallPools::new);
        Some(pools.add_pool_and_acquire(new_heap, key, Instant::now()))
    })?;

    let hr = super::real::dev_create_placed_resource(
        this,
        heap.as_raw() as usize,
        offset_units as usize * UNIT,
        &small_desc as *const _ as usize,
        state as usize,
        clear as usize,
        riid,
        ppv,
    );
    if hr < 0 || (*ppv).is_null() {
        let dropped = {
            let mut g = SMALL_POOLS.lock().unwrap();
            if let Some(pools) = g.get_mut(&dev_key) {
                pools.release_key(key, Instant::now(), true);
                pools.reclaim_empty_pools(super::vram::keep_spare_pools())
            } else {
                Vec::new()
            }
        };
        log_and_drop_reclaimed(dropped);
        return None;
    }
    let raw = *ppv as usize;
    SMALL_POOLS.lock().unwrap().entry(dev_key).or_insert_with(SmallPools::new).note_resource(raw, key);
    Some(hr)
}

fn log_and_drop_reclaimed(dropped: Vec<ID3D12Heap>) {
    if !dropped.is_empty() {
        let mb = (dropped.len() * POOL_UNITS as usize * UNIT) >> 20;
        crate::xlog_info!(
            "D3D12.X: reclaimed {} empty 16 MiB small texture pool heap(s) ({mb} MiB returned to GPU)",
            dropped.len(),
        );
        drop(dropped);
    }
}

/// Sweep expired idle/cooling small-pool blocks for `dev_key` and release any empty heaps
/// above `XCL_VRAM_KEEP_SPARE_POOLS`, dropping their `ID3D12Heap`s outside `SMALL_POOLS.lock()`.
pub(super) fn reclaim(dev_key: usize, now: Instant) {
    let dropped = {
        let mut g = SMALL_POOLS.lock().unwrap();
        let Some(pools) = g.get_mut(&dev_key) else { return };
        pools.sweep_and_reclaim(now, super::vram::keep_spare_pools())
    };
    log_and_drop_reclaimed(dropped);
}

/// Release a small-pool texture on COM release. Returns `true` if `resource` belonged to a small
/// pool (so `vram::forget` can skip locking `PAGES`).
pub(super) fn forget(resource: usize) -> bool {
    let (found, dropped) = {
        let mut g = SMALL_POOLS.lock().unwrap();
        let now = Instant::now();
        let mut found = false;
        let mut dropped = Vec::new();
        for pools in g.values_mut() {
            if pools.forget(resource, now) {
                dropped = pools.sweep_and_reclaim(now, super::vram::keep_spare_pools());
                found = true;
                break;
            }
        }
        (found, dropped)
    };
    log_and_drop_reclaimed(dropped);
    found
}

/// `(used_bytes, pool_bytes)` across all devices.
pub(super) fn bytes() -> (usize, usize) {
    let g = SMALL_POOLS.lock().unwrap();
    g.values().fold((0, 0), |(used, pools), p| {
        let (u, t) = p.bytes();
        (used + u, pools + t)
    })
}

#[cfg(test)]
pub(super) fn bytes_for(device: usize) -> (usize, usize) {
    let mut g = SMALL_POOLS.lock().unwrap();
    let Some(p) = g.get_mut(&device) else { return (0, 0) };
    p.sweep_idle(Instant::now());
    p.bytes()
}

#[cfg(test)]
pub(super) fn flush_idle_for(device: usize) {
    let mut g = SMALL_POOLS.lock().unwrap();
    if let Some(p) = g.get_mut(&device) {
        p.flush_idle(Instant::now());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_pool_packs_shares_and_recycles_by_size_class() {
        let t0 = Instant::now();
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        let mut p: SmallPools<u32> = SmallPools::new();
        let alloc = |p: &mut SmallPools<u32>, va: usize, units: u32, res: usize, now: Instant| -> (u32, u32) {
            let got = p.try_acquire((va, units), now).unwrap_or_else(|| {
                let id = p.pools.len() as u32;
                p.add_pool_and_acquire(id, (va, units), now)
            });
            p.note_resource(res, (va, units));
            got
        };

        // Sixteen 4 KiB textures pack into 64 KiB (not 16 * 64 KiB = 1 MiB).
        for i in 0..16usize {
            let (h, off) = alloc(&mut p, 0x10_0000 + i * UNIT, 1, 100 + i, at(0));
            assert_eq!((h, off), (0, i as u32));
        }
        assert_eq!(p.bytes(), (16 * UNIT, POOL_UNITS as usize * UNIT));

        // Re-placing the exact same (va, units) shares the exact same (heap, offset) with zero growth.
        let (h_dup, off_dup) = alloc(&mut p, 0x10_4000, 1, 200, at(10));
        assert_eq!((h_dup, off_dup), (0, 4));
        assert_eq!(p.bytes().0, 16 * UNIT);

        // Releasing one of the two aliases keeps the slot alive; releasing both makes it idle.
        assert!(p.forget(200, at(20)));
        assert_eq!(p.slots[&(0x10_4000, 1)].users, 1);
        assert!(p.forget(104, at(30)));
        assert_eq!(p.slots[&(0x10_4000, 1)].users, 0);

        // Re-acquiring within IDLE reuses the exact same slot.
        let (h_re, off_re) = alloc(&mut p, 0x10_4000, 1, 201, at(1000));
        assert_eq!((h_re, off_re), (0, 4));
        assert!(p.forget(201, at(1100)));

        // After IDLE (2 s) + COOL (2 s), the 4 KiB block is recycled for a new address.
        let (h_wait, off_wait) = alloc(&mut p, 0x20_0000, 1, 202, at(3200));
        assert_eq!((h_wait, off_wait), (0, 16), "still cooling at +2.1s, must not reuse yet");
        let (h_rec, off_rec) = alloc(&mut p, 0x20_1000, 1, 203, at(5300));
        assert_eq!((h_rec, off_rec), (0, 4), "block (0, 4) recycled after IDLE + COOL");

        // Rollback on creation failure immediately returns the block to the free list.
        let (h_fail, off_fail) = p.try_acquire((0x30_0000, 4), at(5400)).unwrap();
        assert_eq!((h_fail, off_fail), (0, 17));
        p.release_key((0x30_0000, 4), at(5400), true);
        let (h_next, off_next) = alloc(&mut p, 0x30_4000, 4, 204, at(5400));
        assert_eq!((h_next, off_next), (0, 17));

        // Concurrent CreateHeap race: if room already exists in pool 0, `add_pool_and_acquire`
        // must reuse existing room instead of pushing a redundant empty 16 MiB pool.
        let pools_before = p.pools.len();
        let (h_raced, _) = p.add_pool_and_acquire(999, (0x40_0000, 1), at(5500));
        assert_eq!(h_raced, 0);
        assert_eq!(p.pools.len(), pools_before, "must not push redundant pool when room already exists");

        // `flush_idle` clears unreferenced idle slots while keeping live (`users > 0`) slots intact.
        let used_before_flush = p.bytes().0;
        let _ = alloc(&mut p, 0x50_0000, 2, 205, at(5600));
        assert!(p.forget(205, at(5610)));
        assert_eq!(p.bytes().0, used_before_flush + 2 * UNIT);
        p.flush_idle(at(5620));
        assert_eq!(p.bytes().0, used_before_flush, "flush_idle must reclaim unreferenced slots immediately");
    }

    #[test]
    fn empty_pools_are_reclaimed_when_keep_spare_is_zero() {
        let t0 = Instant::now();
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        let mut p: SmallPools<u32> = SmallPools::new();
        let mut next_heap = 0u32;
        let mut alloc = |p: &mut SmallPools<u32>, va: usize, units: u32, res: usize, now: Instant| -> (u32, u32) {
            let got = p.try_acquire((va, units), now).unwrap_or_else(|| {
                let id = next_heap;
                next_heap += 1;
                p.add_pool_and_acquire(id, (va, units), now)
            });
            p.note_resource(res, (va, units));
            got
        };

        // Allocate in a single pool and then release it.
        let (h0, _) = alloc(&mut p, 0x10_0000, 4, 1, at(0));
        assert_eq!(h0, 0);
        assert_eq!(p.bytes(), (4 * UNIT, POOL_UNITS as usize * UNIT));
        assert!(p.forget(1, at(10)));

        // Still idle at +1s: must not reclaim.
        assert!(p.sweep_and_reclaim(at(1000), 0).is_empty());
        assert_eq!(p.bytes().1, POOL_UNITS as usize * UNIT);

        // Swept to cooling at +2.1s, but COOL (2s) has not elapsed: must not reclaim yet.
        assert!(p.sweep_and_reclaim(at(2100), 0).is_empty());
        assert_eq!(p.bytes().1, POOL_UNITS as usize * UNIT);

        // After IDLE + COOL: with keep_spare=1 the warm spare stays; with keep_spare=0 it drops to 0.
        assert!(p.sweep_and_reclaim(at(4200), 1).is_empty());
        assert_eq!(p.bytes(), (0, POOL_UNITS as usize * UNIT));

        let reclaimed = p.sweep_and_reclaim(at(4200), 0);
        assert_eq!(reclaimed, vec![0]);
        assert_eq!(p.bytes(), (0, 0), "emptying class with keep_spare=0 must drop retained bytes to 0");
        assert!(p.free.iter().all(Vec::is_empty), "free lists must not retain stale blocks from reclaimed pool");

        // Subsequent allocation reuses the vacated pool slot cleanly.
        let (h1, off1) = alloc(&mut p, 0x20_0000, 4, 2, at(4300));
        assert_eq!((h1, off1), (1, 0));
        assert_eq!(p.pools.len(), 1, "vacated pool slot should be reused");
        assert_eq!(p.bytes(), (4 * UNIT, POOL_UNITS as usize * UNIT));
    }

    #[test]
    fn rollback_of_resumed_idle_slot_preserves_cooldown_against_other_key() {
        let t0 = Instant::now();
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        let mut p: SmallPools<u32> = SmallPools::new();

        // Key A allocates block (0, 0) and releases it at t=10ms, entering the IDLE window.
        let key_a = (0x10_0000usize, 4u32);
        let key_b = (0x20_0000usize, 4u32);
        let (ha, offa) = p.add_pool_and_acquire(0, key_a, at(0));
        assert_eq!((ha, offa), (0, 0));
        p.note_resource(1, key_a);
        assert!(p.forget(1, at(10)));

        // At t=500ms (still within IDLE), Key A re-acquires the idle slot, then creation fails
        // and rolls back. The block must NOT return to `free` prematurely.
        let (ha2, offa2) = p.try_acquire(key_a, at(500)).unwrap();
        assert_eq!((ha2, offa2), (0, 0));
        p.release_key(key_a, at(500), true);

        // Key B tries to allocate at t=500ms: must get a fresh offset (0, 4), NOT (0, 0).
        let (hb1, offb1) = p.try_acquire(key_b, at(500)).unwrap();
        assert_eq!((hb1, offb1), (0, 4), "other key must not reuse rolled-back idle slot before IDLE+COOL");

        // At t=2100ms (IDLE elapsed since t=10ms), Key A's slot moves to cooling; still not reusable.
        let key_c = (0x30_0000usize, 4u32);
        let (hc, offc) = p.try_acquire(key_c, at(2100)).unwrap();
        assert_eq!((hc, offc), (0, 8), "block must still be cooling at +2.1s");

        // At t=4200ms (COOL elapsed after sweep at 2100ms), block (0, 0) is finally safe to reuse.
        let key_d = (0x40_0000usize, 4u32);
        let (hd, offd) = p.try_acquire(key_d, at(4200)).unwrap();
        assert_eq!((hd, offd), (0, 0), "block becomes reusable after IDLE + COOL");
    }

    #[test]
    fn candidate_filters_large_gpu_owned_and_msaa_textures() {
        let mut d = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
            Width: 64,
            Height: 64,
            DepthOrArraySize: 1,
            MipLevels: 1,
            SampleDesc: windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Layout: D3D12_TEXTURE_LAYOUT_UNKNOWN,
            Flags: D3D12_RESOURCE_FLAG_NONE,
            ..Default::default()
        };
        assert!(candidate(&d, Some(16 * 1024)));
        assert!(!candidate(&d, Some(64 * 1024)), "64 KiB stays on vram::Pages");
        assert!(!candidate(&d, None));
        assert!(!candidate(&d, Some(0)));
        d.Flags = D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET;
        assert!(!candidate(&d, Some(16 * 1024)));
        d.Flags = D3D12_RESOURCE_FLAG_NONE;
        d.SampleDesc.Count = 4;
        assert!(!candidate(&d, Some(16 * 1024)));
        d.SampleDesc.Count = 1;
        d.Dimension = D3D12_RESOURCE_DIMENSION_BUFFER;
        assert!(!candidate(&d, Some(16 * 1024)));
    }

    /// On the GPU: 512 small 4 KiB (64x64 BC1) and 16 KiB (64x64 RGBA8) sampled textures pack at
    /// 4 KiB granularity inside a single 16 MiB heap (16x and 4x smaller than 64 KiB tiles),
    /// same-address placements alias their pixels on hardware, adjacent 4 KiB textures have zero
    /// crosstalk, and a small sampled texture over an existing 64 KiB GPU target falls back to
    /// `vram::Pages` so target aliasing is preserved.
    #[test]
    fn gpu_small_textures_pack_at_4k_granularity_and_preserve_pixels() {
        let _vram_guard = super::super::vram::VRAM_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_11_0;
        use windows::Win32::Graphics::Dxgi::Common::*;
        use windows::Win32::Graphics::Dxgi::*;
        unsafe {
            let factory: IDXGIFactory6 = CreateDXGIFactory1().expect("DXGI factory");
            let sel = crate::gpu::adapter::Selector::from_env_or("auto");
            let mut dev: Option<ID3D12Device> = None;
            for (adapter, _) in crate::gpu::adapter::candidates(&factory, &sel) {
                if D3D12CreateDevice(&adapter, D3D_FEATURE_LEVEL_11_0, &mut dev).is_ok() && dev.is_some() {
                    break;
                }
            }
            let Some(dev) = dev else {
                crate::gpu::recomp::runner::skip_test("no D3D12 device");
                return;
            };
            let mut opts = D3D12_FEATURE_DATA_D3D12_OPTIONS::default();
            if dev.CheckFeatureSupport(D3D12_FEATURE_D3D12_OPTIONS, &mut opts as *mut _ as *mut c_void, std::mem::size_of_val(&opts) as u32).is_err()
                || opts.ResourceHeapTier.0 < 2
            {
                crate::gpu::recomp::runner::skip_test(&format!("resource heap tier {} < 2", opts.ResourceHeapTier.0));
                return;
            }
            super::super::real::adopt(dev.as_raw(), super::super::real::Kind::Device);
            let dev_key = dev.as_raw() as usize;
            let riid = &ID3D12Resource::IID as *const windows::core::GUID as *const windows_sys::core::GUID;

            let desc_of = |w: u64, h: u32, fmt: DXGI_FORMAT, flags: D3D12_RESOURCE_FLAGS| D3D12_RESOURCE_DESC {
                Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
                Width: w,
                Height: h,
                DepthOrArraySize: 1,
                MipLevels: 1,
                Format: fmt,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Layout: D3D12_TEXTURE_LAYOUT_UNKNOWN,
                Flags: flags,
                ..Default::default()
            };
            let place_sampled = |va: usize, d: &D3D12_RESOURCE_DESC, cbytes: u64| -> ID3D12Resource {
                let mut p = std::ptr::null_mut();
                let hr = super::super::vram::place_pages(dev.as_raw(), va, d, D3D12_RESOURCE_STATE_COMMON.0 as u32, std::ptr::null(), Some(cbytes), riid, &mut p);
                assert!(hr.is_some_and(|h| h >= 0) && !p.is_null(), "place_pages at {va:#x} failed: {hr:x?}");
                assert!(super::super::vram::is_placed_texture(p as usize));
                ID3D12Resource::from_raw(p)
            };

            // 1. 512 x 64x64 BC1 (2 KiB on console -> 4 KiB small resource on PC): must pack into
            //    2 MiB (16x smaller than 512 x 64 KiB = 32 MiB!) inside a single 16 MiB heap.
            super::super::vram::flush_idle_for(dev_key);
            let bc1 = desc_of(64, 64, DXGI_FORMAT_BC1_UNORM, D3D12_RESOURCE_FLAG_NONE);
            let bc1_info = dev.GetResourceAllocationInfo(0, &[D3D12_RESOURCE_DESC { Alignment: D3D12_SMALL_RESOURCE_PLACEMENT_ALIGNMENT as u64, ..bc1 }]);
            if bc1_info.SizeInBytes == u64::MAX || bc1_info.SizeInBytes == 0 || bc1_info.Alignment != UNIT as u64 || bc1_info.SizeInBytes >= 0x1_0000 {
                crate::gpu::recomp::runner::skip_test(&format!(
                    "adapter does not support 4 KiB small-resource placement for 64x64 BC1 (align={}, size={})",
                    bc1_info.Alignment, bc1_info.SizeInBytes
                ));
                return;
            }
            let (small0, _) = bytes_for(dev_key);
            let (pages0, _) = super::super::vram::page_bytes_for(dev_key);
            let bc1_base = 0x60_0000_0000usize;
            let mut bc1_res = Vec::with_capacity(512);
            for k in 0..512usize {
                bc1_res.push(place_sampled(bc1_base + k * 0x1000, &bc1, 2048));
            }
            let (small_after_bc1, small_pool_after_bc1) = bytes_for(dev_key);
            let bc1_small_delta = small_after_bc1 - small0;
            let bc1_total_delta = super::super::vram::page_bytes_for(dev_key).0 - pages0;
            println!("512 x 64x64 BC1: +{bc1_small_delta} B in small pools (pool={small_pool_after_bc1} B), +{bc1_total_delta} B total (vs 33554432 B as 64 KiB tiles)");
            assert_eq!(bc1_small_delta, 512 * UNIT, "each 64x64 BC1 must take 4 KiB in small pool");
            assert_eq!(bc1_total_delta, 512 * UNIT, "zero 64 KiB Pages tiles allocated for 4 KiB BC1 textures");
            drop(bc1_res);

            // 2. Hardware upload + readback: two adjacent 64x64 RGBA8 (16 KiB) textures and a
            //    same-address alias verify zero crosstalk and exact pixel sharing on the GPU.
            super::super::vram::flush_idle_for(dev_key);
            let rgba = desc_of(64, 64, DXGI_FORMAT_R8G8B8A8_UNORM, D3D12_RESOURCE_FLAG_NONE);
            let rgba_info = dev.GetResourceAllocationInfo(0, &[D3D12_RESOURCE_DESC { Alignment: D3D12_SMALL_RESOURCE_PLACEMENT_ALIGNMENT as u64, ..rgba }]);
            if rgba_info.SizeInBytes != 16384 || rgba_info.Alignment != UNIT as u64 {
                crate::gpu::recomp::runner::skip_test(&format!(
                    "step 2: adapter does not support 4 KiB 16 KiB placement for 64x64 RGBA8 (align={}, size={})",
                    rgba_info.Alignment, rgba_info.SizeInBytes
                ));
                return;
            }
            let va_a = 0x61_0000_1000usize;
            let va_b = 0x61_0000_5000usize;
            let a1 = place_sampled(va_a, &rgba, 16384);
            let (small_after_a1, _) = bytes_for(dev_key);
            let a2 = place_sampled(va_a, &rgba, 16384);
            assert_eq!(bytes_for(dev_key).0, small_after_a1, "same-va small texture must share slot");
            let b = place_sampled(va_b, &rgba, 16384);
            assert_eq!(bytes_for(dev_key).0 - small_after_a1, 16384, "64x64 RGBA8 must take 16 KiB");

            let queue: ID3D12CommandQueue = dev.CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC::default()).unwrap();
            let fill_a = vec![0x3Cu8; 64 * 64 * 4];
            let fill_b = vec![0xA5u8; 64 * 64 * 4];
            let sub = |data: Vec<u8>| crate::gpu::texture::Subresource { rows: 64, row_bytes: 64 * 4, depth: 1, data };
            let batch = vec![
                (a2.clone(), vec![sub(fill_a.clone())], vec![D3D12_RESOURCE_STATE_COMMON]),
                (b.clone(), vec![sub(fill_b.clone())], vec![D3D12_RESOURCE_STATE_COMMON]),
            ];
            let failed = crate::gpu::texture::upload_batch(&dev, &queue, &batch).unwrap();
            assert!(failed.is_empty(), "upload failed: {failed:?}");

            let readback_tex = |r: &ID3D12Resource| -> Vec<u8> {
                let rd = r.GetDesc();
                let mut fp = D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default();
                let (mut rows, mut row_bytes, mut total) = (0u32, 0u64, 0u64);
                dev.GetCopyableFootprints(&rd, 0, 1, 0, Some(&mut fp), Some(&mut rows), Some(&mut row_bytes), Some(&mut total));
                let mut rb: Option<ID3D12Resource> = None;
                dev.CreateCommittedResource(
                    &D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_READBACK, ..Default::default() },
                    D3D12_HEAP_FLAG_NONE,
                    &D3D12_RESOURCE_DESC {
                        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER, Width: total, Height: 1, DepthOrArraySize: 1, MipLevels: 1,
                        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 }, Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR, ..Default::default()
                    },
                    D3D12_RESOURCE_STATE_COPY_DEST, None, &mut rb,
                ).unwrap();
                let rb = rb.unwrap();
                let alloc: ID3D12CommandAllocator = dev.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT).unwrap();
                let list: ID3D12GraphicsCommandList = dev.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &alloc, None).unwrap();
                let dst = D3D12_TEXTURE_COPY_LOCATION { pResource: std::mem::transmute_copy(&rb), Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT, Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { PlacedFootprint: fp } };
                let src = D3D12_TEXTURE_COPY_LOCATION { pResource: std::mem::transmute_copy(r), Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX, Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: 0 } };
                list.CopyTextureRegion(&dst, 0, 0, 0, &src, None);
                list.Close().unwrap();
                queue.ExecuteCommandLists(&[Some(list.cast().unwrap())]);
                crate::gpu::texture::wait(&dev, &queue).unwrap();
                let mut p = std::ptr::null_mut();
                rb.Map(0, None, Some(&mut p)).unwrap();
                let mut out = Vec::with_capacity(rows as usize * row_bytes as usize);
                for y in 0..rows as usize {
                    out.extend_from_slice(std::slice::from_raw_parts((p as *const u8).add(y * fp.Footprint.RowPitch as usize), row_bytes as usize));
                }
                rb.Unmap(0, None);
                out
            };

            assert_eq!(readback_tex(&a1), fill_a, "same-va alias a1 must see pixels uploaded to a2");
            assert_eq!(readback_tex(&b), fill_b, "adjacent 4 KiB texture b must not crosstalk with a");
            drop((b, a2, a1));

            // 3. A small sampled texture at a 64 KiB-aligned va occupied by an existing GPU-owned
            //    render target must stay on vram::Pages (not vram_small) so it aliases the target.
            if opts.TiledResourcesTier.0 < D3D12_TILED_RESOURCES_TIER_2.0 {
                crate::gpu::recomp::runner::skip_test(&format!("step 3: tiled resources tier {} < 2", opts.TiledResourcesTier.0));
                return;
            }
            super::super::vram::flush_idle_for(dev_key);
            let rt_va = 0x62_0000_0000usize;
            let rt_desc = desc_of(64, 64, DXGI_FORMAT_R8G8B8A8_UNORM, D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET);
            let mut rt_raw = std::ptr::null_mut();
            let hr = super::super::vram::place(dev.as_raw(), rt_va, &rt_desc, D3D12_RESOURCE_STATE_RENDER_TARGET.0 as u32, std::ptr::null(), Some(16384), riid, &mut rt_raw);
            assert!(hr.is_some_and(|h| h >= 0) && !rt_raw.is_null());
            let rt = ID3D12Resource::from_raw(rt_raw);
            let small_before_alias = bytes_for(dev_key).0;
            let sampled_view = place_sampled(rt_va, &rgba, 16384);
            assert_eq!(bytes_for(dev_key).0, small_before_alias, "sampled view over live GPU target must alias vram::Pages, not vram_small");
            drop((sampled_view, rt));
        }
    }
}
