//! GPU-owned textures in title memory, aliased as on the console.
//!
//! A console title places render targets, depth buffers and UAV textures at addresses of its
//! own memory and reuses that memory: transient targets of a frame share it, and it creates
//! several resource objects over the same address expecting one memory (seen on XDefiant: 16
//! placements at an occupied address in its first seconds, also of other formats and sizes).
//! A committed PC resource per placement breaks that – what the GPU writes through one never
//! shows in the other. So these textures are placed in *arenas*: D3D12 default heaps (video
//! memory) standing for ranges of the title's address space (32 MiB+), at the offset of their
//! address in it. Resources at one address then share memory as on the console (the same
//! description: the same PC layout, so the same texels). The CPU never sees these textures
//! (on the console it could, but GPU-owned targets are not CPU-written).
//!
//! Arenas cost video memory for whole ranges, and a placement straddling two made a third over
//! both while the two lived on with their resources: a retail game's streaming targets, packed
//! back to back over 3.5 GB of its addresses, made 15 GB of arenas in two seconds – past the
//! card's memory and the system's commit, and the device was removed. So the first choice is
//! the console's own model (`Pages`): a 64 KiB page of video memory for each 64 KiB page of
//! the title's address a target covers, and each target a reserved (tiled) resource whose tiles
//! are mapped onto the pages of its addresses. Targets at one address share pages, a page
//! exists once however many targets cover it, and what no target covers takes nothing.
//! Arenas remain for what cannot be tiled (an address not 64 KiB aligned, a resource the
//! driver does not make reserved: D32S8, MSAA): heaps of the placement's size, shared by
//! placements inside them, let go 2 s after their last resource (256 MiB ones: 2.8 GB of D32S8).

use std::collections::HashSet;
use std::ffi::c_void;
use std::sync::Mutex;

use windows::core::Interface;
use windows::Win32::Graphics::Direct3D12::*;

use crate::util::HRESULT;

/// A heap of video memory for placements at title addresses `[base, base + size)`.
struct Arena<H> {
    dev: usize,
    base: usize,
    size: usize,
    /// the heap's alignment (a placement that needs more does not go in)
    align: usize,
    heap: H,
    /// resources placed in it
    users: u32,
    /// when the last of them went
    idle_since: Option<std::time::Instant>,
    id: u64,
}

/// The arenas, apart from D3D12 (`H`: the heap; numbers in the tests).
struct Arenas<H> {
    list: Vec<Arena<H>>,
    next_id: u64,
}

impl<H: Clone> Arenas<H> {
    const fn new() -> Self {
        Arenas { list: Vec::new(), next_id: 0 }
    }

    /// Return expired heaps for `dev` (preserving insertion order) to retire/drop outside `ARENAS.lock()`.
    fn sweep_dropped_for(&mut self, dev: usize, now: std::time::Instant) -> Vec<H> {
        let is_expired = |a: &Arena<H>| a.dev == dev && a.users == 0 && a.idle_since.is_some_and(|t| now.duration_since(t) >= IDLE);
        if !self.list.iter().any(is_expired) {
            return Vec::new();
        }
        let mut dropped = Vec::new();
        let mut kept = Vec::with_capacity(self.list.len());
        for a in self.list.drain(..) {
            if is_expired(&a) {
                dropped.push(a.heap);
            } else {
                kept.push(a);
            }
        }
        self.list = kept;
        dropped
    }

    /// Place `bytes` at `va` (`align`) on `dev`; returns `(placement, expired_heaps)` without dropping inside `ARENAS.lock()`.
    fn take_for(&mut self, dev: usize, va: usize, bytes: usize, align: usize, now: std::time::Instant, make: impl FnOnce(usize, usize) -> Option<H>) -> (Option<(H, usize, u64)>, Vec<H>) {
        let expired = self.sweep_dropped_for(dev, now);
        let fits = |a: &Arena<H>| a.dev == dev && a.align >= align && a.base <= va && va + bytes <= a.base + a.size && (va - a.base) % align == 0;
        let i = match self.list.iter().rposition(fits) {
            Some(i) => i,
            None => {
                let size = bytes.div_ceil(align) * align;
                let Some(heap) = make(size, align) else { return (None, expired) };
                self.next_id += 1;
                self.list.push(Arena { dev, base: va, size, align, heap, users: 0, idle_since: None, id: self.next_id });
                self.list.len() - 1
            }
        };
        let a = &mut self.list[i];
        a.users += 1;
        a.idle_since = None;
        (Some((a.heap.clone(), va - a.base, a.id)), expired)
    }

    /// A resource in arena `id` released.
    fn release(&mut self, id: u64, now: std::time::Instant) {
        if let Some(a) = self.list.iter_mut().find(|a| a.id == id) {
            a.users = a.users.saturating_sub(1);
            if a.users == 0 {
                a.idle_since = Some(now);
            }
        }
    }

    fn bytes(&self) -> usize { self.list.iter().map(|a| a.size).sum() }
}

// D3D12 heaps are free-threaded
unsafe impl Send for Arenas<ID3D12Heap> {}

static ARENAS: Mutex<Arenas<ID3D12Heap>> = Mutex::new(Arenas::new());
/// resource → its arena's id
static ARENA_OF: Mutex<Option<std::collections::HashMap<usize, u64>>> = Mutex::new(None);
/// bytes of every arena made
static MADE_BYTES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// (bytes of the arenas in use, bytes of all arenas made) – `XCL_ACTIVITY`
pub fn arena_bytes() -> (usize, usize) {
    (ARENAS.lock().unwrap().bytes(), MADE_BYTES.load(std::sync::atomic::Ordering::Relaxed))
}
static PLACED_TEXTURES: Mutex<Option<HashSet<usize>>> = Mutex::new(None);

/// A texture backed by one of the aliasable D3D12 heaps.
pub(super) fn is_placed_texture(resource: usize) -> bool {
    PLACED_TEXTURES.lock().unwrap().as_ref().is_some_and(|textures| textures.contains(&resource))
}

pub(super) fn forget(resource: usize) {
    let placed = PLACED_TEXTURES.lock().unwrap().as_mut().is_some_and(|textures| textures.remove(&resource));
    if placed {
        forget_pages(resource);
        if let Some(id) = ARENA_OF.lock().unwrap().as_mut().and_then(|m| m.remove(&resource)) {
            ARENAS.lock().unwrap().release(id, std::time::Instant::now());
        }
    }
}

/// Whether a console texture with these D3D12_RESOURCE_FLAGS is the GPU's (render target,
/// depth buffer, UAV).
pub fn gpu_owned(flags: u32) -> bool {
    flags & 0x7 != 0
}

/// A new arena's heap: `size` bytes of video memory for title addresses from `va`.
unsafe fn arena_heap(dev: &ID3D12Device, va: usize, size: usize, align: usize) -> Option<ID3D12Heap> {
    let desc = D3D12_HEAP_DESC {
        SizeInBytes: size as u64,
        Properties: D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_DEFAULT, ..Default::default() },
        // 64 KiB, or 4 MiB for MSAA targets
        Alignment: align as u64,
        Flags: D3D12_HEAP_FLAG_NONE,
    };
    let mut heap: Option<ID3D12Heap> = None;
    if let Err(e) = dev.CreateHeap(&desc, &mut heap) {
        crate::xlog_warn!("D3D12.X: video-memory arena for {va:#x}: CreateHeap: {e}");
        return None;
    }
    let made = MADE_BYTES.fetch_add(size, std::sync::atomic::Ordering::Relaxed) + size;
    static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    if n < 16 || n.is_power_of_two() {
        crate::xlog_info!("D3D12.X: video-memory arena #{n} for title addresses {va:#x}..{:#x} ({made} bytes of arenas made so far)", va + size);
    }
    heap
}

/// bytes of a page – of the title's addresses and of a tile
const PAGE: usize = 0x1_0000;
/// pages of a video-memory pool heap (64 MiB)
const POOL_PAGES: u32 = 1024;
/// a page no target uses any more is kept this long for the next target at its address (with
/// what it holds – a title may make a target again over memory it rendered to)
const IDLE: std::time::Duration = std::time::Duration::from_secs(2);
/// a pool tile given back is not reused before this (the GPU may still be at work on what the
/// title released)
const COOL: std::time::Duration = std::time::Duration::from_secs(2);

/// A pool tile: (pool heap, tile in it).
type Tile = (u32, u32);

struct Page {
    tile: Tile,
    /// targets using the page
    users: u32,
    /// when the last of them went
    idle_since: Option<std::time::Instant>,
}

/// A page of `Pages::pages` (its title address) or of `Pages::extra_pages` (its key there).
#[derive(Clone, Copy)]
enum PageKey {
    Own(usize),
    Extra(usize, usize),
}

/// The pages of `idle` – those whose last user went, oldest first, each with when – that no
/// resource has used for `IDLE` go back to the pools: out of their map, their tile to `cooling`.
/// An entry whose page was used again since (in use now, or idle again from a later moment, with
/// a later entry) is dropped. Only the queue is looked at: every page of both maps, looked at at
/// every placement, grew with the square of what a title streams.
fn sweep_idle(
    idle: &mut std::collections::VecDeque<(PageKey, std::time::Instant)>,
    pages: &mut std::collections::HashMap<usize, Page>,
    extra_pages: &mut std::collections::HashMap<(usize, usize), Page>,
    cooling: &mut std::collections::VecDeque<(Tile, std::time::Instant)>,
    now: std::time::Instant,
) {
    while idle.front().is_some_and(|&(_, since)| now.duration_since(since) >= IDLE) {
        let Some((key, since)) = idle.pop_front() else { break };
        let still_idle = |p: &Page| p.idle_since == Some(since);
        let page = match key {
            PageKey::Own(a) if pages.get(&a).is_some_and(still_idle) => pages.remove(&a),
            PageKey::Extra(va, i) if extra_pages.get(&(va, i)).is_some_and(still_idle) => extra_pages.remove(&(va, i)),
            _ => None,
        };
        if let Some(p) = page {
            cooling.push_back((p.tile, now));
        }
    }
}

#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
struct TileShape {
    mip_levels: u16,
    array_slices: u16,
    packed_mips: u8,
    packed_start: u32,
    packed_tiles: u32,
}

impl TileShape {
    fn from_tiling(mips: u16, slices: u16, total: u32, pk: &D3D12_PACKED_MIP_INFO) -> Self {
        let (packed_mips, packed_start, packed_tiles) = if pk.NumPackedMips > 0 {
            (
                pk.NumPackedMips,
                pk.StartTileIndexInOverallResource.min(total),
                pk.NumTilesForPackedMips.min(total),
            )
        } else {
            (0, 0, 0)
        };
        Self {
            mip_levels: mips,
            array_slices: slices.max(1),
            packed_mips,
            packed_start,
            packed_tiles,
        }
    }
}

/// Partition `[0..total)` into atomic packed-mip groups (kept whole per D3D12 spec) and unpacked
/// standard tiles in coarsest-first priority order across `shape.array_slices`.
fn packed_and_standard_layout(
    total: usize, shape: TileShape,
) -> (Vec<std::ops::Range<usize>>, Vec<usize>) {
    let slices = shape.array_slices.max(1) as usize;
    let round_robin_coarsest = |count: usize, n_slices: usize| -> Vec<usize> {
        if n_slices > 1 && count % n_slices == 0 {
            let per = count / n_slices;
            let mut out = Vec::with_capacity(count);
            for rev in 0..per {
                for s in 0..n_slices {
                    out.push(s * per + (per - 1 - rev));
                }
            }
            out
        } else {
            (0..count).rev().collect()
        }
    };
    if shape.packed_mips > 0 && shape.packed_tiles > 0 {
        let (p_start, p_count) = (shape.packed_start as usize, shape.packed_tiles as usize);
        if total % slices == 0 && p_start.saturating_add(p_count) == total / slices {
            let per = total / slices;
            let packed = (0..slices).map(|s| s * per + p_start..s * per + per).collect();
            let mut std_tiles = Vec::with_capacity(p_start * slices);
            for rev in 0..p_start {
                for s in 0..slices {
                    std_tiles.push(s * per + (p_start - 1 - rev));
                }
            }
            return (packed, std_tiles);
        }
        if p_start.saturating_add(p_count) == total {
            return (vec![p_start..total], round_robin_coarsest(p_start, slices));
        }
        let end = p_start.saturating_add(p_count).min(total);
        let packed = (p_start < end).then_some(p_start..end).into_iter().collect();
        let std_tiles = (0..p_start).rev().chain((end..total).rev()).collect();
        return (packed, std_tiles);
    }
    let std_tiles = if shape.mip_levels > 1 {
        round_robin_coarsest(total, slices)
    } else {
        (0..total).collect()
    };
    (Vec::new(), std_tiles)
}

struct Target {
    va: usize,
    console_bytes: Option<u64>,
    total: u32,
    shape: TileShape,
    mapped: bool,
    used: Vec<usize>,
    used_extra: Vec<(usize, usize)>,
    /// its levels get their pages one by one (`map_levels`); None: all of it has pages
    levels: Option<Levels>,
    /// placed as GPU-owned: its pages are keyed by the physical page under them (`page_key`)
    gpu_owned: bool,
    /// top mips left without video memory (`top_mip_tiles`): their levels never get pages
    skip_mips: u32,
    /// their tiles (empty: none), kept for `remap_if_unmapped` / `backfill_incomplete_state`
    skip: Vec<bool>,
}

impl Target {
    fn is_complete(&self) -> bool {
        let backed = self.used.len() + self.used_extra.len();
        let wanted = (self.total as usize).saturating_sub(self.skip.iter().filter(|&&s| s).count());
        self.mapped && (self.levels.is_some() || backed >= wanted)
    }
}

/// The tiles of a texture whose levels get video memory when an upload first has texels for
/// them (`level_pages`): a group for each level with tiles of its own, in order, then one for
/// the packed levels.
#[derive(Clone, Debug, PartialEq)]
struct Levels {
    /// (the group's first subresource, its first tile in the resource, its tiles, mapped)
    groups: Vec<(u32, u32, u32, bool)>,
    /// the texture's levels (the last group's go up to here)
    mips: u32,
}

impl Levels {
    /// From the resource's tiling: `standard` (first tile, tiles) of each level with tiles of
    /// its own, `packed` the same of the packed levels, `total` tiles in all. None: they do not
    /// add up to the resource (it then gets all its pages at once).
    fn new(standard: &[(u32, u32)], packed: Option<(u32, u32)>, total: u32, mips: u32) -> Option<Levels> {
        let mut groups: Vec<(u32, u32, u32, bool)> = standard.iter().enumerate().map(|(i, &(first, n))| (i as u32, first, n, false)).collect();
        if let Some((first, n)) = packed {
            groups.push((standard.len() as u32, first, n, false));
        }
        let mut next = 0u32;
        let mut by_tile = groups.clone();
        by_tile.sort_by_key(|g| g.1);
        for g in &by_tile {
            if g.2 == 0 || g.1 != next {
                return None;
            }
            next = g.1.checked_add(g.2)?;
        }
        (next == total && !groups.is_empty()).then_some(Levels { groups, mips })
    }

    /// The levels of group `k`.
    fn levels_of(&self, k: usize) -> std::ops::Range<u32> {
        self.groups[k].0..self.groups.get(k + 1).map_or(self.mips, |g| g.0)
    }

    /// The group level `level` is in (the packed levels share the last one).
    fn group_of(&self, level: u32) -> usize {
        self.groups.partition_point(|g| g.0 <= level).saturating_sub(1)
    }
}

/// `XCL_TEXTURE_LEVEL_PAGES=1` (off by default): a sampled 2D texture of one slice on the
/// shared pages gets the pages of a level when an upload first has texels for it. A retail game
/// places its streamed textures at full size and maps memory under their levels as it loads
/// them, the small ones first; with a page for every 64 KiB a texture covers most of the pages
/// are for levels that are not loaded.
pub(super) fn level_pages() -> bool {
    #[cfg(test)]
    if LEVEL_PAGES_FOR_TEST.load(std::sync::atomic::Ordering::Relaxed) {
        return true;
    }
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        let on = std::env::var("XCL_TEXTURE_LEVEL_PAGES").is_ok_and(|v| v.trim() == "1");
        if on {
            crate::xlog_info!("D3D12.X: XCL_TEXTURE_LEVEL_PAGES=1 - a sampled texture's levels get their video memory pages when an upload first has texels for them");
        }
        on
    })
}

/// `level_pages` for a test (under `VRAM_TEST_LOCK`)
#[cfg(test)]
pub(super) static LEVEL_PAGES_FOR_TEST: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Of the textures with lazy levels there are now: how many, the tiles they stand for, and the
/// tiles of them that have pages; and the level groups given pages so far.
static LEVEL_STATS: [std::sync::atomic::AtomicU64; 4] = [const { std::sync::atomic::AtomicU64::new(0) }; 4];

/// (textures with lazy levels, the tiles they stand for, the tiles of them with pages, level
/// groups given pages so far)
fn level_stats() -> (u64, u64, u64, u64) {
    let l = |k: usize| LEVEL_STATS[k].load(std::sync::atomic::Ordering::Relaxed);
    (l(0), l(1), l(2), l(3))
}

impl Levels {
    /// Its tiles that have pages.
    fn mapped_tiles(&self) -> u64 {
        self.groups.iter().filter(|g| g.3).map(|g| g.2 as u64).sum()
    }
}

/// The tiling of reserved texture `res` of `mips` levels as `Levels`, its tiles in all, and
/// `D3D12_PACKED_MIP_INFO`.
unsafe fn level_tiling(
    dev: &ID3D12Device, res: &ID3D12Resource, mips: u32,
) -> (Option<Levels>, u32, D3D12_PACKED_MIP_INFO) {
    let mut total = 0u32;
    let mut packed = D3D12_PACKED_MIP_INFO::default();
    let mut n = mips;
    let mut tilings = vec![D3D12_SUBRESOURCE_TILING::default(); mips as usize];
    dev.GetResourceTiling(res, Some(&mut total), Some(&mut packed), None, Some(&mut n), 0, tilings.as_mut_ptr());
    let standard: Vec<(u32, u32)> = tilings
        .iter()
        .take((packed.NumStandardMips as usize).min(n as usize))
        .map(|t| (t.StartTileIndexInOverallResource, t.WidthInTiles * t.HeightInTiles as u32 * t.DepthInTiles as u32))
        .collect();
    let tail = (packed.NumPackedMips > 0).then_some((packed.StartTileIndexInOverallResource, packed.NumTilesForPackedMips));
    let whole = standard.len() == packed.NumStandardMips as usize && packed.NumStandardMips as u32 + packed.NumPackedMips as u32 == mips;
    (if whole { Levels::new(&standard, tail, total, mips) } else { None }, total, packed)
}

/// Per-heap free-tile allocator for `Pages`.
///
/// Keeping free tiles partitioned by pool heap (sorted descending so `.pop()` yields ascending
/// contiguous tile indices) prevents streaming churn across 100+ 64 MiB pools from scattering a
/// single texture across dozens of heaps — which otherwise explodes `Pages::map` into dozens of
/// `UpdateTileMappings` calls per texture.
struct TilePool {
    /// Free tile indices for each pool heap (descending when `!dirty_sort[h]`).
    free_by_heap: Vec<Vec<u32>>,
    /// Whether `free_by_heap[h]` must be sorted descending before popping.
    dirty_sort: Vec<bool>,
    /// Total free tiles currently available across all heaps.
    free_total: usize,
    /// Tiles given back, reusable after `COOL`.
    cooling: std::collections::VecDeque<(Tile, std::time::Instant)>,
}

impl TilePool {
    fn new() -> Self {
        TilePool {
            free_by_heap: Vec::new(),
            dirty_sort: Vec::new(),
            free_total: 0,
            cooling: std::collections::VecDeque::new(),
        }
    }

    fn add_heap_at(&mut self, slot: u32, pool_pages: u32) {
        let idx = slot as usize;
        if idx >= self.free_by_heap.len() {
            self.free_by_heap.resize_with(idx + 1, Vec::new);
            self.dirty_sort.resize(idx + 1, false);
        }
        debug_assert!(self.free_by_heap[idx].is_empty());
        self.free_by_heap[idx] = (0..pool_pages).rev().collect();
        self.dirty_sort[idx] = false;
        self.free_total += pool_pages as usize;
    }

    /// Reclaim completely empty cooled heaps (`free_by_heap[h].len() == pool_pages`), keeping up
    /// to `keep_spare` empty heaps warm so steady-state streaming never thrashes `CreateHeap`.
    /// Returns the reclaimed heap indices so the caller can drop their `ID3D12Heap`s.
    fn reclaim_empty_heaps(&mut self, keep_spare: usize, pool_pages: u32) -> Vec<u32> {
        let full = pool_pages as usize;
        let mut empty: Vec<usize> = self
            .free_by_heap
            .iter()
            .enumerate()
            .filter_map(|(h, tiles)| (tiles.len() == full).then_some(h))
            .collect();
        if empty.len() <= keep_spare {
            return Vec::new();
        }
        let mut reclaimed = Vec::with_capacity(empty.len() - keep_spare);
        for h in empty.drain(keep_spare..) {
            self.free_by_heap[h].clear();
            self.free_by_heap[h].shrink_to_fit();
            self.dirty_sort[h] = false;
            self.free_total -= full;
            reclaimed.push(h as u32);
        }
        reclaimed
    }

    fn push_cooling(&mut self, tile: Tile, now: std::time::Instant) {
        self.cooling.push_back((tile, now));
    }

    /// Move every cooled tile (`>= COOL`) into its pool heap's free list so best-fit allocation
    /// sees all ready tiles at once.
    fn drain_cooling(&mut self, now: std::time::Instant) {
        while self.cooling.front().is_some_and(|&(_, at)| now.duration_since(at) >= COOL) {
            let Some(((h, t), _)) = self.cooling.pop_front() else { break };
            let idx = h as usize;
            if idx < self.free_by_heap.len() {
                self.free_by_heap[idx].push(t);
                self.dirty_sort[idx] = true;
                self.free_total += 1;
            }
        }
    }

    fn return_tiles(&mut self, tiles: &[Tile]) {
        for &(h, t) in tiles {
            let idx = h as usize;
            if idx < self.free_by_heap.len() {
                self.free_by_heap[idx].push(t);
                self.dirty_sort[idx] = true;
                self.free_total += 1;
            }
        }
    }

    /// Pick the pool heap to take up to `rem` tiles from:
    /// 1. `preferred` if it fits all `rem` tiles (keeps partial-overlap placements on 1 heap),
    /// 2. otherwise best-fit heap with `len >= rem` (fits the whole texture in 1 heap without
    ///    splitting fuller pools),
    /// 3. otherwise the heap with the most free tiles (minimizing distinct heaps touched).
    fn pick_heap(&self, rem: usize, preferred: Option<u32>) -> Option<usize> {
        if self.free_total == 0 {
            return None;
        }
        if let Some(p) = preferred.map(|h| h as usize).filter(|&h| h < self.free_by_heap.len()) {
            if self.free_by_heap[p].len() >= rem {
                return Some(p);
            }
        }
        let mut best_fit: Option<(usize, usize)> = None;
        let mut largest: Option<(usize, usize)> = None;
        for (h, tiles) in self.free_by_heap.iter().enumerate() {
            let len = tiles.len();
            if len == 0 {
                continue;
            }
            if len >= rem {
                if best_fit.is_none_or(|(_, blen)| len < blen) {
                    best_fit = Some((h, len));
                }
            } else if largest.is_none_or(|(lh, llen)| len > llen || (len == llen && preferred == Some(h as u32) && preferred != Some(lh as u32))) {
                largest = Some((h, len));
            }
        }
        best_fit.or(largest).map(|(h, _)| h)
    }

    /// Allocate `count` tiles, preferring a single heap and ascending contiguous offsets.
    /// `make_heap` allocates a new backing pool heap of `pool_pages` tiles when `free_total == 0`.
    /// On failure, any tiles already taken are returned untouched.
    fn take_batch(
        &mut self,
        count: usize,
        preferred: Option<u32>,
        now: std::time::Instant,
        pool_pages: u32,
        mut make_heap: impl FnMut() -> Option<u32>,
    ) -> Option<Vec<Tile>> {
        if count == 0 {
            return Some(Vec::new());
        }
        self.drain_cooling(now);
        let mut out = Vec::with_capacity(count);
        while out.len() < count {
            let rem = count - out.len();
            let h = match self.pick_heap(rem, preferred) {
                Some(h) => h,
                None => {
                    let Some(new_h) = make_heap() else {
                        self.return_tiles(&out);
                        return None;
                    };
                    self.add_heap_at(new_h, pool_pages);
                    new_h as usize
                }
            };
            if self.dirty_sort[h] {
                self.free_by_heap[h].sort_unstable_by(|a, b| b.cmp(a));
                self.dirty_sort[h] = false;
            }
            let free = &mut self.free_by_heap[h];
            let take = rem.min(free.len());
            for _ in 0..take {
                let t = free.pop().unwrap();
                out.push((h as u32, t));
            }
            self.free_total -= take;
        }
        Some(out)
    }

    /// Allocate `0..=count` tiles, preferring one heap and ascending runs, stopping at cap/OOM.
    fn take_up_to(
        &mut self, count: usize, preferred: Option<u32>, now: std::time::Instant, pool_pages: u32,
        mut make_heap: impl FnMut() -> Option<u32>,
    ) -> Vec<Tile> {
        if count == 0 { return Vec::new(); }
        self.drain_cooling(now);
        let mut out = Vec::with_capacity(count);
        while out.len() < count {
            let rem = count - out.len();
            let h = match self.pick_heap(rem, preferred) {
                Some(h) => h,
                None => {
                    let Some(new_h) = make_heap() else { break };
                    self.add_heap_at(new_h, pool_pages);
                    new_h as usize
                }
            };
            if self.dirty_sort[h] {
                self.free_by_heap[h].sort_unstable_by(|a, b| b.cmp(a));
                self.dirty_sort[h] = false;
            }
            let tiles = &mut self.free_by_heap[h];
            let take = rem.min(tiles.len());
            for _ in 0..take { out.push((h as u32, tiles.pop().unwrap())); }
            self.free_total -= take;
        }
        out
    }
}

