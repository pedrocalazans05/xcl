//! Process-wide table of modules XCL knows about:
//! * images mapped by the XCL loader (game exe + game-shipped DLLs),
//! * pseudo handles for HLE modules (`kernelx.dll` & co. have no real image).
//!
//! Address → module lookups used from exception dispatch (`RtlPcToFileHeader` hook) go
//! through a lock-free fixed array so they can never deadlock.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use crate::pe::OwnedExports;

pub struct MappedModule {
    /// Lower-case file name, e.g. "game.exe".
    pub name: String,
    pub path: PathBuf,
    pub base: usize,
    pub size: usize,
    pub entry_rva: u32,
    pub is_dll: bool,
    pub exports: OwnedExports,
    pub tls_callbacks: Mutex<Vec<usize>>,
    /// DisableThreadLibraryCalls was called.
    pub no_thread_calls: AtomicBool,
    /// DllMain(PROCESS_ATTACH) has completed.
    pub initialized: AtomicBool,
    pub stack_reserve: u64,
}

impl MappedModule {
    pub fn contains(&self, addr: usize) -> bool {
        addr >= self.base && addr < self.base + self.size
    }
}

static MAPPED: RwLock<Vec<Arc<MappedModule>>> = RwLock::new(Vec::new());
static MAIN: RwLock<Option<Arc<MappedModule>>> = RwLock::new(None);

const MAX_RANGES: usize = 512;
static RANGE_BASE: [AtomicUsize; MAX_RANGES] = [const { AtomicUsize::new(0) }; MAX_RANGES];
static RANGE_SIZE: [AtomicUsize; MAX_RANGES] = [const { AtomicUsize::new(0) }; MAX_RANGES];
static RANGE_GEN: [AtomicUsize; MAX_RANGES] = [const { AtomicUsize::new(0) }; MAX_RANGES];
static RANGE_COUNT: AtomicUsize = AtomicUsize::new(0);

/// Update range slot `i` using a per-slot seqlock protocol so lock-free readers never observe
/// a torn `(base, size)` pair when a freed slot is reused. Caller must hold `MAPPED.write()`.
fn write_range_slot(i: usize, base: usize, size: usize) {
    let g = RANGE_GEN[i].load(Ordering::Relaxed);
    RANGE_GEN[i].store(g.wrapping_add(1), Ordering::Relaxed);
    std::sync::atomic::fence(Ordering::Release);
    RANGE_SIZE[i].store(size, Ordering::Relaxed);
    RANGE_BASE[i].store(base, Ordering::Relaxed);
    RANGE_GEN[i].store(g.wrapping_add(2), Ordering::Release);
}

pub fn register_mapped(m: Arc<MappedModule>) {
    let mut mapped = MAPPED.write().unwrap();
    if m.base != 0 {
        let n = RANGE_COUNT.load(Ordering::Relaxed).min(MAX_RANGES);
        let slot = (0..n)
            .find(|&i| RANGE_BASE[i].load(Ordering::Relaxed) == 0)
            .or_else(|| (n < MAX_RANGES).then_some(n));
        if let Some(i) = slot {
            write_range_slot(i, m.base, m.size);
            if i == n {
                RANGE_COUNT.store(n + 1, Ordering::Release);
            }
        }
    }
    mapped.push(m);
}

/// The image at `base` failed to load: it is no longer found (by name, base, address), so a
/// later load maps it again – as Windows' loader unloads a DLL that failed.
pub fn unregister_mapped(base: usize) {
    let mut mapped = MAPPED.write().unwrap();
    mapped.retain(|m| m.base != base);
    let n = RANGE_COUNT.load(Ordering::Relaxed).min(MAX_RANGES);
    for i in 0..n {
        if RANGE_BASE[i].load(Ordering::Relaxed) == base {
            write_range_slot(i, 0, 0);
        }
    }
}

/// Lock-free: base of the mapped image containing `addr`.
pub fn image_base_for_addr(addr: usize) -> Option<usize> {
    let n = RANGE_COUNT.load(Ordering::Acquire).min(MAX_RANGES);
    for i in 0..n {
        for _ in 0..16 {
            let g1 = RANGE_GEN[i].load(Ordering::Acquire);
            if g1 & 1 != 0 {
                std::hint::spin_loop();
                continue;
            }
            let b = RANGE_BASE[i].load(Ordering::Relaxed);
            let s = RANGE_SIZE[i].load(Ordering::Relaxed);
            std::sync::atomic::fence(Ordering::Acquire);
            let g2 = RANGE_GEN[i].load(Ordering::Relaxed);
            if g1 == g2 {
                if b != 0 && addr >= b && addr < b.saturating_add(s) {
                    return Some(b);
                }
                break;
            }
        }
    }
    None
}

pub fn mapped_modules() -> Vec<Arc<MappedModule>> {
    MAPPED.read().map(|v| v.clone()).unwrap_or_default()
}

pub fn find_mapped_by_name(name: &str) -> Option<Arc<MappedModule>> {
    let key = crate::util::dll_key(name);
    MAPPED.read().ok()?.iter().find(|m| m.name == key).cloned()
}

pub fn find_mapped_by_base(base: usize) -> Option<Arc<MappedModule>> {
    MAPPED.read().ok()?.iter().find(|m| m.base == base).cloned()
}

pub fn find_mapped_by_addr(addr: usize) -> Option<Arc<MappedModule>> {
    MAPPED.read().ok()?.iter().find(|m| m.contains(addr)).cloned()
}

