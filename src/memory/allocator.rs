use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};
use std::ptr::null_mut;
use std::sync::Mutex;
use std::sync::Condvar;

/// Default arena size — used when the config doesn't specify one.
const ARENA_MAX: usize = 8 * 1024 * 1024 * 1024;

static ARENA_BASE: AtomicPtr<u8> = AtomicPtr::new(null_mut());

/// Tail reservation: 2x the biggest message received + overhead.
/// Mutated from `server.rs` via `set_locked_size()`.
pub static LOCKED_SIZE: AtomicUsize = AtomicUsize::new(0);
static DEALLOC_COUNT: AtomicUsize = AtomicUsize::new(0);
// Condvar pair for waking the background thread. Neither allocates on
// wait/notify (futex on Linux, SRW+CV on Windows), so it's safe to use
// from inside the global allocator.
static DEFRAG_MUTEX:   Mutex<bool> = Mutex::new(false);
static DEFRAG_CONDVAR: Condvar     = Condvar::new();
struct MemAllocator {
    allocated: AtomicUsize,
    used: AtomicUsize,
    offset: AtomicUsize,
}
/// Written into a freed slot to chain it onto the free list. Must fit inside
/// the smallest slot we ever hand out — we enforce that via `MIN_SLOT_SIZE`.
#[repr(C)]
struct FreeNode {
    size: usize,
    next: *mut FreeNode,
}
/// Written at the start of every live slot so `dealloc` can recover the slot
/// boundaries without relying on `Layout` (which doesn't describe padding).
#[repr(C)]
struct AllocHeader {
    /// Bytes from slot start to the returned user pointer.
    slot_offset: usize,
    /// Total carved slot size, in bytes.
    slot_size: usize,
}
const _: () = assert!(size_of::<FreeNode>() == size_of::<AllocHeader>());
const _: () = assert!(align_of::<FreeNode>() == align_of::<AllocHeader>());

const MIN_SLOT_SIZE: usize = size_of::<FreeNode>(); // 16 on 64-bit
const SLOT_ALIGN: usize = align_of::<FreeNode>(); // 8 on 64-bit

struct FreeListHead(*mut FreeNode);
unsafe impl Send for FreeListHead {}

static FREE_LIST: Mutex<FreeListHead> = Mutex::new(FreeListHead(null_mut()));

