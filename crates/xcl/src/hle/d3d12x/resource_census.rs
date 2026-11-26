//! The D3D12 resources XCL makes for itself (not the title's), by where it makes them: how many
//! are alive and their bytes (#2688, workstream 1). A resource is counted when it is made
//! (`track`) and taken off when D3D12 destroys it: `track` gives it a small COM object as private
//! data, which D3D12 releases with the resource. Growth that follows the title's submits shows up
//! as one place whose count keeps climbing.

use std::collections::BTreeMap;
use std::ffi::c_void;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;

use windows::core::{Interface, GUID, HRESULT, IUnknown};
use windows::Win32::Graphics::Direct3D12::*;

use super::memory_report::Kind;

/// What a place has made: alive now, their bytes, made in all.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Count {
    pub live: u64,
    pub bytes: u64,
    pub made: u64,
}

/// (place, which memory) → its count
static SITES: Mutex<BTreeMap<(&'static str, KindKey), Count>> = Mutex::new(BTreeMap::new());

/// `Kind` as a map key
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum KindKey {
    Video,
    NonLocal,
}

/// The private data a tracked resource holds (its id is XCL's own).
const TRACKER: GUID = GUID::from_u128(0x5c1d_7e2a_8b44_4f0e_9a61_3d2c_7b1e_0a95);

/// The COM object D3D12 keeps as a tracked resource's private data: its release takes the
/// resource off its place's count.
#[repr(C)]
struct Tracker {
    vtbl: *const Vtbl,
    refs: AtomicU32,
    site: &'static str,
    kind: KindKey,
    bytes: u64,
}

/// IUnknown's three methods
#[repr(C)]
struct Vtbl {
    query_interface: unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> HRESULT,
    add_ref: unsafe extern "system" fn(*mut c_void) -> u32,
    release: unsafe extern "system" fn(*mut c_void) -> u32,
}

static VTBL: Vtbl = Vtbl { query_interface, add_ref, release };

unsafe extern "system" fn query_interface(this: *mut c_void, iid: *const GUID, out: *mut *mut c_void) -> HRESULT {
    if !out.is_null() && !iid.is_null() && *iid == IUnknown::IID {
        add_ref(this);
        *out = this;
        return HRESULT(0);
    }
    if !out.is_null() {
        *out = std::ptr::null_mut();
    }
    HRESULT(0x8000_4002_u32 as i32) // E_NOINTERFACE
}

unsafe extern "system" fn add_ref(this: *mut c_void) -> u32 {
    (*(this as *const Tracker)).refs.fetch_add(1, Ordering::AcqRel) + 1
}

unsafe extern "system" fn release(this: *mut c_void) -> u32 {
    let t = this as *mut Tracker;
    let n = (*t).refs.fetch_sub(1, Ordering::AcqRel) - 1;
    if n == 0 {
        let t = Box::from_raw(t);
        if let Some(c) = SITES.lock().unwrap_or_else(|e| e.into_inner()).get_mut(&(t.site, t.kind)) {
            c.live = c.live.saturating_sub(1);
            c.bytes = c.bytes.saturating_sub(t.bytes);
        }
    }
    n
}

/// Count `res`, which XCL made at `site` (a short name: "copy scratch", "upload staging"), until
/// D3D12 destroys it. Its bytes are its allocation's size. One in an upload or readback heap is
/// in system memory (`Kind::NonLocal` here; `memory_report` puts it where the adapter's DXGI
/// segments count it), the rest in video memory.
pub unsafe fn track(res: &ID3D12Resource, site: &'static str) {
    let desc = res.GetDesc();
    let mut dev: Option<ID3D12Device> = None;
    let bytes = match res.GetDevice(&mut dev).ok().and(dev) {
        Some(dev) => dev.GetResourceAllocationInfo(0, &[desc]).SizeInBytes,
        None => 0,
    };
    let bytes = if bytes == u64::MAX { 0 } else { bytes };
    let mut heap = D3D12_HEAP_PROPERTIES::default();
    let kind = match res.GetHeapProperties(Some(&mut heap), None) {
        Ok(()) if matches!(heap.Type, D3D12_HEAP_TYPE_UPLOAD | D3D12_HEAP_TYPE_READBACK) => KindKey::NonLocal,
        _ => KindKey::Video,
    };
    let t = Box::into_raw(Box::new(Tracker { vtbl: &VTBL, refs: AtomicU32::new(1), site, kind, bytes }));
    // (ours: the one reference `Box::into_raw` made; D3D12 takes its own)
    let unknown = IUnknown::from_raw(t as *mut c_void);
    {
        let mut g = SITES.lock().unwrap_or_else(|e| e.into_inner());
        let c = g.entry((site, kind)).or_default();
        c.live += 1;
        c.bytes += bytes;
        c.made += 1;
    }
    if res.SetPrivateDataInterface(&TRACKER, &unknown).is_err() {
        // (not kept: the count it made goes again when `unknown` is dropped below)
    }
    drop(unknown);
}

/// `track` of the resource in `r`, if any; `r` back.
pub fn tracked(r: Option<ID3D12Resource>, site: &'static str) -> Option<ID3D12Resource> {
    if let Some(res) = &r {
        // SAFETY: a live resource of XCL's own
        unsafe { track(res, site) };
    }
    r
}

/// Every place's count now, with the memory it takes.
pub fn counts() -> Vec<(&'static str, Kind, Count)> {
    SITES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .map(|(&(site, k), &c)| (site, if k == KindKey::NonLocal { Kind::NonLocal } else { Kind::Video }, c))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tracked resource counts at its place while it lives (its bytes, upload heaps as
    /// non-local) and goes off the count when D3D12 destroys it; `made` stays.
    #[test]
    fn a_tracked_resource_counts_until_it_is_destroyed() {
        if super::super::backend() != super::super::Backend::Real {
            crate::gpu::recomp::runner::skip_test("no GPU");
            return;
        }
        let count = |site: &str| counts().into_iter().find(|(s, _, _)| *s == site).map(|(_, k, c)| (k, c));
        unsafe {
            let raw = super::super::real::device_raw();
            let dev = ID3D12Device::from_raw_borrowed(&raw).unwrap();
            let buffer = |heap: D3D12_HEAP_TYPE, size: u64| -> ID3D12Resource {
                let desc = D3D12_RESOURCE_DESC {
                    Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
                    Width: size,
                    Height: 1,
                    DepthOrArraySize: 1,
                    MipLevels: 1,
                    SampleDesc: windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                    Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
                    ..Default::default()
                };
                let state = if heap == D3D12_HEAP_TYPE_UPLOAD { D3D12_RESOURCE_STATE_GENERIC_READ } else { D3D12_RESOURCE_STATE_COMMON };
                let mut r: Option<ID3D12Resource> = None;
                dev.CreateCommittedResource(&D3D12_HEAP_PROPERTIES { Type: heap, ..Default::default() }, D3D12_HEAP_FLAG_NONE, &desc, state, None, &mut r).unwrap();
                r.unwrap()
            };
            let a = buffer(D3D12_HEAP_TYPE_DEFAULT, 1 << 20);
            let b = buffer(D3D12_HEAP_TYPE_DEFAULT, 1 << 20);
            track(&a, "census test video");
            track(&b, "census test video");
            let up = buffer(D3D12_HEAP_TYPE_UPLOAD, 3 << 20);
            track(&up, "census test upload");
            let (k, c) = count("census test video").unwrap();
            assert_eq!((k, c.live, c.made), (Kind::Video, 2, 2));
            assert!(c.bytes >= 2 << 20, "{c:?}");
            let (k, c) = count("census test upload").unwrap();
            assert_eq!((k, c.live), (Kind::NonLocal, 1));
            assert!(c.bytes >= 3 << 20, "{c:?}");
            // a reference elsewhere keeps it alive and counted
            let a2 = a.clone();
            drop(a);
            assert_eq!(count("census test video").unwrap().1.live, 2);
            drop(a2);
            let (_, c) = count("census test video").unwrap();
            assert_eq!((c.live, c.made), (1, 2), "destroyed: off the count");
            drop(b);
            drop(up);
            assert_eq!(count("census test video").unwrap().1, Count { live: 0, bytes: 0, made: 2 });
            assert_eq!(count("census test upload").unwrap().1.live, 0);
        }
    }
}