/// Allocate one 64 MiB default heap for `Pages`.
unsafe fn alloc_pool_heap(dev: &ID3D12Device) -> Option<ID3D12Heap> {
    let desc = D3D12_HEAP_DESC {
        SizeInBytes: POOL_PAGES as u64 * PAGE as u64,
        Properties: D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_DEFAULT, ..Default::default() },
        Alignment: 0,
        // every kind of texture (resource heap tier 2: the only flags for both)
        Flags: D3D12_HEAP_FLAG_NONE,
    };
    let mut heap: Option<ID3D12Heap> = None;
    if let Err(e) = super::title_textures::own_work(|| dev.CreateHeap(&desc, &mut heap)) {
        crate::xlog_warn!("D3D12.X: no video memory for more pages of title textures ({e})");
        return None;
    }
    heap
}

/// Backing 64 MiB default heaps for `Pages`, supporting slot reuse when completely empty heaps
/// are reclaimed back to local VRAM. `len()` reports the count of live heaps.
struct PoolHeaps {
    slots: Vec<Option<ID3D12Heap>>,
    free_slots: Vec<u32>,
    live: usize,
}

impl PoolHeaps {
    fn new() -> Self {
        Self { slots: Vec::new(), free_slots: Vec::new(), live: 0 }
    }

    fn len(&self) -> usize {
        self.live
    }

    fn push_heap(&mut self, heap: ID3D12Heap) -> u32 {
        self.live += 1;
        if let Some(idx) = self.free_slots.pop() {
            debug_assert!(self.slots[idx as usize].is_none());
            self.slots[idx as usize] = Some(heap);
            idx
        } else {
            let idx = self.slots.len() as u32;
            self.slots.push(Some(heap));
            idx
        }
    }

    fn take_heap(&mut self, idx: u32) -> Option<ID3D12Heap> {
        let h = self.slots.get_mut(idx as usize)?.take()?;
        self.live -= 1;
        self.free_slots.push(idx);
        Some(h)
    }
}

impl std::ops::Index<usize> for PoolHeaps {
    type Output = ID3D12Heap;
    fn index(&self, idx: usize) -> &Self::Output {
        self.slots[idx].as_ref().expect("live VRAM pool heap")
    }
}

fn keep_spare_pools() -> usize {
    static N: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *N.get_or_init(|| {
        std::env::var("XCL_VRAM_KEEP_SPARE_POOLS")
            .ok()
            .and_then(|v| v.trim().parse::<usize>().ok())
            .unwrap_or(1)
    })
}

const POOL_BYTES: u64 = POOL_PAGES as u64 * PAGE as u64;
const OOM_BACKOFF: std::time::Duration = std::time::Duration::from_secs(1);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AdapterBudget { Local(u64), Dedicated(u64) }

fn parse_target_pages_mb(raw: Option<&str>) -> Option<u64> { raw?.trim().parse::<u64>().ok() }

/// Max 64 MiB pool heaps for `Pages` and the source that set it: `XCL_TARGET_PAGES_MB=0` disables
/// the cap, `Some(mb)` sets `mb` MiB, UMA adapters (`D3D12_FEATURE_DATA_ARCHITECTURE::UMA`) use
/// `title_memory_mb`, and discrete adapters use `min(title_memory_mb, 75% of DXGI local budget)`
/// (#646, #1337).
fn compute_max_pools(
    env_mb: Option<u64>, title_budget_mb: u64, is_uma: bool, adapter_budget: Option<AdapterBudget>,
) -> (usize, &'static str) {
    match env_mb {
        Some(0) => (usize::MAX, "XCL_TARGET_PAGES_MB=0"),
        Some(mb) => {
            let n = ((mb.saturating_mul(1 << 20)) / POOL_BYTES).max(1) as usize;
            (n, "XCL_TARGET_PAGES_MB")
        }
        None => {
            let title_cap = title_budget_mb.saturating_mul(1 << 20);
            let (cap, source) = match (is_uma, adapter_budget) {
                (true, _) => (title_cap, "title_memory_mb on UMA adapter"),
                (false, Some(ab)) => {
                    let (bytes, label) = match ab {
                        AdapterBudget::Local(b) => (b, "75% of DXGI local budget"),
                        AdapterBudget::Dedicated(b) => (b, "75% of DedicatedVideoMemory"),
                    };
                    let pct75 = (bytes / 4).saturating_mul(3);
                    if bytes > 0 && pct75 < title_cap {
                        (pct75, label)
                    } else {
                        (title_cap, "title_memory_mb")
                    }
                }
                (false, None) => (title_cap, "title_memory_mb"),
            };
            (((cap / POOL_BYTES) as usize).max(1), source)
        }
    }
}

unsafe fn target_pages_max_pools(dev: &ID3D12Device) -> (usize, &'static str) {
    use windows::Win32::Graphics::Dxgi::*;
    let env_mb = parse_target_pages_mb(std::env::var("XCL_TARGET_PAGES_MB").ok().as_deref());
    let mut arch = D3D12_FEATURE_DATA_ARCHITECTURE::default();
    let (arch_ptr, arch_sz) = (&mut arch as *mut _ as *mut c_void, std::mem::size_of_val(&arch));
    let is_uma = env_mb.is_none()
        && dev.CheckFeatureSupport(D3D12_FEATURE_ARCHITECTURE, arch_ptr, arch_sz as u32).is_ok()
        && arch.UMA.0 != 0;
    let adapter_budget = (env_mb.is_none() && !is_uma).then(|| {
        let factory: IDXGIFactory4 = CreateDXGIFactory1().ok()?;
        let adapter: IDXGIAdapter3 = factory.EnumAdapterByLuid(dev.GetAdapterLuid()).ok()?;
        let desc = adapter.GetDesc1().ok()?;
        if desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0 { return None; }
        let mut local = DXGI_QUERY_VIDEO_MEMORY_INFO::default();
        if adapter.QueryVideoMemoryInfo(0, DXGI_MEMORY_SEGMENT_GROUP_LOCAL, &mut local).is_ok()
            && local.Budget > 0
        {
            Some(AdapterBudget::Local(local.Budget))
        } else {
            (desc.DedicatedVideoMemory > 0)
                .then_some(AdapterBudget::Dedicated(desc.DedicatedVideoMemory as u64))
        }
    }).flatten();
    let title_mb = crate::state::config().title_memory_budget_mb();
    compute_max_pools(env_mb, title_mb, is_uma, adapter_budget)
}

/// Allocate a budgeted 64 MiB pool heap on `dev`, counting active + retiring 64 MiB pool heaps
/// (`active + retiring < max_pools`) and polling completed retirements first before refusing.
unsafe fn alloc_budgeted_pool(
    dev: &ID3D12Device, dev_key: usize, max_pools: usize,
    oom_until: &mut Option<std::time::Instant>, pools: &mut PoolHeaps, now: std::time::Instant,
) -> Option<u32> {
    if pools.len() >= max_pools { return None; }
    let (retiring, freed_pool) = poll_retiring_pool_heaps(dev_key);
    if freed_pool { *oom_until = None; }
    if pools.len().saturating_add(retiring) >= max_pools || oom_until.is_some_and(|t| now < t) {
        return None;
    }
    alloc_pool_heap(dev).map(|h| pools.push_heap(h)).or_else(|| {
        *oom_until = Some(now + OOM_BACKOFF);
        None
    })
}

/// The title's pages of video memory for tiled textures (see the module's comment).
struct Pages {
    queue: ID3D12CommandQueue,
    /// signalled after each mapping; title queues wait for it before their next work
    fence: ID3D12Fence,
    value: u64,
    /// per-device timestamp of the last periodic reclaim check
    last_reclaim: std::time::Instant,
    pools: PoolHeaps,
    pool: TilePool,
    max_pools: usize,
    oom_until: Option<std::time::Instant>,
    /// title page address → its video memory
    pages: std::collections::HashMap<usize, Page>,
    /// extra / sub-64 KiB tiles keyed by `(va, tile_index)` so repeated placements at the same
    /// address share them without aliasing unrelated addresses
    extra_pages: std::collections::HashMap<(usize, usize), Page>,
    /// pages of both maps whose last user went, oldest first, with when (`sweep_idle`)
    idle: std::collections::VecDeque<(PageKey, std::time::Instant)>,
    /// resource → its mapping metadata and currently held pages / extra keys
    targets: std::collections::HashMap<usize, Target>,
    /// queue → the fence value it last waited for
    waited: std::collections::HashMap<usize, u64>,
    /// cached `reservable()` answers for `(DXGI_FORMAT, SampleCount)` on this device
    reservable_cache: std::collections::HashMap<(i32, u32), bool>,
    /// reusable scratch buffers for `map()`
    scratch_heaps: Vec<u32>,
    scratch_flags: Vec<D3D12_TILE_RANGE_FLAGS>,
    scratch_offsets: Vec<u32>,
    scratch_counts: Vec<u32>,
}

// D3D12 objects are free-threaded
unsafe impl Send for Pages {}

/// device raw pointer → its pages (None inside: tiled tier < 2 on this device)
static PAGES: Mutex<std::collections::BTreeMap<usize, Option<Pages>>> = Mutex::new(std::collections::BTreeMap::new());