#[inline]
fn align_up(value: usize, align: usize) -> usize {
    (value + align - 1) & !(align - 1)
}
fn required_slot_size(layout: Layout) -> Option<usize> {
    let header = size_of::<AllocHeader>();
    let worst = header.
        checked_add(layout.align().saturating_sub(1))?.
        checked_add(layout.size())?;
    Some(align_up(worst.max(MIN_SLOT_SIZE), SLOT_ALIGN))
}
unsafe fn place_header(slot: *mut u8, slot_size: usize, layout: Layout) -> *mut u8 {
    unsafe {
        let header_size = size_of::<AllocHeader>();
        let min_user = slot.add(header_size) as usize;
        let user_addr = align_up(min_user, layout.align());
        let user_ptr = user_addr as *mut u8;
        let header_ptr = user_ptr.sub(header_size) as *mut AllocHeader;
        (*header_ptr).slot_offset = user_ptr.offset_from(slot) as usize;
        (*header_ptr).slot_size = slot_size;
        user_ptr
    }
}
unsafe impl GlobalAlloc for MemAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let base = ARENA_BASE.load(Ordering::Acquire);
        if base.is_null() {
            // Arena not initialized — fall back to the system allocator.
            // Handles pre-main allocations and test builds where init() is never called.
            return unsafe { System.alloc(layout) };
        }
        let needed = match required_slot_size(layout) {
            Some(size) => { size },
            None => { return null_mut() },
        };
        if !self.reserve_used(needed) {
            return null_mut();
        }
        unsafe {
            if let Some(ptr) = self.alloc_from_free_list(needed, layout) {
                return ptr;
            }
            if let Some(ptr) = self.alloc_from_bump(base, needed, layout) {
                return ptr;
            }
            self.used.fetch_sub(needed, Ordering::AcqRel);
            null_mut()
        }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        let base = ARENA_BASE.load(Ordering::Acquire);
        if base.is_null() {
            // Arena not initialized — pointer was allocated by the system allocator.
            return unsafe { System.dealloc(ptr, layout) };
        }

        unsafe {
            let capacity = self.allocated.load(Ordering::Acquire);
            // Quick range check BEFORE reading the in-slot header — if ptr is
            // outside the arena, it was allocated by the system allocator
            // (pre-init allocation). Delegate without touching the header bytes.
            if ptr < base || ptr >= base.add(capacity) {
                return System.dealloc(ptr, layout);
            }

            let header_ptr = ptr.sub(size_of::<AllocHeader>()) as *const AllocHeader;
            let slot_offset = (*header_ptr).slot_offset;
            let slot_size = (*header_ptr).slot_size;
            let slot_start = ptr.sub(slot_offset);

            let arena_end = base.add(capacity);
            if slot_start < base || slot_start.add(slot_size) > arena_end {
                return;
            }

            let node = slot_start as *mut FreeNode;
            {
                let mut head = FREE_LIST.lock().expect("FREE_LIST broken");
                (*node).size = slot_size;
                (*node).next = head.0;
                head.0 = node;
            }

            self.used.fetch_sub(slot_size, Ordering::AcqRel);
            if DEALLOC_COUNT.fetch_add(1, Ordering::Relaxed) & 31 == 31 {
                if let Ok(mut guard) = DEFRAG_MUTEX.lock() {
                    *guard = true;
                    DEFRAG_CONDVAR.notify_one();
                }
            }
        }
    }
}
impl MemAllocator {
    fn reserve_used(&self, needed: usize) -> bool {
        let capacity = self.allocated.load(Ordering::Acquire);
        let mut current = self.used.load(Ordering::Relaxed);
        loop {
            let locked = LOCKED_SIZE.load(Ordering::Acquire);
            let ceiling = match capacity.checked_sub(locked) {
                Some(ceiling) => { ceiling },
                None => { return false },
            };
            let new_used = match current.checked_add(needed) {
                Some(value) if value <= ceiling => { value },
                _ => { return false },
            };
            match self.used.compare_exchange_weak(current, new_used, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => { return true },
                Err(observed) => { current = observed },
            }
        }
    }
    unsafe fn alloc_from_free_list(&self, needed: usize, layout: Layout) -> Option<*mut u8> {
        let mut head = FREE_LIST.lock().ok()?;
        let mut prev: *mut *mut FreeNode = &mut head.0;
        unsafe {
            while !(*prev).is_null() {
                let node = *prev;
                let node_size = (*node).size;
                if node_size >= needed {
                    *prev = (*node).next;

                    let leftover = node_size - needed;
                    if leftover >= MIN_SLOT_SIZE {
                        let remainder = (node as *mut u8).add(needed) as *mut FreeNode;
                        (*remainder).size = leftover;
                        (*remainder).next = *prev;
                        *prev = remainder;

                        drop(head);
                        let slot = node as *mut u8;
                        return Some(place_header(slot, needed, layout));
                    }

                    drop(head);
                    let slot = node as *mut u8;
                    let excess = node_size - needed;
                    if excess > 0 {
                        self.used.fetch_add(excess, Ordering::Relaxed);
                    }
                    return Some(place_header(slot, node_size, layout));
                }
                prev = &mut (*node).next;
            }
        }
        None
    }
    unsafe fn alloc_from_bump(&self, base: *mut u8, needed: usize, layout: Layout) -> Option<*mut u8> {
        let capacity = self.allocated.load(Ordering::Acquire);
        let mut current = self.offset.load(Ordering::Relaxed);
        loop {
            let slot_start = align_up(current, SLOT_ALIGN);
            let next = slot_start.checked_add(needed)?;
            if next > capacity {
                return None;
            }
            unsafe {
                match self.offset.compare_exchange_weak(current, next, Ordering::AcqRel, Ordering::Relaxed) {
                    Ok(_) => {
                        let slot = base.add(slot_start);
                        return Some(place_header(slot, needed, layout));
                    }
                    Err(observed) => {
                        current = observed;
                    }
                }
            }
        }
    }
}
#[global_allocator]
static GLOBAL: MemAllocator = MemAllocator {
    allocated: AtomicUsize::new(0),
    used: AtomicUsize::new(0),
    offset: AtomicUsize::new(0),
};
#[cfg(windows)]
pub(crate) fn init(arena_size: usize) {
    use windows_sys::Win32::System::Memory::{
        VirtualAlloc, MEM_COMMIT, MEM_RESERVE, PAGE_READWRITE,
    };
    assert!(arena_size > 1 * 1024 * 1024 * 1024,
        "arena must be larger than 1GiB, got {arena_size}KiB"
    );
    assert!(ARENA_BASE.load(Ordering::Acquire).is_null(),
        "allocator init called multiple times"
    );
    // MEM_RESERVE | MEM_COMMIT reserves address space *and* backs it with the
    // pagefile immediately — satisfies "commit everything up front". A null base
    // lets the OS pick the address.                                                                                                                                                                                                                                                          //
    // SAFETY: VirtualAlloc with a null base and MEM_RESERVE | MEM_COMMIT is
    // always sound — it can only succeed or return NULL.
    let ptr = unsafe {
        VirtualAlloc(
            std::ptr::null(),
            arena_size,
            MEM_RESERVE | MEM_COMMIT,
            PAGE_READWRITE,
        )
    } as *mut u8;
    if ptr.is_null() {
        panic!("VirtualAlloc returned null");
    }

    ARENA_BASE.store(ptr, Ordering::Release);
    GLOBAL.allocated.store(arena_size, Ordering::Release);
    // GLOBAL.used stays at 0.

    std::thread::Builder::new().
        name("arena-defrag".into()).
        spawn(defrag_thread).
        expect("Failed to spawn defrag thread");
}
pub(crate) fn get_free_mem_size() -> usize {
    let allocated = GLOBAL.allocated.load(Ordering::Acquire);
    if allocated == 0 {
        return usize::MAX;
    }
    let used = GLOBAL.used.load(Ordering::Acquire);

    allocated - used
}
#[cfg(unix)]
pub fn init(arena_size: usize) {
    use libc::{mmap, MAP_ANON, MAP_FAILED, MAP_PRIVATE, PROT_READ, PROT_WRITE};
    assert!(
        arena_size >= 8 * 1024 * 1024 * 1024,
        "arena must be >= 8 GiB (got {arena_size})",                                                                                                                                                                                                                                          );
    assert!(
        ARENA_BASE.load(Ordering::Acquire).is_null(),
        "allocator::init called twice",                                                                                                                                                                                                                                                       );

    // MAP_PRIVATE | MAP_ANON *without* MAP_NORESERVE asks the kernel to reserve
    // RAM+swap backing for the whole range immediately. On Linux with default
    // overcommit settings the reservation is accounted against commit limits,
    // matching the Windows MEM_COMMIT behavior.
    //
    // SAFETY: mmap with a null addr is always sound — it either returns a fresh
    // mapping or MAP_FAILED.
    let ptr = unsafe {                                                                                                                                                                                                                                                                              mmap(
        std::ptr::null_mut(),
        arena_size,
        PROT_READ | PROT_WRITE,
        MAP_PRIVATE | MAP_ANON,
        -1,                                                                                                                                                                                                                                                                                       0,
    )
    };

    if ptr == MAP_FAILED {
        // errno would give the reason (usually ENOMEM).
        panic!("allocator::init: mmap failed to commit {arena_size} bytes");
    }
    ARENA_BASE.store(ptr as *mut u8, Ordering::Release);
    GLOBAL.allocated.store(arena_size, Ordering::Release);
    // GLOBAL.used stays at 0.                                                                                                                                                                                                                                                            }
}
pub fn set_locked_size(size: usize) -> bool {
    let capacity = GLOBAL.allocated.load(Ordering::Acquire);
    let used = GLOBAL.used.load(Ordering::Acquire);
    if used.checked_add(size).map_or(true, |result| result > capacity) {
        return false;
    }
    LOCKED_SIZE.store(size, Ordering::Release);
    true
}
fn defrag_thread() {
    loop {
        let mut guard = DEFRAG_MUTEX.lock().expect("DEFRAG_MUTEX is broken");
        while !*guard {
            guard = DEFRAG_CONDVAR.wait(guard).expect("DEFRAG_CONDVAR wait failed");
        }
        *guard = false;
        drop(guard);

        unsafe {
            defragment();
        }
    }
}
unsafe fn defragment() {
    let mut head = FREE_LIST.lock().expect("FREE_LIST is broken");
    let mut sorted: *mut FreeNode = null_mut();
    let mut current = head.0;

    unsafe {
        while !current.is_null() {
            let next = (*current).next;
            let mut prev: *mut *mut FreeNode = &mut sorted;
            while !(*prev).is_null() && (*prev as usize) < (current as usize) {
                prev = &mut (**prev).next;
            }
            (*current).next = *prev;
            *prev = current;
            current = next;
        }
        let mut node = sorted;
        while !node.is_null() && !(*node).next.is_null() {
            let next = (*node).next;
            let node_end = (node as *mut u8).add((*node).size);

            if node_end == next as *mut u8 {
                (*node).size += (*next).size;
                (*node).next = (*next).next;
            } else {
                node = next;
            }
        }
    }
    head.0 = sorted;
}