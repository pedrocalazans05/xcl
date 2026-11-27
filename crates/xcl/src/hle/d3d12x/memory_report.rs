//! Where XCL's memory goes: the parts it knows of, against what Windows and DXGI report (#2688,
//! workstream 1). One snapshot holds the process's commit, the adapter's local (video) and
//! non-local (system) memory in use against their budgets, and XCL's owners of each. What the
//! owners don't cover is shown as its own part: the title's own resources, the driver, and
//! whatever XCL doesn't count yet.

/// Which memory a part takes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    /// the adapter's own memory (DXGI local segment)
    Video,
    /// system memory the GPU reads (DXGI non-local segment: upload and readback heaps)
    NonLocal,
    /// the process's commit
    Commit,
}

/// One owner's bytes.
#[derive(Clone, Debug, PartialEq)]
pub struct Part {
    pub owner: &'static str,
    pub kind: Kind,
    pub bytes: u64,
}

/// What DXGI reports for one segment group.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Segment {
    pub usage: u64,
    pub budget: u64,
}

#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    /// the process's committed bytes (`PrivateUsage`; None where it reads 0: Wine leaves it so)
    pub commit: Option<u64>,
    /// None: no device or DXGI did not answer
    pub local: Option<Segment>,
    pub non_local: Option<Segment>,
    pub parts: Vec<Part>,
}

impl Snapshot {
    /// The bytes of the parts of `kind`.
    pub fn counted(&self, kind: Kind) -> u64 {
        self.parts.iter().filter(|p| p.kind == kind).map(|p| p.bytes).sum()
    }

    /// What the total of `kind` holds besides the parts XCL counts (negative when the parts are
    /// more than the total: a part counted on the wrong side, or memory not made yet).
    pub fn other(&self, kind: Kind) -> Option<i64> {
        let total = match kind {
            Kind::Video => self.local?.usage,
            Kind::NonLocal => self.non_local?.usage,
            Kind::Commit => self.commit?,
        };
        Some(total as i64 - self.counted(kind) as i64)
    }

    /// One log line: each total, its budget, its parts and what is left (MB).
    pub fn line(&self) -> String {
        let mb = |b: u64| b >> 20;
        let smb = |b: i64| b >> 20;
        let parts = |kind: Kind| {
            let mut s: Vec<String> = self.parts.iter().filter(|p| p.kind == kind).map(|p| format!("{} {}", p.owner, mb(p.bytes))).collect();
            if let Some(o) = self.other(kind) {
                s.push(format!("other {}", smb(o)));
            }
            s.join(", ")
        };
        let segment = |name: &str, seg: Option<Segment>, kind: Kind| match seg {
            Some(g) => format!("{name} {} of a {} budget ({})", mb(g.usage), mb(g.budget), parts(kind)),
            None => format!("{name} unknown ({})", parts(kind)),
        };
        format!(
            // (not "memory: N MB committed": the line `scripts/smoke.ps1` reads stays as it is)
            "memory by owner (MB): commit {} ({}); {}; {}",
            self.commit.map_or("unknown".into(), |c| mb(c).to_string()),
            parts(Kind::Commit),
            segment("video memory", self.local, Kind::Video),
            segment("non-local", self.non_local, Kind::NonLocal)
        )
    }
}

/// A snapshot as `report.json` keeps it (MB), with the second of the run it was taken at.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Record {
    pub seconds: f64,
    pub commit_mb: Option<u64>,
    pub video_mb: Option<u64>,
    pub video_budget_mb: Option<u64>,
    pub non_local_mb: Option<u64>,
    pub non_local_budget_mb: Option<u64>,
    /// XCL's parts: (owner, "commit" / "video" / "non-local", MB)
    pub parts: Vec<(String, String, u64)>,
    /// what each total holds besides the parts (MB; absent where the total is unknown)
    pub other_mb: std::collections::BTreeMap<String, i64>,
}