impl Pages {
    unsafe fn new(dev: &ID3D12Device) -> Option<Pages> {
        let mut o = D3D12_FEATURE_DATA_D3D12_OPTIONS::default();
        dev.CheckFeatureSupport(D3D12_FEATURE_D3D12_OPTIONS, &mut o as *mut _ as *mut c_void, std::mem::size_of_val(&o) as u32).ok()?;
        // tier 2: unmapped tiles read as zero, writes to them go nowhere
        if o.TiledResourcesTier.0 < D3D12_TILED_RESOURCES_TIER_2.0 {
            crate::xlog_info!("D3D12.X: tiled resources tier {}: GPU-owned textures in arenas", o.TiledResourcesTier.0);
            return None;
        }
        let queue: ID3D12CommandQueue = dev.CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC::default()).ok()?;
        let _ = queue.SetName(windows::core::w!("xcl vram tile-mapping queue"));
        let fence: ID3D12Fence = dev.CreateFence(0, D3D12_FENCE_FLAG_NONE).ok()?;
        let (max_pools, source) = target_pages_max_pools(dev);
        let cap_note = if max_pools == usize::MAX {
            format!("pool budget uncapped via {source}")
        } else {
            let budget_mb = (max_pools as u64 * POOL_BYTES) >> 20;
            format!("pool budget {budget_mb} MiB, {max_pools} pools from {source}")
        };
        crate::xlog_info!(
            "D3D12.X: textures in title memory: tiled resources on 64 KiB pages of video \
             memory, one per page of the title's ({cap_note})"
        );
        Some(Pages {
            queue,
            fence,
            value: 0,
            last_reclaim: std::time::Instant::now(),
            pools: PoolHeaps::new(),
            pool: TilePool::new(),
            max_pools,
            oom_until: None,
            pages: std::collections::HashMap::new(),
            extra_pages: std::collections::HashMap::new(),
            idle: std::collections::VecDeque::new(),
            targets: std::collections::HashMap::new(),
            waited: std::collections::HashMap::new(),
            reservable_cache: std::collections::HashMap::new(),
            scratch_heaps: Vec::new(),
            scratch_flags: Vec::new(),
            scratch_offsets: Vec::new(),
            scratch_counts: Vec::new(),
        })
    }

    /// Pages no resource used for `IDLE` go back to the pools.
    fn sweep(&mut self, now: std::time::Instant) {
        sweep_idle(&mut self.idle, &mut self.pages, &mut self.extra_pages, &mut self.pool.cooling, now);
    }

    /// Sweep expired idle pages, drain cooled tiles, and reclaim any completely empty 64 MiB
    /// pool heaps (above the warm spare count), returning their `ID3D12Heap`s to be dropped
    /// outside `PAGES.lock()`.
    fn sweep_and_reclaim(&mut self, now: std::time::Instant) -> Vec<ID3D12Heap> {
        self.sweep(now);
        self.pool.drain_cooling(now);
        let reclaimed_ids = self.pool.reclaim_empty_heaps(keep_spare_pools(), POOL_PAGES);
        let mut dropped = Vec::with_capacity(reclaimed_ids.len());
        for idx in reclaimed_ids {
            if let Some(h) = self.pools.take_heap(idx) {
                dropped.push(h);
            }
        }
        if !dropped.is_empty() {
            self.oom_until = None;
            let mb = (dropped.len() * POOL_PAGES as usize * PAGE) >> 20;
            crate::xlog_info!(
                "D3D12.X: reclaimed {} empty 64 MiB VRAM pool heap(s) ({mb} MiB returned to GPU; {} pools active)",
                dropped.len(),
                self.pools.len()
            );
        }
        dropped
    }

    /// Map every tile of `res` from the first tile of subresource `sub` on: backed tiles bind to
    /// their pool heaps (one call per heap), and unbacked (`None`) tiles explicitly bind
    /// `D3D12_TILE_RANGE_FLAG_NULL` (`VK_NULL_HANDLE` under `vkd3d-proton`).
    unsafe fn map(&mut self, res: &ID3D12Resource, sub: u32, tiles: &[Option<Tile>]) {
        if tiles.is_empty() {
            return;
        }
        self.scratch_heaps.clear();
        self.scratch_heaps.extend(tiles.iter().filter_map(|t| t.map(|x| x.0)));
        if self.scratch_heaps.is_empty() {
            let num_tiles = tiles.len() as u32;
            let coord = D3D12_TILED_RESOURCE_COORDINATE { Subresource: sub, ..Default::default() };
            let region = D3D12_TILE_REGION_SIZE {
                NumTiles: num_tiles, UseBox: false.into(), ..Default::default()
            };
            let flag = D3D12_TILE_RANGE_FLAG_NULL;
            self.queue.UpdateTileMappings(
                res, 1, Some(&coord), Some(&region), None, 1, Some(&flag), None, Some(&num_tiles),
                D3D12_TILE_MAPPING_FLAG_NONE,
            );
            self.value += 1;
            let _ = self.queue.Signal(&self.fence, self.value);
            return;
        }
        self.scratch_heaps.sort_unstable();
        self.scratch_heaps.dedup();
        for i in 0..self.scratch_heaps.len() {
            let h = self.scratch_heaps[i];
            let ranges = heap_ranges_trimmed(h, tiles, i == 0);
            if ranges.is_empty() {
                continue;
            }
            self.scratch_flags.clear();
            self.scratch_offsets.clear();
            self.scratch_counts.clear();
            let mut num_tiles = 0u32;
            for &r in &ranges {
                let (flag, off, count) = r.parts();
                self.scratch_flags.push(flag);
                self.scratch_offsets.push(off);
                self.scratch_counts.push(count);
                num_tiles += count;
            }
            let coord = D3D12_TILED_RESOURCE_COORDINATE { Subresource: sub, ..Default::default() };
            let region = D3D12_TILE_REGION_SIZE { NumTiles: num_tiles, UseBox: false.into(), ..Default::default() };
            self.queue.UpdateTileMappings(
                res,
                1,
                Some(&coord),
                Some(&region),
                &self.pools[h as usize],
                ranges.len() as u32,
                Some(self.scratch_flags.as_ptr()),
                Some(self.scratch_offsets.as_ptr()),
                Some(self.scratch_counts.as_ptr()),
                D3D12_TILE_MAPPING_FLAG_NONE,
            );
        }
        self.value += 1;
        let _ = self.queue.Signal(&self.fence, self.value);
    }

    /// Unmap every tile of `res` (`D3D12_TILE_RANGE_FLAG_NULL`) so its unshared tiles can return
    /// to the pool even while the title keeps the COM object alive.
    unsafe fn unmap(&mut self, res: &ID3D12Resource, total: u32) {
        if total == 0 {
            return;
        }
        let coord = D3D12_TILED_RESOURCE_COORDINATE::default();
        let region = D3D12_TILE_REGION_SIZE { NumTiles: total, UseBox: false.into(), ..Default::default() };
        let flag = D3D12_TILE_RANGE_FLAG_NULL;
        self.queue.UpdateTileMappings(
            res,
            1,
            Some(&coord),
            Some(&region),
            None,
            1,
            Some(&flag),
            None,
            Some(&total),
            D3D12_TILE_MAPPING_FLAG_NONE,
        );
        self.value += 1;
        let _ = self.queue.Signal(&self.fence, self.value);
    }

    fn release_tiles(&mut self, used: Vec<usize>, used_extra: Vec<(usize, usize)>, now: std::time::Instant, immediate_cool: bool) {
        for a in used {
            if let Some(p) = self.pages.get_mut(&a) {
                p.users = p.users.saturating_sub(1);
                if p.users == 0 {
                    if immediate_cool {
                        if let Some(p) = self.pages.remove(&a) {
                            self.pool.push_cooling(p.tile, now);
                        }
                    } else {
                        p.idle_since = Some(now);
                        self.idle.push_back((PageKey::Own(a), now));
                    }
                }
            }
        }
        for k in used_extra {
            if let Some(p) = self.extra_pages.get_mut(&k) {
                p.users = p.users.saturating_sub(1);
                if p.users == 0 {
                    if immediate_cool {
                        if let Some(p) = self.extra_pages.remove(&k) {
                            self.pool.push_cooling(p.tile, now);
                        }
                    } else {
                        p.idle_since = Some(now);
                        self.idle.push_back((PageKey::Extra(k.0, k.1), now));
                    }
                }
            }
        }
    }

    /// Acquire/share up to `total` tiles for `res` at `va` within `max_pools` and map them on the
    /// `Pages` queue; unbacked tiles (including those marked in `skip`) bind
    /// `D3D12_TILE_RANGE_FLAG_NULL` (#333, #646, #1337).
    /// (`phys[i]`: the title's physical page at owned page i, if any – see `page_key`).
    #[allow(clippy::too_many_arguments)]
    unsafe fn bind(
        &mut self, dev: &ID3D12Device, res: &ID3D12Resource, va: usize,
        console_bytes: Option<u64>, total: u32, shape: TileShape, phys: &[Option<usize>],
        skip: &[bool],
    ) -> Option<(Vec<usize>, Vec<(usize, usize)>)> {
        self.rebind(dev, res, va, console_bytes, total, shape, false, &[], &[], phys, skip)
    }

    #[allow(clippy::too_many_arguments)]
    unsafe fn rebind(
        &mut self, dev: &ID3D12Device, res: &ID3D12Resource, va: usize,
        console_bytes: Option<u64>, total: u32, shape: TileShape, was_mapped: bool,
        prev_used: &[usize], prev_extra: &[(usize, usize)], phys: &[Option<usize>],
        skip: &[bool],
    ) -> Option<(Vec<usize>, Vec<(usize, usize)>)> {
        let (first, own) = (total > 0).then(|| owned_pages(va, console_bytes, total as usize))??;
        let now = std::time::Instant::now();
        self.sweep(now);
        let prev_backed = prev_used.len() + prev_extra.len();
        if was_mapped {
            for &a in prev_used {
                if let Some(p) = self.pages.get_mut(&a) { p.users = p.users.saturating_sub(1); }
            }
            for &k in prev_extra {
                if let Some(p) = self.extra_pages.get_mut(&k) {
                    p.users = p.users.saturating_sub(1);
                }
            }
        }
        let (dev_key, max_pools) = (dev.as_raw() as usize, self.max_pools);
        let (oom_until, pools, pool) = (&mut self.oom_until, &mut self.pools, &mut self.pool);
        let (tiles, used, used_extra, dropped) = Self::bind_tiles(
            &mut self.pages, &mut self.extra_pages, pool, (va, first, own, total as usize, shape),
            phys, skip, (now, POOL_PAGES),
            || alloc_budgeted_pool(dev, dev_key, max_pools, oom_until, pools, now),
        );
        let backed = used.len() + used_extra.len();
        if dropped > 0 && (!was_mapped || backed > prev_backed) {
            static LOGS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let n = LOGS.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
            if n <= 4 || n.is_power_of_two() {
                let (active, retiring) = (self.pools.len(), retiring_pool_heaps_count(dev_key));
                let pools_mb = (active.saturating_add(retiring) as u64 * POOL_BYTES) >> 20;
                crate::xlog_warn!(
                    "D3D12.X: target-page pool cap reached ({pools_mb} MiB, {active} active + \
                     {retiring} retiring pools): {va:#x} mapped {backed}/{total} tiles (#{n})"
                );
            }
        }
        if !was_mapped || backed > prev_backed {
            self.map(res, 0, &tiles);
        }
        Some((used, used_extra))
    }

    #[allow(clippy::too_many_arguments)]
    fn bind_tiles(
        pages: &mut std::collections::HashMap<usize, Page>,
        extra_pages: &mut std::collections::HashMap<(usize, usize), Page>,
        pool: &mut TilePool,
        (va, first, own, total, shape): (usize, usize, usize, usize, TileShape),
        phys: &[Option<usize>],
        skip: &[bool],
        (now, pool_pages): (std::time::Instant, u32),
        make_heap: impl FnMut() -> Option<u32>,
    ) -> (Vec<Option<Tile>>, Vec<usize>, Vec<(usize, usize)>, usize) {
        let skipped = |i: usize| skip.get(i).copied().unwrap_or(false);
        let mut missing = 0usize;
        let mut skip_count = 0usize;
        let mut preferred: Option<u32> = None;
        for i in 0..total {
            if skipped(i) {
                skip_count += 1;
                continue;
            }
            let h = if i < own {
                let key = page_key(first + i * PAGE, phys.get(i).copied().flatten());
                pages.get(&key).map(|p| p.tile.0)
            } else {
                extra_pages.get(&(va, i)).map(|p| p.tile.0)
            };
            match h {
                Some(h) => { preferred.get_or_insert(h); }
                None => missing += 1,
            }
        }
        let fresh = pool.take_up_to(missing, preferred, now, pool_pages, make_heap);
        let mut keep: Option<Vec<bool>> = None;
        let mut skip_existing: Option<Vec<bool>> = None;
        if fresh.len() < missing {
            let (packed_groups, std_order) = packed_and_standard_layout(total, shape);
            let mut rem = fresh.len();
            let mut mask = vec![false; total];
            let mut forbid = vec![false; total];
            let is_missing = |i: usize| {
                if skipped(i) {
                    return false;
                }
                if i < own {
                    let key = page_key(first + i * PAGE, phys.get(i).copied().flatten());
                    !pages.contains_key(&key)
                } else {
                    !extra_pages.contains_key(&(va, i))
                }
            };
            for g in packed_groups {
                let need = g.clone().filter(|&i| is_missing(i)).count();
                if need <= rem {
                    for i in g {
                        if is_missing(i) {
                            mask[i] = true;
                        }
                    }
                    rem -= need;
                } else {
                    for i in g {
                        forbid[i] = true;
                    }
                }
            }
            if rem > 0 {
                for i in std_order {
                    if is_missing(i) {
                        mask[i] = true;
                        rem -= 1;
                        if rem == 0 { break; }
                    }
                }
            }
            keep = Some(mask);
            skip_existing = Some(forbid);
        }
        let mut fresh_iter = fresh.into_iter();
        let mut tiles = Vec::with_capacity(total);
        let mut used = Vec::with_capacity(own);
        let mut used_extra = Vec::new();
        for i in 0..total {
            if skipped(i) || skip_existing.as_deref().is_some_and(|s| s[i]) {
                tiles.push(None);
                continue;
            }
            let own_key = (i < own).then(|| page_key(first + i * PAGE, phys.get(i).copied().flatten()));
            let existing = if let Some(key) = own_key {
                pages.get_mut(&key)
            } else {
                extra_pages.get_mut(&(va, i))
            };
            let tile = if let Some(p) = existing {
                (p.users, p.idle_since) = (p.users + 1, None);
                if let Some(key) = own_key { used.push(key); } else { used_extra.push((va, i)); }
                Some(p.tile)
            } else {
                let t = keep.as_deref().is_none_or(|m| m[i]).then(|| fresh_iter.next()).flatten();
                if let Some(t) = t {
                    let p = Page { tile: t, users: 1, idle_since: None };
                    if let Some(key) = own_key {
                        pages.insert(key, p);
                        used.push(key);
                    } else {
                        extra_pages.insert((va, i), p);
                        used_extra.push((va, i));
                    }
                }
                t
            };
            tiles.push(tile);
        }
        let unused: Vec<Tile> = fresh_iter.collect();
        if !unused.is_empty() {
            pool.return_tiles(&unused);
        }
        let dropped = total.saturating_sub(skip_count).saturating_sub(used.len() + used_extra.len());
        (tiles, used, used_extra, dropped)
    }

    /// Minimum fresh pool tiles needed for `rebind` to back at least one additional tile on an
    /// incomplete eager target (`0` when a shared page is already resident in `pages`).
    #[allow(clippy::too_many_arguments)]
    fn min_fresh_for_target(
        &self, va: usize, console_bytes: Option<u64>, total: u32, shape: TileShape,
        used: &[usize], used_extra: &[(usize, usize)], phys: &[Option<usize>], skip: &[bool],
    ) -> Option<usize> {
        let skipped = |i: usize| skip.get(i).copied().unwrap_or(false);
        let wanted = (total as usize).saturating_sub(skip.iter().filter(|&&s| s).count());
        if used.len() + used_extra.len() >= wanted {
            return None;
        }
        let (first, own) = (total > 0)
            .then(|| owned_pages(va, console_bytes, total as usize))
            .flatten()?;
        let held = |i: usize| {
            if skipped(i) {
                return true;
            }
            if i < own {
                let key = page_key(first + i * PAGE, phys.get(i).copied().flatten());
                used.contains(&key)
            } else {
                used_extra.contains(&(va, i))
            }
        };
        let is_missing = |i: usize| {
            if skipped(i) {
                return false;
            }
            if i < own {
                let key = page_key(first + i * PAGE, phys.get(i).copied().flatten());
                !self.pages.contains_key(&key)
            } else {
                !self.extra_pages.contains_key(&(va, i))
            }
        };
        let (packed_groups, std_order) = packed_and_standard_layout(total as usize, shape);
        let std_min = std_order.into_iter()
            .filter(|&i| !held(i))
            .map(|i| usize::from(is_missing(i)))
            .min();
        let pk_min = packed_groups.into_iter()
            .filter(|g| g.clone().any(|i| !held(i)))
            .map(|g| g.filter(|&i| is_missing(i)).count())
            .min();
        match (std_min, pk_min) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (Some(a), None) => Some(a),
            (None, Some(b)) => Some(b),
            (None, None) => None,
        }
    }

    /// `bind` for the tiles `range` of the resource's `total`, which start at subresource `sub`
    /// (`XCL_TEXTURE_LEVEL_PAGES=1`), respecting `max_pools` (including retiring pools) and
    /// `oom_until` so unbacked level groups stay unmapped (`None`) and retry once pools free up.
    #[allow(clippy::too_many_arguments)]
    unsafe fn bind_range(&mut self, dev: &ID3D12Device, res: &ID3D12Resource, va: usize, console_bytes: Option<u64>, total: u32, sub: u32, range: std::ops::Range<usize>, phys: &[Option<usize>]) -> Option<(Vec<usize>, Vec<(usize, usize)>)> {
        let (first, own) = (total > 0).then(|| owned_pages(va, console_bytes, total as usize)).flatten()?;
        let now = std::time::Instant::now();
        self.sweep(now);

        let mut missing = 0usize;
        let mut preferred: Option<u32> = None;
        for i in range.clone() {
            let existing = if i < own {
                let key = page_key(first + i * PAGE, phys.get(i).copied().flatten());
                self.pages.get(&key).map(|p| p.tile.0)
            } else {
                self.extra_pages.get(&(va, i)).map(|p| p.tile.0)
            };
            match existing {
                Some(h) if preferred.is_none() => preferred = Some(h),
                Some(_) => {}
                None => missing += 1,
            }
        }

        let dev_key = dev.as_raw() as usize;
        let max_pools = self.max_pools;
        let oom_until = &mut self.oom_until;
        let pools = &mut self.pools;
        let pool = &mut self.pool;
        let fresh = pool.take_batch(missing, preferred, now, POOL_PAGES, || {
            alloc_budgeted_pool(dev, dev_key, max_pools, oom_until, pools, now)
        })?;
        let mut fresh_iter = fresh.into_iter();

        let mut tiles = Vec::with_capacity(range.len());
        let mut used = Vec::with_capacity(own.min(range.len()));
        let mut used_extra = Vec::new();
        for i in range {
            let tile = if i < own {
                let key = page_key(first + i * PAGE, phys.get(i).copied().flatten());
                let t = match self.pages.get_mut(&key) {
                    Some(p) => {
                        p.users += 1;
                        p.idle_since = None;
                        p.tile
                    }
                    None => {
                        let t = fresh_iter.next().unwrap();
                        self.pages.insert(key, Page { tile: t, users: 1, idle_since: None });
                        t
                    }
                };
                used.push(key);
                t
            } else {
                let k = (va, i);
                let t = match self.extra_pages.get_mut(&k) {
                    Some(p) => {
                        p.users += 1;
                        p.idle_since = None;
                        p.tile
                    }
                    None => {
                        let t = fresh_iter.next().unwrap();
                        self.extra_pages.insert(k, Page { tile: t, users: 1, idle_since: None });
                        t
                    }
                };
                used_extra.push(k);
                t
            };
            tiles.push(Some(tile));
        }
        self.map(res, sub, &tiles);
        Some((used, used_extra))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TileRange {
    Map(u32, u32),
    Null(u32),
    Skip(u32),
}

impl TileRange {
    fn parts(self) -> (D3D12_TILE_RANGE_FLAGS, u32, u32) {
        match self {
            Self::Map(off, n) => (D3D12_TILE_RANGE_FLAG_NONE, off, n),
            Self::Null(n) => (D3D12_TILE_RANGE_FLAG_NULL, 0, n),
            Self::Skip(n) => (D3D12_TILE_RANGE_FLAG_SKIP, 0, n),
        }
    }
}

/// The ranges of one `UpdateTileMappings` call for pool heap `heap` over `tiles`: consecutive runs
/// of `heap` (`Map`), `Null` runs for unbacked (`None`) tiles when `null_unbacked` is set (the
/// first heap pass), and `Skip` runs for other heaps' tiles (or `None` tiles on later passes).
fn heap_ranges(heap: u32, tiles: &[Option<Tile>], null_unbacked: bool) -> Vec<TileRange> {
    let mut out: Vec<TileRange> = Vec::new();
    for &entry in tiles {
        match entry {
            Some((h, t)) if h == heap => match out.last_mut() {
                Some(TileRange::Map(off, n)) if *off + *n == t => *n += 1,
                _ => out.push(TileRange::Map(t, 1)),
            },
            None if null_unbacked => match out.last_mut() {
                Some(TileRange::Null(n)) => *n += 1,
                _ => out.push(TileRange::Null(1)),
            },
            _ => match out.last_mut() {
                Some(TileRange::Skip(n)) => *n += 1,
                _ => out.push(TileRange::Skip(1)),
            },
        }
    }
    out
}

/// Like `heap_ranges`, with any trailing `Skip` range trimmed (trailing `Null` ranges stay intact
/// so unbacked tail tiles still receive `D3D12_TILE_RANGE_FLAG_NULL`).
fn heap_ranges_trimmed(heap: u32, tiles: &[Option<Tile>], null_unbacked: bool) -> Vec<TileRange> {
    let mut out = heap_ranges(heap, tiles, null_unbacked);
    while matches!(out.last(), Some(TileRange::Skip(_))) {
        out.pop();
    }
    out
}

/// 64 KiB page boundaries `P` with `va <= P < va + bytes`, capped at `total` tiles:
/// `(first_page, count)`. Because every `P` lies strictly inside `[va, va + bytes)`, two
/// non-overlapping ranges never share a page here even when `va` is only 4 KiB aligned.
fn owned_pages(va: usize, console_bytes: Option<u64>, total: usize) -> Option<(usize, usize)> {
    match console_bytes {
        Some(b) => {
            let bytes = usize::try_from(b).ok()?;
            let first = va.checked_next_multiple_of(PAGE)?;
            let end = va.saturating_add(bytes);
            let n = if first < end { (end - first).div_ceil(PAGE) } else { 0 };
            Some((first, n.min(total)))
        }
        None => (va % PAGE == 0).then_some((va, total)),
    }
}

/// A reserved texture needs TILED in the format's support (`support2`) and, multisampled, a
/// tiled quality level at its sample count.
fn can_reserve(support2: i32, samples: u32, tiled_msaa_levels: u32) -> bool {
    support2 & D3D12_FORMAT_SUPPORT2_TILED.0 != 0 && (samples <= 1 || tiled_msaa_levels > 0)
}

/// Whether the device can make `desc` a reserved texture, asked before CreateReservedResource:
/// refused, it is an error in the debug layer (#940: R32G8X24_TYPELESS depth, 1 and 4 samples).
/// A query that fails says yes, and creation decides.
unsafe fn reservable(dev: &ID3D12Device, desc: &D3D12_RESOURCE_DESC) -> bool {
    let mut f = D3D12_FEATURE_DATA_FORMAT_SUPPORT { Format: desc.Format, ..Default::default() };
    if dev.CheckFeatureSupport(D3D12_FEATURE_FORMAT_SUPPORT, &mut f as *mut _ as *mut c_void, std::mem::size_of_val(&f) as u32).is_err() {
        return true;
    }
    // a format without TILED is not reservable whatever the sample count's query says
    if !can_reserve(f.Support2.0, 1, 0) {
        return false;
    }
    let samples = desc.SampleDesc.Count;
    let mut m = D3D12_FEATURE_DATA_MULTISAMPLE_QUALITY_LEVELS { Format: desc.Format, SampleCount: samples, Flags: D3D12_MULTISAMPLE_QUALITY_LEVELS_FLAG_TILED_RESOURCE, NumQualityLevels: 0 };
    if samples > 1 && dev.CheckFeatureSupport(D3D12_FEATURE_MULTISAMPLE_QUALITY_LEVELS, &mut m as *mut _ as *mut c_void, std::mem::size_of_val(&m) as u32).is_err() {
        return true;
    }
    can_reserve(f.Support2.0, samples, m.NumQualityLevels)
}

/// Canonical physical-backing address for `va`: when `va` lies inside an unmapped GPU mirror view
/// of title CPU memory (`kernelx::mirrors::source`), map both views to the same backing address so
/// shared 64 KiB VRAM pages and arenas alias instead of duplicating video memory. If distinct
/// physical or committed pages are mapped directly at `va`, keep `va` unaliased (matching
/// `title_textures::mirror_source`).
fn canonical_va(va: usize) -> usize {
    let Some(src) = crate::hle::kernelx::mirrors::source(va) else { return va };
    if crate::hle::kernelx::physical_page_at(va) {
        return va;
    }
    use windows_sys::Win32::System::Memory::{VirtualQuery, MEMORY_BASIC_INFORMATION, MEM_COMMIT};
    let mut m: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
    let mapped = unsafe { VirtualQuery(va as *const c_void, &mut m, std::mem::size_of_val(&m)) } != 0 && m.State == MEM_COMMIT;
    if mapped { va } else { src }
}

/// Keys of `Pages::pages` for the title's physical pages (title addresses never have bit 63).
const PHYS_KEY: usize = 1 << 63;

/// The key of the page of video memory for title page address `a`: the physical page mapped
/// there (`phys`), else the address. A retail game maps one physical page under render targets
/// at two addresses (aliasing, as on the console): keyed by address, each got video memory of
/// its own (1.85 GB more in its first 40 s) and what the GPU wrote through one never showed in
/// the other.
fn page_key(a: usize, phys: Option<usize>) -> usize {
    phys.map_or(a, |p| PHYS_KEY | p)
}

/// The physical pages under a GPU-owned texture's owned pages (`require_aligned`), asked before
/// `PAGES` is taken; empty for others (keyed by address) and without the console size.
fn physical_pages_under(va: usize, console_bytes: Option<u64>, require_aligned: bool) -> Vec<Option<usize>> {
    if !require_aligned {
        return Vec::new();
    }
    match console_bytes.and_then(|b| owned_pages(va, Some(b), usize::MAX)) {
        Some((first, n)) if n > 0 => crate::hle::kernelx::physical_pages(first, n),
        _ => Vec::new(),
    }
}

/// A texture at `va` as a reserved resource on the title's shared 64 KiB pages
/// (`console_bytes`: its size on the console; tiles past the owned page boundaries are shared by
/// `(va, tile_index)` so repeated placements at the same address do not leak). None: it cannot be.
#[allow(clippy::too_many_arguments)]
unsafe fn place_on_pages(
    dev: &ID3D12Device,
    this: *mut c_void,
    va: usize,
    desc: &D3D12_RESOURCE_DESC,
    state: u32,
    clear: *const c_void,
    console_bytes: Option<u64>,
    require_aligned: bool,
    lazy: bool,
    riid: *const windows_sys::core::GUID,
    ppv: *mut *mut c_void,
) -> Option<HRESULT> {
    let va = canonical_va(va);
    if desc.Layout != D3D12_TEXTURE_LAYOUT_UNKNOWN || desc.Dimension == D3D12_RESOURCE_DIMENSION_TEXTURE1D {
        return None;
    }
    if (require_aligned || console_bytes.is_none()) && va % PAGE != 0 {
        return None;
    }
    let phys = physical_pages_under(va, console_bytes, require_aligned);
    // reserved textures have the PC's 64 KiB tiles
    let tiled = D3D12_RESOURCE_DESC { Layout: D3D12_TEXTURE_LAYOUT_64KB_UNDEFINED_SWIZZLE, ..*desc };
    static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let log = |why: &str| {
        if require_aligned && N.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 4 {
            crate::xlog_info!("D3D12.X: GPU-owned texture at {va:#x} ({}x{}, format {}, {} samples) not reserved ({why}): in an arena", desc.Width, desc.Height, desc.Format.0, desc.SampleDesc.Count);
        }
    };
    let dev_key = dev.as_raw() as usize;
    let is_reservable = {
        let mut g = PAGES.lock().unwrap();
        let pages = g.entry(dev_key).or_insert_with(|| Pages::new(dev)).as_mut()?;
        *pages.reservable_cache.entry((tiled.Format.0, tiled.SampleDesc.Count)).or_insert_with(|| reservable(dev, &tiled))
    };
    if !is_reservable {
        log("no tiled support for its format or sample count");
        return None;
    }
    // CreateReservedResource and GetResourceTiling do not touch `PAGES`; keep them outside the
    // lock so concurrent streaming threads, `before_execute`, and `page_bytes` do not serialize.
    let hr = super::real::dev_create_reserved_resource(this, &tiled as *const _ as usize, state as usize, clear as usize, riid, ppv);
    if hr < 0 || (*ppv).is_null() {
        log(&format!("{hr:#x}"));
        return None;
    }
    let raw = *ppv;
    let res = ID3D12Resource::from_raw_borrowed(&raw)?;
    let mips = if desc.MipLevels == 0 { res.GetDesc().MipLevels } else { desc.MipLevels };
    let mut total = 0u32;
    let mut packed = D3D12_PACKED_MIP_INFO::default();
    let levels = if lazy {
        let (levels, n, pk) = level_tiling(dev, res, mips as u32);
        (total, packed) = (n, pk);
        levels.filter(|_| total > 0 && owned_pages(va, console_bytes, total as usize).is_some())
    } else {
        let none = std::ptr::null_mut();
        dev.GetResourceTiling(res, Some(&mut total), Some(&mut packed), None, None, 0, none);
        None
    };
    let array_slices = if desc.Dimension == D3D12_RESOURCE_DIMENSION_TEXTURE3D {
        1
    } else {
        desc.DepthOrArraySize.max(1)
    };
    let shape = TileShape::from_tiling(mips, array_slices, total, &packed);
    // sampled textures only: what the GPU renders to keeps all of its memory
    let (skip_mips, skip) = if require_aligned { (0, Vec::new()) } else { top_mip_tiles(dev, res, &tiled, total) };
    let skip_pages = if skip_mips > 0 { skipped_pages(va, console_bytes, total, &skip) } else { Vec::new() };
    let bound = {
        let mut g = PAGES.lock().unwrap();
        if let Some(Some(pages)) = g.get_mut(&dev_key) {
            let pair = if levels.is_some() {
                // nothing mapped yet: a level gets its pages with its first texels (`map_levels`)
                LEVEL_STATS[0].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                LEVEL_STATS[1].fetch_add(total as u64, std::sync::atomic::Ordering::Relaxed);
                Some((Vec::new(), Vec::new()))
            } else {
                pages.bind(dev, res, va, console_bytes, total, shape, &phys, &skip)
            };
            pair.map(|(used, used_extra)| {
                let target = Target {
                    va, console_bytes, total, shape, mapped: true, used, used_extra, levels,
                    gpu_owned: require_aligned, skip_mips, skip,
                };
                pages.targets.insert(raw as usize, target);
            }).is_some()
        } else {
            false
        }
    };
    if !bound {
        drop(windows::core::IUnknown::from_raw(raw));
        *ppv = std::ptr::null_mut();
        return None;
    }
    PLACED_TEXTURES.lock().unwrap().get_or_insert_with(HashSet::new).insert(raw as usize);
    if skip_mips > 0 {
        note_skipped_mips(raw as usize, skip_mips, skip_pages);
    }
    Some(hr)
}

/// Unmap a placed texture superseded by a placement with a different address range, returning its
/// unshared tiles (`extra_pages` and unshared tail pages) to the cooling pool immediately.
pub(super) unsafe fn unmap_superseded(res: &ID3D12Resource) {
    let key = res.as_raw() as usize;
    let mut g = PAGES.lock().unwrap();
    let now = std::time::Instant::now();
    for pages in g.values_mut().flatten() {
        let Some(t) = pages.targets.get_mut(&key) else { continue };
        if !t.mapped {
            return;
        }
        let total = t.total;
        let used = std::mem::take(&mut t.used);
        let used_extra = std::mem::take(&mut t.used_extra);
        t.mapped = false;
        if let Some(l) = t.levels.as_mut() {
            LEVEL_STATS[2].fetch_sub(l.mapped_tiles(), std::sync::atomic::Ordering::Relaxed);
            l.groups.iter_mut().for_each(|g| g.3 = false);
        }
        pages.unmap(res, total);
        pages.release_tiles(used, used_extra, now, true);
        return;
    }
}

/// Attempt to backfill missing tiles on an unmapped or partially backed eager `Pages` target.
/// Returns `(registered_in_pages, gained_tiles, min_fresh_tiles_still_needed)`.
pub(super) unsafe fn backfill_incomplete_state(
    res: &ID3D12Resource,
) -> (bool, bool, Option<usize>) {
    let key = res.as_raw() as usize;
    let (va, console_bytes, gpu_owned, complete, has_levels) = {
        let g = PAGES.lock().unwrap();
        let Some((_, Some(pages))) =
            g.iter().find(|(_, p)| p.as_ref().is_some_and(|p| p.targets.contains_key(&key)))
        else {
            return (false, false, None);
        };
        let t = &pages.targets[&key];
        (t.va, t.console_bytes, t.gpu_owned, t.is_complete(), t.levels.is_some())
    };
    if complete {
        return (true, false, None);
    }
    // (the physical pages are asked without `PAGES` held)
    let phys = if has_levels { Vec::new() } else { physical_pages_under(va, console_bytes, gpu_owned) };
    let mut g = PAGES.lock().unwrap();
    let Some((&dev_key, Some(pages))) =
        g.iter_mut().find(|(_, p)| p.as_ref().is_some_and(|p| p.targets.contains_key(&key)))
    else {
        return (false, false, None);
    };
    let (va, console_bytes, total, shape, was_mapped, complete, has_levels, prev_used, prev_extra, skip) =
    {
        let t = &pages.targets[&key];
        (
            t.va, t.console_bytes, t.total, t.shape, t.mapped, t.is_complete(),
            t.levels.is_some(), t.used.clone(), t.used_extra.clone(), t.skip.clone(),
        )
    };
    if complete {
        return (true, false, None);
    }
    if has_levels {
        if let Some(t) = pages.targets.get_mut(&key) {
            t.mapped = true;
        }
        return (true, false, None);
    }
    let d = dev_key as *mut c_void;
    let Some(dev) = ID3D12Device::from_raw_borrowed(&d) else { return (false, false, None) };
    let prev_backed = prev_used.len() + prev_extra.len();
    let Some((used, used_extra)) = pages.rebind(
        dev, res, va, console_bytes, total, shape, was_mapped, &prev_used, &prev_extra, &phys, &skip,
    ) else {
        return (true, false, None);
    };
    let backed = used.len() + used_extra.len();
    let gained = !was_mapped || backed > prev_backed;
    let still_needed = pages.min_fresh_for_target(
        va, console_bytes, total, shape, &used, &used_extra, &phys, &skip,
    );
    if let Some(t) = pages.targets.get_mut(&key) {
        (t.used, t.used_extra, t.mapped) = (used, used_extra, true);
    }
    (true, gained, still_needed)
}

/// Re-map a previously unmapped (or partially backed at cap) placed texture once pages free up.
pub(super) unsafe fn remap_if_unmapped(res: &ID3D12Resource) -> bool {
    let (found, _, still_needed) = backfill_incomplete_state(res);
    found && still_needed.is_none()
}

/// Free/cooled pool tiles plus any headroom below `max_pools` on `dev_key` (`title_textures`).
pub(super) unsafe fn available_tiles(dev_key: usize) -> usize {
    let now = std::time::Instant::now();
    let mut g = PAGES.lock().unwrap();
    let Some(Some(pages)) = g.get_mut(&dev_key) else { return 0 };
    pages.sweep(now);
    pages.pool.drain_cooling(now);
    let free = pages.pool.free_total;
    if pages.pools.len() >= pages.max_pools {
        return free;
    }
    let (retiring, freed_pool) = poll_retiring_pool_heaps(dev_key);
    if freed_pool {
        pages.oom_until = None;
    }
    if pages.oom_until.is_some_and(|t| now < t) {
        return free;
    }
    let used_pools = pages.pools.len().saturating_add(retiring);
    let spare_pools = pages.max_pools.saturating_sub(used_pools);
    free.saturating_add(spare_pools.saturating_mul(POOL_PAGES as usize))
}

/// A resource released: its pages are no longer its.
fn forget_pages(resource: usize) {
    forget_skipped_mips(resource);
    let mut g = PAGES.lock().unwrap();
    let now = std::time::Instant::now();
    for pages in g.values_mut().flatten() {
        let Some(t) = pages.targets.remove(&resource) else { continue };
        if let Some(l) = &t.levels {
            LEVEL_STATS[0].fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
            LEVEL_STATS[1].fetch_sub(t.total as u64, std::sync::atomic::Ordering::Relaxed);
            LEVEL_STATS[2].fetch_sub(l.mapped_tiles(), std::sync::atomic::Ordering::Relaxed);
        }
        if t.mapped {
            pages.release_tiles(t.used, t.used_extra, now, false);
        }
        return;
    }
}

/// Reclaimed heaps waiting for `unmap_mark` (if any) and every live queue's fence mark on one device.
struct RetiringBatch<H, F> {
    heaps: Vec<H>, pool_heaps: usize, unmap_mark: Option<(F, u64)>, marks: Vec<(F, u64)>,
}

/// Per-device multi-queue retirement state (`H` = heap, `F` = fence; pure-CPU types in tests).
struct RetireQueue<H, F> {
    fences: std::collections::HashMap<usize, (F, u64)>,
    batches: Vec<RetiringBatch<H, F>>,
    retiring_pools: usize,
    pending_pools: usize,
}

impl<H, F: Clone> RetireQueue<H, F> {
    fn new() -> Self {
        Self {
            fences: std::collections::HashMap::new(),
            batches: Vec::new(),
            retiring_pools: 0,
            pending_pools: 0,
        }
    }

    /// Seal `heaps` behind `unmap_mark` and one fence signal per queue (signaled BEFORE queueing).
    /// Returns `heaps` immediately when zero marks were signaled and `unmap_mark` is `None`.
    fn retire_with<Q>(&mut self, heaps: Vec<H>, unmap_mark: Option<(F, u64)>, queues: &[(usize, Q)], mut make_fence: impl FnMut() -> Option<F>, mut signal: impl FnMut(&Q, &F, u64) -> bool) -> Vec<H> {
        if heaps.is_empty() { return Vec::new(); }
        let mut marks = Vec::with_capacity(queues.len());
        for (k, q) in queues {
            let (f, v) = match self.fences.entry(*k) {
                std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
                std::collections::hash_map::Entry::Vacant(e) => { let Some(f) = make_fence() else { continue }; e.insert((f, 0)) }
            };
            if signal(q, f, *v + 1) { *v += 1; marks.push((f.clone(), *v)); }
        }
        if marks.is_empty() && unmap_mark.is_none() { return heaps; }
        self.batches.push(RetiringBatch { heaps, pool_heaps: 0, unmap_mark, marks });
        Vec::new()
    }

    /// Reap completed batches and prune dead-queue fences once completed; caller drops outside locks.
    fn sweep_with(&mut self, live_queues: &HashSet<usize>, mut completed: impl FnMut(&F) -> u64) -> (Vec<RetiringBatch<H, F>>, Vec<F>) {
        let done = self.reap_completed(&mut completed);
        let stale: Vec<usize> = self.fences.iter().filter(|(k, (f, v))| !live_queues.contains(k) && completed(f) >= *v).map(|(&k, _)| k).collect();
        let pruned = stale.into_iter().filter_map(|k| self.fences.remove(&k).map(|(f, _)| f)).collect();
        (done, pruned)
    }

    fn counted_pools(&self) -> usize { self.retiring_pools.saturating_add(self.pending_pools) }

    fn reserve_pending_pools(&mut self, pool_heaps: usize) {
        self.pending_pools = self.pending_pools.saturating_add(pool_heaps);
    }

    fn note_queued_pools(&mut self, pool_heaps: usize) {
        if let Some(b) = self.batches.last_mut().filter(|_| pool_heaps > 0) {
            debug_assert!(b.pool_heaps + pool_heaps <= b.heaps.len());
            b.pool_heaps += pool_heaps;
            self.retiring_pools += pool_heaps;
        }
    }

    fn reap_completed(&mut self, mut completed: impl FnMut(&F) -> u64) -> Vec<RetiringBatch<H, F>> {
        let (mut done, mut i) = (Vec::new(), 0);
        while i < self.batches.len() {
            let b = &self.batches[i];
            if b.unmap_mark.as_ref().is_none_or(|(f, v)| completed(f) >= *v)
                && b.marks.iter().all(|(f, v)| completed(f) >= *v)
            {
                let b = self.batches.remove(i);
                self.retiring_pools = self.retiring_pools.saturating_sub(b.pool_heaps);
                done.push(b);
            } else {
                i += 1;
            }
        }
        done
    }
}

unsafe impl Send for RetireQueue<ID3D12Heap, ID3D12Fence> {}

static RETIRE: Mutex<Option<std::collections::HashMap<usize, RetireQueue<ID3D12Heap, ID3D12Fence>>>> = Mutex::new(None);
static RETIRING_BATCHES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
#[cfg(test)]
type ReclaimGap = (usize, std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>);
#[cfg(test)]
static RECLAIM_GAP: Mutex<Option<ReclaimGap>> = Mutex::new(None);

fn retiring_pool_heaps_count(dev_key: usize) -> usize {
    let g = RETIRE.lock().unwrap();
    g.as_ref().and_then(|m| m.get(&dev_key)).map_or(0, RetireQueue::counted_pools)
}

unsafe fn poll_retiring_pool_heaps(dev_key: usize) -> (usize, bool) {
    let (done, remaining) = {
        let mut guard = RETIRE.lock().unwrap();
        let Some(rq) = guard.as_mut().and_then(|m| m.get_mut(&dev_key)) else { return (0, false) };
        let before = rq.batches.len();
        let done = rq.reap_completed(|f| f.GetCompletedValue());
        let delta = before - rq.batches.len();
        if delta > 0 { RETIRING_BATCHES.fetch_sub(delta, std::sync::atomic::Ordering::AcqRel); }
        (done, rq.counted_pools())
    };
    let freed_pool = done.iter().any(|b| !b.heaps.is_empty() && b.pool_heaps > 0);
    drop(done);
    (remaining, freed_pool)
}

/// Collect all known live command queues belonging to `dev_key` (deduplicated by raw ptr).
unsafe fn collect_device_queues(dev_key: usize, extra: Option<&ID3D12CommandQueue>) -> Vec<(usize, ID3D12CommandQueue)> {
    let (mut seen, mut out) = (HashSet::new(), Vec::new());
    let mut add = |q: ID3D12CommandQueue| {
        let key = q.as_raw() as usize;
        let mut q_dev: Option<ID3D12Device> = None;
        if seen.insert(key) && q.GetDevice(&mut q_dev).is_ok() && q_dev.is_some_and(|d| d.as_raw() as usize == dev_key) {
            out.push((key, q));
        }
    };
    for raw in super::present::known_queues() {
        let raw = raw as *mut c_void;
        if let Some(q) = ID3D12CommandQueue::from_raw_borrowed(&raw) { add(q.clone()); }
    }
    for q in extra.into_iter().cloned().chain(super::title_textures::upload_queues()).chain(super::sparse::queue()) {
        add(q);
    }
    out
}

/// Retire `newly_reclaimed` (`pool_heaps` of which are 64 MiB `Pages` pool heaps) across all live
/// queues on `dev_key`, sweep completed batches, and prune dead-queue fences; drops every finished
/// `ID3D12Heap` and `ID3D12Fence` outside all locks.
unsafe fn retire_heaps_and_sweep(
    dev: &ID3D12Device,
    dev_key: usize,
    newly_reclaimed: Vec<ID3D12Heap>,
    pool_heaps: usize,
    unmap_mark: Option<(ID3D12Fence, u64)>,
    extra_queue: Option<&ID3D12CommandQueue>,
) {
    let queues = collect_device_queues(dev_key, extra_queue);
    let live_keys: HashSet<usize> = queues.iter().map(|&(k, _)| k).collect();
    let _drop_outside_locks = {
        let mut guard = RETIRE.lock().unwrap();
        let map = guard.get_or_insert_with(std::collections::HashMap::new);
        if newly_reclaimed.is_empty() && !map.contains_key(&dev_key) { return; }
        let rq = map.entry(dev_key).or_insert_with(RetireQueue::new);
        let before = rq.batches.len();
        let immediate = rq.retire_with(newly_reclaimed, unmap_mark, &queues, || dev.CreateFence::<ID3D12Fence>(0, D3D12_FENCE_FLAG_NONE).ok(), |q, f, v| q.Signal(f, v).is_ok());
        debug_assert!(rq.pending_pools >= pool_heaps);
        rq.pending_pools = rq.pending_pools.saturating_sub(pool_heaps);
        if immediate.is_empty() {
            rq.note_queued_pools(pool_heaps);
        }
        let (done, pruned) = rq.sweep_with(&live_keys, |f| f.GetCompletedValue());
        RETIRING_BATCHES.fetch_add(rq.batches.len().wrapping_sub(before), std::sync::atomic::Ordering::AcqRel);
        (immediate, done, pruned)
    };
}

// ---------------------------------------------------------------------------------------
// Sampled textures without their top mips while video memory fills up
//
// A title streams more textures than a PC card's video memory holds once the console's copy
// of its memory is in video memory too (#1773): past the card's budget the driver pages, or
// removes the device. So when the device's local video memory is filling up, a new sampled
// texture gets no video memory for its largest mips: their tiles stay unmapped (they read as
// zero), and every view of the texture is clamped to the first mip that has memory
// (`clamp_skipped_mips`), so sampling never reaches them. The texture keeps its shape, its
// mip numbering and the title's uploads; it is only less sharp up close.
//
// `XCL_TEXTURE_BUDGET=0` turns it off; `XCL_TEXTURE_MAX=<texels>` caps the largest side of
// every new sampled texture at that whatever the fill (tests, measurements).
// ---------------------------------------------------------------------------------------

/// The largest side a new sampled texture keeps when the device's local video memory is
/// `fill` (usage / budget) full; None: all of it.
fn cap_for_fill(fill: f64) -> Option<u64> {
    if fill < 0.75 {
        None
    } else if fill < 0.85 {
        Some(2048)
    } else if fill < 0.92 {
        Some(1024)
    } else {
        Some(512)
    }
}

/// How many top mips of a texture whose largest side is `largest` to leave out for none of its
/// sides to pass `cap`, keeping at least one mip that has tiles of its own (`standard_mips`:
/// the mips before the packed tail).
fn mips_to_skip(largest: u64, standard_mips: u32, cap: u64) -> u32 {
    let mut skip = 0;
    while largest >> skip > cap && skip + 1 < standard_mips {
        skip += 1;
    }
    skip
}

/// The cap that applies now (None: textures keep all their mips).
unsafe fn texture_cap(dev: &ID3D12Device) -> Option<u64> {
    static SET: std::sync::OnceLock<(bool, Option<u64>)> = std::sync::OnceLock::new();
    let (on, max) = *SET.get_or_init(|| {
        let on = std::env::var("XCL_TEXTURE_BUDGET").map_or(true, |v| v.trim() != "0");
        let max = std::env::var("XCL_TEXTURE_MAX").ok().and_then(|v| v.trim().parse::<u64>().ok()).filter(|&m| m > 0);
        (on, max)
    });
    if !on {
        return None;
    }
    if max.is_some() {
        return max;
    }
    cap_for_fill(local_fill(dev)?)
}

/// The device's local video memory in use / its budget, asked at most every 100 ms.
unsafe fn local_fill(dev: &ID3D12Device) -> Option<f64> {
    use windows::Win32::Graphics::Dxgi::*;
    struct Cached {
        luid: (u32, i32),
        adapter: IDXGIAdapter3,
        at: std::time::Instant,
        fill: f64,
    }
    // DXGI adapters are free-threaded
    unsafe impl Send for Cached {}
    static CACHE: Mutex<Option<Cached>> = Mutex::new(None);
    let luid = dev.GetAdapterLuid();
    let luid = (luid.LowPart, luid.HighPart);
    let mut g = CACHE.lock().unwrap();
    if g.as_ref().is_none_or(|c| c.luid != luid) {
        let factory: IDXGIFactory4 = CreateDXGIFactory1().ok()?;
        let adapter: IDXGIAdapter3 = factory.EnumAdapterByLuid(dev.GetAdapterLuid()).ok()?;
        *g = Some(Cached { luid, adapter, at: std::time::Instant::now() - std::time::Duration::from_secs(1), fill: 0.0 });
    }
    let c = g.as_mut()?;
    if c.at.elapsed() >= std::time::Duration::from_millis(100) {
        let mut info = DXGI_QUERY_VIDEO_MEMORY_INFO::default();
        c.adapter.QueryVideoMemoryInfo(0, DXGI_MEMORY_SEGMENT_GROUP_LOCAL, &mut info).ok()?;
        c.fill = info.CurrentUsage as f64 / info.Budget.max(1) as f64;
        c.at = std::time::Instant::now();
    }
    Some(c.fill)
}

/// For a new sampled texture `res` (`desc`, `total` tiles): how many top mips get no video
/// memory now, and which of its tiles those are (empty: none).
unsafe fn top_mip_tiles(dev: &ID3D12Device, res: &ID3D12Resource, desc: &D3D12_RESOURCE_DESC, total: u32) -> (u32, Vec<bool>) {
    if desc.Dimension != D3D12_RESOURCE_DIMENSION_TEXTURE2D || desc.MipLevels < 2 || desc.SampleDesc.Count > 1 {
        return (0, Vec::new());
    }
    let Some(cap) = texture_cap(dev) else { return (0, Vec::new()) };
    let (standard, tilings) = mip_tilings(dev, res, desc);
    let skip = mips_to_skip(desc.Width.max(u64::from(desc.Height)), standard, cap);
    if skip == 0 {
        return (0, Vec::new());
    }
    (skip, top_mip_mask(desc, &tilings, total, skip))
}

/// The mips of `res` with tiles of their own (before the packed tail), and every
/// subresource's tiling.
unsafe fn mip_tilings(dev: &ID3D12Device, res: &ID3D12Resource, desc: &D3D12_RESOURCE_DESC) -> (u32, Vec<D3D12_SUBRESOURCE_TILING>) {
    let mips = u32::from(desc.MipLevels);
    let mut count = mips * u32::from(desc.DepthOrArraySize.max(1));
    let mut packed = D3D12_PACKED_MIP_INFO::default();
    let mut tilings = vec![D3D12_SUBRESOURCE_TILING::default(); count as usize];
    dev.GetResourceTiling(res, None, Some(&mut packed), None, Some(&mut count), 0, tilings.as_mut_ptr());
    (u32::from(packed.NumStandardMips).min(mips), tilings)
}

/// The tiles of the `skip` top mips of every slice, of `total`.
fn top_mip_mask(desc: &D3D12_RESOURCE_DESC, tilings: &[D3D12_SUBRESOURCE_TILING], total: u32, skip: u32) -> Vec<bool> {
    let mips = u32::from(desc.MipLevels);
    let slices = u32::from(desc.DepthOrArraySize.max(1));
    let mut mask = vec![false; total as usize];
    for slice in 0..slices {
        for mip in 0..skip {
            let Some(t) = tilings.get((slice * mips + mip) as usize) else { continue };
            if t.StartTileIndexInOverallResource == u32::MAX {
                continue;
            }
            let n = u32::from(t.WidthInTiles) * u32::from(t.HeightInTiles) * u32::from(t.DepthInTiles);
            for i in t.StartTileIndexInOverallResource..t.StartTileIndexInOverallResource.saturating_add(n) {
                if let Some(m) = mask.get_mut(i as usize) {
                    *m = true;
                }
            }
        }
    }
    mask
}

/// A page a left-out tile would have had: (title page address, `usize::MAX`) for one of the
/// texture's own pages, (its address, tile index) for an extra tile – as `Pages::bind` keys them.
type SkippedPage = (usize, usize);

/// resource → (top mips it has no video memory for, the pages of their tiles)
type SkippedMips = std::collections::HashMap<usize, (u32, Vec<SkippedPage>)>;
static SKIPPED_MIPS: Mutex<Option<SkippedMips>> = Mutex::new(None);
/// whether any texture has (the views' fast path)
static ANY_SKIPPED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The pages of the tiles `skip` marks of a texture at `va` (`console_bytes`, `total` tiles).
fn skipped_pages(va: usize, console_bytes: Option<u64>, total: u32, skip: &[bool]) -> Vec<SkippedPage> {
    let Some((first, own)) = owned_pages(va, console_bytes, total as usize) else { return Vec::new() };
    skip.iter().enumerate().filter(|&(_, &s)| s).map(|(i, _)| if i < own { (first + i * PAGE, usize::MAX) } else { (va, i) }).collect()
}

fn note_skipped_mips(resource: usize, mips: u32, pages: Vec<SkippedPage>) {
    let first = {
        let mut g = SKIPPED_MIPS.lock().unwrap();
        let m = g.get_or_insert_with(std::collections::HashMap::new);
        m.insert(resource, (mips, pages));
        !ANY_SKIPPED.swap(true, std::sync::atomic::Ordering::AcqRel)
    };
    if first {
        crate::xlog_info!("D3D12.X: video memory is filling up: new sampled textures go without their largest mips (XCL_TEXTURE_BUDGET=0 keeps them)");
    }
}

fn forget_skipped_mips(resource: usize) {
    if ANY_SKIPPED.load(std::sync::atomic::Ordering::Acquire) {
        if let Some(m) = SKIPPED_MIPS.lock().unwrap().as_mut() {
            m.remove(&resource);
        }
    }
}

/// How many top mips `resource` has no video memory for (0: all of them).
pub(super) fn skipped_mips(resource: usize) -> u32 {
    if !ANY_SKIPPED.load(std::sync::atomic::Ordering::Acquire) {
        return 0;
    }
    SKIPPED_MIPS.lock().unwrap().as_ref().and_then(|m| m.get(&resource).map(|e| e.0)).unwrap_or(0)
}

/// (textures alive now without their top mips, bytes of the pages they left out that no texture
/// has now) – `XCL_ACTIVITY`. A page textures at one address share counts once, and not at all
/// while another texture there has it.
pub fn skipped_textures() -> (u64, u64) {
    let (textures, wanted) = {
        let g = SKIPPED_MIPS.lock().unwrap();
        let Some(m) = g.as_ref() else { return (0, 0) };
        let wanted: HashSet<SkippedPage> = m.values().flat_map(|e| e.1.iter().copied()).collect();
        (m.len() as u64, wanted)
    };
    let g = PAGES.lock().unwrap();
    let held = |&(a, i): &SkippedPage| g.values().flatten().any(|p| if i == usize::MAX { p.pages.contains_key(&a) } else { p.extra_pages.contains_key(&(a, i)) });
    (textures, (wanted.iter().filter(|k| !held(k)).count() * PAGE) as u64)
}

/// A texture view of a resource without its `mips` top mips: sampling clamped to the first mip
/// it has (`ResourceMinLODClamp` counts from the resource's first mip, not the view's).
pub(super) fn clamp_skipped_mips(v: &mut D3D12_SHADER_RESOURCE_VIEW_DESC, mips: u32) {
    let lod = mips as f32;
    // SAFETY: the union member the view dimension names
    unsafe {
        let a = &mut v.Anonymous;
        let clamp = match v.ViewDimension {
            D3D12_SRV_DIMENSION_TEXTURE2D => &mut a.Texture2D.ResourceMinLODClamp,
            D3D12_SRV_DIMENSION_TEXTURE2DARRAY => &mut a.Texture2DArray.ResourceMinLODClamp,
            D3D12_SRV_DIMENSION_TEXTURECUBE => &mut a.TextureCube.ResourceMinLODClamp,
            D3D12_SRV_DIMENSION_TEXTURECUBEARRAY => &mut a.TextureCubeArray.ResourceMinLODClamp,
            _ => return,
        };
        *clamp = clamp.max(lod);
    }
}

/// Before GPU work on `queue`: have it wait for every tile mapping made so far on its device,
/// release any previously retired heaps whose GPU fences have completed, and periodically (once
/// per second per device) retire completely empty 64 MiB VRAM pool heaps and idle arenas behind
/// per-queue GPU fence signals across all live queues on the device.
pub(super) unsafe fn before_execute(queue: &ID3D12CommandQueue) {
    let mut dev: Option<ID3D12Device> = None;
    if queue.GetDevice(&mut dev).is_err() {
        return;
    }
    let Some(dev) = dev else { return };
    let dev_key = dev.as_raw() as usize;
    let now = std::time::Instant::now();
    let mut newly_reclaimed = Vec::new();
    let mut do_reclaim = false;
    let mut unmap_mark: Option<(ID3D12Fence, u64)> = None;
    {
        let mut g = PAGES.lock().unwrap();
        if let Some(Some(pages)) = g.get_mut(&dev_key) {
            let key = queue.as_raw() as usize;
            if pages.waited.get(&key).copied().unwrap_or(0) < pages.value {
                let _ = queue.Wait(&pages.fence, pages.value);
                super::list_x::note_internal_wait(key, "texture page mappings", &pages.fence, pages.value);
                pages.waited.insert(key, pages.value);
            }
            if now.duration_since(pages.last_reclaim) >= std::time::Duration::from_secs(1) {
                pages.last_reclaim = now;
                do_reclaim = true;
                newly_reclaimed = pages.sweep_and_reclaim(now);
                if !newly_reclaimed.is_empty() {
                    let mut retire = RETIRE.lock().unwrap();
                    let rq = retire.get_or_insert_with(std::collections::HashMap::new)
                        .entry(dev_key).or_insert_with(RetireQueue::new);
                    rq.reserve_pending_pools(newly_reclaimed.len());
                    unmap_mark = Some((pages.fence.clone(), pages.value));
                }
            }
        }
    }
    let reclaimed_pools = newly_reclaimed.len();
    #[cfg(test)]
    if reclaimed_pools > 0 {
        let gate = {
            let mut hook = RECLAIM_GAP.lock().unwrap();
            if hook.as_ref().is_some_and(|(k, _, _)| *k == dev_key) { hook.take() } else { None }
        };
        if let Some((_, reached, resume)) = gate {
            if reached.send(()).is_ok() {
                resume.recv_timeout(std::time::Duration::from_secs(5))
                    .expect("reclaim test hook was not resumed");
            }
        }
    }
    if do_reclaim {
        newly_reclaimed.extend(ARENAS.lock().unwrap().sweep_dropped_for(dev_key, now));
    }
    if do_reclaim || !newly_reclaimed.is_empty() || RETIRING_BATCHES.load(std::sync::atomic::Ordering::Acquire) > 0 {
        retire_heaps_and_sweep(
            &dev, dev_key, newly_reclaimed, reclaimed_pools, unmap_mark, Some(queue),
        );
    }
}

#[cfg(test)]
pub(super) static VRAM_TEST_LOCK: Mutex<()> = Mutex::new(());

/// Tile identities of one mapped resource: title pages, then extra pages.
/// A device-wide byte count also falls when unrelated idle pages are swept.
#[cfg(test)]
pub(super) fn mapped_tiles_for(device: usize, resource: usize) -> Option<(Vec<(u32, u32)>, Vec<(u32, u32)>)> {
    let g = PAGES.lock().unwrap();
    let p = g.get(&device)?.as_ref()?;
    let target = p.targets.get(&resource)?;
    if !target.mapped {
        return None;
    }
    let used = target.used.iter().map(|key| p.pages.get(key).map(|page| page.tile)).collect::<Option<Vec<_>>>()?;
    let extra = target.used_extra.iter().map(|key| p.extra_pages.get(key).map(|page| page.tile)).collect::<Option<Vec<_>>>()?;
    (used.len() + extra.len() == target.total as usize).then_some((used, extra))
}

#[cfg(test)]
pub(super) fn page_bytes_for(device: usize) -> (usize, usize) {
    let g = PAGES.lock().unwrap();
    let Some(Some(p)) = g.get(&device) else { return (0, 0) };
    ((p.pages.len() + p.extra_pages.len()) * PAGE, p.pools.len() * POOL_PAGES as usize * PAGE)
}

/// Pool/tile identities used by one mapped resource: title pages, then extra pages.
/// Unlike device-wide byte counts, these cannot shrink when unrelated idle targets expire.
#[cfg(test)]
pub(super) fn tile_mappings_for(device: usize, resource: usize) -> Option<(Vec<(u32, u32)>, Vec<(u32, u32)>)> {
    let g = PAGES.lock().unwrap();
    let p = g.get(&device)?.as_ref()?;
    let target = p.targets.get(&resource)?;
    if !target.mapped { return None; }
    let used = target.used.iter().map(|key| p.pages.get(key).map(|page| page.tile)).collect::<Option<Vec<_>>>()?;
    let extra = target.used_extra.iter().map(|key| p.extra_pages.get(key).map(|page| page.tile)).collect::<Option<Vec<_>>>()?;
    (used.len() + extra.len() == target.total as usize).then_some((used, extra))
}

/// Pool tiles of `device` that are neither free, cooling, nor any page's (taken and lost).
#[cfg(test)]
pub(super) fn tiles_unaccounted_for(device: usize) -> usize {
    let g = PAGES.lock().unwrap();
    let Some(Some(p)) = g.get(&device) else { return 0 };
    (p.pools.len() * POOL_PAGES as usize).saturating_sub(p.pool.free_total + p.pool.cooling.len() + p.pages.len() + p.extra_pages.len())
}

/// (bytes of pages in use by tiled textures, bytes of active+retiring pool heaps) - `XCL_ACTIVITY`
pub fn page_bytes() -> (usize, usize) {
    let g = PAGES.lock().unwrap();
    g.iter().fold((0, 0), |(used, pools), (&dev_key, p)| {
        let Some(p) = p.as_ref() else { return (used, pools) };
        let resident = p.pools.len().saturating_add(retiring_pool_heaps_count(dev_key));
        (
            used + (p.pages.len() + p.extra_pages.len()) * PAGE,
            pools + resident * POOL_PAGES as usize * PAGE,
        )
    })
}

/// Heaps of every kind of resource in one (resource heap tier 2) – else no arenas.
fn tier2(dev: &ID3D12Device) -> bool {
    let mut o = D3D12_FEATURE_DATA_D3D12_OPTIONS::default();
    unsafe { dev.CheckFeatureSupport(D3D12_FEATURE_D3D12_OPTIONS, &mut o as *mut _ as *mut c_void, std::mem::size_of_val(&o) as u32).is_ok() && o.ResourceHeapTier.0 >= 2 }
}

/// A sampled texture of the title at `va` (`desc`: the PC description, `console_bytes` its size on
/// the console) backed by shared 64 KiB video-memory pages; None when reserved placement is not
/// available (the caller falls back to a committed resource).
#[allow(clippy::too_many_arguments)]
pub unsafe fn place_pages(
    this: *mut c_void,
    va: usize,
    desc: &D3D12_RESOURCE_DESC,
    state: u32,
    clear: *const c_void,
    console_bytes: Option<u64>,
    riid: *const windows_sys::core::GUID,
    ppv: *mut *mut c_void,
) -> Option<HRESULT> {
    let dev = ID3D12Device::from_raw_borrowed(&this)?;
    if !tier2(dev) || std::env::var_os("XCL_VRAM_ARENAS").is_some() {
        return None;
    }
    if console_has_packed_3d_mips(desc) {
        return None;
    }
    // (one slice: the tiles of an array's packed levels are not asked about here)
    let lazy = level_pages() && desc.Dimension == D3D12_RESOURCE_DIMENSION_TEXTURE2D && desc.DepthOrArraySize == 1 && desc.SampleDesc.Count == 1 && desc.Flags == D3D12_RESOURCE_FLAG_NONE;
    place_on_pages(dev, this, va, desc, state, clear, console_bytes, false, lazy, riid, ppv)
}

pub(super) struct LevelMapResult {
    pub gained: bool,
    pub unmapped: Vec<u32>,
    pub min_tiles: usize,
}

/// The levels `levels` of `res` (None: all of them) get their pages if they have none yet: a
/// texture placed with lazy levels (any other: nothing to do). Reports whether any group gained
/// pages (`gained`), plus any requested groups still unmapped (`unmapped`) and the smallest
/// missing-tile count needed to map one (`min_tiles`).
pub(super) unsafe fn map_levels(res: &ID3D12Resource, levels: Option<&[u32]>) -> LevelMapResult {
    let empty = || LevelMapResult { gained: false, unmapped: Vec::new(), min_tiles: 0 };
    let key = res.as_raw() as usize;
    let mut g = PAGES.lock().unwrap();
    for (&dev_key, pages) in g.iter_mut() {
        let Some(pages) = pages.as_mut() else { continue };
        let Some(t) = pages.targets.get(&key) else { continue };
        let Some(lv) = t.levels.as_ref().filter(|_| t.mapped) else { return empty() };
        let mut wanted: Vec<usize> = match levels {
            Some(l) => l.iter().map(|&x| lv.group_of(x)).collect(),
            None => (0..lv.groups.len()).collect(),
        };
        wanted.sort_unstable();
        wanted.dedup();
        let todo: Vec<(usize, u32, u32, u32, std::ops::Range<u32>)> = groups_to_map(lv, wanted, t.skip_mips)
            .into_iter()
            .map(|k| (k, lv.groups[k].0, lv.groups[k].1, lv.groups[k].2, lv.levels_of(k)))
            .collect();
        if todo.is_empty() {
            return empty();
        }
        let (va, console_bytes, total) = (t.va, t.console_bytes, t.total);
        let d = dev_key as *mut c_void;
        let Some(dev) = ID3D12Device::from_raw_borrowed(&d) else { return empty() };
        let Some((first_own, own)) = (total > 0)
            .then(|| owned_pages(va, console_bytes, total as usize))
            .flatten()
        else {
            return empty();
        };
        let mut gained = false;
        let mut unmapped = Vec::new();
        let mut min_tiles = usize::MAX;
        for (k, sub, first, n, group_levels) in todo {
            let range = first as usize..(first + n) as usize;
            let bound = pages.bind_range(
                dev, res, va, console_bytes, total, sub, range.clone(), &[],
            );
            if let Some((used, used_extra)) = bound {
                let Some(t) = pages.targets.get_mut(&key) else { break };
                t.used.extend(used);
                t.used_extra.extend(used_extra);
                if let Some(l) = t.levels.as_mut() {
                    l.groups[k].3 = true;
                }
                LEVEL_STATS[2].fetch_add(n as u64, std::sync::atomic::Ordering::Relaxed);
                let groups = LEVEL_STATS[3].fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                if groups >= 1024 && groups.is_power_of_two() {
                    let (textures, stand, mapped, _) = level_stats();
                    let (stand_mb, mapped_mb) = (stand >> 4, mapped >> 4);
                    crate::xlog_info!(
                        "D3D12.X: level pages: {textures} textures standing for {stand_mb} MB \
                         have pages for {mapped_mb} MB ({groups} levels given pages so far)"
                    );
                }
                gained = true;
            } else {
                let missing = range.filter(|&i| {
                    if i < own {
                        !pages.pages.contains_key(&(first_own + i * PAGE))
                    } else {
                        !pages.extra_pages.contains_key(&(va, i))
                    }
                }).count();
                unmapped.push(group_levels.start);
                min_tiles = min_tiles.min(missing.max(1));
            }
        }
        let min_tiles = if unmapped.is_empty() { 0 } else { min_tiles };
        return LevelMapResult { gained, unmapped, min_tiles };
    }
    empty()
}

#[cfg(test)]
pub(super) struct CappedPagesTestGuard {
    dev_key: usize,
    held: Vec<Tile>,
    previous: Option<Option<Pages>>,
}

#[cfg(test)]
impl CappedPagesTestGuard {
    pub(super) unsafe fn install(dev: &ID3D12Device, free_tiles: u32) -> Option<Self> {
        let mut pages = Pages::new(dev)?;
        pages.max_pools = 1;
        let slot = pages.pools.push_heap(alloc_pool_heap(dev)?);
        pages.pool.add_heap_at(slot, POOL_PAGES);
        let take = POOL_PAGES.saturating_sub(free_tiles) as usize;
        let now = std::time::Instant::now();
        let held = pages.pool.take_batch(take, None, now, POOL_PAGES, || None)?;
        let dev_key = dev.as_raw() as usize;
        let previous = PAGES.lock().unwrap().insert(dev_key, Some(pages));
        Some(Self { dev_key, held, previous })
    }

    pub(super) fn free_tiles(&mut self, count: usize) {
        let n = count.min(self.held.len());
        let give: Vec<Tile> = self.held.drain(..n).collect();
        if let Some(Some(p)) = PAGES.lock().unwrap().get_mut(&self.dev_key) {
            p.pool.return_tiles(&give);
        }
    }
}

#[cfg(test)]
impl Drop for CappedPagesTestGuard {
    fn drop(&mut self) {
        let done = PAGES.lock().unwrap().remove(&self.dev_key).flatten();
        if let Some(prev) = self.previous.take() {
            PAGES.lock().unwrap().insert(self.dev_key, prev);
        }
        if let Some(p) = done {
            let d = self.dev_key as *mut c_void;
            unsafe {
                if let Some(dev) = ID3D12Device::from_raw_borrowed(&d) {
                    let _ = crate::gpu::fence::wait(&p.fence, p.value, dev, None, 5_000);
                }
            }
        }
    }
}

/// Of the level groups `wanted`, those to give pages now: not mapped yet, and not of the
/// `skip_mips` top mips left without video memory (`top_mip_tiles`), which never get pages.
fn groups_to_map(lv: &Levels, wanted: Vec<usize>, skip_mips: u32) -> Vec<usize> {
    wanted.into_iter().filter(|&k| !lv.groups[k].3 && lv.levels_of(k).start >= skip_mips).collect()
}

/// Whether `resource` is a texture whose levels get their pages one by one (`map_levels`).
pub(super) fn has_lazy_levels(resource: usize) -> bool {
    PAGES.lock().unwrap().values().flatten().any(|p| p.targets.get(&resource).is_some_and(|t| t.levels.is_some()))
}

/// XG and the PC driver pack volume mip tails differently. Keep these textures on the
/// detile/upload path rather than mapping title pages directly onto the driver's packed tail.
fn console_has_packed_3d_mips(desc: &D3D12_RESOURCE_DESC) -> bool {
    if desc.Dimension != D3D12_RESOURCE_DIMENSION_TEXTURE3D || desc.MipLevels <= 1 {
        return false;
    }
    let mut console_desc = [0u8; 0x38];
    unsafe {
        std::ptr::copy_nonoverlapping(
            desc as *const _ as *const u8,
            console_desc.as_mut_ptr(),
            console_desc.len(),
        )
    };
    crate::hle::xg::Texture::new(&console_desc)
        .ok()
        .and_then(|t| t.texels().map(|s| s.depth > 0 && s.first_tail < s.chain.levels.len()))
        .unwrap_or(false)
}

/// A GPU-owned texture of the title at `va` (`desc`: the PC description, `console_bytes` its
/// size on the console): on the title's pages there, else in the arena for that address (None:
/// neither – the caller makes a resource of its own).
#[allow(clippy::too_many_arguments)]
pub unsafe fn place(this: *mut c_void, va: usize, desc: &D3D12_RESOURCE_DESC, state: u32, clear: *const c_void, console_bytes: Option<u64>, riid: *const windows_sys::core::GUID, ppv: *mut *mut c_void) -> Option<HRESULT> {
    let va = canonical_va(va);
    let dev = ID3D12Device::from_raw_borrowed(&this)?;
    if !tier2(dev) {
        return None;
    }
    if std::env::var_os("XCL_VRAM_ARENAS").is_none() {
        if let Some(hr) = place_on_pages(dev, this, va, desc, state, clear, console_bytes, true, false, riid, ppv) {
            return Some(hr);
        }
    }
    let info = dev.GetResourceAllocationInfo(0, &[*desc]);
    if info.SizeInBytes == u64::MAX || info.SizeInBytes == 0 || va as u64 % info.Alignment.max(1) != 0 {
        return None;
    }
    let align = (info.Alignment as usize).max(PAGE);
    let now = std::time::Instant::now();
    let dev_key = dev.as_raw() as usize;
    let (taken, expired) = ARENAS.lock().unwrap().take_for(dev_key, va, info.SizeInBytes as usize, align, now, |size, align| arena_heap(dev, va, size, align));
    if !expired.is_empty() {
        retire_heaps_and_sweep(dev, dev_key, expired, 0, None, None);
    }
    let (heap, offset, id) = taken?;
    let hr = super::real::dev_create_placed_resource(this, heap.as_raw() as usize, offset, desc as *const _ as usize, state as usize, clear as usize, riid, ppv);
    if hr < 0 || (*ppv).is_null() {
        ARENAS.lock().unwrap().release(id, now);
        return None;
    }
    PLACED_TEXTURES.lock().unwrap().get_or_insert_with(HashSet::new).insert(*ppv as usize);
    ARENA_OF.lock().unwrap().get_or_insert_with(std::collections::HashMap::new).insert(*ppv as usize, id);
    Some(hr)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    const MIB: usize = 1 << 20;

    /// The levels of a texture with lazy levels: a group of tiles for each level with tiles of its
    /// own and one for the packed levels; a level past the last standard one is in the packed
    /// group; a tiling that does not add up to the resource's tiles gives none.
    #[test]
    fn the_tiles_of_a_texture_are_grouped_by_its_levels() {
        // 2048x2048 BC7, 12 levels: 64, 16, 4, 1 tiles and 1 for the 8 packed levels
        let l = Levels::new(&[(0, 64), (64, 16), (80, 4), (84, 1)], Some((85, 1)), 86, 12).unwrap();
        assert_eq!(l.groups, vec![(0, 0, 64, false), (1, 64, 16, false), (2, 80, 4, false), (3, 84, 1, false), (4, 85, 1, false)]);
        assert_eq!((0..12).map(|i| l.group_of(i)).collect::<Vec<_>>(), [0, 1, 2, 3, 4, 4, 4, 4, 4, 4, 4, 4]);
        assert_eq!((l.levels_of(0), l.levels_of(3), l.levels_of(4)), (0..1, 3..4, 4..12));
        // all of it packed: one group
        let small = Levels::new(&[], Some((0, 1)), 1, 5).unwrap();
        assert_eq!((small.group_of(0), small.group_of(4), small.levels_of(0)), (0, 0, 0..5));
        // no packed levels: the last level's group ends at the texture's levels
        let plain = Levels::new(&[(0, 4), (4, 1)], None, 5, 2).unwrap();
        assert_eq!((plain.group_of(1), plain.levels_of(1)), (1, 1..2));
        // a gap, an overlap, a tile count that is not the resource's, nothing at all
        assert_eq!(Levels::new(&[(0, 4), (5, 1)], None, 6, 2), None);
        assert_eq!(Levels::new(&[(0, 4), (3, 2)], None, 5, 2), None);
        assert_eq!(Levels::new(&[(0, 4), (4, 1)], None, 6, 2), None);
        assert_eq!(Levels::new(&[(0, 4), (4, 0)], None, 4, 2), None);
        assert_eq!(Levels::new(&[], None, 0, 1), None);
    }

    #[test]
    fn packed_console_3d_mip_tails_use_the_detile_upload_path() {
        fn desc(
            dimension: u32,
            width: u64,
            height: u32,
            depth: u16,
            mips: u16,
            format: u32,
            layout: u32,
        ) -> D3D12_RESOURCE_DESC {
            let mut bytes = [0u8; 0x38];
            bytes[0..4].copy_from_slice(&dimension.to_le_bytes());
            bytes[0x10..0x18].copy_from_slice(&width.to_le_bytes());
            bytes[0x18..0x1C].copy_from_slice(&height.to_le_bytes());
            bytes[0x1C..0x1E].copy_from_slice(&depth.to_le_bytes());
            bytes[0x1E..0x20].copy_from_slice(&mips.to_le_bytes());
            bytes[0x20..0x24].copy_from_slice(&format.to_le_bytes());
            bytes[0x24..0x28].copy_from_slice(&1u32.to_le_bytes());
            bytes[0x2C..0x30].copy_from_slice(&layout.to_le_bytes());
            unsafe { std::ptr::read_unaligned(bytes.as_ptr() as *const D3D12_RESOURCE_DESC) }
        }

        let packed_3d = desc(4, 64, 64, 64, 4, 28, 0x105);
        assert!(console_has_packed_3d_mips(&packed_3d));
        let single_mip_3d = desc(4, 64, 64, 64, 1, 28, 0x105);
        assert!(!console_has_packed_3d_mips(&single_mip_3d));
        let packed_2d = desc(3, 64, 64, 1, 7, 28, 0x105);
        assert!(!console_has_packed_3d_mips(&packed_2d));
    }

    /// One call per heap: its tiles in runs, unbacked (`None`) tiles explicitly `Null` on the
    /// first pass (`D3D12_TILE_RANGE_FLAG_NULL`), and other heaps' tiles `Skip`ped.
    #[test]
    fn tile_ranges_per_heap() {
        use TileRange::{Map, Null, Skip};
        let tiles = [
            Some((0, 5)), Some((0, 6)), Some((1, 0)), Some((1, 1)),
            Some((0, 9)), Some((0, 10)), Some((0, 11)),
        ];
        assert_eq!(heap_ranges(0, &tiles, true), [Map(5, 2), Skip(2), Map(9, 3)]);
        assert_eq!(heap_ranges(1, &tiles, false), [Skip(2), Map(0, 2), Skip(3)]);
        assert_eq!(heap_ranges_trimmed(1, &tiles, false), [Skip(2), Map(0, 2)]);
        // shared pages out of order: runs break where the tiles do
        let ooo = [Some((0, 3)), Some((0, 1)), Some((0, 2))];
        assert_eq!(heap_ranges(0, &ooo, true), [Map(3, 1), Map(1, 2)]);
        // partial bind: pass 0 keeps leading/trailing `Null` while pass 1 trims trailing `Skip`
        let partial = [None, None, Some((0, 4)), Some((0, 5)), Some((1, 2)), None];
        assert_eq!(heap_ranges_trimmed(0, &partial, true), [Null(2), Map(4, 2), Skip(1), Null(1)]);
        assert_eq!(heap_ranges_trimmed(1, &partial, false), [Skip(4), Map(2, 1)]);
        assert_eq!(heap_ranges_trimmed(0, &[None, None], true), [Null(2)]);
        assert!(heap_ranges_trimmed(0, &[None, None], false).is_empty());
    }

    #[test]
    fn target_pages_budget_respects_env_title_memory_uma_and_adapter_vram() {
        use AdapterBudget::{Dedicated, Local};
        let cm = compute_max_pools;
        assert_eq!(parse_target_pages_mb(Some(" 4096 ")), Some(4096));
        assert_eq!(parse_target_pages_mb(Some("0")), Some(0));
        assert_eq!(parse_target_pages_mb(Some("invalid")), None);
        let low = Some(Local(6_800 << 20));
        let (mid, deck) = (Some(Local(11_100 << 20)), Some(Local(1_024 << 20)));
        assert_eq!(cm(Some(0), 4096, false, low), (usize::MAX, "XCL_TARGET_PAGES_MB=0"));
        assert_eq!(cm(Some(3072), 13_824, false, low), (48, "XCL_TARGET_PAGES_MB"));
        assert_eq!(cm(Some(16), 13_824, false, low), (1, "XCL_TARGET_PAGES_MB"));
        assert_eq!(cm(None, 13_824, true, deck), (216, "title_memory_mb on UMA adapter"));
        assert_eq!(cm(None, 8_192, true, deck), (128, "title_memory_mb on UMA adapter"));
        assert_eq!(cm(None, 4096, false, mid), (64, "title_memory_mb"));
        assert_eq!(cm(None, 13_824, false, mid), (130, "75% of DXGI local budget"));
        assert_eq!(cm(None, 13_824, false, low), (79, "75% of DXGI local budget"));
        assert_eq!(cm(None, 8_192, false, low), (79, "75% of DXGI local budget"));
        let (ded8, big) = (Some(Dedicated(8_192 << 20)), Some(Local(22_500 << 20)));
        assert_eq!(cm(None, 13_824, false, ded8), (96, "75% of DedicatedVideoMemory"));
        assert_eq!(cm(None, 13_824, false, big), (216, "title_memory_mb"));
        assert_eq!(cm(None, 8_192, false, None), (128, "title_memory_mb"));
    }

    #[test]
    fn budgeted_bind_shares_pages_retains_cubemap_coarse_mips_and_recovers_after_free() {
        use TileRange::{Map, Null};
        let t0 = Instant::now();
        const P: u32 = 8; // 8 tiles/pool, max 2 pools = 16 tiles total budget
        let heaps_made = std::cell::Cell::new(0u32);
        let make = || {
            let h = heaps_made.get();
            (h < 2).then(|| { heaps_made.set(h + 1); h })
        };
        let (mut pages, mut extra, mut pool) = (
            std::collections::HashMap::new(), std::collections::HashMap::new(), TilePool::new(),
        );
        let sh = |mip_levels, array_slices, packed_mips, packed_start, packed_tiles| {
            TileShape { mip_levels, array_slices, packed_mips, packed_start, packed_tiles }
        };
        macro_rules! bind {
            ($va:expr, $own:expr, $tot:expr, $sh:expr, $t:expr) => {
                Pages::bind_tiles(
                    &mut pages, &mut extra, &mut pool, ($va, $va, $own, $tot, $sh), &[], &[], ($t, P), make,
                )
            };
        }
        let (tiles_a, used_a, extra_a, dropped_a) = bind!(0x10_0000, 12, 12, sh(1, 1, 0, 0, 0), t0);
        assert_eq!((dropped_a, heaps_made.get(), pages.len(), pool.free_total), (0, 2, 12, 4));
        assert!(tiles_a.iter().all(Option::is_some) && extra_a.is_empty());
        let (tiles_b, used_b, extra_b, dropped_b) = bind!(0x30_0000, 7, 8, sh(4, 1, 2, 7, 1), t0);
        assert_eq!((dropped_b, heaps_made.get(), pages.len() + extra.len()), (4, 2, 16));
        assert_eq!(&tiles_b[..4], &[None; 4]);
        assert_eq!(&tiles_b[4..], &[Some((1, 4)), Some((1, 5)), Some((1, 6)), Some((1, 7))]);
        assert_eq!(heap_ranges_trimmed(1, &tiles_b, true), [Null(4), Map(4, 4)]);
        assert_eq!(&used_b, &[0x34_0000, 0x35_0000, 0x36_0000]);
        assert_eq!(&extra_b, &[(0x30_0000, 7)]);

        let mut target_b = Target {
            va: 0x30_0000, console_bytes: None, total: 8, shape: sh(4, 1, 2, 7, 1),
            mapped: true, used: used_b, used_extra: extra_b, levels: None, gpu_owned: false,
            skip_mips: 0, skip: Vec::new(),
        };
        assert!(!target_b.is_complete());
        let (tiles_alias, used_alias, _, dropped_alias) =
            bind!(0x10_0000, 4, 4, sh(1, 1, 0, 0, 0), t0);
        assert_eq!((dropped_alias, used_alias.len(), pages[&0x10_0000].users), (0, 4, 2));
        assert!(tiles_alias.iter().all(Option::is_some));
        let (tiles_c, used_c, extra_c, dropped_c) = bind!(0x50_0000, 4, 4, sh(1, 1, 0, 0, 0), t0);
        assert_eq!((dropped_c, used_c.len(), extra_c.len()), (4, 0, 0));
        assert_eq!(heap_ranges_trimmed(0, &tiles_c, true), [Null(4)]);

        // Free 8 unaliased pages of A: rebinding B (`4/8`) and C (`0/4`) keeps B's `(1, 4..=7)`
        // with `users == 1` and backfills both to `is_complete()`.
        for a in used_a.into_iter().skip(4) {
            pool.push_cooling(pages.remove(&a).unwrap().tile, t0);
        }
        let t1 = t0 + COOL + Duration::from_millis(1);
        for &a in &target_b.used { pages.get_mut(&a).unwrap().users -= 1; }
        for &k in &target_b.used_extra { extra.get_mut(&k).unwrap().users -= 1; }
        let (tiles_b2, used_b2, extra_b2, dropped_b2) = bind!(0x30_0000, 7, 8, target_b.shape, t1);
        (target_b.used, target_b.used_extra) = (used_b2, extra_b2);
        assert!(dropped_b2 == 0 && target_b.is_complete() && tiles_b2.iter().all(Option::is_some));
        assert_eq!(&tiles_b2[4..], &[Some((1, 4)), Some((1, 5)), Some((1, 6)), Some((1, 7))]);
        assert!(target_b.used.iter().all(|a| pages[a].users == 1));
        assert_eq!(extra[&(0x30_0000, 7)].users, 1);
        let (tiles_c2, used_c2, extra_c2, dropped_c2) =
            bind!(0x50_0000, 4, 4, sh(1, 1, 0, 0, 0), t1);
        let target_c = Target {
            va: 0x50_0000, console_bytes: None, total: 4, shape: sh(1, 1, 0, 0, 0),
            mapped: true, used: used_c2, used_extra: extra_c2, levels: None, gpu_owned: false,
            skip_mips: 0, skip: Vec::new(),
        };
        assert!(dropped_c2 == 0 && target_c.is_complete() && tiles_c2.iter().all(Option::is_some));

        // 6-face cubemap with 4 tiles/face (3 standard + 1 packed at `packed_start = 3`,
        // `packed_tiles = 1`, `total = 24`): a 6-tile budget selects `3, 7, 11, 15, 19, 23`
        // (every face keeps its coarsest packed mip), and 12 tiles add `2, 6, 10, 14, 18, 22`.
        for a in target_b.used.into_iter().chain(target_c.used) {
            pool.push_cooling(pages.remove(&a).unwrap().tile, t1);
        }
        pool.push_cooling(extra.remove(&(0x30_0000, 7)).unwrap().tile, t1);
        let t2 = t1 + COOL + Duration::from_millis(1);
        let hold_six = pool.take_up_to(6, None, t2, P, make);
        assert_eq!(hold_six.len(), 6);
        let cube_sh = sh(4, 6, 2, 3, 1);
        let (tiles_c6, used_c6, _, dropped_c6) = bind!(0x80_0000, 24, 24, cube_sh, t2);
        assert_eq!((dropped_c6, used_c6.len()), (18, 6));
        for i in 0..24 {
            assert_eq!(tiles_c6[i].is_some(), i % 4 == 3, "tile {i}");
        }
        pool.return_tiles(&hold_six);
        for &a in &used_c6 { pages.get_mut(&a).unwrap().users -= 1; }
        let (tiles_c12, used_c12, _, dropped_c12) = bind!(0x80_0000, 24, 24, cube_sh, t2);
        assert_eq!((dropped_c12, used_c12.len()), (12, 12));
        for i in 0..24 {
            assert_eq!(tiles_c12[i].is_some(), i % 4 >= 2, "tile {i}");
        }
        for a in used_c12 { pool.push_cooling(pages.remove(&a).unwrap().tile, t2); }

        // 2-tile packed group with only 1 tile available stays whole-unmapped (never half-mapped)
        // and returns unused fresh tiles to `pool` without leaking; `SINGLE_MIPTAIL_BIT` works.
        let t3 = t2 + COOL + Duration::from_millis(1);
        let hold_eleven = pool.take_up_to(11, None, t3, P, make);
        assert_eq!((hold_eleven.len(), pool.free_total), (11, 1));
        let (tiles_pk2, used_pk2, _, dropped_pk2) = bind!(0xB0_0000, 2, 2, sh(2, 1, 2, 0, 2), t3);
        assert_eq!((dropped_pk2, used_pk2.len(), pool.free_total), (2, 0, 1));
        assert_eq!(tiles_pk2, vec![None, None]);
        let (tiles_mix, used_mix, _, dropped_mix) = bind!(0xC0_0000, 4, 4, sh(3, 1, 2, 2, 2), t3);
        assert_eq!((dropped_mix, used_mix.len(), pool.free_total), (3, 1, 0));
        assert!(tiles_mix[1].is_some() && tiles_mix[0].is_none());
        assert_eq!(&tiles_mix[2..4], &[None, None]);
        for a in used_mix { pool.push_cooling(pages.remove(&a).unwrap().tile, t3); }
        pool.return_tiles(&hold_eleven);
        let t4 = t3 + COOL + Duration::from_millis(1);
        let _hold5 = pool.take_up_to(5, None, t4, P, make);
        let (tiles_st, _, _, dropped_st) = bind!(0xE0_0000, 43, 43, sh(4, 6, 2, 42, 1), t4);
        assert!(dropped_st == 36 && tiles_st[42].is_some());
        for face in 0..6 { assert!(tiles_st[face * 7 + 6].is_some()); }
    }

    #[test]
    fn retire_queue_pool_cap_counts_retiring_pools_ignores_arenas_and_polls_completion() {
        use std::sync::{atomic::{AtomicU64, Ordering}, Arc};
        let mut rq = RetireQueue::<u32, Arc<AtomicU64>>::new();
        let unmap_f = Arc::new(AtomicU64::new(0));
        let mut qf = std::collections::HashMap::<usize, Arc<AtomicU64>>::new();
        let mk = || Some(Arc::new(AtomicU64::new(0)));
        rq.reserve_pending_pools(1);
        assert_eq!((rq.retiring_pools, rq.counted_pools()), (0, 1));
        let imm = rq.retire_with(
            vec![10, 200, 201], Some((unmap_f.clone(), 1)), &[(1, 1usize)], mk,
            |q, f, _| { qf.entry(*q).or_insert_with(|| f.clone()); true },
        );
        assert!(imm.is_empty());
        rq.pending_pools = rq.pending_pools.saturating_sub(1);
        rq.note_queued_pools(1);
        assert!(rq.retire_with(vec![300], None, &[(1, 1usize)], mk, |_, _, _| true).is_empty());
        assert_eq!((rq.retiring_pools, rq.counted_pools()), (1, 1));
        assert!(rq.reap_completed(|f| f.load(Ordering::SeqCst)).is_empty());
        assert!(1 + rq.counted_pools() >= 2);
        unmap_f.store(1, Ordering::SeqCst);
        qf[&1].store(1, Ordering::SeqCst);
        let done = rq.reap_completed(|f| f.load(Ordering::SeqCst));
        assert_eq!(
            (done.len(), done[0].pool_heaps, rq.counted_pools(), rq.batches.len()), (1, 1, 0, 1),
        );
    }

    #[test]
    fn reclaimed_pool_still_counts_before_retirement_is_queued() {
        let _vram_guard = VRAM_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_11_0;
        unsafe {
            let Ok(dev) = crate::gpu::agility::create_device(None, D3D_FEATURE_LEVEL_11_0) else {
                return crate::gpu::recomp::runner::skip_test("no GPU");
            };
            let Some(mut pages) = Pages::new(&dev) else {
                return crate::gpu::recomp::runner::skip_test("no GPU");
            };
            let spare = keep_spare_pools();
            if spare > 2 { return; }
            let cap = spare + 1;
            for _ in 0..cap {
                let heap = alloc_pool_heap(&dev).unwrap();
                let slot = pages.pools.push_heap(heap);
                pages.pool.add_heap_at(slot, POOL_PAGES);
            }
            pages.max_pools = cap;
            pages.last_reclaim = Instant::now() - Duration::from_secs(2);
            let queue = pages.queue.clone();
            let dev_k = dev.as_raw() as usize;
            let previous_retire = RETIRE.lock().unwrap().as_mut()
                .and_then(|map| map.remove(&dev_k));
            let previous = PAGES.lock().unwrap().insert(dev_k, Some(pages));
            let (reached_tx, reached_rx) = std::sync::mpsc::channel();
            let (resume_tx, resume_rx) = std::sync::mpsc::channel();
            *RECLAIM_GAP.lock().unwrap() = Some((dev_k, reached_tx, resume_rx));
            let worker = std::thread::spawn(move || before_execute(&queue));
            let reached = reached_rx.recv_timeout(Duration::from_secs(5)).is_ok();
            let (active, counted, replacement) = if reached {
                let mut g = PAGES.lock().unwrap();
                let p = g.get_mut(&dev_k).unwrap().as_mut().unwrap();
                let active = p.pools.len();
                let counted = retiring_pool_heaps_count(dev_k);
                let replacement = alloc_budgeted_pool(
                    &dev, dev_k, cap, &mut p.oom_until, &mut p.pools, Instant::now(),
                );
                (active, counted, replacement)
            } else {
                (0, 0, None)
            };
            let _ = resume_tx.send(());
            worker.join().unwrap();
            *RECLAIM_GAP.lock().unwrap() = None;
            let pages = PAGES.lock().unwrap().remove(&dev_k).flatten();
            if let Some(previous) = previous { PAGES.lock().unwrap().insert(dev_k, previous); }
            drop(pages);
            let marks = {
                let g = RETIRE.lock().unwrap();
                g.as_ref().and_then(|m| m.get(&dev_k)).map(|rq| {
                    rq.batches.iter()
                        .flat_map(|b| b.marks.iter().chain(b.unmap_mark.iter()).cloned())
                        .collect::<Vec<_>>()
                }).unwrap_or_default()
            };
            for (f, v) in marks { crate::gpu::fence::wait(&f, v, &dev, None, 5_000).unwrap(); }
            let _ = poll_retiring_pool_heaps(dev_k);
            let remaining_batches = {
                let mut guard = RETIRE.lock().unwrap();
                if let Some(m) = guard.as_mut() {
                    let ours = m.remove(&dev_k);
                    let remaining = ours.as_ref().map_or(0, |rq| rq.batches.len());
                    if let Some(old) = previous_retire { m.insert(dev_k, old); }
                    remaining
                } else { 0 }
            };
            assert_eq!(remaining_batches, 0);
            assert!(reached, "before_execute did not reclaim the empty pool");
            assert_eq!(active, cap - 1);
            assert_eq!(counted, 1, "the reclaimed pool must remain reserved during handoff");
            assert!(replacement.is_none(), "pending retirement pool must not free cap capacity");
        }
    }

    #[test]
    fn capped_pages_keep_over_budget_placements_alive() {
        let _vram_guard = VRAM_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_11_0;
        use windows::Win32::Graphics::Dxgi::Common::*;
        unsafe {
            let Ok(dev) = crate::gpu::agility::create_device(None, D3D_FEATURE_LEVEL_11_0) else {
                return crate::gpu::recomp::runner::skip_test("no GPU");
            };
            let Some(mut pages) = Pages::new(&dev) else {
                return crate::gpu::recomp::runner::skip_test("no GPU");
            };
            pages.max_pools = 1;
            let reserve = |w: u64, h: u32, slices: u16, mips: u16| {
                let desc = D3D12_RESOURCE_DESC {
                    Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D, Width: w, Height: h,
                    DepthOrArraySize: slices, MipLevels: mips, Format: DXGI_FORMAT_R8G8B8A8_UNORM,
                    SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                    Layout: D3D12_TEXTURE_LAYOUT_64KB_UNDEFINED_SWIZZLE, ..Default::default()
                };
                let (mut r, mut n, mut pk) = (None, 0u32, D3D12_PACKED_MIP_INFO::default());
                let st = D3D12_RESOURCE_STATE_COMMON;
                dev.CreateReservedResource(&desc, st, None, &mut r).unwrap();
                let (r, null) = (r.unwrap(), std::ptr::null_mut());
                dev.GetResourceTiling(&r, Some(&mut n), Some(&mut pk), None, None, 0, null);
                (r, n, TileShape::from_tiling(mips, slices, n, &pk))
            };
            let (small, ns, ss) = reserve(256, 256, 1, 1);
            let (cube, nc, sc) = reserve(1024, 1024, 6, 4);
            let (big, nb, sb) = reserve(4096, 4096, 1, 4);
            let (over, no, so) = reserve(256, 256, 1, 1);
            let (alias, na, sa) = reserve(256, 256, 1, 1);
            assert!(ns > 0 && ns + nc < POOL_PAGES && nb > POOL_PAGES);
            let bytes = |n: u32| Some(n as u64 * PAGE as u64);
            let (us, _) = pages.bind(&dev, &small, 0x1000_0000, bytes(ns), ns, ss, &[], &[]).unwrap();
            let (uc, _) = pages.bind(&dev, &cube, 0x1800_0000, bytes(nc), nc, sc, &[], &[]).unwrap();
            let (ub, _) = pages.bind(&dev, &big, 0x2000_0000, bytes(nb), nb, sb, &[], &[]).unwrap();
            let (uo, _) = pages.bind(&dev, &over, 0x3000_0000, bytes(no), no, so, &[], &[]).unwrap();
            let (ua, _) = pages.bind(&dev, &alias, 0x1000_0000, bytes(na), na, sa, &[], &[]).unwrap();
            assert_eq!(pages.pools.len(), 1);
            assert_eq!(
                (us.len() as u32, uc.len() as u32, ub.len() as u32, uo.len(), ua.len() as u32),
                (ns, nc, POOL_PAGES - ns - nc, 0, na),
            );
            crate::gpu::fence::wait(&pages.fence, pages.value, &dev, None, 5_000).unwrap();

            let (big_k, over_k) = (big.as_raw() as usize, over.as_raw() as usize);
            let dev_k = dev.as_raw() as usize;
            let tgt = |va, total, shape, used| Target {
                va, console_bytes: bytes(total), total, shape, mapped: true, used,
                used_extra: Vec::new(), levels: None, gpu_owned: false,
                skip_mips: 0, skip: Vec::new(),
            };
            pages.targets.insert(big_k, tgt(0x2000_0000, nb, sb, ub));
            pages.targets.insert(over_k, tgt(0x3000_0000, no, so, uo));
            pages.max_pools = 2;
            struct TestStateGuard {
                dev_k: usize,
                dev: ID3D12Device,
                gate: ID3D12Fence,
                prev_pages: Option<Option<Pages>>,
                prev_retire: Option<RetireQueue<ID3D12Heap, ID3D12Fence>>,
            }
            impl Drop for TestStateGuard {
                fn drop(&mut self) {
                    unsafe {
                        let _ = self.gate.Signal(1);
                        let _ = poll_retiring_pool_heaps(self.dev_k);
                        {
                            let mut rg = RETIRE.lock().unwrap();
                            if let Some(m) = rg.as_mut() {
                                if let Some(ours) = m.remove(&self.dev_k) {
                                    let n = ours.batches.len();
                                    if n > 0 {
                                        RETIRING_BATCHES.fetch_sub(
                                            n, std::sync::atomic::Ordering::AcqRel,
                                        );
                                    }
                                }
                                if let Some(prev) = self.prev_retire.take() {
                                    m.insert(self.dev_k, prev);
                                }
                            }
                        }
                        let done = PAGES.lock().unwrap().remove(&self.dev_k).flatten();
                        if let Some(prev) = self.prev_pages.take() {
                            PAGES.lock().unwrap().insert(self.dev_k, prev);
                        }
                        if let Some(p) = done {
                            let _ = crate::gpu::fence::wait(
                                &p.fence, p.value, &self.dev, None, 5_000,
                            );
                        }
                    }
                }
            }
            let gate: ID3D12Fence = dev.CreateFence(0, D3D12_FENCE_FLAG_NONE).unwrap();
            let retire_heap = alloc_pool_heap(&dev).unwrap();
            let prev_retire = {
                let mut g = RETIRE.lock().unwrap();
                let map = g.get_or_insert_with(std::collections::HashMap::new);
                let prev = map.remove(&dev_k);
                let mut rq = RetireQueue::new();
                rq.retiring_pools = 1;
                rq.batches.push(RetiringBatch {
                    heaps: vec![retire_heap], pool_heaps: 1,
                    unmap_mark: Some((gate.clone(), 1)), marks: Vec::new(),
                });
                map.insert(dev_k, rq);
                RETIRING_BATCHES.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
                prev
            };
            let prev_pages = PAGES.lock().unwrap().insert(dev_k, Some(pages));
            let _state_guard = TestStateGuard {
                dev_k, dev: dev.clone(), gate: gate.clone(), prev_pages, prev_retire,
            };
            assert!(!remap_if_unmapped(&over) && retiring_pool_heaps_count(dev_k) == 1);
            let needed = (ns + nc + nb + no).div_ceil(POOL_PAGES) as usize;
            if let Some(Some(p)) = PAGES.lock().unwrap().get_mut(&dev_k) {
                p.max_pools = needed.max(2);
            }
            gate.Signal(1).unwrap();
            assert!(remap_if_unmapped(&over) && retiring_pool_heaps_count(dev_k) == 0);
            assert!(remap_if_unmapped(&big));
            assert!(mapped_tiles_for(dev_k, big_k).is_some());
            assert!(mapped_tiles_for(dev_k, over_k).is_some());
        }
    }

    /// Streaming churn across 100+ 64 MiB pool heaps: cooling tiles returned in interleaved
    /// cross-heap order are regrouped per heap and sorted into contiguous ascending runs, so new
    /// textures stay inside 1 pool heap (1 UpdateTileMappings call, 1 contiguous range) instead of
    /// scattering across dozens of heaps.
    #[test]
    fn tile_pool_keeps_churned_allocations_in_single_heap_and_contiguous() {
        let t0 = Instant::now();
        let mut pool = TilePool::new();
        let heaps_made = std::cell::Cell::new(0u32);
        let mut make = || {
            let h = heaps_made.get();
            heaps_made.set(h + 1);
            Some(h)
        };

        // Fill 100 pools (64 tiles each for a fast unit test, same multi-heap topology as 1024).
        const P: u32 = 64;
        let mut live: Vec<Vec<Tile>> = Vec::new();
        for _ in 0..100 {
            live.push(pool.take_batch(P as usize, None, t0, P, &mut make).unwrap());
        }
        assert_eq!(heaps_made.get(), 100);
        assert_eq!(pool.free_total, 0);

        // Free 32 tiles from every pool in interleaved round-robin order (worst-case churn across
        // all 100 pools in `cooling`).
        for tile_idx in 0..32usize {
            for h in 0..100usize {
                pool.push_cooling(live[h][tile_idx], t0);
            }
        }
        assert_eq!(pool.cooling.len(), 3200);

        // After COOL, allocating 100 textures of 32 tiles each must:
        // - allocate ZERO new heaps,
        // - place EVERY texture into exactly 1 single pool heap (1 UpdateTileMappings call),
        // - yield 1 single contiguous tile range (0..32) per texture!
        let t1 = t0 + COOL + Duration::from_millis(1);
        for _ in 0..100 {
            let alloc = pool.take_batch(32, None, t1, P, &mut make).unwrap();
            assert_eq!(alloc.len(), 32);
            let h0 = alloc[0].0;
            assert!(alloc.iter().all(|&(h, _)| h == h0), "texture scattered across multiple heaps");
            let wrapped: Vec<Option<Tile>> = alloc.iter().copied().map(Some).collect();
            let ranges = heap_ranges_trimmed(h0, &wrapped, true);
            assert_eq!(ranges, [TileRange::Map(0, 32)], "expected 1 contiguous ascending range");
        }
        assert_eq!(heaps_made.get(), 100, "must reuse cooled tiles without making new heaps");
        assert_eq!(pool.free_total, 0);

        // Partial overlap preference: if heap 7 has free tiles and preferred=Some(7), use heap 7.
        for t in 10..18u32 {
            pool.push_cooling((7, t), t1);
            pool.push_cooling((3, t), t1);
        }
        let t2 = t1 + COOL + Duration::from_millis(1);
        let pref = pool.take_batch(8, Some(7), t2, P, &mut make).unwrap();
        assert!(pref.iter().all(|&(h, _)| h == 7), "preferred heap should be chosen");
        let pref_opt: Vec<Option<Tile>> = pref.iter().copied().map(Some).collect();
        assert_eq!(heap_ranges_trimmed(7, &pref_opt, true), [TileRange::Map(10, 8)]);

        // Rollback on OOM: requesting more tiles than available when make_heap fails returns every
        // popped tile cleanly so free_total is preserved; `take_up_to` takes the 8 available tiles.
        assert_eq!(pool.free_total, 8);
        let fail = pool.take_batch(16, None, t2, P, || None);
        assert!(fail.is_none());
        assert_eq!(pool.free_total, 8, "OOM rollback must return all popped tiles");
        let partial = pool.take_up_to(16, None, t2, P, || None);
        assert_eq!((partial.len(), pool.free_total), (8, 0));
    }

    /// Empty cooled pool heaps (above the warm spare count) are reclaimed cleanly back to VRAM and
    /// their slot indices can be reused without growing the heap table.
    #[test]
    fn tile_pool_reclaims_empty_heaps_keeping_warm_spare_and_reuses_slots() {
        let t0 = Instant::now();
        let mut pool = TilePool::new();
        let free_slots: std::cell::RefCell<Vec<u32>> = std::cell::RefCell::new(Vec::new());
        let next_slot = std::cell::Cell::new(0u32);
        let mut alloc_slot = || {
            free_slots.borrow_mut().pop().or_else(|| {
                let h = next_slot.get();
                next_slot.set(h + 1);
                Some(h)
            })
        };

        const P: u32 = 64;
        let h0 = pool.take_batch(P as usize, None, t0, P, &mut alloc_slot).unwrap();
        let h1 = pool.take_batch(P as usize, None, t0, P, &mut alloc_slot).unwrap();
        let h2 = pool.take_batch(P as usize, None, t0, P, &mut alloc_slot).unwrap();
        let h3 = pool.take_batch(P as usize, None, t0, P, &mut alloc_slot).unwrap();
        assert_eq!(next_slot.get(), 4);
        assert_eq!(pool.free_total, 0);

        // Keep 1 tile live in heap 0; free all tiles in heaps 1, 2, 3.
        for &tile in &h0[1..] {
            pool.push_cooling(tile, t0);
        }
        for batch in [&h1, &h2, &h3] {
            for &tile in batch {
                pool.push_cooling(tile, t0);
            }
        }
        let t1 = t0 + COOL + Duration::from_millis(1);
        pool.drain_cooling(t1);
        assert_eq!(pool.free_total, 63 + 64 * 3);

        // Reclaim empty heaps keeping 1 warm spare: heaps 2 and 3 reclaim, heap 1 stays warm,
        // heap 0 still has 1 live tile so it is untouched.
        let reclaimed = pool.reclaim_empty_heaps(1, P);
        assert_eq!(reclaimed, vec![2, 3]);
        assert_eq!(pool.free_total, 63 + 64);
        free_slots.borrow_mut().extend(reclaimed);

        // First 64-tile allocation reuses warm spare heap 1 (no new heap allocated).
        let reuse_spare = pool.take_batch(P as usize, None, t1, P, &mut alloc_slot).unwrap();
        assert!(reuse_spare.iter().all(|&(h, _)| h == 1));
        assert_eq!(free_slots.borrow().len(), 2);

        // Next 64-tile allocation consumes heap 0's 63 free tiles plus 1 tile from reclaimed slot 3
        // without advancing `next_slot`.
        let reuse_slot = pool.take_batch(P as usize, None, t1, P, &mut alloc_slot).unwrap();
        assert_eq!(reuse_slot.iter().filter(|&&(h, _)| h == 0).count(), 63);
        assert_eq!(reuse_slot.iter().filter(|&&(h, _)| h == 3).count(), 1);
        assert_eq!(next_slot.get(), 4);
        assert_eq!(free_slots.borrow().len(), 1);
    }

    #[test]
    fn arenas_sweep_dropped_returns_expired_heaps() {
        let t0 = Instant::now();
        let mut arenas: Arenas<u32> = Arenas::new();
        let (_, _, id1) = arenas.take_for(1, 0x10_0000, 0x1_0000, PAGE, t0, |_, _| Some(101)).0.unwrap();
        let (_, _, id2) = arenas.take_for(1, 0x20_0000, 0x1_0000, PAGE, t0, |_, _| Some(202)).0.unwrap();
        arenas.release(id1, t0);
        arenas.release(id2, t0 + Duration::from_secs(1));
        assert!(arenas.sweep_dropped_for(1, t0 + Duration::from_millis(1900)).is_empty());
        assert_eq!(arenas.sweep_dropped_for(1, t0 + IDLE + Duration::from_millis(10)), vec![101]);
        assert_eq!(arenas.bytes(), 0x1_0000);
        assert_eq!(arenas.sweep_dropped_for(1, t0 + Duration::from_secs(1) + IDLE + Duration::from_millis(10)), vec![202]);
        assert_eq!(arenas.bytes(), 0);
    }

    #[test]
    fn arenas_sweep_dropped_preserves_newest_fitting_order() {
        let t0 = Instant::now();
        let mut arenas: Arenas<u32> = Arenas::new();
        let id1 = arenas.take_for(1, 0x10_0000, PAGE, PAGE, t0, |_, _| Some(101)).0.unwrap().2;
        let id2 = arenas.take_for(1, 0x20_0000, PAGE, PAGE, t0, |_, _| Some(202)).0.unwrap().2;
        let id3 = arenas.take_for(1, 0x20_0000, 2 * PAGE, PAGE, t0, |_, _| Some(303)).0.unwrap().2;
        arenas.release(id1, t0);
        let t1 = t0 + IDLE + Duration::from_millis(10);
        assert_eq!(arenas.sweep_dropped_for(1, t1), vec![101]);
        let (heap, off, picked_id) = arenas.take_for(1, 0x20_0000, PAGE, PAGE, t1, |_, _| panic!("must reuse existing arena")).0.unwrap();
        assert_eq!((heap, off, picked_id), (303, 0, id3), "sweep must preserve insertion order so rposition selects the newest fitting arena");
        assert_ne!(picked_id, id2);
    }

    #[test]
    fn arenas_isolate_devices_and_take_for_returns_expired_heaps() {
        use std::sync::{atomic::{AtomicUsize, Ordering}, Arc};
        struct DropFlag(Arc<AtomicUsize>);
        impl Drop for DropFlag { fn drop(&mut self) { self.0.fetch_add(1, Ordering::SeqCst); } }
        let (t0, d1, d2) = (Instant::now(), Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        let mut arenas: Arenas<Arc<DropFlag>> = Arenas::new();
        let (h1, _, id1) = arenas.take_for(0x1000, 0x10_0000, PAGE, PAGE, t0, |_, _| Some(Arc::new(DropFlag(d1.clone())))).0.unwrap();
        let (h2, _, id2) = arenas.take_for(0x2000, 0x10_0000, PAGE, PAGE, t0, |_, _| Some(Arc::new(DropFlag(d2.clone())))).0.unwrap();
        assert_ne!(id1, id2, "distinct devices at the same VA must never alias arenas");
        drop((h1, h2));
        arenas.release(id1, t0);
        arenas.release(id2, t0);
        let t1 = t0 + IDLE + Duration::from_millis(10);
        let exp1 = arenas.sweep_dropped_for(0x1000, t1);
        assert_eq!((exp1.len(), arenas.bytes(), d1.load(Ordering::SeqCst), d2.load(Ordering::SeqCst)), (1, PAGE, 0, 0));
        let (_, exp2) = arenas.take_for(0x2000, 0x20_0000, PAGE, PAGE, t1, |_, _| Some(Arc::new(DropFlag(Arc::new(AtomicUsize::new(0))))));
        assert_eq!((exp2.len(), d2.load(Ordering::SeqCst)), (1, 0), "take_for must not drop expired heaps inside ARENAS");
        drop((exp1, exp2));
        assert_eq!((d1.load(Ordering::SeqCst), d2.load(Ordering::SeqCst)), (1, 1));
    }

    #[test]
    fn retire_queue_waits_for_all_queues_and_prunes_dead_queue_fences() {
        use std::sync::{atomic::{AtomicU64, Ordering}, Arc};
        let (mut rq, unmap_f) = (RetireQueue::<u32, Arc<AtomicU64>>::new(), Arc::new(AtomicU64::new(0)));
        let mut qf: std::collections::HashMap<usize, Arc<AtomicU64>> = std::collections::HashMap::new();
        let mk = || Some(Arc::new(AtomicU64::new(0)));
        assert!(rq.retire_with(vec![101], Some((unmap_f.clone(), 1)), &[(1, 1usize), (2, 2), (3, 3)], mk, |q, f, _| { qf.entry(*q).or_insert_with(|| f.clone()); true }).is_empty());
        assert!(rq.retire_with(vec![202], None, &[(1, 1usize), (2, 2), (4, 4)], mk, |q, f, _| { qf.entry(*q).or_insert_with(|| f.clone()); true }).is_empty());
        qf[&2].store(2, Ordering::SeqCst);
        qf[&3].store(1, Ordering::SeqCst);
        unmap_f.store(1, Ordering::SeqCst);
        let live: HashSet<usize> = [1, 2].into_iter().collect();
        let (done, pruned) = rq.sweep_with(&live, |f| f.load(Ordering::SeqCst));
        assert!(done.is_empty() && pruned.len() == 1 && Arc::ptr_eq(&pruned[0], &qf[&3]) && rq.fences.contains_key(&4));
        drop(pruned);
        qf[&1].store(1, Ordering::SeqCst);
        let (done, pruned) = rq.sweep_with(&live, |f| f.load(Ordering::SeqCst));
        assert_eq!((done.len(), done[0].heaps.as_slice(), pruned.len()), (1, &[101][..], 0));
        assert!(Arc::strong_count(&qf[&3]) > 1, "completed batch fences must drop outside lock");
        drop(done);
        qf[&1].store(2, Ordering::SeqCst);
        qf[&4].store(1, Ordering::SeqCst);
        let (done, pruned) = rq.sweep_with(&live, |f| f.load(Ordering::SeqCst));
        assert_eq!((done[0].heaps.as_slice(), pruned.len(), rq.batches.is_empty()), (&[202][..], 1, true));
        assert!(Arc::ptr_eq(&pruned[0], &qf[&4]));
        assert_eq!(rq.retire_with(vec![303], None, &[] as &[(usize, usize)], mk, |_, _, _| true), vec![303]);
    }

    #[test]
    fn gpu_retire_queue_holds_heap_until_blocked_queue_passes_mark() {
        let _vram_guard = VRAM_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        use std::sync::{atomic::{AtomicBool, Ordering}, Arc};
        struct GuardedHeap(#[allow(dead_code)] ID3D12Heap, Arc<AtomicBool>);
        impl Drop for GuardedHeap { fn drop(&mut self) { self.1.store(true, Ordering::SeqCst); } }
        unsafe {
            let Ok(dev) = crate::gpu::agility::create_device(None, windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_11_0) else {
                return crate::gpu::recomp::runner::skip_test("no GPU");
            };
            let q1: ID3D12CommandQueue = dev.CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC::default()).unwrap();
            let q2: ID3D12CommandQueue = dev.CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC::default()).unwrap();
            let gate: ID3D12Fence = dev.CreateFence(0, D3D12_FENCE_FLAG_NONE).unwrap();
            let probe: ID3D12Fence = dev.CreateFence(0, D3D12_FENCE_FLAG_NONE).unwrap();
            let desc = D3D12_HEAP_DESC { SizeInBytes: PAGE as u64, Properties: D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_DEFAULT, ..Default::default() }, Alignment: PAGE as u64, Flags: D3D12_HEAP_FLAG_NONE };
            let mut heap: Option<ID3D12Heap> = None;
            dev.CreateHeap(&desc, &mut heap).unwrap();
            let dropped = Arc::new(AtomicBool::new(false));
            q1.Wait(&gate, 1).unwrap();
            let (k1, k2) = (q1.as_raw() as usize, q2.as_raw() as usize);
            let mut rq: RetireQueue<GuardedHeap, ID3D12Fence> = RetireQueue::new();
            assert!(rq.retire_with(vec![GuardedHeap(heap.unwrap(), dropped.clone())], None, &[(k1, q1.clone()), (k2, q2.clone())], || dev.CreateFence::<ID3D12Fence>(0, D3D12_FENCE_FLAG_NONE).ok(), |q, f, v| q.Signal(f, v).is_ok()).is_empty());
            q2.Signal(&probe, 1).unwrap();
            probe.SetEventOnCompletion(1, windows::Win32::Foundation::HANDLE::default()).unwrap();
            let (done, pruned) = rq.sweep_with(&[k1, k2].into_iter().collect(), |f| f.GetCompletedValue());
            assert!(done.is_empty() && pruned.is_empty() && !dropped.load(Ordering::SeqCst), "heap dropped while Q1 was still blocked");
            gate.Signal(1).unwrap();
            q1.Signal(&probe, 2).unwrap();
            probe.SetEventOnCompletion(2, windows::Win32::Foundation::HANDLE::default()).unwrap();
            let (done, pruned) = rq.sweep_with(&[k1].into_iter().collect(), |f| f.GetCompletedValue());
            assert_eq!((done.len(), pruned.len(), dropped.load(Ordering::SeqCst)), (1, 1, false));
            drop((done, pruned));
            assert!(dropped.load(Ordering::SeqCst));
        }
    }

    #[test]
    fn canonical_va_aliases_unmapped_gpu_view_but_preserves_committed_override() {
        use windows_sys::Win32::System::Memory::{VirtualAlloc, VirtualFree, MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE};
        const PAGES_64K: u32 = 0x2040_0000;
        const GRAPHICS_NOACCESS: u32 = 0x800;
        unsafe {
            let cpu = VirtualAlloc(std::ptr::null(), PAGE, MEM_RESERVE | MEM_COMMIT, PAGE_READWRITE) as usize;
            let gpu_unmapped = VirtualAlloc(std::ptr::null(), PAGE, MEM_RESERVE, PAGE_READWRITE) as usize;
            assert!(cpu != 0 && gpu_unmapped != 0);
            crate::hle::kernelx::mirrors::reserved(cpu, PAGE, PAGES_64K, 0x04);
            crate::hle::kernelx::mirrors::reserved(gpu_unmapped, PAGE, PAGES_64K, GRAPHICS_NOACCESS | 0x01);

            // Unmapped GPU view aliases to its CPU view, and reverse lookup finds the GPU view.
            assert_eq!(canonical_va(gpu_unmapped), cpu);
            assert!(crate::hle::kernelx::mirrors::gpu_views_of(cpu).contains(&gpu_unmapped));

            // Once distinct committed memory is mapped directly at the GPU view, canonical_va must
            // NOT alias it back to the CPU view.
            assert!(!VirtualAlloc(gpu_unmapped as *const c_void, PAGE, MEM_COMMIT, PAGE_READWRITE).is_null());
            assert_eq!(canonical_va(gpu_unmapped), gpu_unmapped);

            crate::hle::kernelx::mirrors::released(gpu_unmapped);
            crate::hle::kernelx::mirrors::released(cpu);
            VirtualFree(gpu_unmapped as *mut c_void, 0, MEM_RELEASE);
            VirtualFree(cpu as *mut c_void, 0, MEM_RELEASE);
        }
    }

    /// Idle pages go back after `IDLE`, and a sweep looks at the queue of idle pages alone: one
    /// used again stays, one idle again later goes when that is `IDLE` ago, and the sweeps of
    /// 20 000 placements beside 200 000 pages in use take no time to speak of (a look at every
    /// page at each of them: 4 000 000 000 looks).
    #[test]
    fn idle_pages_are_swept_from_their_queue() {
        use std::collections::{HashMap, VecDeque};
        let t0 = Instant::now();
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        let mut idle: VecDeque<(PageKey, Instant)> = VecDeque::new();
        let mut pages: HashMap<usize, Page> = (0..200_000usize).map(|i| (i * PAGE, Page { tile: (0, i as u32), users: 1, idle_since: None })).collect();
        let mut extra: HashMap<(usize, usize), Page> = HashMap::from([((7 * PAGE, 3), Page { tile: (1, 0), users: 1, idle_since: None })]);
        let mut cooling: VecDeque<(Tile, Instant)> = VecDeque::new();
        // what `release_tiles` and `bind` do to a page
        fn release(p: &mut Page, idle: &mut VecDeque<(PageKey, Instant)>, key: PageKey, when: Instant) {
            (p.users, p.idle_since) = (0, Some(when));
            idle.push_back((key, when));
        }
        fn use_again(p: &mut Page) {
            (p.users, p.idle_since) = (1, None);
        }
        // pages 1, 2, 3 idle at 0 ms and an extra page at 100 ms; page 2 is used again at
        // 500 ms; page 3 too, and is idle again at 1500 ms
        for a in [PAGE, 2 * PAGE, 3 * PAGE] {
            release(pages.get_mut(&a).unwrap(), &mut idle, PageKey::Own(a), at(0));
        }
        release(extra.get_mut(&(7 * PAGE, 3)).unwrap(), &mut idle, PageKey::Extra(7 * PAGE, 3), at(100));
        use_again(pages.get_mut(&(2 * PAGE)).unwrap());
        use_again(pages.get_mut(&(3 * PAGE)).unwrap());
        release(pages.get_mut(&(3 * PAGE)).unwrap(), &mut idle, PageKey::Own(3 * PAGE), at(1500));

        sweep_idle(&mut idle, &mut pages, &mut extra, &mut cooling, at(1900));
        assert!(cooling.is_empty() && pages.len() == 200_000 && idle.len() == 5, "nothing is idle for IDLE yet");
        sweep_idle(&mut idle, &mut pages, &mut extra, &mut cooling, at(2050));
        assert_eq!(cooling.iter().map(|c| c.0).collect::<Vec<_>>(), [(0, 1)], "page 1 alone: 2 is in use, 3 idle since 1500 ms");
        assert!(!pages.contains_key(&PAGE) && pages.contains_key(&(2 * PAGE)) && pages.contains_key(&(3 * PAGE)) && extra.len() == 1);
        sweep_idle(&mut idle, &mut pages, &mut extra, &mut cooling, at(2150));
        assert_eq!(cooling.iter().map(|c| c.0).collect::<Vec<_>>(), [(0, 1), (1, 0)], "the extra page, idle since 100 ms");
        assert!(extra.is_empty() && pages.len() == 199_999);
        sweep_idle(&mut idle, &mut pages, &mut extra, &mut cooling, at(3499));
        assert!(pages.contains_key(&(3 * PAGE)));
        sweep_idle(&mut idle, &mut pages, &mut extra, &mut cooling, at(3500));
        assert_eq!(cooling.iter().map(|c| c.0).collect::<Vec<_>>(), [(0, 1), (1, 0), (0, 3)]);
        assert!(idle.is_empty() && pages.len() == 199_998 && pages[&(2 * PAGE)].users == 1);

        // every placement sweeps: with nothing idle that is nothing, whatever is in use
        let t = Instant::now();
        for k in 0..20_000u64 {
            sweep_idle(&mut idle, &mut pages, &mut extra, &mut cooling, at(4000 + k));
        }
        assert!(t.elapsed() < Duration::from_secs(2), "{:?} for 20 000 sweeps", t.elapsed());
        assert_eq!(pages.len(), 199_998);
    }

    /// Any 64 KiB page boundary inside `[va, va + bytes)` belongs to that range alone, so
    /// non-overlapping ranges never share an owned page even when only 4 KiB aligned.
    #[test]
    fn owned_pages_never_alias_disjoint_ranges() {
        assert_eq!(owned_pages(0x1_0000, Some(0x1_0000), 4), Some((0x1_0000, 1)));
        assert_eq!(owned_pages(0x1_0000, Some(0x1_4000), 4), Some((0x1_0000, 2)));
        assert_eq!(owned_pages(0x1_4000, Some(0x4000), 4), Some((0x2_0000, 0)));
        assert_eq!(owned_pages(0x1_4000, Some(0x1_0000), 4), Some((0x2_0000, 1)));
        assert_eq!(owned_pages(0x1_4000, Some(0x4_0000), 4), Some((0x2_0000, 4)));
        assert_eq!(owned_pages(0x1_4000, None, 4), None);
        // Exhaustively check 4 KiB-aligned disjoint pairs over 256 KiB.
        const K4: usize = 0x1000;
        for a in (0..16).map(|i| 0x10_0000 + i * K4) {
            for sa in [K4, 4 * K4, 16 * K4, 20 * K4] {
                let (fa, na) = owned_pages(a, Some(sa as u64), 8).unwrap();
                let pa: Vec<usize> = (0..na).map(|i| fa + i * PAGE).collect();
                for b in (0..16).map(|i| 0x10_0000 + i * K4) {
                    for sb in [K4, 4 * K4, 16 * K4, 20 * K4] {
                        if a + sa <= b || b + sb <= a {
                            let (fb, nb) = owned_pages(b, Some(sb as u64), 8).unwrap();
                            for j in 0..nb {
                                assert!(!pa.contains(&(fb + j * PAGE)), "disjoint [{a:#x}..{:#x}) and [{b:#x}..{:#x}) shared page", a + sa, b + sb);
                            }
                        }
                    }
                }
            }
        }
    }

    /// Reserved only with TILED support, and for MSAA a tiled sample count (#940: the driver
    /// refused R32G8X24_TYPELESS reserved, at 1 and 4 samples, and the debug layer said so).
    #[test]
    fn reserved_needs_tiled_format_and_tiled_msaa() {
        let tiled = D3D12_FORMAT_SUPPORT2_TILED.0;
        assert!(can_reserve(tiled, 1, 0));
        assert!(!can_reserve(0, 1, 0), "format without TILED");
        assert!(can_reserve(tiled, 4, 1));
        assert!(!can_reserve(tiled, 4, 0), "no tiled quality level at 4 samples");
        assert!(!can_reserve(0, 4, 1));
    }

    /// The device's answer agrees with CreateReservedResource: what `reservable` turns away the
    /// driver refuses, what it lets through the driver makes (WARP or a hardware adapter).
    #[test]
    fn reservable_agrees_with_the_driver() {
        let _vram_guard = VRAM_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_11_0;
        use windows::Win32::Graphics::Dxgi::Common::*;
        unsafe {
            let mut dev: Option<ID3D12Device> = None;
            if D3D12CreateDevice(None, D3D_FEATURE_LEVEL_11_0, &mut dev).is_err() {
                let factory: windows::Win32::Graphics::Dxgi::IDXGIFactory4 = windows::Win32::Graphics::Dxgi::CreateDXGIFactory1().expect("DXGI factory");
                let Ok(warp) = factory.EnumWarpAdapter::<windows::Win32::Graphics::Dxgi::IDXGIAdapter>() else { return };
                let _ = D3D12CreateDevice(&warp, D3D_FEATURE_LEVEL_11_0, &mut dev);
            }
            let Some(dev) = dev else { return };
            let mut o = D3D12_FEATURE_DATA_D3D12_OPTIONS::default();
            if dev.CheckFeatureSupport(D3D12_FEATURE_D3D12_OPTIONS, &mut o as *mut _ as *mut c_void, std::mem::size_of_val(&o) as u32).is_err() || o.TiledResourcesTier.0 < D3D12_TILED_RESOURCES_TIER_2.0 {
                return;
            }
            for (format, samples) in [(DXGI_FORMAT_R32G8X24_TYPELESS, 1), (DXGI_FORMAT_R32G8X24_TYPELESS, 4), (DXGI_FORMAT_R32_TYPELESS, 1), (DXGI_FORMAT_R8G8B8A8_UNORM, 1)] {
                let flags = if format == DXGI_FORMAT_R8G8B8A8_UNORM { D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET } else { D3D12_RESOURCE_FLAG_ALLOW_DEPTH_STENCIL };
                let desc = D3D12_RESOURCE_DESC {
                    Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D, Width: 128, Height: 128, DepthOrArraySize: 1, MipLevels: 1, Format: format,
                    SampleDesc: DXGI_SAMPLE_DESC { Count: samples, Quality: 0 }, Layout: D3D12_TEXTURE_LAYOUT_64KB_UNDEFINED_SWIZZLE, Flags: flags, ..Default::default()
                };
                let mut r: Option<ID3D12Resource> = None;
                let made = dev.CreateReservedResource(&desc, D3D12_RESOURCE_STATE_COMMON, None, &mut r).is_ok() && r.is_some();
                assert_eq!(reservable(&dev, &desc), made, "format {} x{samples}: reservable vs CreateReservedResource", format.0);
            }
        }
    }

    /// A placement's arena id, with heaps numbered as they are made.
    fn take(a: &mut Arenas<u32>, va: usize, bytes: usize, align: usize, now: Instant) -> u64 {
        let n = a.list.len() as u32;
        a.take_for(1, va, bytes, align, now, |_, _| Some(n)).0.unwrap().2
    }

    #[test]
    fn an_arena_is_shared_at_its_addresses_and_otherwise_its_placements_size() {
        let (t, a0) = (Instant::now(), 0x1_0000_0000);
        let mut a = Arenas::new();
        let first = take(&mut a, a0, 70 * MIB, PAGE, t);
        assert_eq!(a.bytes(), 70 * MIB, "the placement's size, not 256 MiB");
        // the same address, and a smaller placement inside: the same memory
        assert_eq!(take(&mut a, a0, 70 * MIB, PAGE, t), first);
        assert_eq!(take(&mut a, a0 + 4 * MIB, 8 * MIB, PAGE, t), first);
        // one straddling its end gets an arena of its own size; so does an MSAA target that needs 4 MiB
        let straddle = take(&mut a, a0 + 64 * MIB, 70 * MIB, PAGE, t);
        let msaa = take(&mut a, a0 + 8 * MIB, 8 * MIB, 4 * MIB, t);
        assert!(straddle != first && msaa != first && msaa != straddle);
        assert_eq!(a.bytes(), (70 + 70 + 8) * MIB);
        // the newest arena holding an address is the one placements there share
        assert_eq!(take(&mut a, a0 + 64 * MIB, 4 * MIB, PAGE, t), straddle);
    }

    #[test]
    fn an_arena_no_resource_uses_goes_after_two_seconds() {
        let (t, a0) = (Instant::now(), 0x1_0000_0000);
        let mut a = Arenas::new();
        let id = take(&mut a, a0, 70 * MIB, PAGE, t);
        a.release(id, t);
        // placed again within 2 s: the same memory (with what it holds)
        assert_eq!(take(&mut a, a0, 70 * MIB, PAGE, t + Duration::from_secs(1)), id);
        a.release(id, t + Duration::from_secs(1));
        take(&mut a, a0 + 512 * MIB, MIB, PAGE, t + Duration::from_secs(3) + Duration::from_millis(1));
        assert_eq!(a.bytes(), MIB, "the idle arena went");
    }

    /// The retail game's pattern (HANDOFF): targets packed back to back over 3.5 GB of its
    /// addresses and placed again and again, each round at the last round's addresses or a third
    /// of a target further on, so placements straddle earlier ones. Main's 256 MiB arenas merged
    /// straddled arenas into growing unions while the old ones lived on (15 GB in 2 s).
    #[test]
    fn streamed_targets_stay_bounded() {
        const S: usize = 64 * MIB; // a 3840x2160 D32S8, rounded
        const K: usize = 8; // targets a round
        const SPAN: usize = 3584 * MIB;
        let (t0, a0) = (Instant::now(), 0x1_0000_0000usize);
        let mut a = Arenas::new();
        let mut live: std::collections::VecDeque<Vec<u64>> = std::collections::VecDeque::new();
        let mut bytes = Vec::new();
        for r in 0..400usize {
            let now = t0 + Duration::from_millis(500 * r as u64);
            let start = ((r / 2) * S / 3) % (SPAN - K * S) & !(PAGE - 1);
            live.push_back((0..K).map(|k| take(&mut a, a0 + start + k * S, S, PAGE, now)).collect());
            // a target lives three rounds (1.5 s)
            if live.len() > 3 {
                for id in live.pop_front().unwrap() {
                    a.release(id, now);
                }
            }
            bytes.push(a.bytes());
        }
        // live: 3 rounds of targets; idle and kept (< 2 s): 4 more
        let most = *bytes.iter().max().unwrap();
        assert!(most <= 7 * K * S, "{most} bytes of arenas");
        let early = *bytes[20..120].iter().max().unwrap();
        let late = *bytes[300..].iter().max().unwrap();
        assert!(late <= early, "arenas grow over time: {early} then {late} bytes");
    }

    /// On the GPU: D32S8 targets 256 MiB apart take their own size of video memory, not an arena
    /// of 256 MiB each; depth and stencil survive a clear, CopyResource and a read of both planes;
    /// arenas no target uses go. Needs a hardware adapter (skips on WARP).
    #[test]
    fn d32s8_targets_take_their_own_size_of_video_memory() {
        let _vram_guard = VRAM_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        use windows::core::Interface;
        use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_11_0;
        use windows::Win32::Graphics::Dxgi::Common::*;
        use windows::Win32::Graphics::Dxgi::*;
        unsafe {
            let factory: IDXGIFactory4 = CreateDXGIFactory1().expect("DXGI factory");
            let Ok(adapter) = factory.EnumAdapters1(0) else { return };
            if adapter.GetDesc1().is_ok_and(|d| d.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0) {
                return;
            }
            let mut dev: Option<ID3D12Device> = None;
            let _ = D3D12CreateDevice(&adapter, D3D_FEATURE_LEVEL_11_0, &mut dev);
            let Some(dev) = dev.filter(|d| tier2(d)) else { return };
            super::super::real::adopt(dev.as_raw(), super::super::real::Kind::Device);
            let memory: IDXGIAdapter3 = adapter.cast().unwrap();
            let local = || {
                let mut i = DXGI_QUERY_VIDEO_MEMORY_INFO::default();
                let _ = memory.QueryVideoMemoryInfo(0, DXGI_MEMORY_SEGMENT_GROUP_LOCAL, &mut i);
                i.CurrentUsage
            };
            let sample = DXGI_SAMPLE_DESC { Count: 1, Quality: 0 };
            let tex = |w: u64, h: u32| D3D12_RESOURCE_DESC { Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D, Width: w, Height: h, DepthOrArraySize: 1, MipLevels: 1, Format: DXGI_FORMAT_R32G8X24_TYPELESS, SampleDesc: sample, Flags: D3D12_RESOURCE_FLAG_ALLOW_DEPTH_STENCIL, ..Default::default() };
            let ds = D3D12_DEPTH_STENCIL_VALUE { Depth: 1.0, Stencil: 0 };
            let clear = D3D12_CLEAR_VALUE { Format: DXGI_FORMAT_D32_FLOAT_S8X24_UINT, Anonymous: D3D12_CLEAR_VALUE_0 { DepthStencil: ds } };
            let riid = &ID3D12Resource::IID as *const windows::core::GUID as *const windows_sys::core::GUID;
            let place_at = |va: usize, d: &D3D12_RESOURCE_DESC| {
                let mut p = std::ptr::null_mut();
                let hr = place(dev.as_raw(), va, d, D3D12_RESOURCE_STATE_DEPTH_WRITE.0 as u32, &clear as *const _ as *const c_void, None, riid, &mut p);
                assert!(hr.is_some_and(|hr| hr >= 0) && !p.is_null(), "D32S8 target at {va:#x}");
                ID3D12Resource::from_raw(p)
            };

            // 14 4K targets over 3.5 GB of addresses
            let big = tex(3840, 2160);
            let size = dev.GetResourceAllocationInfo(0, &[big]).SizeInBytes as usize;
            let (used0, arenas0) = (local(), arena_bytes().0);
            let targets: Vec<ID3D12Resource> = (0..14).map(|k| place_at(0x40_0000_0000 + k * 256 * MIB, &big)).collect();
            let (used, arenas) = (local().saturating_sub(used0) as usize, arena_bytes().0 - arenas0);
            println!("14 D32S8 3840x2160 targets ({size} bytes each): {arenas} bytes of arenas, {used} bytes more local video memory");
            assert!(arenas <= 14 * size.next_multiple_of(PAGE), "{arenas} bytes of arenas");
            assert!(used < 14 * 256 * MIB, "{used} bytes of video memory");
            drop(targets);

            // clear one, copy it to another, read both planes back
            let small = tex(64, 64);
            let (a, b) = (place_at(0x48_0000_0000, &small), place_at(0x48_1000_0000, &small));
            let dsv_heap: ID3D12DescriptorHeap = dev.CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC { Type: D3D12_DESCRIPTOR_HEAP_TYPE_DSV, NumDescriptors: 1, ..Default::default() }).unwrap();
            let dsv = dsv_heap.GetCPUDescriptorHandleForHeapStart();
            let view = D3D12_DEPTH_STENCIL_VIEW_DESC { Format: DXGI_FORMAT_D32_FLOAT_S8X24_UINT, ViewDimension: D3D12_DSV_DIMENSION_TEXTURE2D, ..Default::default() };
            dev.CreateDepthStencilView(&a, Some(&view), dsv);
            let mut layouts = [D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default(); 2];
            let mut total = 0u64;
            dev.GetCopyableFootprints(&small, 0, 2, 0, Some(layouts.as_mut_ptr()), None, None, Some(&mut total));
            let buffer = D3D12_RESOURCE_DESC { Dimension: D3D12_RESOURCE_DIMENSION_BUFFER, Width: total, Height: 1, DepthOrArraySize: 1, MipLevels: 1, SampleDesc: sample, Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR, ..Default::default() };
            let mut readback: Option<ID3D12Resource> = None;
            let heap = D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_READBACK, ..Default::default() };
            dev.CreateCommittedResource(&heap, D3D12_HEAP_FLAG_NONE, &buffer, D3D12_RESOURCE_STATE_COPY_DEST, None, &mut readback).unwrap();
            let readback = readback.unwrap();
            let queue: ID3D12CommandQueue = dev.CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC::default()).unwrap();
            let alloc: ID3D12CommandAllocator = dev.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT).unwrap();
            let list: ID3D12GraphicsCommandList = dev.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &alloc, None).unwrap();
            let barrier = |r: &ID3D12Resource, from: D3D12_RESOURCE_STATES, to: D3D12_RESOURCE_STATES| {
                let t = D3D12_RESOURCE_TRANSITION_BARRIER { pResource: std::mem::transmute_copy(r), Subresource: D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES, StateBefore: from, StateAfter: to };
                D3D12_RESOURCE_BARRIER { Type: D3D12_RESOURCE_BARRIER_TYPE_TRANSITION, Anonymous: D3D12_RESOURCE_BARRIER_0 { Transition: std::mem::ManuallyDrop::new(t) }, ..Default::default() }
            };
            list.ClearDepthStencilView(dsv, D3D12_CLEAR_FLAG_DEPTH | D3D12_CLEAR_FLAG_STENCIL, 0.25, 0x5A, None);
            list.ResourceBarrier(&[barrier(&a, D3D12_RESOURCE_STATE_DEPTH_WRITE, D3D12_RESOURCE_STATE_COPY_SOURCE), barrier(&b, D3D12_RESOURCE_STATE_DEPTH_WRITE, D3D12_RESOURCE_STATE_COPY_DEST)]);
            list.CopyResource(&b, &a);
            list.ResourceBarrier(&[barrier(&b, D3D12_RESOURCE_STATE_COPY_DEST, D3D12_RESOURCE_STATE_COPY_SOURCE)]);
            for (plane, layout) in layouts.iter().enumerate() {
                let dst = D3D12_TEXTURE_COPY_LOCATION { pResource: std::mem::transmute_copy(&readback), Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT, Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { PlacedFootprint: *layout } };
                let src = D3D12_TEXTURE_COPY_LOCATION { pResource: std::mem::transmute_copy(&b), Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX, Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: plane as u32 } };
                list.CopyTextureRegion(&dst, 0, 0, 0, &src, None);
            }
            list.Close().unwrap();
            queue.ExecuteCommandLists(&[Some(list.cast().unwrap())]);
            let fence: ID3D12Fence = dev.CreateFence(0, D3D12_FENCE_FLAG_NONE).unwrap();
            queue.Signal(&fence, 1).unwrap();
            // no event: returns when the fence has the value
            fence.SetEventOnCompletion(1, windows::Win32::Foundation::HANDLE::default()).unwrap();
            let mut p = std::ptr::null_mut();
            readback.Map(0, None, Some(&mut p)).unwrap();
            let bytes = std::slice::from_raw_parts(p as *const u8, total as usize);
            let row = |plane: usize, y: usize| layouts[plane].Offset as usize + y * layouts[plane].Footprint.RowPitch as usize;
            for y in [0, 31, 63] {
                let depth = f32::from_le_bytes(bytes[row(0, y)..row(0, y) + 4].try_into().unwrap());
                assert_eq!((depth, bytes[row(1, y)]), (0.25, 0x5A), "row {y}: depth, stencil");
            }
            readback.Unmap(0, None);
            drop((a, b));

            // 2 s later the arenas no target uses are let go: one new target's is all that is left
            std::thread::sleep(IDLE + Duration::from_millis(100));
            let last = place_at(0x50_0000_0000, &small);
            let small_size = dev.GetResourceAllocationInfo(0, &[small]).SizeInBytes as usize;
            assert_eq!(arena_bytes().0 - arenas0, small_size.next_multiple_of(PAGE));
            drop(last);
        }
    }

    /// Reproduces the multi-pool streaming churn pattern on the active D3D12 driver (comparing
    /// scattered cross-heap `UpdateTileMappings` against single-heap contiguous
    /// `UpdateTileMappings`, both with `FLAG_NONE` and per-texture `Signal`):
    /// `cargo test -p xcl --lib -- --ignored --exact hle::d3d12x::vram::tests::churn_update_tile_mappings_timing --nocapture`
    #[test]
    #[ignore]
    fn churn_update_tile_mappings_timing() {
        let _vram_guard = VRAM_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_11_0;
        use windows::Win32::Graphics::Dxgi::Common::*;
        unsafe {
            let mut dev: Option<ID3D12Device> = None;
            D3D12CreateDevice(None, D3D_FEATURE_LEVEL_11_0, &mut dev).expect("D3D12 device");
            let dev = dev.unwrap();
            let queue: ID3D12CommandQueue = dev.CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC::default()).unwrap();
            let fence: ID3D12Fence = dev.CreateFence(0, D3D12_FENCE_FLAG_NONE).unwrap();
            const HEAPS: usize = 64;
            const TILES_PER_TEX: usize = 32; // 2 MiB tiled texture
            const TEX_COUNT: usize = 256;
            let heap_desc = D3D12_HEAP_DESC {
                SizeInBytes: 64 * PAGE as u64,
                Properties: D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_DEFAULT, ..Default::default() },
                Alignment: 0,
                Flags: D3D12_HEAP_FLAG_NONE,
            };
            let mut heaps = Vec::with_capacity(HEAPS);
            for _ in 0..HEAPS {
                let mut h: Option<ID3D12Heap> = None;
                dev.CreateHeap(&heap_desc, &mut h).unwrap();
                heaps.push(h.unwrap());
            }
            let tex_desc = D3D12_RESOURCE_DESC {
                Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
                Width: 1024,
                Height: 512,
                DepthOrArraySize: 1,
                MipLevels: 1,
                Format: DXGI_FORMAT_R8G8B8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Layout: D3D12_TEXTURE_LAYOUT_64KB_UNDEFINED_SWIZZLE,
                Flags: D3D12_RESOURCE_FLAG_NONE,
                ..Default::default()
            };
            let make_textures = || -> Vec<ID3D12Resource> {
                (0..TEX_COUNT)
                    .map(|_| {
                        let mut r: Option<ID3D12Resource> = None;
                        dev.CreateReservedResource(&tex_desc, D3D12_RESOURCE_STATE_COMMON, None, &mut r).unwrap();
                        r.unwrap()
                    })
                    .collect()
            };

            // BEFORE pattern: flat-FIFO cooling scatters each 32-tile texture across 32 heaps,
            // calling UpdateTileMappings(FLAG_NONE) per heap + Signal per texture.
            let old_res = make_textures();
            let mut fval = 0u64;
            let t0 = Instant::now();
            let mut old_calls = 0usize;
            for (k, r) in old_res.iter().enumerate() {
                let tiles: Vec<Option<Tile>> = (0..TILES_PER_TEX)
                    .map(|i| Some((((k + i) % HEAPS) as u32, (i % 64) as u32)))
                    .collect();
                let mut hs: Vec<u32> = tiles.iter().filter_map(|t| t.map(|x| x.0)).collect();
                hs.sort_unstable();
                hs.dedup();
                for (pass, h) in hs.into_iter().enumerate() {
                    let ranges = heap_ranges(h, &tiles, pass == 0);
                    let flags: Vec<_> = ranges.iter().map(|r| r.parts().0).collect();
                    let offsets: Vec<_> = ranges.iter().map(|r| r.parts().1).collect();
                    let counts: Vec<_> = ranges.iter().map(|r| r.parts().2).collect();
                    let coord = D3D12_TILED_RESOURCE_COORDINATE::default();
                    let region = D3D12_TILE_REGION_SIZE { NumTiles: tiles.len() as u32, UseBox: false.into(), ..Default::default() };
                    queue.UpdateTileMappings(r, 1, Some(&coord), Some(&region), &heaps[h as usize], ranges.len() as u32, Some(flags.as_ptr()), Some(offsets.as_ptr()), Some(counts.as_ptr()), D3D12_TILE_MAPPING_FLAG_NONE);
                    old_calls += 1;
                }
                fval += 1;
                queue.Signal(&fence, fval).unwrap();
            }
            let old_submit = t0.elapsed();
            fence.SetEventOnCompletion(fval, windows::Win32::Foundation::HANDLE::default()).unwrap();
            let old_total = t0.elapsed();

            // AFTER pattern: per-heap contiguous TilePool -> 1 heap, 1 contiguous range,
            // D3D12_TILE_MAPPING_FLAG_NONE, per-texture Signal.
            let new_res = make_textures();
            let t1 = Instant::now();
            let mut new_calls = 0usize;
            for (k, r) in new_res.iter().enumerate() {
                let h = k % HEAPS;
                let coord = D3D12_TILED_RESOURCE_COORDINATE::default();
                let region = D3D12_TILE_REGION_SIZE { NumTiles: TILES_PER_TEX as u32, UseBox: false.into(), ..Default::default() };
                let flag = D3D12_TILE_RANGE_FLAG_NONE;
                let off = 0u32;
                let cnt = TILES_PER_TEX as u32;
                queue.UpdateTileMappings(r, 1, Some(&coord), Some(&region), &heaps[h], 1, Some(&flag), Some(&off), Some(&cnt), D3D12_TILE_MAPPING_FLAG_NONE);
                new_calls += 1;
                fval += 1;
                queue.Signal(&fence, fval).unwrap();
            }
            let new_submit = t1.elapsed();
            fence.SetEventOnCompletion(fval, windows::Win32::Foundation::HANDLE::default()).unwrap();
            let new_total = t1.elapsed();

            println!(
                "BEFORE ({TEX_COUNT} x 2MiB textures, {old_calls} UpdateTileMappings + {TEX_COUNT} Signals): submit={old_submit:?} gpu_done={old_total:?}"
            );
            println!(
                "AFTER  ({TEX_COUNT} x 2MiB textures, {new_calls} UpdateTileMappings + {TEX_COUNT} Signals): submit={new_submit:?} gpu_done={new_total:?}"
            );
            assert!(new_calls < old_calls);
        }
    }

    /// A texture with lazy levels (`XCL_TEXTURE_LEVEL_PAGES=1`) whose top mips were left out:
    /// those levels never get pages, the others do once, and the packed tail with them.
    #[test]
    fn lazy_levels_never_map_the_top_mips_left_out() {
        // 4 levels with tiles of their own (16, 4, 1, 1 tiles), then a packed tail of 1 tile
        let mut lv = Levels::new(&[(0, 16), (16, 4), (20, 1), (21, 1)], Some((22, 1)), 23, 8).unwrap();
        let all: Vec<usize> = (0..lv.groups.len()).collect();
        assert_eq!(groups_to_map(&lv, all.clone(), 0), vec![0, 1, 2, 3, 4]);
        assert_eq!(groups_to_map(&lv, all.clone(), 2), vec![2, 3, 4], "levels 0 and 1 left out");
        lv.groups[2].3 = true;
        assert_eq!(groups_to_map(&lv, all.clone(), 2), vec![3, 4], "a group with pages is not mapped again");
        assert_eq!(groups_to_map(&lv, vec![0, 1], 2), Vec::<usize>::new(), "an upload of the left-out levels maps nothing");
    }

    /// The pages of left-out tiles are keyed as `Pages::bind` keys them, so two textures at one
    /// address name the same pages (and `skipped_textures` counts them once).
    #[test]
    fn left_out_tiles_name_the_pages_bind_would_give_them() {
        let (va, total) = (0x10_0000_0000usize, 6u32);
        let skip = [true, true, false, false, false, false];
        let a = skipped_pages(va, None, total, &skip);
        assert_eq!(a, vec![(va, usize::MAX), (va + PAGE, usize::MAX)]);
        assert_eq!(skipped_pages(va, None, total, &skip), a, "a second texture there: the same pages");
        // a texture covering 2 pages of its own (from the next 64 KiB boundary): the rest are extra tiles
        let at = va + 0x1000;
        let b = skipped_pages(at, Some((2 * PAGE) as u64), total, &[false, true, true, false, false, false]);
        assert_eq!(b, vec![(va + 2 * PAGE, usize::MAX), (at, 2)]);
        assert!(skipped_pages(va, None, total, &[false; 6]).is_empty());
    }

    #[test]
    fn the_cap_follows_how_full_video_memory_is() {
        assert_eq!(cap_for_fill(0.0), None);
        assert_eq!(cap_for_fill(0.749), None);
        assert_eq!(cap_for_fill(0.75), Some(2048));
        assert_eq!(cap_for_fill(0.85), Some(1024));
        assert_eq!(cap_for_fill(0.92), Some(512));
        assert_eq!(cap_for_fill(1.3), Some(512));
    }

    #[test]
    fn top_mips_go_until_the_largest_side_fits_and_one_tiled_mip_stays() {
        assert_eq!(mips_to_skip(4096, 13, 2048), 1);
        assert_eq!(mips_to_skip(4096, 13, 1024), 2);
        assert_eq!(mips_to_skip(4096, 13, 4096), 0);
        assert_eq!(mips_to_skip(2048, 13, 2048), 0);
        assert_eq!(mips_to_skip(4096, 13, 512), 3);
        // never into the packed tail: the last mip with tiles of its own stays
        assert_eq!(mips_to_skip(4096, 2, 256), 1);
        assert_eq!(mips_to_skip(4096, 1, 256), 0);
    }

    /// A reserved texture bound without its top mip on the pages, the way a sampled texture is
    /// when video memory fills up: the top mip takes no page, sampling through a clamped view
    /// reads the first mip that has memory, and the clamp counts from the resource's first mip,
    /// not the view's (a view from mip 1 clamped to 2 reads mip 2, not mip 3).
    #[test]
    fn gpu_a_texture_without_its_top_mip_samples_the_first_it_has() {
        use std::mem::{transmute_copy, ManuallyDrop};
        use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_12_0;
        use windows::Win32::Graphics::Dxgi::Common::*;
        use windows::Win32::Graphics::Dxgi::*;
        let _vram_guard = VRAM_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let skip = crate::gpu::recomp::runner::skip_test;
        let dev = unsafe {
            let Ok(factory) = CreateDXGIFactory2::<IDXGIFactory6>(DXGI_CREATE_FACTORY_FLAGS(0)) else { return skip("no DXGI") };
            let sel = crate::gpu::adapter::Selector::from_env_or("auto");
            let Some((a, d)) = crate::gpu::adapter::candidates(&factory, &sel).into_iter().next() else { return skip("no GPU") };
            let Ok(dev) = crate::gpu::agility::create_device(&a, D3D_FEATURE_LEVEL_12_0) else { return skip("no GPU") };
            println!("adapter: {}", crate::gpu::adapter::name(&d));
            dev
        };
        if !tier2(&dev) || crate::gpu::recomp::dxc::location().is_err() {
            return skip("no tiled resources tier 2 or no DXC");
        }
        let src = r#"
#define RS "RootFlags(CBV_SRV_UAV_HEAP_DIRECTLY_INDEXED), UAV(u0), StaticSampler(s0, filter=FILTER_MIN_MAG_MIP_POINT)"
RWByteAddressBuffer outb : register(u0);
SamplerState pt : register(s0);
[RootSignature(RS)]
[numthreads(1, 1, 1)]
void main() {
    for (uint i = 0; i < 3; i++) {
        Texture2D<float4> t = ResourceDescriptorHeap[i];
        outb.Store(4 * i, uint(t.SampleLevel(pt, float2(0.5, 0.5), 0.0).r * 255.0 + 0.5));
    }
}
"#;
        const MIPS: u16 = 4;
        let shade = |mip: u32| 0x11 * (mip + 1); // mip 0 → 0x11 … mip 3 → 0x44
        unsafe {
            let desc = D3D12_RESOURCE_DESC {
                Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
                Width: 512,
                Height: 512,
                DepthOrArraySize: 1,
                MipLevels: MIPS,
                Format: DXGI_FORMAT_R8G8B8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Layout: D3D12_TEXTURE_LAYOUT_64KB_UNDEFINED_SWIZZLE,
                ..Default::default()
            };
            let mut tex: Option<ID3D12Resource> = None;
            dev.CreateReservedResource(&desc, D3D12_RESOURCE_STATE_COPY_DEST, None, &mut tex).unwrap();
            let tex = tex.unwrap();
            let mut total = 0u32;
            let mut packed = D3D12_PACKED_MIP_INFO::default();
            dev.GetResourceTiling(&tex, Some(&mut total), Some(&mut packed), None, None, 0, std::ptr::null_mut());
            let shape = TileShape::from_tiling(MIPS, 1, total, &packed);
            let (standard, tilings) = mip_tilings(&dev, &tex, &desc);
            assert!(standard >= 3, "512x512 RGBA8 has its first mips tiled ({standard})");
            let mask = top_mip_mask(&desc, &tilings, total, 1);
            let mip0 = u32::from(tilings[0].WidthInTiles) * u32::from(tilings[0].HeightInTiles);
            assert_eq!(mask.iter().filter(|&&m| m).count() as u32, mip0, "exactly mip 0's tiles");
            assert!(mask.iter().take(mip0 as usize).all(|&m| m));

            let mut pages = Pages::new(&dev).unwrap();
            let (used, extra) = pages.bind(&dev, &tex, 0x10_0000_0000, None, total, shape, &[], &mask).unwrap();
            assert_eq!((used.len() + extra.len()) as u32, total - mip0, "mip 0 takes no page");

            // the title's upload of every mip: mip 0's goes nowhere
            let mut layouts = vec![D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default(); MIPS as usize];
            let mut rows = vec![0u32; MIPS as usize];
            let mut row_bytes = vec![0u64; MIPS as usize];
            let mut upload_bytes = 0u64;
            dev.GetCopyableFootprints(&desc, 0, u32::from(MIPS), 0, Some(layouts.as_mut_ptr()), Some(rows.as_mut_ptr()), Some(row_bytes.as_mut_ptr()), Some(&mut upload_bytes));
            let buffer = |size: u64, heap: D3D12_HEAP_TYPE, state: D3D12_RESOURCE_STATES, uav: bool| {
                let d = D3D12_RESOURCE_DESC {
                    Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
                    Width: size,
                    Height: 1,
                    DepthOrArraySize: 1,
                    MipLevels: 1,
                    SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                    Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
                    Flags: if uav { D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS } else { D3D12_RESOURCE_FLAG_NONE },
                    ..Default::default()
                };
                let mut r: Option<ID3D12Resource> = None;
                dev.CreateCommittedResource(&D3D12_HEAP_PROPERTIES { Type: heap, ..Default::default() }, D3D12_HEAP_FLAG_NONE, &d, state, None, &mut r).unwrap();
                r.unwrap()
            };
            let upload = buffer(upload_bytes, D3D12_HEAP_TYPE_UPLOAD, D3D12_RESOURCE_STATE_GENERIC_READ, false);
            let mut p = std::ptr::null_mut();
            upload.Map(0, None, Some(&mut p)).unwrap();
            for (m, l) in layouts.iter().enumerate() {
                for r in 0..rows[m] as usize {
                    let at = (p as *mut u8).add(l.Offset as usize + r * l.Footprint.RowPitch as usize) as *mut u32;
                    for x in 0..l.Footprint.Width as usize {
                        at.add(x).write_unaligned(0xFF00_0000 | shade(m as u32));
                    }
                }
            }
            upload.Unmap(0, None);

            let dxil = crate::gpu::recomp::dxc::compile(src, "main", "cs_6_6", &[]).unwrap();
            let rs: ID3D12RootSignature = dev.CreateRootSignature(0, &dxil).unwrap();
            let pso: ID3D12PipelineState = dev
                .CreateComputePipelineState(&D3D12_COMPUTE_PIPELINE_STATE_DESC {
                    pRootSignature: transmute_copy(&rs),
                    CS: D3D12_SHADER_BYTECODE { pShaderBytecode: dxil.as_ptr() as _, BytecodeLength: dxil.len() },
                    ..Default::default()
                })
                .unwrap();
            let dh: ID3D12DescriptorHeap = dev
                .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC { Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV, NumDescriptors: 3, Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE, NodeMask: 0 })
                .unwrap();
            let inc = dev.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV) as usize;
            let view = |first: u32, skipped: u32| {
                let mut v = D3D12_SHADER_RESOURCE_VIEW_DESC {
                    Format: desc.Format,
                    ViewDimension: D3D12_SRV_DIMENSION_TEXTURE2D,
                    Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
                    Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 { Texture2D: D3D12_TEX2D_SRV { MostDetailedMip: first, MipLevels: u32::MAX, ..Default::default() } },
                };
                if skipped > 0 {
                    clamp_skipped_mips(&mut v, skipped);
                }
                v
            };
            // 0: the whole texture, its top mip left out; 1: a view from mip 1 of a texture
            // without two; 2: the whole texture unclamped (mip 0 has no memory: zero)
            for (i, v) in [view(0, 1), view(1, 2), view(0, 0)].iter().enumerate() {
                let at = D3D12_CPU_DESCRIPTOR_HANDLE { ptr: dh.GetCPUDescriptorHandleForHeapStart().ptr + i * inc };
                dev.CreateShaderResourceView(&tex, Some(v), at);
            }
            let out = buffer(256, D3D12_HEAP_TYPE_DEFAULT, D3D12_RESOURCE_STATE_UNORDERED_ACCESS, true);
            let rb = buffer(256, D3D12_HEAP_TYPE_READBACK, D3D12_RESOURCE_STATE_COPY_DEST, false);
            let queue: ID3D12CommandQueue = dev.CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC::default()).unwrap();
            queue.Wait(&pages.fence, pages.value).unwrap();
            let alloc: ID3D12CommandAllocator = dev.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT).unwrap();
            let list: ID3D12GraphicsCommandList = dev.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &alloc, &pso).unwrap();
            for (m, l) in layouts.iter().enumerate() {
                let dst = D3D12_TEXTURE_COPY_LOCATION { pResource: transmute_copy(&tex), Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX, Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: m as u32 } };
                let src = D3D12_TEXTURE_COPY_LOCATION { pResource: transmute_copy(&upload), Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT, Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { PlacedFootprint: *l } };
                list.CopyTextureRegion(&dst, 0, 0, 0, &src, None);
            }
            let barrier = |r: &ID3D12Resource, before: D3D12_RESOURCE_STATES, after: D3D12_RESOURCE_STATES| D3D12_RESOURCE_BARRIER {
                Type: D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
                Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
                Anonymous: D3D12_RESOURCE_BARRIER_0 {
                    Transition: ManuallyDrop::new(D3D12_RESOURCE_TRANSITION_BARRIER { pResource: transmute_copy(r), Subresource: D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES, StateBefore: before, StateAfter: after }),
                },
            };
            list.ResourceBarrier(&[barrier(&tex, D3D12_RESOURCE_STATE_COPY_DEST, D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE)]);
            list.SetDescriptorHeaps(&[Some(dh.clone())]);
            list.SetComputeRootSignature(&rs);
            list.SetComputeRootUnorderedAccessView(0, out.GetGPUVirtualAddress());
            list.Dispatch(1, 1, 1);
            list.ResourceBarrier(&[barrier(&out, D3D12_RESOURCE_STATE_UNORDERED_ACCESS, D3D12_RESOURCE_STATE_COPY_SOURCE)]);
            list.CopyBufferRegion(&rb, 0, &out, 0, 256);
            list.Close().unwrap();
            queue.ExecuteCommandLists(&[Some(list.cast().unwrap())]);
            let fence: ID3D12Fence = dev.CreateFence(0, D3D12_FENCE_FLAG_NONE).unwrap();
            queue.Signal(&fence, 1).unwrap();
            crate::gpu::fence::wait(&fence, 1, &dev, None, 20_000).map_err(|e| format!("{e}")).unwrap();
            let mut q = std::ptr::null_mut();
            rb.Map(0, None, Some(&mut q)).unwrap();
            let got = std::slice::from_raw_parts(q as *const u32, 3).to_vec();
            rb.Unmap(0, None);
            assert_eq!(got, vec![shade(1), shade(2), 0], "clamped to mip 1; clamped from the resource's mip 0 (mip 2, not 3); mip 0 unmapped reads zero");
        }
    }

    /// The picture a texture without its top mip gives, filtered: 64x64 samples with trilinear and
    /// 16x anisotropic filtering, over level of detail -3 to +3 and anisotropy 1 to 16 (gradients
    /// given), from two copies of one 256x256 texture with nine mips. In both, mip 0 holds a
    /// poison colour (red) that no other mip has. One copy has mip 0 left out (no pages) and its
    /// view clamped; the other has every mip in memory and the same clamp. The two pictures are
    /// the same to the bit, and neither has any red: the left-out mip adds nothing to any pixel
    /// (not even a filter tap reading zeros), so the texture looks the way the clamp alone makes
    /// it look. Without the clamp the same samples do show red (the picture reaches mip 0).
    #[test]
    fn gpu_a_texture_without_its_top_mip_filters_like_the_clamp_alone() {
        use std::mem::{transmute_copy, ManuallyDrop};
        use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_12_0;
        use windows::Win32::Graphics::Dxgi::Common::*;
        use windows::Win32::Graphics::Dxgi::*;
        let _vram_guard = VRAM_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let skip = crate::gpu::recomp::runner::skip_test;
        let dev = unsafe {
            let Ok(factory) = CreateDXGIFactory2::<IDXGIFactory6>(DXGI_CREATE_FACTORY_FLAGS(0)) else { return skip("no DXGI") };
            let sel = crate::gpu::adapter::Selector::from_env_or("auto");
            let Some((a, d)) = crate::gpu::adapter::candidates(&factory, &sel).into_iter().next() else { return skip("no GPU") };
            let Ok(dev) = crate::gpu::agility::create_device(&a, D3D_FEATURE_LEVEL_12_0) else { return skip("no GPU") };
            println!("adapter: {}", crate::gpu::adapter::name(&d));
            dev
        };
        if !tier2(&dev) || crate::gpu::recomp::dxc::location().is_err() {
            return skip("no tiled resources tier 2 or no DXC");
        }
        // views 0..3 (left out + clamp, all in memory + clamp, all in memory unclamped) x samplers
        // (trilinear, anisotropic 16x): six pictures of N x N, RGBA8 packed
        const N: u32 = 64;
        let src = r#"
#define RS "RootFlags(CBV_SRV_UAV_HEAP_DIRECTLY_INDEXED), UAV(u0), StaticSampler(s0, filter=FILTER_MIN_MAG_MIP_LINEAR), StaticSampler(s1, filter=FILTER_ANISOTROPIC, maxAnisotropy=16)"
RWByteAddressBuffer outb : register(u0);
SamplerState tri : register(s0);
SamplerState aniso : register(s1);
uint pack(float4 c) {
    uint4 b = uint4(saturate(c) * 255.0 + 0.5);
    return b.r | (b.g << 8) | (b.b << 16) | (b.a << 24);
}
[RootSignature(RS)]
[numthreads(8, 8, 1)]
void main(uint3 id : SV_DispatchThreadID) {
    const uint N = 64;
    float2 uv = (float2(id.xy) + 0.5) / N;
    // level of detail -3 .. +3 down the picture, anisotropy 1 .. 16 across it
    float scale = exp2(lerp(-3.0, 3.0, id.y / float(N - 1))) / 256.0;
    float ratio = exp2(4.0 * id.x / float(N - 1));
    // every other column turned 45 degrees: the footprint is not only along the axes
    float2 major = (id.x & 1) ? float2(0.7071, 0.7071) : float2(1.0, 0.0);
    float2 minor = float2(-major.y, major.x);
    float2 ddx = major * scale * ratio;
    float2 ddy = minor * scale;
    for (uint v = 0; v < 3; v++) {
        Texture2D<float4> t = ResourceDescriptorHeap[v];
        uint at = ((v * 2) * N * N + id.y * N + id.x) * 4;
        outb.Store(at, pack(t.SampleGrad(tri, uv, ddx, ddy)));
        outb.Store(at + N * N * 4, pack(t.SampleGrad(aniso, uv, ddx, ddy)));
    }
}
"#;
        const MIPS: u16 = 9;
        const PICTURES: usize = 6;
        let bytes = (PICTURES as u32 * N * N * 4) as u64;
        // mip 0: red (the poison); mips 1..8: no red, a checker of 4x4 texels in green and blue
        // that differs from mip to mip, so filtering between mips changes the picture
        let texel = |mip: u32, x: u32, y: u32| -> u32 {
            if mip == 0 {
                return 0xFF00_00FF;
            }
            let odd = ((x / 4) + (y / 4)) & 1;
            let g = if odd == 1 { 255 - 24 * mip } else { 24 * mip };
            let b = if odd == 1 { 40 } else { 200 };
            0xFF00_0000 | (b << 16) | (g << 8)
        };
        unsafe {
            let desc = D3D12_RESOURCE_DESC {
                Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
                Width: 256,
                Height: 256,
                DepthOrArraySize: 1,
                MipLevels: MIPS,
                Format: DXGI_FORMAT_R8G8B8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Layout: D3D12_TEXTURE_LAYOUT_64KB_UNDEFINED_SWIZZLE,
                ..Default::default()
            };
            let reserved = || {
                let mut t: Option<ID3D12Resource> = None;
                dev.CreateReservedResource(&desc, D3D12_RESOURCE_STATE_COPY_DEST, None, &mut t).unwrap();
                t.unwrap()
            };
            let (left_out, whole) = (reserved(), reserved());
            let mut total = 0u32;
            let mut packed = D3D12_PACKED_MIP_INFO::default();
            dev.GetResourceTiling(&left_out, Some(&mut total), Some(&mut packed), None, None, 0, std::ptr::null_mut());
            let shape = TileShape::from_tiling(MIPS, 1, total, &packed);
            let (standard, tilings) = mip_tilings(&dev, &left_out, &desc);
            assert!(standard >= 1, "256x256 RGBA8 has mip 0 tiled ({standard})");
            let mask = top_mip_mask(&desc, &tilings, total, 1);
            let mip0 = u32::from(tilings[0].WidthInTiles) * u32::from(tilings[0].HeightInTiles);
            assert_eq!(mask.iter().filter(|&&m| m).count() as u32, mip0, "exactly mip 0's tiles left out");

            let mut pages = Pages::new(&dev).unwrap();
            let (used, extra) = pages.bind(&dev, &left_out, 0x10_0000_0000, None, total, shape, &[], &mask).unwrap();
            assert_eq!((used.len() + extra.len()) as u32, total - mip0, "mip 0 takes no page");
            pages.bind(&dev, &whole, 0x20_0000_0000, None, total, shape, &[], &vec![false; total as usize]).unwrap();

            let mut layouts = vec![D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default(); MIPS as usize];
            let mut rows = vec![0u32; MIPS as usize];
            let mut row_bytes = vec![0u64; MIPS as usize];
            let mut upload_bytes = 0u64;
            dev.GetCopyableFootprints(&desc, 0, u32::from(MIPS), 0, Some(layouts.as_mut_ptr()), Some(rows.as_mut_ptr()), Some(row_bytes.as_mut_ptr()), Some(&mut upload_bytes));
            let buffer = |size: u64, heap: D3D12_HEAP_TYPE, state: D3D12_RESOURCE_STATES, uav: bool| {
                let d = D3D12_RESOURCE_DESC {
                    Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
                    Width: size,
                    Height: 1,
                    DepthOrArraySize: 1,
                    MipLevels: 1,
                    SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                    Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
                    Flags: if uav { D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS } else { D3D12_RESOURCE_FLAG_NONE },
                    ..Default::default()
                };
                let mut r: Option<ID3D12Resource> = None;
                dev.CreateCommittedResource(&D3D12_HEAP_PROPERTIES { Type: heap, ..Default::default() }, D3D12_HEAP_FLAG_NONE, &d, state, None, &mut r).unwrap();
                r.unwrap()
            };
            let upload = buffer(upload_bytes, D3D12_HEAP_TYPE_UPLOAD, D3D12_RESOURCE_STATE_GENERIC_READ, false);
            let mut p = std::ptr::null_mut();
            upload.Map(0, None, Some(&mut p)).unwrap();
            for (m, l) in layouts.iter().enumerate() {
                for y in 0..rows[m] {
                    let at = (p as *mut u8).add(l.Offset as usize + y as usize * l.Footprint.RowPitch as usize) as *mut u32;
                    for x in 0..l.Footprint.Width {
                        at.add(x as usize).write_unaligned(texel(m as u32, x, y));
                    }
                }
            }
            upload.Unmap(0, None);

            let dxil = crate::gpu::recomp::dxc::compile(src, "main", "cs_6_6", &[]).unwrap();
            let rs: ID3D12RootSignature = dev.CreateRootSignature(0, &dxil).unwrap();
            let pso: ID3D12PipelineState = dev
                .CreateComputePipelineState(&D3D12_COMPUTE_PIPELINE_STATE_DESC {
                    pRootSignature: transmute_copy(&rs),
                    CS: D3D12_SHADER_BYTECODE { pShaderBytecode: dxil.as_ptr() as _, BytecodeLength: dxil.len() },
                    ..Default::default()
                })
                .unwrap();
            let dh: ID3D12DescriptorHeap = dev
                .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC { Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV, NumDescriptors: 3, Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE, NodeMask: 0 })
                .unwrap();
            let inc = dev.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV) as usize;
            let view = |skipped: u32| {
                let mut v = D3D12_SHADER_RESOURCE_VIEW_DESC {
                    Format: desc.Format,
                    ViewDimension: D3D12_SRV_DIMENSION_TEXTURE2D,
                    Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
                    Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 { Texture2D: D3D12_TEX2D_SRV { MostDetailedMip: 0, MipLevels: u32::MAX, ..Default::default() } },
                };
                if skipped > 0 {
                    clamp_skipped_mips(&mut v, skipped);
                }
                v
            };
            for (i, (t, v)) in [(&left_out, view(1)), (&whole, view(1)), (&whole, view(0))].into_iter().enumerate() {
                let at = D3D12_CPU_DESCRIPTOR_HANDLE { ptr: dh.GetCPUDescriptorHandleForHeapStart().ptr + i * inc };
                dev.CreateShaderResourceView(t, Some(&v), at);
            }
            let out = buffer(bytes, D3D12_HEAP_TYPE_DEFAULT, D3D12_RESOURCE_STATE_UNORDERED_ACCESS, true);
            let rb = buffer(bytes, D3D12_HEAP_TYPE_READBACK, D3D12_RESOURCE_STATE_COPY_DEST, false);
            let queue: ID3D12CommandQueue = dev.CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC::default()).unwrap();
            queue.Wait(&pages.fence, pages.value).unwrap();
            let alloc: ID3D12CommandAllocator = dev.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT).unwrap();
            let list: ID3D12GraphicsCommandList = dev.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &alloc, &pso).unwrap();
            for t in [&left_out, &whole] {
                for (m, l) in layouts.iter().enumerate() {
                    let dst = D3D12_TEXTURE_COPY_LOCATION { pResource: transmute_copy(t), Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX, Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: m as u32 } };
                    let src = D3D12_TEXTURE_COPY_LOCATION { pResource: transmute_copy(&upload), Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT, Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { PlacedFootprint: *l } };
                    list.CopyTextureRegion(&dst, 0, 0, 0, &src, None);
                }
            }
            let barrier = |r: &ID3D12Resource, before: D3D12_RESOURCE_STATES, after: D3D12_RESOURCE_STATES| D3D12_RESOURCE_BARRIER {
                Type: D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
                Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
                Anonymous: D3D12_RESOURCE_BARRIER_0 {
                    Transition: ManuallyDrop::new(D3D12_RESOURCE_TRANSITION_BARRIER { pResource: transmute_copy(r), Subresource: D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES, StateBefore: before, StateAfter: after }),
                },
            };
            let to_read = D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE;
            list.ResourceBarrier(&[barrier(&left_out, D3D12_RESOURCE_STATE_COPY_DEST, to_read), barrier(&whole, D3D12_RESOURCE_STATE_COPY_DEST, to_read)]);
            list.SetDescriptorHeaps(&[Some(dh.clone())]);
            list.SetComputeRootSignature(&rs);
            list.SetComputeRootUnorderedAccessView(0, out.GetGPUVirtualAddress());
            list.Dispatch(N / 8, N / 8, 1);
            list.ResourceBarrier(&[barrier(&out, D3D12_RESOURCE_STATE_UNORDERED_ACCESS, D3D12_RESOURCE_STATE_COPY_SOURCE)]);
            list.CopyBufferRegion(&rb, 0, &out, 0, bytes);
            list.Close().unwrap();
            queue.ExecuteCommandLists(&[Some(list.cast().unwrap())]);
            let fence: ID3D12Fence = dev.CreateFence(0, D3D12_FENCE_FLAG_NONE).unwrap();
            queue.Signal(&fence, 1).unwrap();
            crate::gpu::fence::wait(&fence, 1, &dev, None, 20_000).map_err(|e| format!("{e}")).unwrap();
            let mut q = std::ptr::null_mut();
            rb.Map(0, None, Some(&mut q)).unwrap();
            let all = std::slice::from_raw_parts(q as *const u32, PICTURES * (N * N) as usize).to_vec();
            rb.Unmap(0, None);

            let n = (N * N) as usize;
            let picture = |v: usize, sampler: usize| &all[(v * 2 + sampler) * n..(v * 2 + sampler + 1) * n];
            let red = |px: &[u32]| px.iter().filter(|&&c| c & 0xFF != 0).count();
            for (sampler, name) in [(0, "trilinear"), (1, "anisotropic 16x")] {
                let (cut, clamped, unclamped) = (picture(0, sampler), picture(1, sampler), picture(2, sampler));
                let differ = cut.iter().zip(clamped).filter(|(a, b)| a != b).count();
                println!("{name}: {differ} of {n} pixels differ; red in: left out {}, clamp alone {}, unclamped {}", red(cut), red(clamped), red(unclamped));
                assert!(red(unclamped) > 0, "{name}: the samples reach mip 0 without the clamp (else this test shows nothing)");
                assert!(cut.iter().any(|&c| c & 0x00FF_FF00 != 0), "{name}: the texture without mip 0 is not black");
                assert_eq!(red(clamped), 0, "{name}: the clamp alone keeps mip 0 out of every pixel");
                assert_eq!(differ, 0, "{name}: without mip 0 in memory the picture is the clamp's, to the bit");
            }
        }
    }
}