pub fn set_main(m: Arc<MappedModule>) {
    *MAIN.write().unwrap() = Some(m);
}

pub fn main_module() -> Option<Arc<MappedModule>> {
    MAIN.read().ok()?.clone()
}

// ---------------------------------------------------------------------------------------
// HLE pseudo handles
// ---------------------------------------------------------------------------------------

static HLE_HANDLES: Mutex<Option<HashMap<String, usize>>> = Mutex::new(None);

/// Stable fake HMODULE for an HLE module: a committed page starting with "MZ".
pub fn hle_handle(module_name: &str) -> usize {
    use windows_sys::Win32::System::Memory::{VirtualAlloc, MEM_COMMIT, MEM_RESERVE, PAGE_READWRITE};
    let key = crate::util::dll_key(module_name);
    let mut g = HLE_HANDLES.lock().unwrap();
    let map = g.get_or_insert_with(HashMap::new);
    if let Some(&h) = map.get(&key) {
        return h;
    }
    let p = unsafe { VirtualAlloc(std::ptr::null(), 0x1000, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE) } as *mut u8;
    unsafe {
        *p = b'M';
        *p.add(1) = b'Z';
    }
    map.insert(key, p as usize);
    p as usize
}

pub fn find_hle_handle(module_name: &str) -> Option<usize> {
    let key = crate::util::dll_key(module_name);
    let g = HLE_HANDLES.lock().ok()?;
    g.as_ref()?.get(&key).copied()
}

pub fn hle_name_by_handle(h: usize) -> Option<String> {
    let g = HLE_HANDLES.lock().ok()?;
    g.as_ref()?.iter().find(|(_, &v)| v == h).map(|(k, _)| k.clone())
}

/// `name+offset` for an address inside its module, else the bare address.
///
/// The addresses `describe_address` gets include raw stack words, which can be anything. Wine
/// may report a module for one with a handle above the address: the subtraction must not panic
/// there, or a crash report turns into a panic in the crash handler.
fn module_offset(name: &str, base: usize, addr: usize) -> String {
    match addr.checked_sub(base) {
        Some(offset) => format!("{name}+{offset:#x}"),
        None => format!("{addr:#x}"),
    }
}

/// "game.exe+0x1234", "KERNELBASE.dll+0x5678" or "0x...".
pub fn describe_address(addr: usize) -> String {
    if let Some(b) = image_base_for_addr(addr) {
        if let Ok(g) = MAPPED.try_read() {
            if let Some(m) = g.iter().find(|m| m.base == b) {
                return format!("{}+{:#x}", m.name, addr - b);
            }
        }
        return format!("{b:#x}+{:#x}", addr - b);
    }
    unsafe {
        use windows_sys::Win32::System::LibraryLoader::{
            GetModuleFileNameW, GetModuleHandleExW, GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS,
            GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
        };
        let mut h = std::ptr::null_mut();
        if GetModuleHandleExW(
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
            addr as *const u16,
            &mut h,
        ) != 0
        {
            let mut buf = [0u16; 260];
            let n = GetModuleFileNameW(h, buf.as_mut_ptr(), 260) as usize;
            let full = String::from_utf16_lossy(&buf[..n]);
            let name = full.rsplit('\\').next().unwrap_or(&full).to_string();
            return module_offset(&name, h as usize, addr);
        }
    }
    format!("{addr:#x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn module_offset_survives_a_base_above_the_address() {
        // What Wine answered for a stack word while reporting a crash of the retail title:
        // the handle lay above the address (the subtraction panicked and the report was lost).
        assert_eq!(
            module_offset("kernelbase.dll", 0x7fff_0000_0000, 0x6f93_bbb0_e7dc),
            "0x6f93bbb0e7dc"
        );
        assert_eq!(module_offset("game.exe", 0x140000000, 0x140001234), "game.exe+0x1234");
        assert_eq!(module_offset("game.exe", 0x140000000, 0x140000000), "game.exe+0x0");
    }

    #[test]
    fn describe_address_takes_arbitrary_stack_words() {
        for addr in [0usize, 1, 0x1000_0000_4, 0x7fff_ffff_ffff, usize::MAX] {
            let _ = describe_address(addr);
        }
    }

    #[test]
    fn register_and_unregister_reuses_slots_beyond_max_ranges() {
        let size = 0x1000usize;
        for iter in 0..(MAX_RANGES + 64) {
            let base = 0x0000_6000_0000_0000usize + iter * 0x1_0000;
            let module = Arc::new(MappedModule {
                name: format!("slot_reuse_{iter}.dll"),
                path: PathBuf::from(format!("slot_reuse_{iter}.dll")),
                base,
                size,
                entry_rva: 0,
                is_dll: true,
                exports: OwnedExports::default(),
                tls_callbacks: Mutex::new(Vec::new()),
                no_thread_calls: AtomicBool::new(false),
                initialized: AtomicBool::new(true),
                stack_reserve: 0,
            });
            register_mapped(module);
            assert_eq!(
                image_base_for_addr(base + 0x123),
                Some(base),
                "module at iteration {iter} must remain visible to lock-free address lookup"
            );
            unregister_mapped(base);
            assert_eq!(
                image_base_for_addr(base + 0x123),
                None,
                "unregistered module at iteration {iter} must no longer match address lookup"
            );
        }
    }
}