impl Snapshot {
    /// This snapshot, taken at `seconds`, as `report.json` keeps it.
    pub fn record(&self, seconds: f64) -> Record {
        let mb = |b: u64| b >> 20;
        let name = |k: Kind| match k {
            Kind::Commit => "commit",
            Kind::Video => "video",
            Kind::NonLocal => "non-local",
        };
        Record {
            seconds,
            commit_mb: self.commit.map(mb),
            video_mb: self.local.map(|s| mb(s.usage)),
            video_budget_mb: self.local.map(|s| mb(s.budget)),
            non_local_mb: self.non_local.map(|s| mb(s.usage)),
            non_local_budget_mb: self.non_local.map(|s| mb(s.budget)),
            parts: self.parts.iter().map(|p| (p.owner.to_string(), name(p.kind).to_string(), mb(p.bytes))).collect(),
            other_mb: [Kind::Commit, Kind::Video, Kind::NonLocal].into_iter().filter_map(|k| self.other(k).map(|o| (name(k).to_string(), o >> 20))).collect(),
        }
    }
}

/// The end-of-run table of `report.json`: the last snapshot the run took and the one where video
/// memory in use was highest (`XCL_ACTIVITY` takes one every 2 s; without it there are none).
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Table {
    pub last: Option<Record>,
    pub peak_video: Option<Record>,
    /// the video memory in use of `peak_video`, in bytes as DXGI read it (None where unknown):
    /// the peak is chosen on these, not on the MB, and a known reading ranks above an unknown one
    #[serde(skip)]
    peak_video_bytes: Option<u64>,
}

static TABLE: std::sync::Mutex<Table> = std::sync::Mutex::new(Table { last: None, peak_video: None, peak_video_bytes: None });

/// Keep `s`, taken at `seconds`, for the end-of-run table.
pub fn note(s: &Snapshot, seconds: f64) {
    let r = s.record(seconds);
    let mut t = TABLE.lock().unwrap_or_else(|e| e.into_inner());
    let bytes = s.local.map(|g| g.usage);
    if t.peak_video.is_none() || bytes > t.peak_video_bytes {
        t.peak_video = Some(r.clone());
        t.peak_video_bytes = bytes;
    }
    t.last = Some(r);
}

/// The table for `report.json` (taken from what `note` kept: no DXGI call here, which a crash
/// report may make from any thread).
pub fn table() -> Table {
    TABLE.try_lock().map(|t| t.clone()).unwrap_or_default()
}

/// The table for `report.json`, falling back to a single snapshot at `seconds` when `note` was
/// never called (e.g. `XCL_ACTIVITY` was unset). When `allow_dxgi` is false (crash / device
/// removal report), skips DXGI queries so the report cannot block in a hung driver.
pub fn table_or_snapshot(allow_dxgi: bool, seconds: f64) -> Table {
    let t = table();
    if t.last.is_some() {
        return t;
    }
    let s = snapshot_inner(allow_dxgi);
    let r = s.record(seconds);
    Table { last: Some(r.clone()), peak_video: Some(r), peak_video_bytes: s.local.map(|g| g.usage) }
}

/// The process's committed bytes, or None where `PrivateUsage` reads 0 (Wine leaves it so; #1097
/// and #2362 read the commit there another way).
fn process_commit() -> Option<u64> {
    unsafe {
        use windows_sys::Win32::System::ProcessStatus::{K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS_EX};
        let mut c: PROCESS_MEMORY_COUNTERS_EX = std::mem::zeroed();
        c.cb = std::mem::size_of_val(&c) as u32;
        K32GetProcessMemoryInfo(windows_sys::Win32::System::Threading::GetCurrentProcess(), &mut c as *mut _ as *mut _, c.cb);
        Some(c.PrivateUsage as u64).filter(|&n| n != 0)
    }
}

/// What DXGI reports for the device's adapter: (local, non-local), or None without a device.
fn video_memory() -> Option<(Segment, Segment)> {
    use windows::core::Interface;
    use windows::Win32::Graphics::Direct3D12::ID3D12Device;
    use windows::Win32::Graphics::Dxgi::*;
    let raw = super::real::device_raw();
    let dev = unsafe { ID3D12Device::from_raw_borrowed(&raw) }?;
    unsafe {
        let factory: IDXGIFactory4 = CreateDXGIFactory1().ok()?;
        let adapter: IDXGIAdapter3 = factory.EnumAdapterByLuid(dev.GetAdapterLuid()).ok()?;
        let query = |group| {
            let mut i = DXGI_QUERY_VIDEO_MEMORY_INFO::default();
            adapter.QueryVideoMemoryInfo(0, group, &mut i).ok().map(|_| Segment { usage: i.CurrentUsage, budget: i.Budget })
        };
        Some((query(DXGI_MEMORY_SEGMENT_GROUP_LOCAL)?, query(DXGI_MEMORY_SEGMENT_GROUP_NON_LOCAL)?))
    }
}

/// The commit Windows charges for the process's D3D12 heaps: what DXGI counts in both segments,
/// less the coherent window D3D12 imported (`imported`: CPU memory, already committed as such);
/// 0 when unknown.
fn heap_charge(local: Option<Segment>, non_local: Option<Segment>, imported: u64) -> u64 {
    match (local, non_local) {
        (Some(l), Some(n)) => (l.usage + n.usage).saturating_sub(imported),
        _ => 0,
    }
}

/// Where DXGI counts memory the GPU reads from system memory: the non-local segment, or on an
/// adapter with one segment (no non-local budget: an integrated GPU) the local one.
fn uma_segment(non_local: Option<Segment>) -> Kind {
    if non_local.is_some_and(|n| n.budget > 0) {
        Kind::NonLocal
    } else {
        Kind::Video
    }
}

/// Where DXGI counts a census part: system memory as `uma_segment` puts it, the rest local.
fn census_kind(kind: Kind, non_local: Option<Segment>) -> Kind {
    if kind == Kind::NonLocal { uma_segment(non_local) } else { kind }
}

/// Now.
pub fn snapshot() -> Snapshot {
    snapshot_inner(true)
}

fn snapshot_inner(allow_dxgi: bool) -> Snapshot {
    let (pages, page_pools) = super::vram::page_bytes();
    let (arenas, _) = super::vram::arena_bytes();
    let (pools, mapped) = super::sparse::pool_bytes();
    let (_, committed_textures) = super::title_textures::committed_bytes();
    let (local, non_local) = if allow_dxgi {
        video_memory().map_or((None, None), |(l, n)| (Some(l), Some(n)))
    } else {
        (None, None)
    };
    let uma = crate::gpu::uma::get().map_or(0, |u| u.size() as u64);
    // (what of it D3D12 has imported, which DXGI counts: none if the import failed)
    let imported = super::real::uma_imported_bytes();
    let b = |n: usize| n as u64;
    let parts = vec![
        // the title's memory in the coherent window: committed once, the GPU reads it there
        Part { owner: "UMA window", kind: Kind::Commit, bytes: uma },
        // a title page in a sparse window is committed on the CPU as well as mapped to a tile
        Part { owner: "sparse CPU copy", kind: Kind::Commit, bytes: b(super::sparse::committed_bytes()) },
        // Windows charges the process's D3D12 heaps to its commit as well, local and non-local
        // alike (1 GiB heap: +1 GiB of commit, on an RTX 4070 Ti and an RDNA2 iGPU) – all DXGI
        // counts but the coherent window, which is CPU memory imported and committed once
        Part { owner: "D3D12 heaps", kind: Kind::Commit, bytes: heap_charge(local, non_local, imported) },
        Part { owner: "placed texture pages", kind: Kind::Video, bytes: b(pages) },
        Part { owner: "placed texture pools free", kind: Kind::Video, bytes: b(page_pools.saturating_sub(pages)) },
        Part { owner: "arenas", kind: Kind::Video, bytes: b(arenas) },
        Part { owner: "sparse tiles", kind: Kind::Video, bytes: b(mapped) },
        Part { owner: "sparse pools free", kind: Kind::Video, bytes: b(pools.saturating_sub(mapped)) },
        Part { owner: "sparse pools retired", kind: Kind::Video, bytes: b(super::sparse::retired_bytes()) },
        Part { owner: "committed textures", kind: Kind::Video, bytes: committed_textures },
        // the GPU reads the coherent window (imported, `OpenExistingHeapFromAddress`) as system
        // memory: DXGI counts it in the non-local segment of a card (an RTX 4070 Ti and an
        // RX 7800 XT), and in the local one of an integrated GPU, whose only segment that is
        // (no non-local budget)
        Part { owner: "UMA window", kind: uma_segment(non_local), bytes: imported },
    ];
    let mut parts = parts;
    // the resources XCL makes for itself, by where (`resource_census`)
    // (in system memory – upload, readback – is non-local only where the adapter has such a
    // segment: on an integrated GPU it is local, as the coherent window)
    parts.extend(super::resource_census::counts().into_iter().filter(|(_, _, c)| c.bytes > 0).map(|(site, kind, c)| Part { owner: site, kind: census_kind(kind, non_local), bytes: c.bytes }));
    Snapshot { commit: process_commit(), local, non_local, parts }
}

/// The resources XCL makes for itself, by where: alive / made, and MB; None before any.
pub fn census_line() -> Option<String> {
    let counts = super::resource_census::counts();
    if counts.is_empty() {
        return None;
    }
    let parts: Vec<String> = counts.iter().map(|(site, _, c)| format!("{site} {} of {} made, {} MB", c.live, c.made, c.bytes >> 20)).collect();
    Some(format!("memory: XCL's own resources (alive): {}", parts.join("; ")))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The rest of each total is its total less its parts, a part is counted on its side only,
    /// and the line shows every part, each total against its budget, and the rest.
    #[test]
    fn the_rest_is_the_total_less_the_parts() {
        const M: u64 = 1 << 20;
        let s = Snapshot {
            commit: Some(10_000 * M),
            local: Some(Segment { usage: 9_000 * M, budget: 11_000 * M }),
            non_local: Some(Segment { usage: 300 * M, budget: 16_000 * M }),
            parts: vec![
                Part { owner: "UMA window", kind: Kind::Commit, bytes: 4_096 * M },
                Part { owner: "placed texture pages", kind: Kind::Video, bytes: 5_000 * M },
                Part { owner: "arenas", kind: Kind::Video, bytes: 1_000 * M },
                Part { owner: "upload staging", kind: Kind::NonLocal, bytes: 400 * M },
            ],
        };
        assert_eq!(s.counted(Kind::Video), 6_000 * M);
        assert_eq!(s.other(Kind::Video), Some((3_000 * M) as i64));
        assert_eq!(s.other(Kind::Commit), Some((5_904 * M) as i64));
        // more counted than DXGI reports: a negative rest, not a wrap
        assert_eq!(s.other(Kind::NonLocal), Some(-((100 * M) as i64)));
        assert_eq!(
            s.line(),
            "memory by owner (MB): commit 10000 (UMA window 4096, other 5904); video memory 9000 of a 11000 budget (placed texture pages 5000, arenas 1000, other 3000); non-local 300 of a 16000 budget (upload staging 400, other -100)"
        );
        // the coherent window: non-local on a card, local on a GPU of one segment
        assert_eq!(uma_segment(Some(Segment { usage: 0, budget: 16_000 * M })), Kind::NonLocal);
        assert_eq!(uma_segment(Some(Segment { usage: 0, budget: 0 })), Kind::Video);
        assert_eq!(uma_segment(None), Kind::Video);
        // the heaps' charge to the commit: both segments less the coherent window
        let (l, n) = (Some(Segment { usage: 9_000 * M, budget: 1 }), Some(Segment { usage: 4_500 * M, budget: 1 }));
        assert_eq!(heap_charge(l, n, 4_096 * M), 9_404 * M);
        assert_eq!(heap_charge(l, None, 4_096 * M), 0);
        assert_eq!(heap_charge(Some(Segment { usage: 100 * M, budget: 1 }), n, 8_000 * M), 0, "never below 0");
        // a window whose import failed is not in DXGI's usage: nothing of it is taken off
        assert_eq!(heap_charge(Some(Segment { usage: 3_000 * M, budget: 1 }), Some(Segment { usage: 0, budget: 1 }), 0), 3_000 * M);
        // a census part in system memory follows it; one in video memory stays there
        let card = Some(Segment { usage: 0, budget: 16_000 * M });
        let one_segment = Some(Segment { usage: 0, budget: 0 });
        assert_eq!(census_kind(Kind::NonLocal, card), Kind::NonLocal);
        assert_eq!(census_kind(Kind::NonLocal, one_segment), Kind::Video);
        assert_eq!(census_kind(Kind::Video, card), Kind::Video);
        let unknown = Snapshot { commit: None, local: None, non_local: None, ..s };
        assert_eq!(unknown.other(Kind::Video), None);
        // a commit that reads 0 (Wine) is unknown, not 0 with a negative rest
        assert_eq!(unknown.other(Kind::Commit), None);
        assert!(unknown.line().starts_with("memory by owner (MB): commit unknown (UMA window 4096)"), "{}", unknown.line());
        assert!(unknown.line().contains("video memory unknown (placed texture pages 5000, arenas 1000)"), "{}", unknown.line());
    }

    /// On a device the snapshot has DXGI's totals with a budget, the process's commit, and every
    /// owner.
    #[test]
    fn a_snapshot_on_the_device_has_its_budget_and_owners() {
        if super::super::backend() != super::super::Backend::Real {
            crate::gpu::recomp::runner::skip_test("no GPU");
            return;
        }
        let s = snapshot();
        // (an integrated GPU has one segment: its non-local budget is 0)
        assert!(s.non_local.is_some(), "{}", s.line());
        assert!(s.local.expect("local video memory").budget > 0, "{}", s.line());
        // (`PrivateUsage` reads 0 under Wine: the commit is unknown there, and the line says so)
        match s.commit {
            Some(c) => assert!(c > 0),
            None => assert!(s.line().contains("commit unknown"), "{}", s.line()),
        }
        for owner in ["UMA window", "sparse CPU copy", "D3D12 heaps", "placed texture pages", "placed texture pools free", "arenas", "sparse tiles", "sparse pools free", "sparse pools retired", "committed textures"] {
            assert!(s.parts.iter().any(|p| p.owner == owner), "{owner} missing: {}", s.line());
        }
    }

    /// A snapshot's record keeps its totals, budgets, parts and rests in MB; the table keeps the
    /// last one and the one of most video memory; a record survives a JSON round trip.
    #[test]
    fn the_table_keeps_the_last_snapshot_and_the_peak_of_video_memory() {
        const M: u64 = 1 << 20;
        let snap = |video: u64, commit: u64| Snapshot {
            commit: Some(commit * M),
            local: Some(Segment { usage: video * M, budget: 10_000 * M }),
            non_local: Some(Segment { usage: 500 * M, budget: 16_000 * M }),
            parts: vec![Part { owner: "placed texture pages", kind: Kind::Video, bytes: 3_000 * M }, Part { owner: "UMA window", kind: Kind::Commit, bytes: 4_096 * M }],
        };
        let r = snap(9_000, 20_000).record(12.5);
        assert_eq!((r.seconds, r.commit_mb, r.video_mb, r.video_budget_mb, r.non_local_mb), (12.5, Some(20_000), Some(9_000), Some(10_000), Some(500)));
        assert_eq!(r.parts, vec![("placed texture pages".into(), "video".into(), 3_000), ("UMA window".into(), "commit".into(), 4_096)]);
        assert_eq!(r.other_mb.get("video"), Some(&6_000));
        assert_eq!(r.other_mb.get("commit"), Some(&15_904));
        assert_eq!(r.other_mb.get("non-local"), Some(&500));
        let back: Record = serde_json::from_str(&serde_json::to_string(&r).unwrap()).unwrap();
        assert_eq!(back, r);
        // (the module's table: its own, as `note` fills it)
        *TABLE.lock().unwrap() = Table::default();
        note(&snap(9_000, 20_000), 10.0);
        note(&snap(12_000, 25_000), 20.0);
        note(&snap(11_000, 26_000), 30.0);
        let t = table();
        assert_eq!(t.peak_video.as_ref().map(|r| (r.seconds, r.video_mb)), Some((20.0, Some(12_000))));
        assert_eq!(t.last.as_ref().map(|r| (r.seconds, r.commit_mb)), Some((30.0, Some(26_000))));
        // two readings within one MB: the peak is the higher one in bytes, with its own parts
        *TABLE.lock().unwrap() = Table::default();
        let with_usage = |usage: u64, commit: u64| Snapshot { local: Some(Segment { usage, budget: 10_000 * M }), ..snap(0, commit) };
        note(&with_usage(100 * M, 20_000), 10.0);
        note(&with_usage(100 * M + 1_023, 21_000), 20.0);
        note(&with_usage(100 * M + 1_000, 22_000), 30.0);
        let t = table();
        assert_eq!(t.peak_video.as_ref().map(|r| (r.seconds, r.video_mb, r.commit_mb)), Some((20.0, Some(100), Some(21_000))));
        // a known reading, even under 1 MB, ranks above an earlier unknown one
        *TABLE.lock().unwrap() = Table::default();
        note(&Snapshot { local: None, ..snap(0, 20_000) }, 10.0);
        note(&with_usage(512 * 1_024, 21_000), 20.0);
        note(&Snapshot { local: None, ..snap(0, 22_000) }, 30.0);
        let t = table();
        assert_eq!(t.peak_video.as_ref().map(|r| (r.seconds, r.video_mb)), Some((20.0, Some(0))));
        *TABLE.lock().unwrap() = Table::default();
    }

    #[test]
    fn table_or_snapshot_falls_back_when_no_activity_notes_exist() {
        *TABLE.lock().unwrap() = Table::default();
        // Without XCL_ACTIVITY notes, fallback populates both `last` and `peak_video` safely even with DXGI disabled.
        let t = table_or_snapshot(false, 42.5);
        let last = t.last.as_ref().expect("fallback must populate last");
        assert_eq!(last.seconds, 42.5);
        assert_eq!(t.peak_video.as_ref(), Some(last));
        assert_eq!(last.video_mb, None, "allow_dxgi=false must skip DXGI video memory query");
        assert!(last.parts.iter().any(|(owner, _, _)| owner == "UMA window"));

        // When a note already exists, table_or_snapshot preserves it without overwriting.
        const M: u64 = 1 << 20;
        let noted = Snapshot {
            commit: Some(1_000 * M),
            local: Some(Segment { usage: 2_000 * M, budget: 8_000 * M }),
            non_local: Some(Segment { usage: 100 * M, budget: 8_000 * M }),
            parts: Vec::new(),
        };
        note(&noted, 15.0);
        let kept = table_or_snapshot(false, 99.0);
        assert_eq!(kept.last.as_ref().map(|r| (r.seconds, r.video_mb)), Some((15.0, Some(2_000))));
        *TABLE.lock().unwrap() = Table::default();
    }
}
