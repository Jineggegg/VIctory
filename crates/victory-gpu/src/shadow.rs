//! Host copies of title buffers (decision 0007, step 3 and "Copies of title
//! memory"). Title buffers are title memory, which the host GPU cannot
//! read, so a buffer a draw uses gets a persistent host buffer of its size
//! on first use (system memory shaders may also write), and the ranges each
//! submission reads (vertex, index and constant data, buffer views) are
//! brought up to date from title memory before it runs: the 64 KiB pages
//! the title wrote since they were last copied ([`crate::watch`]: read-only
//! guards and the runtime's reports), all of it where the memory is not the
//! runtime's. A title GPU address inside the buffer is the same offset
//! in its copy. Ranges shaders write go back to title memory after the
//! submission ([`Shadow::gpu_writes`]).
//!
//! The ranges brought up to date are those draws are known to read (vertex
//! and index buffer views, constant buffers in tables, root descriptors,
//! buffer views in tables) and, for shaders that index the heap
//! themselves, every buffer range the heap's views reach
//! (`crate::bindless`).

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use windows::Win32::Graphics::Direct3D12::ID3D12Resource;

use crate::host::{HostList, host};
use crate::watch::PAGE;

#[cfg(test)]
fn merge_writeback(title: &mut [u8], host: &mut [u8], old: &[u8]) -> bool {
    let mut changed = 0;
    for ((title_byte, host_byte), old_byte) in title.iter_mut().zip(host).zip(old) {
        let mask = 0u8.wrapping_sub(u8::from(*host_byte != *old_byte));
        let delta = (*title_byte ^ *host_byte) & mask;
        *title_byte ^= delta;
        *host_byte = *title_byte;
        changed |= delta;
    }
    changed != 0
}

#[test]
fn writeback_merge_matches_bytewise_oracle() {
    for len in [0, 1, 63, 64, 65, 193, 65_536] {
        let old = vec![0x11; len];
        for stride in [1, 5, 67, len + 1] {
            let mut title = old.clone();
            let mut host = old.clone();
            for index in (0..len).step_by(3) {
                title[index] = 0x22;
            }
            for index in (0..len).step_by(stride) {
                host[index] = if index % 2 == 0 { 0x22 } else { 0x33 };
            }
            let expected: Vec<_> = host
                .iter()
                .zip(&old)
                .zip(&title)
                .map(|((&gpu, &previous), &cpu)| if gpu == previous { cpu } else { gpu })
                .collect();
            let changed = expected != title;
            assert_eq!(merge_writeback(&mut title, &mut host, &old), changed);
            assert_eq!(title, expected);
            assert_eq!(host, expected);
        }
    }
}

/// A bounded number of callbacks on the persistent Windows process pool.
/// The caller also works and joins every callback before returning.
fn parallel_items(n: usize, f: impl Fn(usize) + Sync) {
    use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
    use windows_sys::Win32::System::Threading::{
        CloseThreadpoolWork, CreateThreadpoolWork, PTP_CALLBACK_INSTANCE, PTP_WORK,
        SubmitThreadpoolWork, WaitForThreadpoolWorkCallbacks,
    };
    struct Job<'a> {
        n: usize,
        next: AtomicUsize,
        f: &'a (dyn Fn(usize) + Sync),
        panic: Mutex<Option<Box<dyn std::any::Any + Send>>>,
    }
    impl Job<'_> {
        fn run(&self) {
            loop {
                let i = self.next.fetch_add(1, Relaxed);
                if i >= self.n {
                    break;
                }
                (self.f)(i);
            }
        }
    }
    unsafe extern "system" fn worker(
        _: PTP_CALLBACK_INSTANCE,
        context: *mut std::ffi::c_void,
        _: PTP_WORK,
    ) {
        // SAFETY: the caller joins all callbacks before dropping the job.
        let job = unsafe { &*context.cast::<Job<'_>>() };
        if let Err(panic) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| job.run())) {
            *job.panic.lock().unwrap_or_else(|e| e.into_inner()) = Some(panic);
        }
    }
    // Also join on unwind: a work item must never outlive the borrowed job.
    struct Work(PTP_WORK);
    impl Drop for Work {
        fn drop(&mut self) {
            // SAFETY: the work handle is uniquely owned and its context is still live.
            unsafe {
                // The caller drained the index, so pending callbacks can be cancelled.
                WaitForThreadpoolWorkCallbacks(self.0, 1);
                CloseThreadpoolWork(self.0);
            }
        }
    }
    let job = Job {
        n,
        next: 0.into(),
        f: &f,
        panic: Mutex::new(None),
    };
    // SAFETY: the context outlives all callbacks. Each index is claimed once.
    unsafe {
        let work = CreateThreadpoolWork(
            Some(worker),
            std::ptr::from_ref(&job).cast_mut().cast(),
            std::ptr::null(),
        );
        if work == 0 {
            job.run();
            return;
        }
        let work = Work(work);
        static WORKERS: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
        let workers = WORKERS.get_or_init(|| {
            std::thread::available_parallelism()
                .map_or(1, usize::from)
                .min(8)
                - 1
        });
        for _ in 0..(*workers).min(n.saturating_sub(1)) {
            SubmitThreadpoolWork(work.0);
        }
        job.run();
        drop(work);
    }
    if let Some(panic) = job.panic.into_inner().unwrap_or_else(|e| e.into_inner()) {
        std::panic::resume_unwind(panic);
    }
}

/// Complete the kernel copies before updating any snapshot of this run.
fn refresh_run(dst: *mut u8, src: usize, len: usize) {
    const PIECE: usize = 2 << 20;
    if len < 4 * PIECE {
        crate::host::copy_mapped(dst, src as *const u8, len, false);
        return;
    }
    let dst = dst as usize;
    parallel_items(len.div_ceil(PIECE), |i| {
        let offset = i * PIECE;
        crate::host::copy_mapped(
            (dst + offset) as *mut u8,
            (src + offset) as *const u8,
            PIECE.min(len - offset),
            false,
        );
    });
}

/// GetWriteWatch succeeds only on a MEM_WRITE_WATCH VirtualAlloc pool. Such
/// private pages cannot also be mapped as section views. Pagefile sections,
/// placeholders, and mixed allocations always keep their original order.
fn private_write_watch(start: usize, len: usize) -> bool {
    use windows_sys::Win32::System::Memory::GetWriteWatch;
    let (mut address, mut count, mut granularity) = (std::ptr::null_mut(), 1, 0);
    // SAFETY: one output slot, no reset of the application's write-watch state.
    unsafe {
        GetWriteWatch(
            0,
            start as *const _,
            len,
            &mut address,
            &mut count,
            &mut granularity,
        ) == 0
    }
}

/// Writes only bytes changed by the shader. Title mappings may disappear on
/// another thread. The guest stores in the x64 leaf have known fault PCs:
/// if either fails, return the remaining byte count and retry only the unfinished runs in
/// the kernel. Host reads and unrelated faults use normal exception handling.
#[cfg(target_arch = "x86_64")]
mod delta_store {
    use windows_sys::Win32::Foundation::{EXCEPTION_ACCESS_VIOLATION, EXCEPTION_GUARD_PAGE, EXCEPTION_IN_PAGE_ERROR};
    use windows_sys::Win32::System::Diagnostics::Debug::{
        AddVectoredExceptionHandler, EXCEPTION_CONTINUE_EXECUTION, EXCEPTION_CONTINUE_SEARCH,
        EXCEPTION_POINTERS,
    };

    // Windows x64 ABI: rcx = destination, rdx = shader output, r8 = change
    // masks, r9 = length. Only volatile registers are used. A vector whose
    // bytes all changed takes one store. Mixed vectors and tails store bytes
    // individually, so another thread can write even the neighboring byte.
    core::arch::global_asm!(
        r#"
        .text
        .p2align 4
        .globl victory_store_delta, victory_store_vector, victory_store_byte, victory_store_return
    victory_store_delta:
        test r9, r9
        jz 5f
    1:
        movzx eax, word ptr [r8]
        test eax, eax
        jz 3f
        cmp eax, 65535
        jne 6f
        // A store spanning two OS pages must have a precise restart byte
        // even if the second page has different protection.
        mov r10d, ecx
        and r10d, 4095
        cmp r10d, 4080
        ja 6f
        movdqu xmm0, [rdx]
    victory_store_vector:
        movdqu [rcx], xmm0
        jmp 3f
    6:
        // Find the lane and clear its bit independently. The next scan
        // need not wait for the index of this store.
        xor r10d, r10d
        bsf r10d, eax
        mov r11b, [rdx + r10]
    victory_store_byte:
        mov [rcx + r10], r11b
        lea r11d, [rax - 1]
        and eax, r11d
        jnz 6b
    3:
        add rcx, 16
        add rdx, 16
        add r8, 2
        sub r9, 16
        ja 1b
    5:
        xor eax, eax
    victory_store_return:
        ret
    "#
    );

    unsafe extern "system" {
        fn victory_store_delta(dst: usize, gpu: *const u8, masks: *const u16, len: usize) -> usize;
        fn victory_store_vector();
        fn victory_store_byte();
        fn victory_store_return();
    }

    unsafe extern "system" fn on_store_fault(info: *mut EXCEPTION_POINTERS) -> i32 {
        // SAFETY: the OS supplies the record and context for this thread.
        let record = unsafe { &*(*info).ExceptionRecord };
        // Only the two destination-store PCs below qualify. A one-shot guard
        // fault need not supply the access parameters of an access violation.
        let write_fault = matches!(record.ExceptionCode, EXCEPTION_ACCESS_VIOLATION | EXCEPTION_IN_PAGE_ERROR)
            && record.NumberParameters >= 2 && record.ExceptionInformation[0] == 1;
        if !write_fault && record.ExceptionCode != EXCEPTION_GUARD_PAGE {
            return EXCEPTION_CONTINUE_SEARCH;
        }
        let pc = record.ExceptionAddress as usize;
        // SAFETY: this is the context of the faulting leaf. R9 counts bytes
        // remaining from RCX; at the scalar store R10 indexes the failed byte.
        let ctx = unsafe { &mut *(*info).ContextRecord };
        let remaining = if pc == victory_store_vector as *const () as usize {
            ctx.R9
        } else if pc == victory_store_byte as *const () as usize {
            ctx.R9 - ctx.R10
        } else {
            return EXCEPTION_CONTINUE_SEARCH;
        };
        ctx.Rax = remaining;
        ctx.Rip = victory_store_return as *const () as u64;
        EXCEPTION_CONTINUE_EXECUTION
    }

    pub(super) fn remaining_after_store(at: usize, host: &[u8], masks: &[u16]) -> usize {
        static INSTALLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        // SAFETY: the handler lives as long as the process.
        let installed = INSTALLED.get_or_init(|| unsafe {
            !AddVectoredExceptionHandler(1, Some(on_store_fault)).is_null()
        });
        if *installed {
            // SAFETY: both host slices cover the entire run, and both
            // destination store PCs are handled if the title removed its mapping.
            unsafe { victory_store_delta(at, host.as_ptr(), masks.as_ptr(), host.len()) }
        } else {
            host.len()
        }
    }
}

/// One bit per byte. Padding bits in a partial final vector remain clear.
fn change_masks(host: &[u8], old: &[u8], out: &mut [u16]) {
    assert_eq!(host.len(), old.len());
    assert_eq!(out.len(), host.len().div_ceil(16));
    if host == old {
        out.fill(0);
        return;
    }
    for ((h, b), mask) in host.chunks(16).zip(old.chunks(16)).zip(out) {
        #[cfg(target_arch = "x86_64")]
        if h.len() == 16 {
            use std::arch::x86_64::{_mm_cmpeq_epi8, _mm_loadu_si128, _mm_movemask_epi8};
            // SAFETY: SSE2 is baseline on x64; both slices have 16 bytes.
            *mask = unsafe {
                let h = _mm_loadu_si128(h.as_ptr().cast());
                let b = _mm_loadu_si128(b.as_ptr().cast());
                !(_mm_movemask_epi8(_mm_cmpeq_epi8(h, b)) as u16)
            };
            continue;
        }
        *mask = h
            .iter()
            .zip(b)
            .enumerate()
            .fold(0, |m, (i, (x, y))| m | (u16::from(x != y) << i));
    }
}

/// The caller has opened the title range. A zero bit always leaves that
/// byte alone, even when its neighbors changed and the CPU just wrote it.
fn write_masked(at: usize, host: &[u8], masks: &[u16]) {
    assert_eq!(masks.len(), host.len().div_ceil(16));
    if let tail @ 1..=15 = host.len() % 16 {
        assert_eq!(masks.last().unwrap() >> tail, 0);
    }
    #[cfg(target_arch = "x86_64")]
    let mut i = host.len() - delta_store::remaining_after_store(at, host, masks);
    #[cfg(not(target_arch = "x86_64"))]
    let mut i = 0;
    #[cfg(test)]
    if i < host.len() && let Some(callback) = BEFORE_KERNEL_RETRY.take() {
        callback();
    }
    while i < host.len() {
        let from = i;
        while i < host.len() && masks[i / 16] & (1 << (i % 16)) != 0 {
            i += 1;
        }
        if i > from {
            crate::host::write_mapped((at + from) as *mut u8, host[from..].as_ptr(), i - from);
        } else {
            i += 1;
        }
    }
}

#[cfg(test)]
fn write_changed(at: usize, host: &[u8], old: &[u8]) {
    let mut masks = vec![0; host.len().div_ceil(16)];
    change_masks(host, old, &mut masks);
    write_masked(at, host, &masks);
}

#[cfg(test)]
thread_local! {
    static BEFORE_KERNEL_RETRY: std::cell::Cell<Option<Box<dyn FnOnce()>>> = const { std::cell::Cell::new(None) };
    static AFTER_MARK_READ: std::cell::Cell<Option<Box<dyn FnOnce()>>> = const { std::cell::Cell::new(None) };
    /// The calling thread's snapshot write-backs keep address order.
    static FORCE_SEQUENTIAL: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Write contiguous shader changes without replaying adjacent CPU bytes.
fn store_snapshot_changes(
    at: usize, gpu: &[u8], old: &[u8], title: &[u8], masks: &mut Vec<u16>,
) -> bool {
    // With an unchanged title snapshot every shader delta needs publication.
    // The masked store leaves intervening CPU-owned bytes untouched.
    if title == old {
        masks.resize(gpu.len().div_ceil(16), 0);
        change_masks(gpu, old, masks);
        if masks.iter().all(|&bits| bits == 0) {
            return false;
        }
        let _writing = crate::watch::writing(at, gpu.len());
        write_masked(at, gpu, masks);
        return true;
    }
    let mut guard = None;
    let mut i = 0;
    while i < gpu.len() {
        if gpu[i] == old[i] {
            i += 1;
            continue;
        }
        let start = i;
        i += 1;
        while i < gpu.len() && gpu[i] != old[i] {
            i += 1;
        }
        if gpu[start..i] != title[start..i] {
            guard.get_or_insert_with(|| crate::watch::writing(at, gpu.len()));
            crate::host::write_mapped((at + start) as *mut u8, gpu[start..i].as_ptr(), i - start);
        }
    }
    guard.is_some()
}

/// A title buffer's host copy.
pub(crate) struct Shadow {
    pub(crate) buffer: ID3D12Resource,
    data: *mut u8,
    gpu: u64,
    /// Where the title buffer starts and how long it is.
    start: u64,
    len: u64,
    /// The pages changed since the copy took them (none: the memory is not
    /// the runtime's, copied whole each time).
    tracked: Option<crate::watch::Tracked>,
    /// Ranges shaders of a submission not yet copied back may be writing:
    /// title memory there is older than the copy.
    pending: Mutex<std::collections::HashSet<(usize, usize)>>,
    /// The copy's bytes before the submission ran where its shaders write:
    /// what the GPU wrote differs. Disjoint segments (offset, bytes) in
    /// order, and the 64 KiB pieces (by title address) already compared.
    before: Mutex<Snapshot>,
    /// Its first bit in the buffer views' written-pages bitmap, one per
    /// 64 KiB title page it touches (tracked memory only): writes through
    /// views of it are marked there ([`Shadow::mark_entry`]).
    marks: Option<u32>,
    /// Where it starts in `buffer` (0 but for copies sharing a buffer), and
    /// the run of the shared buffer it holds ([`Arena`]), given back when it
    /// goes.
    base: u64,
    slice: Option<Slice>,
}

/// Snapshot segments (offset, bytes) and the pieces compared.
type Snapshot = (Vec<(usize, Vec<u8>)>, std::collections::HashSet<usize>);

/// Snapshot memory kept for reuse (already touched, so no page faults).
static POOL: Mutex<Vec<Vec<u8>>> = Mutex::new(Vec::new());

/// An empty buffer with room for `n` bytes, from the pool if it has one.
fn pooled(n: usize) -> Vec<u8> {
    let mut pool = POOL.lock().unwrap_or_else(|e| e.into_inner());
    let fit = pool
        .iter()
        .enumerate()
        .filter(|(_, v)| v.capacity() >= n)
        .min_by_key(|(_, v)| v.capacity())
        .map(|(i, _)| i);
    let mut v = match fit {
        Some(i) => pool.swap_remove(i),
        None => Vec::with_capacity(n),
    };
    v.clear();
    v
}

/// Returns a snapshot buffer to the pool, which keeps the largest up to
/// [`POOL_BYTES`] in all: a world submission snapshots several copies, and
/// fresh buffers cost a page fault per 4 KiB touched (millions per frame).
fn give_back(v: Vec<u8>) {
    let mut pool = POOL.lock().unwrap_or_else(|e| e.into_inner());
    pool.push(v);
    pool.sort_by_key(|v| std::cmp::Reverse(v.capacity()));
    let mut total = 0;
    pool.retain(|v| {
        total += v.capacity();
        total <= POOL_BYTES
    });
}

/// Snapshot memory kept for reuse.
const POOL_BYTES: usize = 512 << 20;

// SAFETY: D3D12 resources are free-threaded; `data` is a persistent mapping
// updated between submissions. Joined workers access disjoint host ranges.
unsafe impl Send for Shadow {}
// SAFETY: as above.
unsafe impl Sync for Shadow {}

impl Shadow {
    /// The host GPU address of a title address inside the buffer.
    pub(crate) fn gpu(&self, address: u64) -> u64 {
        self.gpu + (address - self.start)
    }

    /// Bytes from a title address to the buffer's end.
    pub(crate) fn left(&self, address: u64) -> u64 {
        (self.start + self.len).saturating_sub(address)
    }

    /// `n` dwords of the copy at title address `address` (tests).
    #[cfg(test)]
    pub(crate) fn read_host(&self, address: u64, n: usize) -> Vec<u32> {
        let at = (address - self.start) as usize;
        assert!(at + 4 * n <= self.len as usize);
        // SAFETY: inside the mapping (checked); the GPU is done with it.
        unsafe { std::slice::from_raw_parts(self.data.add(at).cast::<u32>(), n).to_vec() }
    }

    /// Writes `bytes` into the copy at title address `address`, as shaders
    /// do (tests).
    #[cfg(test)]
    pub(crate) fn write_host(&self, address: u64, bytes: &[u8]) {
        let at = (address - self.start) as usize;
        assert!(at + bytes.len() <= self.len as usize);
        // SAFETY: inside the mapping (checked).
        unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), self.data.add(at), bytes.len()) };
    }

    /// Up to 64 bytes of the copy at title address `address`, as floats, and
    /// which copy it is (diagnostics: what shaders see there).
    #[cfg(feature = "diagnostics")]
    pub(crate) fn peek(&self, address: u64, len: u64) -> String {
        let at = address.saturating_sub(self.start) as usize;
        let n = (len.min(64) as usize).min((self.len as usize).saturating_sub(at)) / 4;
        // SAFETY: inside the mapping (checked); a racing GPU write only makes
        // the logged value stale.
        let v = unsafe { std::slice::from_raw_parts(self.data.add(at).cast::<f32>(), n) };
        format!("copy {:#x}+{:#x} {v:?}", self.start, self.len)
    }

    /// Offset of a title address from the buffer's start.
    pub(crate) fn offset(&self, address: u64) -> u64 {
        address - self.start + self.base
    }

    /// Where the title memory it copies starts, and its length.
    pub(crate) fn range(&self) -> (u64, u64) {
        (self.start, self.len)
    }

    /// After a submission whose shaders wrote title memory at addresses they
    /// computed (`crate::memory`), somewhere in the 64 KiB page at `page`:
    /// the bytes of the page (cut to the copy) that differ from title memory
    /// go back into it, when the title has not written the page since the
    /// copy took it (then title memory is the copy as it was before the
    /// submission, and what differs is what the GPU wrote). Returns the
    /// bytes written; a page the title wrote meanwhile keeps the title's
    /// bytes (logged once).
    // shortcut: a page the copy holds only in part (at its ends) has no
    // "copied" mark and is compared without the title-write check.
    pub(crate) fn copy_back_page(&self, page: usize) -> usize {
        let (start, end) = (self.start as usize, (self.start + self.len) as usize);
        let (from, to) = (page.max(start), (page + PAGE).min(end));
        if from >= to {
            return 0;
        }
        if let Some(t) = &self.tracked
            && t.changed(page / PAGE)
        {
            static ONCE: std::sync::Once = std::sync::Once::new();
            ONCE.call_once(|| {
                tracing::warn!(
                    page = format_args!("{page:#x}"),
                    "the title and translated shaders both wrote a page of title memory in one submission: the title's bytes kept (logged once)"
                )
            });
            return 0;
        }
        let n = to - from;
        // SAFETY: inside the mapping; only the submitting thread writes it.
        let host = unsafe { std::slice::from_raw_parts(self.data.add(from - start), n) };
        let mut title = vec![0u8; n];
        if !crate::host::read_into(from as *const u8, &mut title) || title == host {
            return 0;
        }
        {
            let _writing = crate::watch::writing(from, n);
            crate::host::write_mapped(from as *mut u8, host.as_ptr(), n);
        }
        // Our write is not the title's: the copy holds the page as it is.
        if let Some(t) = &self.tracked {
            t.taken(&[page / PAGE]);
        }
        n
    }

    /// Brings `len` bytes at `address` up to date when `list` is submitted,
    /// and keeps the copy alive until the list is reset.
    pub(crate) fn refresh(self: &Arc<Self>, list: &HostList, address: u64, len: u64) {
        let len = len.min(self.start + self.len - address);
        list.defer_refresh(self, address as usize, len as usize);
    }

    fn new(buffer: ID3D12Resource, data: *mut u8, gpu: u64, start: u64, len: u64) -> Arc<Shadow> {
        Shadow::new_at(buffer, data, gpu, start, len, None)
    }

    /// [`Shadow::new`] of a copy in a run of a shared buffer (`data` and
    /// `gpu` are the run's).
    fn new_at(buffer: ID3D12Resource, data: *mut u8, gpu: u64, start: u64, len: u64, slice: Option<Slice>) -> Arc<Shadow> {
        let tracked = crate::watch::track_copy(start as usize, len as usize, true);
        let marks = tracked.as_ref().and_then(|_| marks().take(pages(start, len)));
        let shadow = Arc::new(Shadow {
            buffer,
            data,
            gpu,
            start,
            len,
            tracked,
            pending: Mutex::default(),
            before: Mutex::default(),
            marks,
            base: slice.map_or(0, |s| s.offset),
            slice,
        });
        if let Some(base) = marks {
            self::marks().owners.insert(base, Arc::downgrade(&shadow));
        }
        shadow
    }

    /// The view-mark entry (`victory_shader::emit`, "Write marks") of a
    /// view of this copy whose first byte is at title address `start`: the
    /// bit of that byte's page and its offset in the page. None where
    /// writes through it are not marked (memory not tracked, or no bits
    /// left): such views declare what they write ([`Shadow::gpu_writes`]).
    pub(crate) fn mark_entry(&self, start: u64) -> Option<(u32, u32)> {
        let base = self.marks?;
        let page = (start >> 16).checked_sub(self.start >> 16)?;
        Some((base + u32::try_from(page).ok()?, (start & 0xFFFF) as u32))
    }

    /// Whether writes through views of this copy are marked.
    pub(crate) fn marked(&self) -> bool {
        self.marks.is_some()
    }

    /// `[address, address + len)` cut to the buffer, as offsets in the copy.
    fn inside(&self, address: usize, len: usize) -> (usize, usize) {
        let (start, end) = (self.start as usize, (self.start + self.len) as usize);
        let from = address.clamp(start, end);
        (from - start, (address + len).clamp(from, end) - start)
    }

    /// Shaders of `list` may write `[address, address + len)` of the copy
    /// (unordered access): after the list's submission has run, the range
    /// is copied back into title memory, where the title reads it; until
    /// then no refresh copies title memory over it. Call after the
    /// [`Shadow::refresh`] that gives the shaders the title's data.
    ///
    /// shortcut: what shaders wrote is found by keeping the range's bytes
    /// before the submission (a copy per declared range and submission);
    /// a GPU-side dirty mask would save that if big ranges make it slow.
    ///
    /// `marked`: the shaders mark the pages they write through this copy's
    /// views (`victory_shader::emit`, "Write marks"): no snapshot then, only
    /// the pages they marked are compared ([`Shadow::copy_back_marked`]).
    pub(crate) fn gpu_writes(self: &Arc<Self>, list: &HostList, address: u64, len: u64, marked: bool) {
        let len = len.min(self.left(address));
        list.defer_gpu_write(self, address as usize, len as usize, marked && self.marked());
    }

    /// At submission, after the range's refresh: a submitted list's shaders
    /// write this range. Its bytes are kept to tell later what the GPU wrote:
    /// per copy and submission, segments covering the ranges the submission
    /// declares (the world declares overlapping views of its 174 MB heap
    /// dozens of times; one segment over all of them copied most of the
    /// heap), in reused memory (fresh snapshots cost ~2.3 million page
    /// faults per world frame).
    ///
    /// With `marked`, nothing is kept: the pages are armed instead, so that
    /// a title write there before the copy-back keeps the page's bytes from
    /// before it ([`crate::watch::Tracked::arm`]).
    pub(crate) fn writing(&self, address: usize, len: usize, marked: bool) {
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert((address, address + len));
        if marked && let Some(t) = &self.tracked {
            t.arm(address, address + len);
            return;
        }
        let (from, to) = self.inside(address, len);
        if from >= to {
            return;
        }
        // SAFETY: inside the mapping.
        let host = |a: usize, b: usize| unsafe { std::slice::from_raw_parts(self.data.add(a), b - a) };
        let mut before = self.before.lock().unwrap_or_else(|e| e.into_inner());
        let segments = &mut before.0;
        // The segments the range overlaps or touches.
        let first = segments.partition_point(|(at, bytes)| at + bytes.len() < from);
        let last = first + segments[first..].iter().take_while(|(at, _)| *at <= to).count();
        if let [(at, bytes)] = &segments[first..last]
            && *at <= from
            && to <= at + bytes.len()
        {
            return;
        }
        let start = segments.get(first).filter(|_| first < last).map_or(from, |s| s.0.min(from));
        let end = segments[first..last].last().map_or(to, |(at, bytes)| (at + bytes.len()).max(to));
        #[cfg(feature = "diagnostics")]
        let _timed = crate::watch::Timed(std::time::Instant::now(), crate::copystat::C::SnapshotUs);
        #[cfg(feature = "diagnostics")]
        crate::copystat::add(crate::copystat::C::SnapshotBytes, (end - start) as u64);
        #[cfg(feature = "diagnostics")]
        crate::copystat::snapshot(self.start, self.len, (end - start) as u64);
        // Parts newly covered from the copy, the rest as it was taken.
        let mut grown = pooled(end - start);
        let mut at = start;
        for (taken, bytes) in segments.drain(first..last) {
            grown.extend_from_slice(host(at, taken));
            grown.extend_from_slice(&bytes);
            at = taken + bytes.len();
            give_back(bytes);
        }
        grown.extend_from_slice(host(at, end));
        segments.insert(first, (start, grown));
    }

    /// `[address, address + len)` expanded outward to 64 KiB page
    /// boundaries and cut to the copy (`[start, start + len)`): whole pages
    /// so [`crate::watch::Tracked::take`] guards them for
    /// [`Shadow::copy_back_marked`].
    pub(crate) fn page_range(&self, address: u64, len: u64) -> (u64, u64) {
        let end = self.start.saturating_add(self.len);
        let from = address.clamp(self.start, end);
        let to = address.saturating_add(len).clamp(from, end);
        if from >= to {
            return (from, 0);
        }
        let page = PAGE as u64;
        let from = (from & !(page - 1)).max(self.start);
        let to = to.checked_next_multiple_of(page).unwrap_or(end).min(end);
        (from, to - from)
    }

    /// After the submission ran: copies the bytes its shaders wrote back
    /// into title memory (unmapped pages left alone), and only those: the
    /// title's threads write the same buffers while the GPU runs (its
    /// command memory shares the big GPU heap with what shaders write), and
    /// putting back the copy's older bytes there broke the title's own
    /// records (a fence list read as -1, title+0x4e9b54a). Returns the bytes
    /// written.
    pub(crate) fn copy_back(&self, address: usize, len: usize) -> usize {
        self.copy_back_or_not(address, len, true)
    }

    /// After the submission ran: what its shaders wrote in the range stays
    /// in the copy and does not go into title memory (they computed it
    /// from memory with no copy, `crate::memory`).
    pub(crate) fn keep_writes(&self, address: usize, len: usize) {
        self.copy_back_or_not(address, len, false);
    }

    fn copy_back_or_not(&self, address: usize, len: usize, back: bool) -> usize {
        self.snapshot_copy(address, len, back, crate::host::read_into)
    }

    #[cfg(test)]
    fn copy_back_or_not_with(
        &self, address: usize, len: usize, back: bool,
        read_title: impl FnMut(*const u8, &mut [u8]) -> bool + Send,
    ) -> usize {
        let read = Mutex::new(read_title);
        self.snapshot_copy(address, len, back, |at, bytes| {
            read.lock().unwrap_or_else(|e| e.into_inner())(at, bytes)
        })
    }

    fn snapshot_copy(
        &self, address: usize, len: usize, back: bool,
        read_title: impl Fn(*const u8, &mut [u8]) -> bool + Sync,
    ) -> usize {
        let mut snapshot = self.before.lock().unwrap_or_else(|e| e.into_inner());
        let (segments, compared) = &mut *snapshot;
        let mut written = 0;
        let (from, to) = self.inside(address, len);
        let start = self.start as usize;
        if let Some((taken, before)) = segments.iter_mut().find(|(at, bytes)| *at <= from && to <= at + bytes.len())
            && back
            && from < to
        {
            let before = &mut before[from - *taken..to - *taken];
            #[cfg(feature = "diagnostics")]
            let _timed = crate::watch::Timed(std::time::Instant::now(), crate::copystat::C::CompareUs);
            // By 64 KiB piece: the copy compared with its bytes before the
            // submission, title memory read only where the GPU wrote.
            const PIECE: usize = 64 << 10;
            // Host bytes and the baseline are stable until the final resync.
            // Find the work before scheduling: a large unchanged or sparse
            // declaration needs neither workers nor a page-sized read buffer.
            let host = |at: usize, n: usize| {
                // SAFETY: each piece is inside the shadow, and the GPU is
                // finished. All reads join before the shadow is updated.
                unsafe { std::slice::from_raw_parts(self.data.add(at), n) }
            };
            let mut changed = Vec::new();
            let mut at = from;
            while at < to {
                let n = (PIECE - (start + at) % PIECE).min(to - at);
                let page = (start + at) & !(PIECE - 1);
                if !compared.contains(&page) {
                    #[cfg(feature = "diagnostics")]
                    crate::copystat::add(crate::copystat::C::CompareBytes, n as u64);
                    if host(at, n) != &before[at - from..at - from + n] {
                        changed.push((at, n));
                    }
                }
                at += n;
            }
            let publish = |pieces: &[(usize, usize)]| {
                let mut title = vec![0; pieces.iter().map(|&(_, n)| n).max().unwrap_or(0)];
                let mut masks = Vec::new();
                let mut synced = Vec::with_capacity(pieces.len());
                for &(at, n) in pieces {
                    let title = &mut title[..n];
                    if read_title((start + at) as *const u8, title) {
                        let old = &before[at - from..at - from + n];
                        let wrote = store_snapshot_changes(start + at, host(at, n), old, title, &mut masks);
                        synced.push((at, n, wrote));
                    }
                }
                synced
            };
            // A successful write-watch query covers one private allocation.
            // Its disjoint pages cannot be aliases of each other while it
            // lives: the hold, taken before the query and kept until every
            // group joins, makes the runtime's releases and view mappings
            // wait. Preserve address order for section views and mixed
            // mappings, and while a remap is under way.
            const MIN_GROUP: usize = 4;
            #[cfg(test)]
            let sequential = FORCE_SEQUENTIAL.get();
            #[cfg(not(test))]
            let sequential = false;
            let held = (!sequential && changed.len() >= 4 * MIN_GROUP)
                .then(crate::watch::hold_mappings)
                .flatten()
                .filter(|_| private_write_watch(start + from, to - from));
            let synced = if let Some(_held) = held {
                // Bound scratch allocations even for large declarations.
                let group_size = MIN_GROUP.max(changed.len().div_ceil(16));
                let groups: Vec<_> = changed.chunks(group_size)
                    .map(|pieces| (pieces, std::sync::OnceLock::new()))
                    .collect();
                #[cfg(test)]
                let calls = std::sync::atomic::AtomicUsize::new(0);
                parallel_items(groups.len(), |i| {
                    let (pieces, result) = &groups[i];
                    #[cfg(test)]
                    let initial = crate::host::MAPPED_WRITE_CALLS.get();
                    let rows = publish(pieces);
                    #[cfg(test)]
                    {
                        // Keep the calling test's kernel-write counter valid
                        // across callbacks, including work it does itself.
                        let count = crate::host::MAPPED_WRITE_CALLS.replace(initial) - initial;
                        calls.fetch_add(count, std::sync::atomic::Ordering::Relaxed);
                    }
                    result.set(rows).expect("each group is claimed once");
                });
                #[cfg(test)]
                crate::host::MAPPED_WRITE_CALLS.set(crate::host::MAPPED_WRITE_CALLS.get() + calls.into_inner());
                groups.into_iter().flat_map(|(_, rows)| rows.into_inner().expect("joined group")).collect()
            } else {
                publish(&changed)
            };
            for &(at, n, wrote) in &synced {
                if wrote { written += n; }
                // Leave failed reads and partial pages eligible for later declarations.
                if n == PIECE { compared.insert(start + at); }
            }
            // Pages made equal to title memory are current in the copy, so
            // our own write is not taken for the title's; others keep what
            // they had (a title write there shows at the next refresh).
            let (first, last) = ((start + from).div_ceil(PAGE), (start + to) / PAGE);
            if let Some(t) = &self.tracked {
                let whole: Vec<usize> = synced.iter()
                    .map(|&(at, _, _)| (start + at) / PAGE)
                    .filter(|p| (first..last).contains(p)).collect();
                t.taken(&whole);
            }
            for (at, n, _) in synced {
                // Rearm first. A later CPU write will mark the copy dirty again.
                // SAFETY: this piece lies inside the mapped shadow.
                let host = unsafe { std::slice::from_raw_parts_mut(self.data.add(at), n) };
                if crate::host::read_into((start + at) as *const u8, host) && n != PIECE {
                    // An overlapping declaration may visit a partial piece again.
                    // The CPU bytes the shadow just took are now its baseline.
                    before[at - from..at - from + n].copy_from_slice(host);
                }
            }
        }
        // Every submitted step that declared the range is done with it (one
        // submission runs at a time), and the snapshot with the last range.
        let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        pending.remove(&(address, address + len));
        if pending.is_empty() {
            compared.clear();
            for (_, bytes) in segments.drain(..) {
                give_back(bytes);
            }
            if let Some(t) = &self.tracked {
                t.disarm();
            }
        }
        written
    }

    /// After a submission whose shaders marked the 64 KiB title page at
    /// `page` and the next `count - 1` pages as written through views of this copy: the bytes they changed
    /// go into title memory, and the copy takes the title's others. What
    /// they changed is what differs from the copy's bytes before the
    /// submission: title memory where the title has not written the page
    /// since the copy took it, else what the page held when the title first
    /// wrote it ([`crate::watch::Tracked::saved`], if the page was armed).
    /// Returns the bytes written.
    // shortcut: a page the title wrote that was not armed (a write through
    // the heap's views no list declared) keeps the title's bytes, losing
    // the shaders' there (counted).
    fn copy_back_marked(&self, page: usize, count: usize) -> usize {
        self.copy_back_marked_with(page, count, |at, bytes| {
            crate::host::read_into(at as *const u8, bytes)
        })
    }

    /// Only valid declarations on this shared page go back. Keep the shader's
    /// withheld bytes in the shadow and leave the page's tracker outstanding.
    fn copy_back_marked_ranges(&self, page: usize, spans: &[(usize, usize)]) -> usize {
        let Some(t) = &self.tracked else { return 0 };
        let start = self.start as usize;
        let from = start.max(page);
        let end = (start + self.len as usize).min(page + PAGE);
        let index = page / PAGE;
        t.arm(from, end);
        let was_dirty = t.changed(index);
        let mut title = vec![0; end - from];
        let read = crate::host::read_into(from as *const u8, &mut title);
        t.unarm(index);
        let saved = t.saved(index);
        if !read || (was_dirty && saved.is_none()) { return 0; }
        let mut written = 0;
        let mut masks = Vec::new();
        let mut snapshot = self.before.lock().unwrap_or_else(|e| e.into_inner());
        for &(a, b) in spans {
            let (a, b) = (a.max(from), b.min(end));
            if a >= b { continue; }
            // SAFETY: this piece lies entirely inside the mapped shadow.
            let host = unsafe { std::slice::from_raw_parts_mut(self.data.add(a - start), b - a) };
            let title = &title[a - from..b - from];
            let old = saved.as_ref().map_or(title, |s| &s[a - page..b - page]);
            if store_snapshot_changes(a, host, old, title, &mut masks) { written += b - a; }
            if crate::host::read_into(a as *const u8, host) {
                for (at, bytes) in &mut snapshot.0 {
                    let (s, e) = ((a - start).max(*at), (b - start).min(*at + bytes.len()));
                    if s < e { bytes[s - *at..e - *at].copy_from_slice(&host[start + s - a..start + e - a]); }
                }
            }
        }
        written
    }

    fn copy_back_marked_with(
        &self,
        page: usize,
        count: usize,
        mut read_title: impl FnMut(usize, &mut [u8]) -> bool,
    ) -> usize {
        let Some(t) = &self.tracked else { return 0 };
        let start = self.start as usize;
        let from = start.max(page);
        let to = (start + self.len as usize).min(page + count * PAGE);
        if from >= to {
            return 0;
        }
        t.arm(from, to);
        // Keep one bit per byte rather than a second copy of every page.
        // One small read buffer is reused for the full run.
        let mut masks = Vec::with_capacity((to - from).div_ceil(16) + count);
        // Batch the kernel reads, leaving the submitting thread's stack small.
        // A failed block still skips the whole page before any publication.
        let mut title = vec![0u8; (to - from).min(PAGE / 2)];
        let mut changed = Vec::new();
        let mut taken = Vec::new();
        let mut synced = Vec::new();
        for p in page / PAGE..to.div_ceil(PAGE) {
            let a = from.max(p * PAGE);
            let b = to.min((p + 1) * PAGE);
            let was_dirty = t.changed(p);
            // SAFETY: the submitted GPU has finished, and the range is cut to this shadow.
            let host = unsafe { std::slice::from_raw_parts(self.data.add(a - start), b - a) };
            let first = masks.len();
            masks.resize(first + host.len().div_ceil(16), 0);
            let mask = &mut masks[first..];
            let mut read = true;
            for (i, block) in host.chunks(title.len()).enumerate() {
                let offset = i * title.len();
                let bytes = &mut title[..block.len()];
                if !read_title(a + offset, bytes) {
                    read = false;
                    break;
                }
                change_masks(
                    block,
                    bytes,
                    &mut mask[offset / 16..(offset + block.len()).div_ceil(16)],
                );
            }
            t.unarm(p);
            let saved = t.saved(p);
            if !read || (was_dirty && saved.is_none()) {
                #[cfg(feature = "diagnostics")]
                if read {
                    crate::copystat::add(crate::copystat::C::MarkedKept, 1);
                }
                continue;
            }
            synced.push((a, b));
            if let Some(before) = saved {
                change_masks(host, &before[a - p * PAGE..b - p * PAGE], mask);
            }
            if mask.iter().any(|&word| word != 0) {
                changed.push((a, b, first));
            }
            if a == p * PAGE && b == (p + 1) * PAGE {
                taken.push(p);
            }
        }
        // A snapshot declaration can overlap a marked view. After this pass
        // the host also contains the CPU's bytes. Comparing them to the old
        // snapshot would put them back over later CPU writes. Each synced
        // range covers every byte of this shadow in that title page.
        {
            let mut snapshot = self.before.lock().unwrap_or_else(|e| e.into_inner());
            let (segments, compared) = &mut *snapshot;
            if !segments.is_empty() {
                for &(a, b) in &synced {
                    let i = segments.partition_point(|(at, bytes)| at + bytes.len() <= a - start);
                    if segments.get(i).is_some_and(|(at, _)| *at < b - start) {
                        compared.insert(a & !(PAGE - 1));
                    }
                }
            }
        }
        if changed.is_empty() && synced.iter().all(|&(a, _)| !t.changed(a / PAGE)) {
            return 0;
        }
        #[cfg(test)]
        AFTER_MARK_READ.with(|slot| {
            if let Some(f) = slot.take() {
                f();
            }
        });
        let mut written = 0;
        // Touch only pages actually written. Adjacent ones can share a guard.
        for run in changed.chunk_by(|left, right| left.1 == right.0) {
            let a = run[0].0;
            let b = run[run.len() - 1].1;
            let _writing = crate::watch::writing(a, b - a);
            for &(at, end, first) in run {
                // SAFETY: the range is inside the shadow.
                let host =
                    unsafe { std::slice::from_raw_parts(self.data.add(at - start), end - at) };
                write_masked(at, host, &masks[first..first + host.len().div_ceil(16)]);
            }
            written += b - a;
        }
        t.taken(&taken);
        // A title write before rearming is included here, and a later one
        // marks the copy dirty. Also reconciles writes through another view.
        // A skipped dirty page retains its GPU bytes and outstanding tracker.
        for run in synced.chunk_by(|left, right| left.1 == right.0) {
            let a = run[0].0;
            let b = run[run.len() - 1].1;
            // SAFETY: the synced subrange lies inside the mapped shadow.
            refresh_run(unsafe { self.data.add(a - start) }, a, b - a);
        }
        written
    }

    /// Whether the title wrote none of the copy's memory since the copy took
    /// it (tracked memory only): [`Shadow::update`] would copy nothing.
    pub(crate) fn clean(&self) -> bool {
        self.tracked.as_ref().is_some_and(crate::watch::Tracked::clean)
    }

    /// Its tracker's bit: `crate::watch::clean_bit` of it is
    /// [`Shadow::clean`] (none for memory that is not tracked: never clean).
    pub(crate) fn dirty_bit(&self) -> Option<u32> {
        self.tracked.as_ref().and_then(crate::watch::Tracked::bit)
    }

    /// At submission: copies what changed of `[address, address + len)`
    /// since it was last copied (all of it if the memory is not
    /// write-watched). Returns the bytes copied.
    pub(crate) fn update(&self, address: usize, len: usize) -> usize {
        if self.clean() {
            return 0;
        }
        let (start, end) = (self.start as usize, (self.start + self.len) as usize);
        let (from, to) = (address.max(start), (address + len).min(end));
        if from >= to {
            return 0;
        }
        let mut busy: Vec<(usize, usize)> = self
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .copied()
            .filter(|&(a, e)| a < to && e > from)
            .collect();
        if self.tracked.is_some() {
            return self.update_tracked(from, to, !busy.is_empty());
        }
        if !busy.is_empty() {
            // Around ranges shaders are writing: the gaps, each as below
            // (in the world most submissions have such ranges; copying the
            // gaps whole cost ~1 s per frame). Under them nothing: what the
            // title wrote there is not known.
            busy.sort_unstable();
            let (mut at, mut bytes) = (from, 0);
            for (a, e) in busy {
                let (a, e) = (a.max(from), e.min(to));
                if a > at {
                    bytes += self.update_untracked(at, a);
                }
                at = at.max(e);
            }
            if at < to {
                bytes += self.update_untracked(at, to);
            }
            return bytes;
        }
        self.update_untracked(from, to)
    }

    /// [`Shadow::update`] of `[from, to)` (inside the buffer) where the
    /// memory is tracked: the pages the title wrote since they were taken,
    /// in one [`crate::watch::Tracked::take`], so that a page wholly in the
    /// range is guarded even where shaders write part of it (split at those
    /// ranges it stayed changed, and [`Shadow::copy_back_marked`] dropped
    /// what they wrote).
    ///
    /// `busy`: a list submitted earlier declared that shaders write in the
    /// range. A submission waits for the one before it, so none of this
    /// submission's shaders has run yet: the pages the title wrote are
    /// copied as anywhere else. Skipped, the title's constants there stayed
    /// stale for the whole submission (a dispatch read its sizes as zeros
    /// and looped until the device was removed). The same bytes go into the
    /// snapshot `writing` took, so copy-back neither takes them for the
    /// GPU's nor puts them back over a later title write.
    fn update_tracked(&self, from: usize, to: usize, busy: bool) -> usize {
        let Some(t) = &self.tracked else {
            return 0;
        };
        #[cfg(feature = "diagnostics")]
        let started = std::time::Instant::now();
        let (runs, unmapped) = t.take_split(from, to);
        #[cfg(feature = "diagnostics")]
        crate::copystat::add(crate::copystat::C::WatchUs, started.elapsed().as_micros() as u64);
        let start = self.start as usize;
        let mut before = busy.then(|| self.before.lock().unwrap_or_else(|e| e.into_inner()));
        let mut sync_before = |a: usize, b: usize| {
            let Some(before) = &mut before else { return };
            let (a, b) = (a - start, b - start);
            for (at, bytes) in before.0.iter_mut() {
                let (s, e) = ((*at).max(a), (*at + bytes.len()).min(b));
                if s < e {
                    // SAFETY: inside the mapping.
                    let host = unsafe { std::slice::from_raw_parts(self.data.add(s), e - s) };
                    bytes[s - *at..e - *at].copy_from_slice(host);
                }
            }
        };
        let mut copied = 0;
        for (a, b) in runs {
            copied += self.copy(a, b);
            sync_before(a, b);
        }
        for (a, b) in unmapped {
            // SAFETY: inside the mapping (`take_split` cuts to the buffer).
            unsafe { self.data.add(a - start).write_bytes(0, b - a) };
            sync_before(a, b);
            #[cfg(feature = "diagnostics")]
            crate::copystat::add(crate::copystat::C::Unmapped, (b - a) as u64);
        }
        // Parts on memory that is not the runtime's: whole, as below.
        for (a, b) in t.untracked(from, to) {
            #[cfg(feature = "diagnostics")]
            let started = std::time::Instant::now();
            copied += self.copy(a, b);
            sync_before(a, b);
            #[cfg(feature = "diagnostics")]
            {
                use crate::copystat::{C, add};
                add(C::WholeBytes, (b - a) as u64);
                add(C::WholeUs, started.elapsed().as_micros() as u64);
            }
        }
        #[cfg(feature = "diagnostics")]
        {
            use crate::copystat::{C, add};
            add(C::Watches, 1);
            add(C::Checked, (to - from) as u64);
            add(C::Unchanged, (to - from).saturating_sub(copied) as u64);
        }
        copied
    }

    /// [`Shadow::update`] of untracked `[from, to)` (inside the buffer).
    fn update_untracked(&self, from: usize, to: usize) -> usize {
        #[cfg(feature = "diagnostics")]
        let started = std::time::Instant::now();
        self.copy(from, to);
        #[cfg(feature = "diagnostics")]
        {
            use crate::copystat::{C, add};
            add(C::WholeBytes, (to - from) as u64);
            add(C::WholeUs, started.elapsed().as_micros() as u64);
        }
        to - from
    }

    /// Copies title memory `[from, to)`, cut to the buffer, into the copy.
    fn copy(&self, from: usize, to: usize) -> usize {
        let (start, end) = (self.start as usize, (self.start + self.len) as usize);
        let (from, to) = (from.max(start), to.min(end));
        // SAFETY: inside the mapping (`from` and `to` are within the buffer).
        let at = unsafe { self.data.add(from - start) };
        refresh_run(at, from, to - from);
        if let Some(t) = &self.tracked {
            // SAFETY: `to - from` bytes inside the mapping, just copied.
            t.refreshed(from, unsafe { std::slice::from_raw_parts(at, to - from) });
        }
        to - from
    }
}

impl Drop for Shadow {
    fn drop(&mut self) {
        if let Some(slice) = self.slice {
            arena().give_back(slice);
        }
        if let Some(base) = self.marks {
            let mut m = marks();
            m.owners.remove(&base);
            m.give_back(base, pages(self.start, self.len));
        }
    }
}

/// The 64 KiB title pages `[start, start + len)` touches.
fn pages(start: u64, len: u64) -> u32 {
    ((start + len.max(1) - 1) >> 16).saturating_sub(start >> 16) as u32 + 1
}

/// The bits of the buffer views' written-pages bitmap (`victory_shader::
/// emit`, "Write marks"): a run per tracked copy, a bit per title page it
/// touches, for as long as it lives.
#[derive(Default)]
struct Marks {
    /// Free runs below `end`, by first bit: their length.
    free: BTreeMap<u32, u32>,
    /// Bits handed out at some time: those past it are clear.
    end: u32,
    /// The copy holding each run, by its first bit.
    owners: BTreeMap<u32, std::sync::Weak<Shadow>>,
}

static MARKS: Mutex<Marks> = Mutex::new(Marks {
    free: BTreeMap::new(),
    end: 0,
    owners: BTreeMap::new(),
});

fn marks() -> std::sync::MutexGuard<'static, Marks> {
    MARKS.lock().unwrap_or_else(|e| e.into_inner())
}

impl Marks {
    /// A run of `n` bits: its first. None when the bitmap is full (the last
    /// bit takes the marks of offsets past it, so it is never handed out).
    fn take(&mut self, n: u32) -> Option<u32> {
        if let Some((&at, &len)) = self.free.iter().find(|&(_, &len)| len >= n) {
            self.free.remove(&at);
            if len > n {
                self.free.insert(at + n, len - n);
            }
            return Some(at);
        }
        if self.end + n < victory_shader::emit::VIEW_WRITE_BITS {
            self.end += n;
            return Some(self.end - n);
        }
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| tracing::warn!("no bits left in the views' written-pages bitmap: copies made now declare their shaders' writes"));
        None
    }

    /// Capture strong owners once, under the bitmap lock. Retain the live
    /// prefix even if a sparse set of marks also names a hole after it.
    fn owners_of<'a>(&self, mut bits: &'a [u32]) -> Vec<(Arc<Shadow>, &'a [u32])> {
        use std::ops::Bound::{Excluded, Unbounded};
        let mut owners = Vec::new();
        while let Some(&first) = bits.first() {
            let next = self
                .owners
                .range((Excluded(first), Unbounded))
                .next()
                .map_or(u32::MAX, |(&base, _)| base);
            let n = bits.partition_point(|&bit| bit < next);
            let (group, rest) = bits.split_at(n);
            if let Some((&base, copy)) = self.owners.range(..=first).next_back()
                && let Some(copy) = copy.upgrade()
            {
                let end = base + pages(copy.start, copy.len);
                let live = group.partition_point(|&bit| bit < end);
                // Keep even an empty group's Arc until the lock is released:
                // dropping the last strong owner returns its bits to this map.
                owners.push((copy, &group[..live]));
            }
            bits = rest;
        }
        owners
    }

    /// The run of `n` bits at `at` is free again (joined to its neighbours).
    fn give_back(&mut self, mut at: u32, mut n: u32) {
        if let Some((&before, &len)) = self.free.range(..at).next_back()
            && before + len == at
        {
            self.free.remove(&before);
            (at, n) = (before, n + len);
        }
        if let Some(len) = self.free.remove(&(at + n)) {
            n += len;
        }
        self.free.insert(at, n);
    }
}

/// The buffer views' written-pages bitmap: the buffer, its mapping (bits
/// as dwords) and host GPU address.
struct Bitmap {
    _buffer: ID3D12Resource,
    data: usize,
    gpu: u64,
}

// SAFETY: D3D12 resources are free-threaded; the mapping is read and cleared
// only between submissions, which run one at a time.
unsafe impl Send for Bitmap {}
// SAFETY: as above.
unsafe impl Sync for Bitmap {}

/// The bitmap of other shaders, or (`memory`) of shaders that use title
/// memory, whose marks go back only while it is not withheld
/// (`crate::memory::withholding`).
fn bitmap(memory: bool) -> Option<&'static Bitmap> {
    static BITMAPS: [std::sync::OnceLock<Option<Bitmap>>; 2] = [const { std::sync::OnceLock::new() }; 2];
    BITMAPS[usize::from(memory)]
        .get_or_init(|| {
            // Written pages, one read mark per heap index, then one per
            // view heap page.
            let bits = victory_shader::emit::VIEW_PAGE_READS + PAGE_MARKS;
            let bytes = u64::from(bits / 8);
            let (buffer, data) = host()?.shared_buffer(bytes)?;
            crate::host::name(&buffer, || "views' written pages and read slots".into());
            // SAFETY: our mapped buffer of `bytes` bytes.
            unsafe { std::ptr::write_bytes(data, 0, bytes as usize) };
            // SAFETY: a live buffer.
            let gpu = unsafe { buffer.GetGPUVirtualAddress() };
            Some(Bitmap {
                _buffer: buffer,
                data: data as usize,
                gpu,
            })
        })
        .as_ref()
}

/// The host GPU address of the buffer views' written-pages bitmap, for the
/// root UAV of shaders that mark their writes (`memory`: shaders that use
/// title memory, see [`bitmap`]).
pub(crate) fn view_writes(memory: bool) -> Option<u64> {
    bitmap(memory).map(|b| b.gpu)
}

/// Heap slots whose texture views shaders read: the marks seen so far (a
/// slot's stays set in the bitmap until [`forget_read`]), and the slots to
/// clear before the next submission ([`forget_reads_now`]) or at the next
/// [`take_reads`].
static READS: Mutex<(Vec<u32>, Vec<u32>)> = Mutex::new((Vec::new(), Vec::new()));

/// Both bitmaps' read marks (shaders that use title memory mark their reads
/// in theirs).
///
/// # Safety
/// The GPU is done with them (one submission runs at a time), and no other
/// caller holds them.
unsafe fn read_maps<'a>() -> Vec<&'a mut [u32]> {
    let words = (victory_shader::emit::HEAP_VIEWS / 32) as usize;
    let first = (victory_shader::emit::VIEW_READS / 32) as usize;
    [false, true]
        .into_iter()
        .filter_map(bitmap)
        // SAFETY: the mapped bitmap's read marks, as the caller says.
        .map(|b| unsafe { std::slice::from_raw_parts_mut((b.data as *mut u32).add(first), words) })
        .collect()
}

/// Read marks cleared by [`clear_forgotten`] before [`reads_since`] took
/// them, a bit per slot: still reads since its last call.
static CLEARED: Mutex<Vec<u32>> = Mutex::new(Vec::new());

/// Clears the read marks of the slots forgotten ([`forget_read`]) since
/// the last call. Marks are by slot, whatever the heap: one set is kept for
/// [`reads_since`] (another heap's shader may have read the slot).
fn clear_forgotten(maps: &mut [&mut [u32]], seen: &mut Vec<u32>, forgotten: &mut Vec<u32>) {
    let words = (victory_shader::emit::HEAP_VIEWS / 32) as usize;
    seen.resize(words, 0);
    if forgotten.is_empty() {
        return;
    }
    let mut cleared = CLEARED.lock().unwrap_or_else(|e| e.into_inner());
    cleared.resize(words, 0);
    for slot in forgotten.drain(..) {
        let (w, bit) = ((slot / 32) as usize, 1u32 << (slot % 32));
        if w < words {
            for bits in maps.iter_mut() {
                cleared[w] |= bits[w] & bit;
                bits[w] &= !bit;
            }
            seen[w] &= !bit;
        }
    }
}

/// Before a submission runs (the last one has): the read marks of slots
/// forgotten since are cleared now, so that the submission's reads of them
/// are taken after it ([`take_reads`]).
pub(crate) fn forget_reads_now() {
    // SAFETY: no submission runs.
    let mut maps = unsafe { read_maps() };
    if maps.is_empty() {
        return;
    }
    let mut reads = READS.lock().unwrap_or_else(|e| e.into_inner());
    let (seen, forgotten) = &mut *reads;
    clear_forgotten(&mut maps, seen, forgotten);
}

/// After a submission: the heap slots its shaders first read texture views
/// at (`victory_shader::emit`, "Read marks"), each once until
/// [`forget_read`]. Clears the slots forgotten since first.
pub(crate) fn take_reads() -> Vec<u32> {
    // SAFETY: the submission has run.
    let mut maps = unsafe { read_maps() };
    if maps.is_empty() {
        return Vec::new();
    }
    let mut reads = READS.lock().unwrap_or_else(|e| e.into_inner());
    let (seen, forgotten) = &mut *reads;
    clear_forgotten(&mut maps, seen, forgotten);
    let mut new = Vec::new();
    for (w, s) in seen.iter_mut().enumerate() {
        let mut fresh = maps.iter().fold(0, |x, bits| x | bits[w]) & !*s;
        *s |= fresh;
        while fresh != 0 {
            new.push(w as u32 * 32 + fresh.trailing_zeros());
            fresh &= fresh - 1;
        }
    }
    new
}

/// The heap slots shaders read texture views at since the last call (both
/// bitmaps, and marks forgotten meanwhile), whose marks are cleared: what
/// the title used (eviction stamps,
/// `crate::resource::evict`). Between submissions, after [`take_reads`]
/// has seen every mark: its own record of slots seen keeps a slot read
/// again from being reported as new.
pub(crate) fn reads_since() -> Vec<u32> {
    let words = (victory_shader::emit::HEAP_VIEWS / 32) as usize;
    let first = (victory_shader::emit::VIEW_READS / 32) as usize;
    let mut slots = Vec::new();
    let cleared = std::mem::take(&mut *CLEARED.lock().unwrap_or_else(|e| e.into_inner()));
    for b in [false, true].into_iter().filter_map(bitmap) {
        // SAFETY: the mapped bitmap's read marks; no submission runs.
        let bits = unsafe { std::slice::from_raw_parts_mut((b.data as *mut u32).add(first), words) };
        for (w, word) in bits.iter_mut().enumerate() {
            let mut x = std::mem::take(word) | cleared.get(w).copied().unwrap_or(0);
            while x != 0 {
                slots.push(w as u32 * 32 + x.trailing_zeros());
                x &= x - 1;
            }
        }
    }
    slots.sort_unstable();
    slots.dedup();
    slots
}

/// Heap slot `slot` now holds a texture view left null: its next read is
/// marked again.
pub(crate) fn forget_read(slot: u32) {
    READS.lock().unwrap_or_else(|e| e.into_inner()).1.push(slot);
}

/// Read marks of view heap pages: one per page of heap indices.
const PAGE_MARKS: u32 = victory_shader::emit::HEAP_VIEWS / victory_shader::emit::VIEW_PAGE_SLOTS;

/// After a submission: the view heap pages its shaders read through views
/// (`victory_shader::emit`, "Read marks"; paged heaps only), from both
/// bitmaps, whose marks are cleared.
pub(crate) fn take_page_reads() -> Vec<u32> {
    let words = (PAGE_MARKS / 32) as usize;
    let first = (victory_shader::emit::VIEW_PAGE_READS / 32) as usize;
    let mut pages = Vec::new();
    let mut read = vec![0u32; words];
    for b in [false, true].into_iter().filter_map(bitmap) {
        // SAFETY: the mapped bitmap's page read marks; the GPU is done with
        // them (one submission runs at a time).
        let bits =
            unsafe { std::slice::from_raw_parts_mut((b.data as *mut u32).add(first), words) };
        for (r, word) in read.iter_mut().zip(bits) {
            *r |= std::mem::take(word);
        }
    }
    for (w, mut x) in read.into_iter().enumerate() {
        while x != 0 {
            pages.push(w as u32 * 32 + x.trailing_zeros());
            x &= x - 1;
        }
    }
    pages
}

/// The final bit is the shader's overflow sink. Its 31 neighbors can belong
/// to live shadows, including when the allocator is down to its last word.
fn drain_marks(bits: &mut [u32], end: usize, into: &mut Vec<u32>) {
    if let Some(last) = bits.last_mut() {
        *last &= !(1 << 31);
    }
    for (w, word) in bits.iter_mut().take(end.div_ceil(32)).enumerate() {
        if *word == 0 {
            continue;
        }
        let mut x = std::mem::take(word);
        while x != 0 {
            into.push((w * 32) as u32 + x.trailing_zeros());
            x &= x - 1;
        }
    }
}

/// Publish the ordinary marked pages after a submission. On a page with
/// withheld declarations, only the remaining valid ranges can go back.
/// A withheld shader's bitmap mark covers its entire shadow page. Retake
/// held pages after publishing ordinary marks of other shadows, since their
/// title mappings can alias. Both write bitmaps are drained before the
/// declared copy-backs disarm the shadows.
pub(crate) fn copy_back_marks(declared: &[(Arc<Shadow>, usize, usize, bool)], memory_withheld: bool) {
    let (mut marked, mut held) = (Vec::new(), Vec::new());
    for memory in [false, true] {
        let Some(b) = bitmap(memory) else {
            continue;
        };
        let words = (victory_shader::emit::VIEW_WRITE_BITS / 32) as usize;
        // SAFETY: the mapped bitmap; the GPU is done with it (one submission
        // runs at a time).
        let bits = unsafe { std::slice::from_raw_parts_mut(b.data as *mut u32, words) };
        let into = if memory && memory_withheld {
            &mut held
        } else {
            &mut marked
        };
        drain_marks(bits, marks().end as usize, into);
    }
    #[cfg(feature = "diagnostics")]
    let _timed = crate::watch::Timed(std::time::Instant::now(), crate::copystat::C::MarkedUs);
    if marked.is_empty() && held.is_empty() {
        return;
    }
    marked.sort_unstable();
    marked.dedup();
    if !held.is_empty() {
        // A bit belongs to one shadow page. Both bitmap classes on that bit
        // share the same GPU bytes, so an ordinary mark cannot publish them.
        // Distinct shadows of the same physical page have distinct bits.
        let mut next_held = 0;
        marked.retain(|bit| {
            while next_held < held.len() && held[next_held] < *bit {
                next_held += 1;
            }
            held.get(next_held) != Some(bit)
        });
    }
    let (owners, held_owners) = {
        let m = marks();
        (m.owners_of(&marked), m.owners_of(&held))
    };
    let page_at = |copy: &Shadow, bit: u32| {
        ((copy.start as usize) & !(PAGE - 1)) + (bit - copy.marks.unwrap()) as usize * PAGE
    };
    // Preserve the clipped declared ranges on pages shared by a valid and a
    // missed list (Marvisak/VIctory#1986). Plain pages keep the batched path.
    let withheld: Vec<_> = declared.iter().filter(|d| !d.3).collect();
    for (copy, bits) in owners {
        struct MarkedRun {
            page: usize,
            pages: usize,
            /// Empty for a run of unrestricted, complete marked pages.
            valid: Vec<(usize, usize)>,
        }
        let mut chunks = Vec::<MarkedRun>::new();
        let mut count = 0;
        for &bit in bits {
            let page = page_at(&copy, bit);
            let end = page + PAGE;
            let on_page = |c: &Arc<Shadow>, a: usize, n: usize| Arc::ptr_eq(c, &copy) && a < end && page < a + n;
            if withheld.iter().any(|(c, a, n, _)| on_page(c, *a, *n)) {
                let spans = |valid| declared.iter()
                    .filter(|(c, a, n, back)| *back == valid && on_page(c, *a, *n))
                    .map(|(_, a, n, _)| ((*a).max(page), (a + n).min(end)))
                    .collect::<Vec<_>>();
                let allowed = subtract(spans(true), &spans(false));
                if !allowed.is_empty() {
                    chunks.push(MarkedRun { page, pages: 1, valid: allowed });
                    count += 1;
                }
                continue;
            }
            count += 1;
            match chunks.last_mut() {
                Some(run) if run.valid.is_empty() && run.pages < 8 && page == run.page + run.pages * PAGE => {
                    run.pages += 1;
                }
                _ => chunks.push(MarkedRun { page, pages: 1, valid: Vec::new() }),
            }
        }
        let Some(first) = chunks.first() else { continue };
        let last = chunks.last().unwrap();
        let end = last.page + last.pages * PAGE;
        let copy_chunk = |i: usize| {
            let run = &chunks[i];
            let written = if run.valid.is_empty() {
                copy.copy_back_marked(run.page, run.pages)
            } else {
                copy.copy_back_marked_ranges(run.page, &run.valid)
            };
            #[cfg(feature = "diagnostics")]
            {
                crate::copystat::add(crate::copystat::C::MarkedPages, run.pages as u64);
                crate::copystat::add(crate::copystat::C::MarkedBytes, written as u64);
            }
            #[cfg(not(feature = "diagnostics"))]
            let _ = written;
        };
        if count >= 128 && private_write_watch(first.page, end - first.page) {
            parallel_items(chunks.len(), copy_chunk);
        } else {
            (0..chunks.len()).for_each(copy_chunk);
        }
    }
    // Invalidating a held page marks all copies of its physical memory dirty.
    // First finish any ordinary marks of other copies that still need its old
    // bytes, including when both views point at one physical page.
    for (copy, bits) in held_owners {
        for &bit in bits {
            crate::memory::drop_withheld(page_at(&copy, bit), PAGE);
        }
    }
}

/// `ranges` (half-open) less `minus`: sorted, disjoint.
fn subtract(mut ranges: Vec<(usize, usize)>, minus: &[(usize, usize)]) -> Vec<(usize, usize)> {
    ranges.sort_unstable();
    let mut joined: Vec<(usize, usize)> = Vec::new();
    for (a, e) in ranges {
        match joined.last_mut() {
            Some(last) if a <= last.1 => last.1 = last.1.max(e),
            _ => joined.push((a, e)),
        }
    }
    for &(ma, me) in minus {
        joined = joined
            .into_iter()
            .flat_map(|(a, e)| [(a, e.min(ma)), (a.max(me), e)])
            .filter(|(a, e)| a < e)
            .collect();
    }
    joined
}

struct Entry {
    end: u64,
    shadow: Option<Arc<Shadow>>,
}

/// Title buffers by (start address, resource), with their copy once made.
static BUFFERS: Mutex<BTreeMap<(u64, usize), Entry>> = Mutex::new(BTreeMap::new());

fn buffers() -> std::sync::MutexGuard<'static, BTreeMap<(u64, usize), Entry>> {
    BUFFERS.lock().unwrap_or_else(|e| e.into_inner())
}

/// Counts copies made and title buffers released: [`all`] may differ when
/// it changed.
static GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The generation of [`all`]: read it before calling that.
pub(crate) fn generation() -> u64 {
    GENERATION.load(std::sync::atomic::Ordering::Acquire)
}

fn changed() {
    GENERATION.fetch_add(1, std::sync::atomic::Ordering::Release);
}

/// A title buffer exists at `start` (`len` bytes).
pub(crate) fn register(resource: usize, start: u64, len: u64) {
    buffers().insert(
        (start, resource),
        Entry {
            end: start + len,
            shadow: None,
        },
    );
}

/// The title released it; lists that still hold its copy keep it alive.
pub(crate) fn unregister(resource: usize, start: u64) {
    if buffers().remove(&(start, resource)).is_some_and(|e| e.shadow.is_some()) {
        changed();
    }
}

/// The copy of the innermost title buffer holding `[address, address +
/// len)`, made on first use.
pub(crate) fn find(address: u64, len: u64) -> Option<Arc<Shadow>> {
    let end = address.checked_add(len.max(1))?;
    let mut map = buffers();
    let key = map
        .range(..=(address, usize::MAX))
        .rev()
        .take(64)
        .find(|(_, e)| e.end >= end)
        .map(|(k, _)| *k);
    let Some(key) = key else {
        drop(map);
        return loose(address, end);
    };
    let entry = map.get_mut(&key)?;
    if entry.shadow.is_none() {
        let len = entry.end - key.0;
        // Rounded up so that views ending in the buffer's last 256 bytes fit.
        // Shaders write some buffers (unordered access), so not upload memory.
        let (buffer, data) = host()?.shared_buffer(len.next_multiple_of(1 << 16))?;
        crate::host::name(&buffer, || format!("title buffer {:#x}+{len:#x}", key.0));
        // SAFETY: a live buffer.
        let gpu = unsafe { buffer.GetGPUVirtualAddress() };
        if len >= 16 << 20 {
            tracing::info!(
                start = format_args!("{:#x}", key.0),
                len,
                "large buffer copied to the host"
            );
        } else {
            tracing::debug!(
                start = format_args!("{:#x}", key.0),
                len,
                "buffer copied to the host"
            );
        }
        entry.shadow = Some(Shadow::new(buffer, data, gpu, key.0, len));
        changed();
    }
    entry.shadow.clone()
}

/// Titles also use memory outside any buffer object as GPU memory (Grand
/// Theft Auto VI's interface vertices, from its own allocator). Such memory
/// gets copies of the aligned windows it touches.
/// shortcut: copies of windows are never freed; key them to the title's
/// allocations if memory use grows.
const WINDOW: u64 = 4 << 20;

/// Copies of title memory outside buffers, by (start, length).
static LOOSE: Mutex<BTreeMap<(u64, u64), Arc<Shadow>>> = Mutex::new(BTreeMap::new());

fn loose(address: u64, end: u64) -> Option<Arc<Shadow>> {
    let start = address & !(WINDOW - 1);
    let len = end.next_multiple_of(WINDOW) - start;
    let mut map = LOOSE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(shadow) = map.get(&(start, len)) {
        return Some(shadow.clone());
    }
    let (buffer, data) = host()?.shared_buffer(len)?;
    crate::host::name(&buffer, || format!("title memory {start:#x}+{len:#x}"));
    // SAFETY: a live buffer.
    let gpu = unsafe { buffer.GetGPUVirtualAddress() };
    tracing::debug!(
        start = format_args!("{start:#x}"),
        len,
        "loose memory copied to the host"
    );
    let shadow = Shadow::new(buffer, data, gpu, start, len);
    map.insert((start, len), shadow.clone());
    changed();
    Some(shadow)
}

/// The copy a buffer view of `len` bytes at `address`, whose elements are
/// `element` bytes, reads: the innermost title buffer's ([`find`]), unless
/// the view does not start on an element of that copy. A host view counts
/// its first element from its buffer's start; a console view starts at any
/// address, and the title places structured views at ring positions inside
/// its big GPU heap (the meshlet culling's 456-byte constants, the colour
/// bakes' parameter blocks: refused, their slots read zeros, every meshlet
/// culled). Such a view gets a copy of its own bytes, starting at its
/// address (`true` with it), which lives while a heap slot made from the
/// view or a list holds it.
pub(crate) fn for_view(address: u64, len: u64, element: u64) -> Option<(Arc<Shadow>, bool)> {
    let shadow = find(address, len)?;
    if element == 0 || shadow.offset(address).is_multiple_of(element) {
        return Some((shadow, false));
    }
    #[cfg(feature = "diagnostics")]
    VIEWS[0].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    exact(address, len, element).map(|s| (s, true))
}

/// Asks for, and copies made of, views off their copy's elements, the
/// copies' bytes and the largest (diagnostics).
#[cfg(feature = "diagnostics")]
static VIEWS: [std::sync::atomic::AtomicU64; 4] = [const { std::sync::atomic::AtomicU64::new(0) }; 4];

/// Logs [`VIEWS`] per frame over `frames`.
#[cfg(feature = "diagnostics")]
pub(crate) fn log_views(frames: f64) {
    use std::sync::atomic::Ordering::Relaxed;
    let [asks, made, bytes, largest] = [0, 1, 2, 3].map(|i| VIEWS[i].swap(0, Relaxed));
    let live = EXACT
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .1
        .values()
        .filter(|w| w.strong_count() > 0)
        .count();
    let (chunks, held) = {
        let a = arena();
        // SAFETY: live buffers.
        let size = |c: &(ID3D12Resource, usize, u64, BTreeMap<u64, u64>)| unsafe { c.0.GetDesc() }.Width;
        let held: u64 = a.chunks.iter().map(|c| size(c) - c.3.values().sum::<u64>()).sum();
        (a.chunks.len(), held)
    };
    if asks > 0 {
        tracing::info!(
            "buffer views off their copy's elements: {:.0} asked for/frame, {:.0} copies made/frame ({:.1} KB), largest {largest} bytes, {live} live in {chunks} shared buffers ({:.1} MB held)",
            asks as f64 / frames,
            made as f64 / frames,
            bytes as f64 / frames / 1e3,
            held as f64 / 1e6
        );
    }
}

/// Copies of single views' bytes ([`for_view`]), by (start, length), held
/// by the heap slots made from them and the lists that use them: a copy is
/// found again for as long as a slot shows it, so draws refresh the copy
/// the GPU reads (a capped cache dropped copies slots still showed, and a
/// new copy of the same bytes took the refreshes). With the size the map
/// is next pruned of dead entries at.
#[allow(clippy::type_complexity)]
static EXACT: Mutex<(usize, BTreeMap<(u64, u64, u64), std::sync::Weak<Shadow>>)> = Mutex::new((0, BTreeMap::new()));

fn exact(address: u64, len: u64, element: u64) -> Option<Arc<Shadow>> {
    let mut guard = EXACT.lock().unwrap_or_else(|e| e.into_inner());
    let (prune_at, map) = &mut *guard;
    if let Some(shadow) = map.get(&(address, len, element)).and_then(std::sync::Weak::upgrade) {
        return Some(shadow);
    }
    // Ring positions change every frame: entries die by the hundred.
    if map.len() >= *prune_at {
        map.retain(|_, w| w.strong_count() > 0);
        *prune_at = (2 * map.len()).max(1024);
    }
    // A run of a shared buffer, its start a whole number of elements in.
    let (buffer, data, gpu, slice) = arena().take(len, element)?;
    let shadow = Shadow::new_at(buffer, data, gpu, address, len, Some(slice));
    map.insert((address, len, element), Arc::downgrade(&shadow));
    #[cfg(feature = "diagnostics")]
    {
        use std::sync::atomic::Ordering::Relaxed;
        VIEWS[1].fetch_add(1, Relaxed);
        VIEWS[2].fetch_add(len, Relaxed);
        VIEWS[3].fetch_max(len, Relaxed);
    }
    Some(shadow)
}

/// A run of an [`Arena`] buffer: the buffer, where the run starts, its
/// bytes.
#[derive(Clone, Copy)]
struct Slice {
    chunk: usize,
    offset: u64,
    len: u64,
}

/// Shared buffers for the copies of single views ([`exact`]): they are
/// small (~2 KB on average) and come and go by the hundred a frame, and a
/// buffer of their own each cost a kernel call to make and one to free on
/// the title's threads and the submitting one (300-820 a frame), and 64 KiB
/// of memory each (~5,000 live).
#[derive(Default)]
struct Arena {
    /// Each buffer, its mapping and host address, and its free runs
    /// (offset → bytes).
    chunks: Vec<(ID3D12Resource, usize, u64, BTreeMap<u64, u64>)>,
}

/// Bytes of each arena buffer (a larger copy gets one of its size).
const CHUNK: u64 = 16 << 20;

static ARENA: Mutex<Arena> = Mutex::new(Arena { chunks: Vec::new() });

fn arena() -> std::sync::MutexGuard<'static, Arena> {
    ARENA.lock().unwrap_or_else(|e| e.into_inner())
}

impl Arena {
    /// A run of `len` bytes starting at a multiple of `element` (and of 256)
    /// in a shared buffer: the buffer, the run's mapping and host address.
    fn take(&mut self, len: u64, element: u64) -> Option<(ID3D12Resource, *mut u8, u64, Slice)> {
        let gcd = |mut a: u64, mut b: u64| {
            while b != 0 {
                (a, b) = (b, a % b);
            }
            a
        };
        let align = 256 / gcd(256, element.max(1)) * element.max(1);
        let len = len.max(1).next_multiple_of(4);
        let fits = |free: &BTreeMap<u64, u64>| {
            free.iter().find_map(|(&at, &n)| {
                let start = at.next_multiple_of(align);
                (start + len <= at + n).then_some((at, n, start))
            })
        };
        let found = self.chunks.iter().enumerate().find_map(|(c, chunk)| fits(&chunk.3).map(|f| (c, f)));
        let (chunk, (at, n, start)) = match found {
            Some(found) => found,
            None => {
                let size = CHUNK.max((len + align).next_multiple_of(1 << 16));
                let (buffer, data) = host()?.shared_buffer(size)?;
                crate::host::name(&buffer, || format!("title views {size:#x}"));
                // SAFETY: a live buffer.
                let gpu = unsafe { buffer.GetGPUVirtualAddress() };
                let free = BTreeMap::from([(0, size)]);
                self.chunks.push((buffer, data as usize, gpu, free));
                let c = self.chunks.len() - 1;
                (c, fits(&self.chunks[c].3)?)
            }
        };
        let (buffer, data, gpu, free) = &mut self.chunks[chunk];
        free.remove(&at);
        if start > at {
            free.insert(at, start - at);
        }
        if at + n > start + len {
            free.insert(start + len, at + n - start - len);
        }
        let slice = Slice { chunk, offset: start, len };
        Some((buffer.clone(), (*data + start as usize) as *mut u8, *gpu + start, slice))
    }

    /// A run goes back, joined to its free neighbours.
    fn give_back(&mut self, slice: Slice) {
        let Some((_, _, _, free)) = self.chunks.get_mut(slice.chunk) else { return };
        let (mut at, mut n) = (slice.offset, slice.len);
        if let Some((&before, &m)) = free.range(..at).next_back()
            && before + m == at
        {
            free.remove(&before);
            (at, n) = (before, n + m);
        }
        if let Some(m) = free.remove(&(at + n)) {
            n += m;
        }
        free.insert(at, n);
    }
}

/// Every host copy of title memory made so far (buffers and windows).
pub(crate) fn all() -> Vec<Arc<Shadow>> {
    let mut all: Vec<Arc<Shadow>> = buffers().values().filter_map(|e| e.shadow.clone()).collect();
    all.extend(LOOSE.lock().unwrap_or_else(|e| e.into_inner()).values().cloned());
    all
}

/// For device-removed reports: the copy of title memory whose host range
/// starts closest below host GPU address `va`, and where `va` is in it.
pub(crate) fn near(va: u64) -> String {
    let mut all: Vec<(u64, u64, u64)> = buffers()
        .values()
        .filter_map(|e| e.shadow.as_deref().map(|s| (s.gpu, s.start, s.len)))
        .collect();
    all.extend(
        LOOSE
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .map(|s| (s.gpu, s.start, s.len)),
    );
    let below = all.iter().filter(|c| c.0 <= va).max();
    let above = all.iter().filter(|c| c.0 > va).min();
    let mut text = match below {
        Some(&(gpu, start, len)) => format!(
            "; the nearest title memory copy below it: {start:#x}+{len:#x} at host {gpu:#x}, offset {:#x}",
            va - gpu
        ),
        None => "; no title memory copy below it".into(),
    };
    if let Some(&(gpu, start, len)) = above {
        text.push_str(&format!(
            "; the nearest above: {start:#x}+{len:#x} at host {gpu:#x} ({} copies in all)",
            all.len()
        ));
    }
    text
}

#[cfg(all(test, feature = "diagnostics"))]
pub(crate) fn bench_mark(first: usize, pages: usize) {
    let bits = bitmap(false).unwrap().data as *mut u32;
    assert!(first + pages < victory_shader::emit::VIEW_WRITE_BITS as usize);
    for bit in first..first + pages {
        // SAFETY: a bounded mark in the mapped bitmap, while the GPU is idle.
        unsafe { *bits.add(bit / 32) |= 1 << (bit % 32) };
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A shader's read mark of heap slot `slot` in the bitmap of shaders
    /// that use title memory (`memory`) or of the others.
    pub(crate) fn mark_read(memory: bool, slot: u32) {
        let b = bitmap(memory).expect("a host GPU");
        let bit = victory_shader::emit::VIEW_READS + slot;
        // SAFETY: a bit of the mapped bitmap; no submission runs (the caller
        // holds the submission lock).
        unsafe { *(b.data as *mut u32).add((bit / 32) as usize) |= 1 << (bit % 32) };
    }

    /// A shader's read mark of view heap page `page`, as [`mark_read`].
    pub(crate) fn mark_page_read(memory: bool, page: u32) {
        let b = bitmap(memory).expect("a host GPU");
        let bit = victory_shader::emit::VIEW_PAGE_READS + page;
        // SAFETY: as in `mark_read`.
        unsafe { *(b.data as *mut u32).add((bit / 32) as usize) |= 1 << (bit % 32) };
    }

    /// A title can release a buffer after its list completes, then recommit
    /// the same virtual address for another buffer. The shared page table
    /// can still hold the old shadow until the next table rebuild.
    #[test]
    fn a_replacement_buffer_tracks_cpu_writes_at_its_predecessors_address() {
        use windows_sys::Win32::System::Memory::{
            MEM_COMMIT, MEM_DECOMMIT, PAGE_READWRITE, VirtualAlloc, VirtualFree,
        };
        let Some(host) = host() else { return };
        let p = crate::watch::tests::pool(1);
        let make = |id| {
            register(id, p as u64, PAGE as u64);
            find(p as u64, PAGE as u64).unwrap()
        };
        let submit = |copy: &Arc<Shadow>, use_table| {
            let list = host.new_list().unwrap();
            copy.refresh(&list, p as u64, PAGE as u64);
            if use_table {
                crate::memory::bind(&list, crate::memory::OURS).unwrap();
            }
            list.close();
            host.execute(&[&list]);
        };
        let old = make(1);
        submit(&old, true);
        let replacement = crate::host::tests::without_submissions(|| {
            unregister(1, p as u64);
            drop(old);
            // SAFETY: the old list has completed and the reservation remains
            // ours. Windows gives the recommitted page fresh contents and protection.
            unsafe {
                assert_ne!(VirtualFree(p as _, PAGE, MEM_DECOMMIT), 0);
                assert_eq!(VirtualAlloc(p as _, PAGE, MEM_COMMIT, PAGE_READWRITE) as usize, p);
            }
            make(2)
        });
        submit(&replacement, false);
        // SAFETY: a byte of the replacement allocation, after the first submission.
        unsafe { (p as *mut u8).write_volatile(0x7b) };
        submit(&replacement, false);
        assert_eq!(replacement.read_host(p as u64, 1), [0x7b]);
        unregister(2, p as u64);
        drop(replacement);
        crate::watch::tests::free(p);
    }

    /// The title writes a constant under a range an earlier list of the
    /// submission declared as written by shaders: the refresh before the
    /// submission runs still brings it to the copy, a shader's write still
    /// goes back, and a title write made after the refresh is not undone.
    #[test]
    fn title_writes_under_a_busy_range_are_copied() {
        let p = crate::watch::tests::pool(2);
        let len = 2 * PAGE;
        register(p, p as u64, len as u64);
        let Some(copy) = find(p as u64, len as u64) else {
            unregister(p, p as u64);
            return; // no host GPU here
        };
        let title = |at: usize| (p + at) as *mut u8;
        let host = |at: usize| copy.read_host((p + at) as u64 & !3, 1)[0].to_le_bytes()[at % 4];
        copy.update(p, len);
        // An earlier list declares all of it as written by shaders.
        copy.writing(p, len, false);
        // SAFETY: (all title reads and writes below) bytes of our pool.
        unsafe { title(PAGE + 20).write_volatile(0x55) };
        assert!(copy.update(p + PAGE + 16, 16) > 0, "the title's write is copied");
        assert_eq!(host(PAGE + 20), 0x55, "the title's constant reaches the copy");
        // The shaders run: one byte written. The title writes meanwhile.
        copy.write_host((p + 10) as u64, &[0xaa]);
        // SAFETY: a byte of our pool.
        unsafe { title(PAGE + 20).write_volatile(0x66) };
        copy.copy_back(p, len);
        // SAFETY: bytes of our pool.
        unsafe {
            assert_eq!(title(10).read_volatile(), 0xaa, "the shader's write goes back");
            assert_eq!(title(PAGE + 20).read_volatile(), 0x66, "the title's later write stays");
        }
        unregister(p, p as u64);
    }

    /// A title buffer over a pool and, after it, a plain allocation (memory
    /// that is not the runtime's): the pool is tracked and guarded; the
    /// plain allocation's bytes are copied at each use and never guarded,
    /// so a value the title keeps there reaches the copy, and again after
    /// the title changes it.
    #[test]
    fn bytes_off_the_runtimes_memory_are_copied_at_each_use() {
        use crate::watch::tests::{MEM_WRITE_WATCH, free};
        use windows_sys::Win32::System::Memory::{
            MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, MEMORY_BASIC_INFORMATION, PAGE_READONLY, PAGE_READWRITE,
            VirtualAlloc, VirtualFree, VirtualQuery,
        };
        // A pool page and a plain one right after it, in two pages found free
        // (other tests allocate meanwhile: tried again if taken).
        let (pool, plain) = (0..8)
            .find_map(|_| {
                // SAFETY: a fresh reservation, released at once; then
                // allocations where it was, freed below.
                unsafe {
                    let p = VirtualAlloc(std::ptr::null(), 2 * PAGE, MEM_RESERVE, PAGE_READWRITE) as usize;
                    VirtualFree(p as *mut _, 0, MEM_RELEASE);
                    let at = |q: usize, kind: u32| {
                        VirtualAlloc(q as *const _, PAGE, MEM_RESERVE | MEM_COMMIT | kind, PAGE_READWRITE) as usize
                    };
                    let (pool, plain) = (at(p, MEM_WRITE_WATCH), at(p + PAGE, 0));
                    if p != 0 && pool == p && plain == p + PAGE {
                        return Some((pool, plain));
                    }
                    [pool, plain].into_iter().filter(|&q| q != 0).for_each(free);
                    None
                }
            })
            .expect("two free pages");
        let len = 2 * PAGE;
        register(pool, pool as u64, len as u64);
        let Some(copy) = find(pool as u64, len as u64) else {
            unregister(pool, pool as u64);
            [pool, plain].into_iter().for_each(free);
            return; // no host GPU here
        };
        let word = (plain + 64) as *mut u32;
        let protection = |p: usize| {
            // SAFETY: plain data filled in by the query.
            let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
            // SAFETY: a valid out pointer of the right size.
            unsafe { VirtualQuery(p as *const _, &mut info, size_of_val(&info)) };
            info.Protect
        };
        // SAFETY: a word of our plain allocation.
        unsafe { word.write_volatile(0x1234) };
        copy.update(pool, len);
        assert_eq!(copy.read_host(word as u64, 1), [0x1234]);
        assert!(!copy.clean(), "never clean: part of it is not tracked");
        // SAFETY: as above.
        unsafe { word.write_volatile(0x5678) };
        copy.update(pool, len);
        assert_eq!(copy.read_host(word as u64, 1), [0x5678], "the title's change reaches the copy");
        assert_eq!(protection(pool), PAGE_READONLY, "the pool is guarded");
        assert_eq!(protection(plain), PAGE_READWRITE, "the plain allocation is not");
        unregister(pool, pool as u64);
        // The pages stay allocated (as the other tests' pools): a copy some
        // page table still holds must not name memory reused meanwhile.
    }

    /// Runs of the shared buffers start a whole number of elements (and of
    /// 256 bytes) in, do not overlap, and join again when given back.
    #[test]
    fn arena_runs_are_aligned_to_their_elements_and_join_again() {
        if host().is_none() {
            return; // no host GPU here
        }
        let mut arena = Arena::default();
        let (_, _, _, a) = arena.take(100, 456).unwrap();
        let (_, _, _, b) = arena.take(5000, 16).unwrap();
        let (_, _, _, c) = arena.take(4, 4).unwrap();
        assert!(a.offset % 456 == 0 && a.offset % 256 == 0 && b.offset % 256 == 0);
        let mut runs = [a, b, c].map(|s| (s.offset, s.offset + s.len));
        runs.sort_unstable();
        assert!(runs.windows(2).all(|w| w[0].1 <= w[1].0), "{runs:?}");
        for s in [b, a, c] {
            arena.give_back(s);
        }
        assert_eq!(arena.chunks.len(), 1);
        assert_eq!(arena.chunks[0].3.len(), 1, "one free run again");
        // Larger than a buffer: one of its own.
        let (_, _, _, d) = arena.take(CHUNK + 1, 4).unwrap();
        assert_eq!((arena.chunks.len(), d.chunk), (2, 1));
    }

    /// Ranges shaders write far apart are kept apart (not the copy between
    /// them); what the GPU wrote goes back once, whichever overlapping
    /// ranges declared it, and nothing else does.
    #[test]
    fn snapshots_cover_the_declared_ranges_and_writes_go_back_once() {
        let p = crate::watch::tests::pool(4);
        let len = 4 * PAGE;
        register(p, p as u64, len as u64);
        let Some(copy) = find(p as u64, len as u64) else {
            unregister(p, p as u64);
            return; // no host GPU here
        };
        copy.update(p, len);
        let bytes = |s: &Shadow| s.before.lock().unwrap().0.iter().map(|(_, b)| b.len()).sum::<usize>();
        copy.writing(p + 16, 64, false);
        copy.writing(p + 3 * PAGE, 100, false);
        assert_eq!(bytes(&copy), 164, "two segments");
        copy.writing(p + PAGE, 2 * PAGE, false);
        copy.writing(p + PAGE, PAGE, false);
        copy.writing(p + 80, 8, false);
        assert_eq!(bytes(&copy), 72 + 2 * PAGE + 100, "merged where they overlap or touch");
        // The GPU writes inside two ranges.
        copy.write_host((p + 20) as u64, &[1, 2, 3]);
        copy.write_host((p + PAGE + 5) as u64, &[9]);
        assert_eq!(copy.copy_back(p + 16, 64), 64, "the range's part of the piece");
        assert_eq!(copy.copy_back(p + PAGE, 2 * PAGE), PAGE, "the piece written");
        assert_eq!(copy.copy_back(p + PAGE, PAGE), 0, "compared once");
        assert_eq!(copy.copy_back(p + 3 * PAGE, 100), 0);
        assert_eq!(copy.copy_back(p + 80, 8), 0);
        // SAFETY: our pool.
        let title = |at: usize| unsafe { (at as *const u8).read_volatile() };
        assert_eq!([title(p + 20), title(p + 21), title(p + 22), title(p + PAGE + 5)], [1, 2, 3, 9]);
        assert_eq!(bytes(&copy), 0, "the snapshot goes with the last range");
        unregister(p, p as u64);
    }

    #[test]
    fn last_word_keeps_all_allocatable_marks() {
        let mut words = [1 << 31, 0xc0000001];
        let mut pages = Vec::new();
        drain_marks(&mut words, 63, &mut pages);
        assert_eq!(pages, [31, 32, 62]);
        assert_eq!(words, [0, 0]);
    }

    #[test]
    fn physical_aliases_merge_cross_page_gpu_and_cpu_stores() {
        let Some(h) = host() else { return };
        let (a, b) = crate::watch::tests::twin_views(2);
        let len = 2 * PAGE;
        assert!(!private_write_watch(a, len));
        let make = |at| {
            let (resource, data) = h.shared_buffer(len as u64).unwrap();
            Shadow::new(resource, data, 0, at as u64, len as u64)
        };
        let (one, two) = (make(a), make(b));
        for (shadow, at) in [(&one, a), (&two, b)] {
            shadow.update(at, len);
            shadow.writing(at, len, true);
        }
        one.write_host((a + PAGE - 2) as u64, &[1, 2, 3, 4]);
        two.write_host((b + PAGE + 1) as u64, &[5, 6]);
        // A title thread writes through the second virtual mapping while the
        // submitted shadows contain different writes to the same physical pages.
        // SAFETY: both addresses are bytes of the second live mapped view.
        std::thread::spawn(move || unsafe {
            ((b + PAGE - 3) as *mut u8).write_volatile(7);
            ((b + PAGE + 3) as *mut u8).write_volatile(8);
        })
        .join()
        .unwrap();
        // Deterministically land another CPU write between the page read and
        // the GPU stores, inside the gap between two shader writes.
        one.write_host((a + PAGE + 5) as u64, &[11]);
        AFTER_MARK_READ.with(|slot| {
            slot.set(Some(Box::new(move || {
                // SAFETY: the second view stays mapped until this thread joins.
                std::thread::spawn(move || unsafe {
                    ((b + PAGE + 4) as *mut u8).write_volatile(9)
                })
                .join()
                .unwrap();
            })))
        });
        one.copy_back_marked(a, 2);
        two.copy_back_marked(b, 2);
        let mut bytes = [0; 9];
        assert!(crate::host::read_into(
            (a + PAGE - 3) as *const u8,
            &mut bytes
        ));
        assert_eq!(bytes, [7, 1, 2, 3, 5, 6, 8, 9, 11]);
        // A missed program also writes through the second mapping. Its
        // invalidation must happen after the ordinary copy's mark goes back.
        one.copy_back(a, len);
        two.copy_back(b, len);
        let list = h.new_list().unwrap();
        for (shadow, at, memory, offset) in [(&one, a, false, 32), (&two, b, true, 33)] {
            let _writer = list.writer_uses_memory(memory);
            shadow.refresh(&list, at as u64, PAGE as u64);
            shadow.gpu_writes(&list, at as u64, PAGE as u64, true);
            let shadow = shadow.clone();
            list.defer_host(move || {
                shadow.write_host((at + offset) as u64, &[0xbb]);
                let bit = shadow.marks.unwrap() as usize;
                let bits = bitmap(memory).unwrap().data as *mut u32;
                // SAFETY: the submission lock excludes another bitmap writer.
                unsafe { *bits.add(bit / 32) |= 1 << (bit % 32) };
            });
        }
        list.defer_missed(|| true);
        list.close();
        h.execute(&[&list]);
        let mut results = [0; 2];
        assert!(crate::host::read_into((a + 32) as *const u8, &mut results));
        assert_eq!(results, [0xbb, 0]);
        drop(list);
        drop((one, two));
        for at in [a, b] {
            crate::watch::title_memory_remapped(at, len);
            // SAFETY: the shadows have released these mapped section views.
            unsafe {
                windows_sys::Win32::System::Memory::UnmapViewOfFile(
                    windows_sys::Win32::System::Memory::MEMORY_MAPPED_VIEW_ADDRESS {
                        Value: at as *mut _,
                    },
                )
            };
        }
    }

    #[test]
    fn one_cpu_store_crosses_two_gpu_written_pages() {
        let Some(h) = host() else { return };
        let p = crate::watch::tests::pool(2);
        let len = 2 * PAGE;
        let (resource, data) = h.shared_buffer(len as u64).unwrap();
        let copy = Shadow::new(resource, data, 0, p as u64, len as u64);
        copy.update(p, len);
        copy.writing(p, len, true);
        let edge = p + PAGE;
        copy.write_host((edge - 3) as u64, &[0xa1]);
        copy.write_host((edge + 2) as u64, &[0xa2]);
        std::thread::spawn(move || {
            // SAFETY: all four bytes are inside the allocation. On x64,
            // issue a single store that faults on both guarded pages.
            unsafe {
                #[cfg(target_arch = "x86_64")]
                core::arch::asm!(
                    "mov dword ptr [{at}], {value:e}",
                    at = in(reg) edge - 2,
                    value = in(reg) 0xbebafecau32,
                    options(nostack, preserves_flags),
                );
                #[cfg(not(target_arch = "x86_64"))]
                ((edge - 2) as *mut u32).write_unaligned(0xbebafeca);
            }
        })
        .join()
        .unwrap();
        copy.copy_back_marked(p, 2);
        let mut bytes = [0; 6];
        assert!(crate::host::read_into((edge - 3) as *const u8, &mut bytes));
        assert_eq!(bytes, [0xa1, 0xca, 0xfe, 0xba, 0xbe, 0xa2]);
        drop(copy);
        crate::watch::tests::free(p);
    }

    #[test]
    fn overlapping_snapshot_does_not_replay_cpu_bytes_from_a_marked_page() {
        let Some(h) = host() else { return };
        let p = crate::watch::tests::pool(1);
        let (resource, data) = h.shared_buffer(PAGE as u64).unwrap();
        let copy = Shadow::new(resource, data, 0, p as u64, PAGE as u64);
        copy.update(p, PAGE);
        copy.writing(p + 2, 8, false);
        copy.writing(p, PAGE, true);
        copy.write_host((p + 2) as u64, &[0xa1]);
        // SAFETY: this byte is inside the live title allocation.
        unsafe { ((p + 3) as *mut u8).write_volatile(0xc1) };
        copy.copy_back_marked(p, 1);
        // The marked pass reconciled 0xc1 into the copy. The snapshot must
        // not misidentify it as a GPU write after a second CPU store.
        // SAFETY: the same live byte, guarded again after the marked pass.
        unsafe { ((p + 3) as *mut u8).write_volatile(0xc2) };
        copy.copy_back(p + 2, 8);
        let mut bytes = [0; 2];
        assert!(crate::host::read_into((p + 2) as *const u8, &mut bytes));
        assert_eq!(bytes, [0xa1, 0xc2]);
        drop(copy);
        crate::watch::tests::free(p);
    }

    #[test]
    fn marked_read_failure_skips_both_sides_of_the_block_boundary() {
        let h = host().expect("D3D12 for marked write-back");
        let p = crate::watch::tests::pool(1);
        let (resource, data) = h.shared_buffer(PAGE as u64).unwrap();
        let copy = Shadow::new(resource, data, 0, p as u64, PAGE as u64);
        copy.update(p, PAGE);
        copy.writing(p, PAGE, true);
        let boundary = p + PAGE / 2;
        copy.write_host((boundary - 1) as u64, &[0xa1, 0xa2]);
        let mut calls = Vec::new();
        let written = copy.copy_back_marked_with(p, 1, |address, out| {
            calls.push((address, out.len()));
            address < boundary && crate::host::read_into(address as *const u8, out)
        });
        assert_eq!(calls, [(p, PAGE / 2), (boundary, PAGE / 2)]);
        assert_eq!(written, 0);
        let mut bytes = [0; 4];
        assert!(crate::host::read_into((boundary - 2) as *const u8, &mut bytes));
        assert_eq!(bytes, [0; 4]);
        assert_eq!(copy.copy_back_marked(p, 1), PAGE);
        assert!(crate::host::read_into((boundary - 2) as *const u8, &mut bytes));
        assert_eq!(bytes, [0, 0xa1, 0xa2, 0]);
        drop(copy);
        crate::watch::tests::free(p);
    }

    #[test]
    fn title_write_mid_scan_uses_the_pre_write_snapshot() {
        let Some(h) = host() else { return };
        let p = crate::watch::tests::pool(1);
        let (resource, data) = h.shared_buffer(PAGE as u64).unwrap();
        let copy = Shadow::new(resource, data, 0, p as u64, PAGE as u64);
        copy.update(p, PAGE);
        copy.writing(p, PAGE, true);
        copy.write_host((p + 9) as u64, &[0xa1]);
        copy.write_host((p + PAGE - 3) as u64, &[0xa2]);
        let mut raced = false;
        let written = copy.copy_back_marked_with(p, 1, |address, out| {
            let read = crate::host::read_into(address as *const u8, out);
            if address == p {
                assert!(out.len() < PAGE);
                // SAFETY: both bytes are in the live allocation. One was
                // already read, while the other will be read in a later block.
                std::thread::spawn(move || unsafe {
                    ((p + 10) as *mut u8).write_volatile(0xc1);
                    ((p + PAGE - 2) as *mut u8).write_volatile(0xc2);
                })
                .join()
                .unwrap();
                raced = true;
            }
            read
        });
        assert!(raced);
        assert_eq!(written, PAGE);
        for (a, expected) in [(p + 9, [0xa1, 0xc1]), (p + PAGE - 3, [0xa2, 0xc2])] {
            let mut bytes = [0; 2];
            assert!(crate::host::read_into(a as *const u8, &mut bytes));
            assert_eq!(bytes, expected);
        }
        assert_eq!(copy.update(p, PAGE), 0);
        assert_eq!(copy.read_host((p + 8) as u64, 1)[0], 0x00c1a100);
        assert_eq!(copy.read_host((p + PAGE - 4) as u64, 1)[0], 0x00c2a200);
        drop(copy);
        crate::watch::tests::free(p);
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn snapshot_stores_retry_a_guard_installed_after_reading_the_title() {
        use windows_sys::Win32::System::Memory::{PAGE_GUARD, PAGE_READWRITE, VirtualProtect};

        let at = crate::watch::tests::pool(1);
        for gpu in [[0x41; 16], std::array::from_fn(|i| if i == 7 { 0x62 } else { 0x41 })] {
            let mut before = [0; 16];
            assert!(crate::host::read_into(at as *const u8, &mut before));
            let title = before;
            let mut previous = 0;
            // SAFETY: the fresh allocation contains this whole 4 KiB page.
            assert_ne!(unsafe { VirtualProtect(at as *const _, 4096, PAGE_READWRITE | PAGE_GUARD, &mut previous) }, 0);
            assert!(store_snapshot_changes(at, &gpu, &before, &title, &mut Vec::new()));
            let mut actual = [0; 16];
            assert!(crate::host::read_into(at as *const u8, &mut actual));
            assert_eq!(actual, gpu);
        }
        crate::watch::tests::free(at);
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn failed_store_retries_only_bytes_after_the_completed_prefix() {
        use windows_sys::Win32::System::Memory::{PAGE_READONLY, PAGE_READWRITE, VirtualProtect};

        const BLOCK: usize = 4096;
        let pool = crate::watch::tests::pool(1);
        // Aligned full vector fault; sparse vector fault with a completed byte
        // in the same mask; full unaligned vector straddling the page boundary.
        for (prefix, sparse) in [(16, false), (24, true), (24, false)] {
            let start = pool + BLOCK - prefix;
            let len = prefix + BLOCK + 1;
            let mut gpu = vec![0xab; len];
            if sparse { gpu[17] = 0; }
            // SAFETY: our pool includes all three pages; restore after each case.
            unsafe { (start as *mut u8).write_bytes(0, len) };
            let mut previous = 0;
            // SAFETY: the second 4 KiB page belongs to this test allocation.
            assert_ne!(unsafe { VirtualProtect((pool + BLOCK) as *const _, BLOCK, PAGE_READONLY, &mut previous) }, 0);
            BEFORE_KERNEL_RETRY.set(Some(Box::new(move || {
                std::thread::spawn(move || {
                    let last = (pool + BLOCK - 1) as *mut u8;
                    // SAFETY: the kernel retry waits for this thread. The last
                    // byte on the first page has already received the GPU value.
                    unsafe {
                        assert_eq!(last.read_volatile(), 0xab);
                        last.write_volatile(0xcd);
                    }
                }).join().unwrap();
            })));
            write_changed(start, &gpu, &vec![0; len]);
            assert!(BEFORE_KERNEL_RETRY.take().is_none(), "no kernel retry");
            let mut actual = vec![0; len];
            assert!(crate::host::read_into(start as *const u8, &mut actual));
            gpu[prefix - 1] = 0xcd;
            gpu[prefix..prefix + BLOCK].fill(0);
            assert_eq!(actual, gpu);
            // SAFETY: the same page of our still-live reservation.
            assert_ne!(unsafe { VirtualProtect((pool + BLOCK) as *const _, BLOCK, PAGE_READWRITE, &mut previous) }, 0);
        }
        crate::watch::tests::free(pool);
    }

    #[test]
    fn inaccessible_store_retries_mapped_bytes_beyond_the_hole() {
        use windows_sys::Win32::System::Memory::{
            MEM_COMMIT, MEM_DECOMMIT, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc, VirtualFree,
        };
        const BLOCK: usize = 4096;
        // SAFETY: a fresh reservation with one deliberately decommitted block.
        let at = unsafe {
            VirtualAlloc(
                std::ptr::null(),
                3 * BLOCK,
                MEM_COMMIT | MEM_RESERVE,
                PAGE_READWRITE,
            )
        } as usize;
        assert_ne!(at, 0);
        assert_ne!(
            // SAFETY: this is the middle block of our three-block reservation.
            unsafe { VirtualFree((at + BLOCK) as *mut _, BLOCK, MEM_DECOMMIT) },
            0
        );
        let old = vec![0u8; 3 * BLOCK];
        let gpu = vec![0x39; 3 * BLOCK];
        write_changed(at, &gpu, &old);
        for start in [at, at + 2 * BLOCK] {
            let mut check = vec![0; BLOCK];
            assert!(crate::host::read_into(start as *const u8, &mut check));
            assert!(check.iter().all(|&b| b == 0x39));
        }
        crate::watch::tests::free(at);
    }

    #[test]
    fn withheld_byte_on_each_side_of_a_boundary_blocks_both_marked_pages() {
        let Some(h) = host() else { return };
        let p = crate::watch::tests::pool(4);
        let len = 4 * PAGE;
        register(p, p as u64, len as u64);
        let copy = find(p as u64, len as u64).unwrap();
        copy.update(p, len);
        let list = h.new_list().unwrap();
        {
            let _writer = list.writer_uses_memory(true);
            copy.gpu_writes(&list, (p + 2 * PAGE - 1) as u64, 2, true);
        }
        let shader = copy.clone();
        list.defer_host(move || {
            let map = bitmap(false).unwrap();
            for i in 0..4 {
                shader.write_host((p + i * PAGE) as u64, &[0x78]);
                let bit = shader.marks.unwrap() as usize + i;
                // SAFETY: a valid bitmap word under the host submission lock.
                unsafe { *(map.data as *mut u32).add(bit / 32) |= 1 << (bit % 32) };
            }
        });
        list.defer_missed(|| true);
        list.close();
        h.execute(&[&list]);
        for (i, expected) in [0x78, 0, 0, 0x78].into_iter().enumerate() {
            let mut value = [0];
            assert!(crate::host::read_into(
                (p + i * PAGE) as *const u8,
                &mut value
            ));
            assert_eq!(value[0], expected);
        }
        drop(list);
        unregister(p, p as u64);
        drop(copy);
        crate::watch::tests::free(p);
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn masked_stores_match_every_vector_mask_and_partial_tail() {
        // Every mask sees late CPU data in its gaps. Skew the destination
        // so some vectors straddle 4 KiB, and put sentinels beyond both ends.
        let masks: Vec<u16> = (0..=u16::MAX).collect();
        let gpu: Vec<u8> = (0..16 * masks.len()).map(|i| (i % 127) as u8).collect();
        let mut dst = vec![0xee; gpu.len() + 2];
        write_masked(dst.as_mut_ptr() as usize + 1, &gpu, &masks);
        for (i, &byte) in gpu.iter().enumerate() {
            let selected = masks[i / 16] & (1 << (i % 16)) != 0;
            assert_eq!(dst[1 + i], if selected { byte } else { 0xee });
        }
        assert_eq!((dst[0], dst[dst.len() - 1]), (0xee, 0xee));

        for len in 1..16 {
            for pattern in 0..16 {
                let mask = (0x1111 * pattern) & ((1 << len) - 1);
                let mut dst = [0xee; 18];
                write_masked(dst.as_mut_ptr() as usize + 1, &gpu[..len], &[mask]);
                for (i, &got) in dst[1..].iter().enumerate() {
                    let selected = i < len && mask & (1 << i) != 0;
                    assert_eq!(got, if selected { gpu[i] } else { 0xee });
                }
                assert_eq!(dst[0], 0xee);
            }
        }
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn patterned_stores_report_the_first_unfinished_byte_on_a_guard() {
        use windows_sys::Win32::System::Memory::{PAGE_GUARD, PAGE_READWRITE, VirtualProtect};

        const BLOCK: usize = 4096;
        let pool = crate::watch::tests::pool(1);
        let at = pool + BLOCK - 16;
        let gpu = [0xab; 32];
        for pattern in 1u16..16 {
            let mask = pattern * 0x1111;
            // SAFETY: both vectors are inside this live allocation. The
            // preceding iteration consumed its one-shot guard, if any.
            unsafe { (at as *mut u8).write_bytes(0xee, gpu.len()) };
            let mut previous = 0;
            // SAFETY: the second vector begins a private OS page of this pool.
            assert_ne!(unsafe { VirtualProtect((pool + BLOCK) as *const _, BLOCK, PAGE_READWRITE | PAGE_GUARD, &mut previous) }, 0);
            let left = delta_store::remaining_after_store(at, &gpu, &[mask, mask]);
            assert_eq!(left, 16 - mask.trailing_zeros() as usize);
            let mut actual = [0; 32];
            assert!(crate::host::read_into(at as *const _, &mut actual));
            for (i, &byte) in actual.iter().enumerate() {
                let completed = i < 16 && mask & (1 << i) != 0;
                assert_eq!(byte, if completed { 0xab } else { 0xee });
            }
        }
        crate::watch::tests::free(pool);
    }

    #[test]
    fn late_cpu_bytes_in_short_gaps_survive_exact_stores() {
        for len in [0, 1, 15, 16, 17, 65, PAGE] {
            let old = vec![0x11; len];
            let host: Vec<_> = (0..len)
                .map(|i| if i % 17 == 1 { 0x11 } else { 0xaa })
                .collect();
            // These writes landed after the copy-back sampled title memory.
            let mut title = vec![0x77u8; len];
            write_changed(title.as_mut_ptr() as usize, &host, &old);
            assert!(
                title
                    .iter()
                    .enumerate()
                    .all(|(i, &b)| b == if i % 17 == 1 { 0x77 } else { 0xaa })
            );
        }
    }

    /// Pages shaders marked (`copy_back_marks`, `victory_shader::emit`
    /// "Write marks") go back by the bytes the GPU changed. The title writes
    /// a marked page after the copy took it (the guard saves its bytes from
    /// before) and a page nobody marked: the shader's byte and the title's
    /// both stay in the first; the second keeps the title's, and the copy's
    /// byte there (no mark) is not put back. The bits are cleared.
    #[test]
    fn marked_pages_go_back_by_the_bytes_the_gpu_changed() {
        let p = crate::watch::tests::pool(3);
        let len = 3 * PAGE;
        register(p, p as u64, len as u64);
        let (Some(copy), Some(b)) = (find(p as u64, len as u64), bitmap(false)) else {
            unregister(p, p as u64);
            return; // no host GPU here
        };
        // Host::execute owns these process-wide bitmaps from refresh through drain.
        crate::host::tests::without_submissions(|| {
            let base = copy.marks.expect("tracked memory gets mark bits") as usize;
            assert_eq!(copy.mark_entry(p as u64 + PAGE as u64 + 8), Some((base as u32 + 1, 8)));
            copy.update(p, len);
            copy.writing(p, len, true);
            assert!(copy.before.lock().unwrap().0.is_empty(), "no snapshot");
            // The shaders write a byte in each page; the title writes pages 1
            // and 2 meanwhile.
            copy.write_host((p + 10) as u64, &[0xaa]);
            copy.write_host((p + PAGE + 20) as u64, &[0xbb]);
            copy.write_host((p + 2 * PAGE + 30) as u64, &[0xcc]);
            let title = |at: usize| (p + at) as *mut u8;
            // SAFETY: (title writes and reads below) bytes of our pool.
            unsafe {
                title(PAGE + 40).write_volatile(0x11);
                title(2 * PAGE + 50).write_volatile(0x22);
            }
            // The shaders marked pages 0 and 1.
            let bits = b.data as *mut u32;
            for n in [base, base + 1] {
                // SAFETY: a dword of the mapped bitmap.
                unsafe { *bits.add(n / 32) |= 1 << (n % 32) };
            }
            copy_back_marks(&[], false);
            // SAFETY: bytes of our pool.
            let read = |at: usize| unsafe { title(at).read_volatile() };
            assert_eq!(read(10), 0xaa, "the shader's byte, page 0");
            assert_eq!((read(PAGE + 20), read(PAGE + 40)), (0xbb, 0x11), "both writers' bytes, page 1");
            assert_eq!((read(2 * PAGE + 30), read(2 * PAGE + 50)), (0, 0x22), "page 2 not marked");
            // SAFETY: dwords of the mapped bitmap.
            let left: u32 = (base / 32..=(base + 1) / 32).map(|w| unsafe { *bits.add(w) }).fold(0, |a, w| a | w);
            assert_eq!(left, 0, "bits cleared");
            assert_eq!(copy.copy_back(p, len), 0, "nothing kept to compare");
        });
        unregister(p, p as u64);
    }

    /// Marks of shaders using title memory stay out of title memory while
    /// their results are held back, and are cleared; the other shaders'
    /// marks of the same submission go back.
    #[test]
    fn marks_of_shaders_using_title_memory_are_held_back() {
        let p = crate::watch::tests::pool(2);
        let len = 2 * PAGE;
        register(p, p as u64, len as u64);
        let (Some(copy), Some(others), Some(memory)) = (find(p as u64, len as u64), bitmap(false), bitmap(true)) else {
            unregister(p, p as u64);
            return; // no host GPU here
        };
        // Host::execute owns these process-wide bitmaps from refresh through drain.
        crate::host::tests::without_submissions(|| {
            let base = copy.marks.expect("tracked memory gets mark bits") as usize;
            copy.update(p, len);
            copy.writing(p, len, true);
            copy.write_host((p + 10) as u64, &[0xaa]);
            copy.write_host((p + PAGE + 10) as u64, &[0xbb]);
            // Page 0 marked by another shader, page 1 by one using title memory.
            for (b, n) in [(others, base), (memory, base + 1)] {
                // SAFETY: a dword of the mapped bitmap.
                unsafe { *(b.data as *mut u32).add(n / 32) |= 1 << (n % 32) };
            }
            copy_back_marks(&[], true);
            // SAFETY: bytes of our pool.
            let read = |at: usize| unsafe { ((p + at) as *const u8).read_volatile() };
            assert_eq!((read(10), read(PAGE + 10)), (0xaa, 0), "page 1 held back");
            // SAFETY: a dword of the mapped bitmap.
            assert_eq!(unsafe { *(memory.data as *const u32).add((base + 1) / 32) } & 1 << ((base + 1) % 32), 0, "cleared");
        });
        unregister(p, p as u64);
    }

    /// What a frame withheld leaves every host copy of its memory at the
    /// frame's end (run textures9's shape): in frame N a program using title
    /// memory writes a record into the copies and the frame misses, so it
    /// stays out of title memory but in the copies for the GPU; in frame
    /// N+1, which misses nothing, a program reading the record must see
    /// title memory's bytes, not frame N's, in a title buffer's copy and in
    /// an inner buffer's copy of the same pages alike.
    #[test]
    fn what_a_frame_withholds_leaves_the_copies_at_its_end() {
        let p = crate::watch::tests::pool(2);
        let len = 2 * PAGE;
        // SAFETY: a byte of our pool.
        unsafe { ((p + 16) as *mut u8).write_volatile(0x11) };
        register(p, p as u64, len as u64);
        register(p + 1, p as u64, PAGE as u64);
        let (Some(outer), Some(inner)) = (find(p as u64, len as u64), find(p as u64 + 16, 16)) else {
            unregister(p + 1, p as u64);
            unregister(p, p as u64);
            return; // no host GPU here
        };
        assert!(!Arc::ptr_eq(&outer, &inner));
        let byte = |c: &Shadow| c.read_host(p as u64 + 16, 1)[0] & 0xff;
        for c in [&outer, &inner] {
            let (start, n) = c.range();
            c.update(start as usize, n as usize);
            c.writing(start as usize, n as usize, false);
            c.write_host(p as u64 + 16, &[0x22]);
            c.keep_writes(start as usize, n as usize);
        }
        crate::memory::drop_withheld(p + 16, 4);
        assert_eq!((byte(&outer), byte(&inner)), (0x22, 0x22), "frame N: the GPU's copies keep it");
        crate::memory::log_frame();
        for c in [&outer, &inner] {
            let (start, n) = c.range();
            c.update(start as usize, n as usize);
        }
        assert_eq!((byte(&outer), byte(&inner)), (0x11, 0x11), "frame N+1: title memory's bytes");
        // SAFETY: as above.
        assert_eq!(unsafe { ((p + 16) as *const u8).read_volatile() }, 0x11, "title memory untouched");
        unregister(p + 1, p as u64);
        unregister(p, p as u64);
    }

    /// A partial refresh of an armed page updates its saved bytes too (both
    /// under and beside a pending marked range): copy-back neither puts the
    /// refreshed byte back over a later title write nor drops a shader write
    /// back to the pre-fault value.
    #[test]
    fn partial_refresh_under_marked_range_updates_saved_bytes() {
        let Some(host) = host() else { return };
        let p = crate::watch::tests::pool(2);
        let len = 2 * PAGE;
        // Other submissions refresh every registered shadow. This fixture
        // controls both writers and the exact point at which its page is taken.
        let (buffer, data) = host.shared_buffer(len as u64).unwrap();
        let copy = Shadow::new(buffer, data, 0, p as u64, len as u64);
        let write = |at: usize, v: u8| {
            // SAFETY: a byte of our pool.
            unsafe { ((p + at) as *mut u8).write_volatile(v) };
        };
        copy.update(p, len);
        // Arms both pages: page 0 is wholly busy; page 1 past +16 is free.
        copy.writing(p, PAGE + 16, true);
        for (at, v) in [(20, 0x55), (24, 0x44), (40, 0x11), (PAGE + 20, 0x55)] {
            write(at, v);
        }
        assert!(copy.update(p + 16, 16) > 0 && copy.update(p + PAGE + 16, 16) > 0);
        copy.write_host((p + 10) as u64, &[0xaa]);
        copy.write_host((p + 24) as u64, &[0x00]);
        copy.write_host((p + PAGE + 10) as u64, &[0xbb]);
        write(20, 0x66);
        write(PAGE + 20, 0x77);
        // A separate title submission walks the registered shadows.
        let other = host.new_list().unwrap();
        crate::memory::bind(&other, crate::memory::OURS).unwrap();
        other.close();
        host.execute(&[&other]);
        copy.copy_back_marked(p, 1);
        copy.copy_back_marked(p + PAGE, 1);
        // SAFETY: bytes of our pool.
        let read = |at: usize| unsafe { ((p + at) as *const u8).read_volatile() };
        assert_eq!(
            [read(10), read(20), read(24), read(40)],
            [0xaa, 0x66, 0x00, 0x11]
        );
        assert_eq!([read(PAGE + 10), read(PAGE + 20)], [0xbb, 0x77]);
        copy.copy_back(p, PAGE + 16);
        drop(copy);
        crate::watch::tests::free(p);
    }

    #[test]
    fn readable_partial_snapshot_retries_after_a_full_page_read_fails() {
        use windows_sys::Win32::System::Memory::{
            MEM_COMMIT, MEM_DECOMMIT, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc, VirtualFree,
        };
        let Some(h) = host() else { return };
        const BLOCK: usize = 4096;
        const OFFSET: usize = 3 * BLOCK + 100;
        // SAFETY: one private allocation, initially readable and writable throughout.
        let p = unsafe {
            VirtualAlloc(std::ptr::null(), PAGE, MEM_RESERVE | MEM_COMMIT, PAGE_READWRITE)
        } as usize;
        assert_ne!(p, 0);
        let (buffer, data) = h.shared_buffer(PAGE as u64).unwrap();
        let copy = Shadow::new(buffer, data, 0, p as u64, PAGE as u64);
        copy.update(p, PAGE);
        copy.writing(p, PAGE, false);
        copy.writing(p + OFFSET, 4, false);
        copy.write_host((p + OFFSET) as u64, &[0xa1, 0xa2, 0xa3, 0xa4]);
        // SAFETY: only this 4 KiB block becomes inaccessible. OFFSET stays mapped.
        assert_ne!(unsafe { VirtualFree((p + BLOCK) as *mut _, BLOCK, MEM_DECOMMIT) }, 0);
        let mut full_read_failed = false;
        copy.copy_back_or_not_with(p, PAGE, true, |at, bytes| {
            let read = crate::host::read_into(at, bytes);
            full_read_failed = !read;
            read
        });
        copy.copy_back(p + OFFSET, 4);
        let mut actual = [0; 4];
        assert!(crate::host::read_into((p + OFFSET) as *const u8, &mut actual));
        drop(copy);
        crate::watch::tests::free(p);
        assert!(full_read_failed);
        assert_eq!(actual, [0xa1, 0xa2, 0xa3, 0xa4], "readable subrange after a failed full-page read");
    }

    #[test]
    fn partial_snapshot_after_a_full_page_preserves_a_new_cpu_store() {
        let Some(h) = host() else { return };
        let p = crate::watch::tests::pool(1);
        let (buffer, data) = h.shared_buffer(PAGE as u64).unwrap();
        let copy = Shadow::new(buffer, data, 0, p as u64, PAGE as u64);
        copy.update(p, PAGE);
        copy.writing(p, PAGE, false);
        copy.writing(p + 100, 4, false);
        copy.write_host((p + 100) as u64, &[0xa1, 0xa2, 0xa3, 0xa4]);
        copy.copy_back(p, PAGE);
        let mut first = [0; 4];
        assert!(crate::host::read_into((p + 100) as *const u8, &mut first));

        // A CPU store after the full-page readback supersedes that shader byte.
        // SAFETY: one byte of the live title allocation.
        unsafe { ((p + 101) as *mut u8).write_volatile(0xc2) };
        copy.copy_back(p + 100, 4);
        let mut second = [0; 4];
        assert!(crate::host::read_into((p + 100) as *const u8, &mut second));
        drop(copy);
        crate::watch::tests::free(p);
        assert_eq!(first, [0xa1, 0xa2, 0xa3, 0xa4]);
        assert_eq!(second, [0xa1, 0xc2, 0xa3, 0xa4], "partial snapshot replayed a completed page");
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn fragmented_megabyte_snapshot_needs_no_kernel_writes() {
        const BYTES: usize = 1 << 20;
        let host = host().expect("D3D12 for snapshot copy-back");
        let pool = crate::watch::tests::pool(BYTES / PAGE);
        let (buffer, data) = host.shared_buffer(BYTES as u64).unwrap();
        let copy = Shadow::new(buffer, data, 0, pool as u64, BYTES as u64);
        copy.update(pool, BYTES);
        copy.writing(pool, BYTES, false);
        // The soak shader's XOR against an all-zero title. Every high byte
        // remains zero, fragmenting the megabyte into over 260,000 runs.
        let gpu: Vec<u8> = (0..(BYTES / 4) as u32)
            .flat_map(|word| (0xabc123 + word).to_le_bytes()).collect();
        copy.write_host(pool as u64, &gpu);
        crate::host::MAPPED_WRITE_CALLS.set(0);
        assert_eq!(copy.copy_back(pool, BYTES), BYTES);
        assert_eq!(crate::host::MAPPED_WRITE_CALLS.get(), 0);
        let mut actual = vec![0; BYTES];
        assert!(crate::host::read_into(pool as *const u8, &mut actual));
        assert!(actual == gpu);
        drop(copy);
        crate::watch::tests::free(pool);
    }

    #[test]
    fn private_snapshot_groups_keep_cpu_gaps_and_retry_a_failed_piece() {
        const PAGES: usize = 18;
        const SKIP: usize = 7;
        let h = host().expect("D3D12 for snapshot copy-back");
        let pool = crate::watch::tests::pool(PAGES);
        let len = PAGES * PAGE;
        let (buffer, data) = h.shared_buffer(len as u64).unwrap();
        let copy = Shadow::new(buffer, data, 0, pool as u64, len as u64);
        copy.update(pool, len);
        // Clip the first and last page. A second declaration can retry a
        // readable subrange of the piece whose full read fails.
        copy.writing(pool + 1, len - 2, false);
        let retry = pool + SKIP * PAGE + 10;
        copy.writing(retry, 3, false);
        for page in 0..PAGES {
            let at = pool + page * PAGE + 10;
            copy.write_host(at as u64, &[0xa1, 0, 0xa3]);
            // SAFETY: preexisting CPU data in the gap selects the kernel
            // run fallback. Each successful piece issues two writes.
            unsafe { ((at + 1) as *mut u8).write_volatile(0xc1) };
        }
        assert!(private_write_watch(pool + 1, len - 2));
        crate::host::MAPPED_WRITE_CALLS.set(0);
        let written = copy.snapshot_copy(pool + 1, len - 2, true, |at, title| {
            let base = at as usize & !(PAGE - 1);
            if base == pool + SKIP * PAGE { return false; }
            assert!(crate::host::read_into(at, title));
            // SAFETY: each callback owns a distinct, live title page. Model
            // a CPU store in the gap after sampling but before publication.
            unsafe { ((base + 11) as *mut u8).write_volatile(0xc2) };
            true
        });
        assert_eq!(written, len - 2 - PAGE);
        assert_eq!(crate::host::MAPPED_WRITE_CALLS.get(), 2 * (PAGES - 1));
        assert_eq!(copy.copy_back(retry, 3), 3);
        for page in 0..PAGES {
            let at = pool + page * PAGE + 10;
            let cpu = if page == SKIP { 0xc1 } else { 0xc2 };
            let mut title = [0; 3];
            assert!(crate::host::read_into(at as *const u8, &mut title));
            assert_eq!(title, [0xa1, cpu, 0xa3]);
            // The joined resync copied the CPU's byte into the shadow too.
            let word = copy.read_host((at - 2) as u64, 1)[0].to_le_bytes();
            assert_eq!(word[3], cpu);
        }
        drop(copy);
        crate::watch::tests::free(pool);
    }

    /// The title releases a pool while a parallel snapshot write-back is
    /// storing into it and maps one physical page at two addresses that
    /// belong to different groups. The release waits for every group, so
    /// the outcome is the address-order write-back followed by the remap.
    #[test]
    fn a_release_and_alias_during_parallel_groups_waits_for_them() {
        use std::sync::atomic::{AtomicBool, Ordering::SeqCst};
        use windows_sys::Win32::System::Memory::{
            CreateFileMappingW, FILE_MAP_WRITE, MEM_RELEASE, MEMORY_MAPPED_VIEW_ADDRESS,
            MapViewOfFile, MapViewOfFileEx, PAGE_READWRITE, UnmapViewOfFile, VirtualFree,
        };
        const PAGES: usize = 16;
        // Groups of four pages: the first and the third group.
        const ALIASES: [usize; 2] = [1, 9];
        let h = host().expect("D3D12 for snapshot copy-back");
        // SAFETY: a pagefile section of one page, filled through a temporary view.
        let section = unsafe {
            let section = CreateFileMappingW(
                -1isize as _, std::ptr::null(), PAGE_READWRITE, 0, PAGE as u32, std::ptr::null(),
            );
            assert!(!section.is_null());
            let view = MapViewOfFile(section, FILE_MAP_WRITE, 0, 0, PAGE);
            assert!(!view.Value.is_null());
            view.Value.cast::<u8>().write_bytes(0x5a, PAGE);
            UnmapViewOfFile(view);
            section as usize
        };
        let section_bytes = || {
            // SAFETY: a fresh read-only use of the live section.
            unsafe {
                let view = MapViewOfFile(section as _, FILE_MAP_WRITE, 0, 0, PAGE);
                let bytes = std::slice::from_raw_parts(view.Value.cast::<u8>(), PAGE).to_vec();
                UnmapViewOfFile(view);
                bytes
            }
        };
        // What the runtime does in `XMemFreeDefault` and then mapping
        // physical pages. False if another thread took the freed range.
        let remap = move |pool: usize| {
            let _remapping = crate::watch::Writing::before_remap(pool, PAGES * PAGE);
            // SAFETY: the test's pool; the shadow tolerates unmapped title memory.
            assert_ne!(unsafe { VirtualFree(pool as *mut _, 0, MEM_RELEASE) }, 0);
            let mapped = ALIASES.iter().all(|page| {
                let at = pool + page * PAGE;
                // SAFETY: a view of the live section in the address range just released.
                let view = unsafe { MapViewOfFileEx(section as _, FILE_MAP_WRITE, 0, 0, PAGE, at as _) };
                view.Value as usize == at
            });
            crate::watch::title_memory_remapped(pool, PAGES * PAGE);
            mapped
        };
        let unmap = |pool: usize| {
            for page in ALIASES {
                // SAFETY: the views mapped by `remap`, if any; the shadow is gone.
                unsafe { UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS { Value: (pool + page * PAGE) as _ }) };
            }
        };
        let run = |race: bool| {
            let mut attempts = 0;
            loop {
                attempts += 1;
                assert!(attempts <= 5, "no undisturbed attempt");
                let pool = crate::watch::tests::pool(PAGES);
                let len = PAGES * PAGE;
                let (buffer, data) = h.shared_buffer(len as u64).unwrap();
                let copy = Shadow::new(buffer, data, 0, pool as u64, len as u64);
                copy.update(pool, len);
                copy.writing(pool, len, false);
                for page in 0..PAGES {
                    copy.write_host((pool + page * PAGE + 10) as u64, &[0xa0 + page as u8, 0, 0xb0]);
                }
                FORCE_SEQUENTIAL.set(!race);
                let held = AtomicBool::new(false);
                let remapped_early = AtomicBool::new(false);
                let remapper = Mutex::new(None);
                let written = copy.snapshot_copy(pool, len, true, |at, title| {
                    if race && at as usize == pool {
                        held.store(crate::watch::mappings_held(), SeqCst);
                        let done = std::sync::Arc::new(AtomicBool::new(false));
                        let finished = done.clone();
                        *remapper.lock().unwrap() = Some(std::thread::spawn(move || {
                            let mapped = remap(pool);
                            finished.store(true, SeqCst);
                            mapped
                        }));
                        let waited = std::time::Instant::now();
                        while !done.load(SeqCst) && waited.elapsed() < std::time::Duration::from_millis(300) {
                            std::thread::yield_now();
                        }
                        remapped_early.store(done.load(SeqCst), SeqCst);
                    }
                    crate::host::read_into(at, title)
                });
                FORCE_SEQUENTIAL.set(false);
                let mapped = match remapper.into_inner().unwrap() {
                    Some(thread) => thread.join().unwrap(),
                    None => remap(pool),
                };
                drop(copy);
                unmap(pool);
                // Another test's remap kept this one in address order, or took
                // the released range: neither is the case under test.
                if !mapped || race && !held.into_inner() {
                    continue;
                }
                break (written, remapped_early.into_inner(), section_bytes());
            }
        };
        let (expected, _, untouched) = run(false);
        assert_eq!(expected, PAGES * PAGE);
        assert!(untouched.iter().all(|&b| b == 0x5a));
        let (written, remapped_early, aliased) = run(true);
        assert!(aliased == untouched, "a group stored into the remapped physical page");
        assert_eq!(written, expected);
        assert!(!remapped_early, "the release completed while groups were storing");
        // SAFETY: the section handle of this test, no views left.
        unsafe { windows_sys::Win32::Foundation::CloseHandle(section as _) };
    }

    #[test]
    fn unmarked_snapshot_preserves_late_cpu_bytes_across_a_page_boundary() {
        let h = host().expect("D3D12 for shadow copy-back");
        let pool = crate::watch::tests::pool(2);
        let (buffer, data) = h.shared_buffer((2 * PAGE) as u64).unwrap();
        let copy = Shadow::new(buffer, data, 0, pool as u64, (2 * PAGE) as u64);
        let start = pool + PAGE - 35;
        let len = 35 + 37;
        copy.update(pool, 2 * PAGE);
        copy.writing(start, len, false);

        // Offsets are local to each piece: both have two full masks and a tail.
        // The GPU owns both segment ends and both sides of the page split.
        for (at, size) in [(start, 35), (pool + PAGE, 37)] {
            for offset in [0, 14, 16, 31, size - 1] {
                copy.write_host((at + offset) as u64, &[0xa1]);
            }
        }
        let mut expected = vec![0; len + 2];
        let mut reads = Vec::new();
        let written = copy.copy_back_or_not_with(start, len, true, |at, snapshot| {
            assert!(crate::host::read_into(at, snapshot));
            assert!(snapshot.iter().all(|&b| b == 0));
            let at = at as usize;
            let size = snapshot.len();
            reads.push((at, size));
            let base = at - start + 1;
            for offset in [0, 14, 16, 31, size - 1] {
                expected[base + offset] = 0xa1;
            }
            for offset in [1, 15, 17, 32, size - 2] {
                expected[base + offset] = 0xc2;
            }
            std::thread::spawn(move || {
                // SAFETY: all stores are in the live two-page title allocation.
                // The worker finishes after the read and before the masked store.
                unsafe {
                    for offset in [1, 15, 17, 32, size - 2] {
                        ((at + offset) as *mut u8).write_volatile(0xc2);
                    }
                    let outside = if at == start { start - 1 } else { start + len };
                    (outside as *mut u8).write_volatile(0xce);
                }
            }).join().unwrap();
            true
        });
        assert_eq!(reads, [(start, 35), (pool + PAGE, 37)]);
        assert_eq!(written, len);
        expected[0] = 0xce;
        expected[len + 1] = 0xce;
        let mut actual = vec![0; len + 2];
        assert!(crate::host::read_into((start - 1) as *const u8, &mut actual));
        assert_eq!(actual, expected);
        drop(copy);
        crate::watch::tests::free(pool);
    }

    #[test]
    fn overlapping_snapshots_keep_late_cpu_stores() {
        let Some(h) = host() else { return };
        let p = crate::watch::tests::pool(1);
        let (buffer, data) = h.shared_buffer(PAGE as u64).unwrap();
        let copy = Shadow::new(buffer, data, 0, p as u64, PAGE as u64);
        copy.update(p, PAGE);
        copy.writing(p + 2, 4, false);
        copy.writing(p + 3, 5, false);
        copy.write_host((p + 2) as u64, &[0xa1]);
        copy.write_host((p + 5) as u64, &[0xa2]);

        let mut raced = false;
        copy.copy_back_or_not_with(p + 2, 4, true, |at, title| {
            assert!(crate::host::read_into(at, title));
            std::thread::spawn(move || {
                // SAFETY: the CPU writes one byte inside the live allocation.
                unsafe { ((p + 4) as *mut u8).write_volatile(0xc1) };
            }).join().unwrap();
            raced = true;
            true
        });
        assert!(raced);
        let mut first = [0; 4];
        assert!(crate::host::read_into((p + 2) as *const u8, &mut first));
        // SAFETY: the same CPU byte, before the overlapping declaration drains.
        unsafe { ((p + 4) as *mut u8).write_volatile(0xc2) };
        copy.copy_back(p + 3, 5);
        let mut second = [0; 4];
        assert!(crate::host::read_into((p + 2) as *const u8, &mut second));
        drop(copy);
        crate::watch::tests::free(p);
        assert_eq!(first, [0xa1, 0, 0xc1, 0xa2], "CPU store during first snapshot");
        assert_eq!(second, [0xa1, 0, 0xc2, 0xa2], "CPU store before overlapping snapshot");
    }

    #[test]
    fn withheld_and_ordinary_marks_on_one_shadow_page_keep_its_title_bytes() {
        let Some(h) = host() else { return };
        let p = crate::watch::tests::pool(2);
        register(p, p as u64, (2 * PAGE) as u64);
        let copy = find(p as u64, (2 * PAGE) as u64).unwrap();
        copy.update(p, 2 * PAGE);
        let list = h.new_list().unwrap();
        let shader = copy.clone();
        list.defer_host(move || {
            // No declared ranges distinguish the two writes on the first page.
            // A neighboring ordinary-only page must still go back.
            let base = shader.marks.unwrap() as usize;
            for (memory, offset, value) in [(true, 100, 0xa1), (false, 200, 0xb2), (false, PAGE, 0xc3)] {
                shader.write_host((p + offset) as u64, &[value]);
                let bit = base + offset / PAGE;
                let bits = bitmap(memory).unwrap().data as *mut u32;
                // SAFETY: a valid owned mark under the host submission lock.
                unsafe { *bits.add(bit / 32) |= 1 << (bit % 32) };
            }
        });
        list.defer_missed(|| true);
        list.close();
        h.execute(&[&list]);

        let mut title = [0; 3];
        for (&offset, byte) in [100, 200, PAGE].iter().zip(&mut title) {
            assert!(crate::host::read_into((p + offset) as *const u8, std::slice::from_mut(byte)));
        }
        let refreshed = copy.update(p, 2 * PAGE);
        let after = [100, 200, PAGE].map(|offset| copy.read_host((p + offset) as u64, 1)[0]);
        drop(list);
        unregister(p, p as u64);
        drop(copy);
        crate::watch::tests::free(p);
        assert_eq!(title, [0, 0, 0xc3], "withheld mark on the same shadow page");
        assert_eq!(refreshed, PAGE);
        assert_eq!(after, [0, 0, 0xc3], "refresh must take the retained title bytes");
    }

    #[test]
    fn publish_valid_declaration_beside_a_withheld_write_on_one_page() {
        let Some(host) = host() else { return };
        let p = crate::watch::tests::pool(1);
        register(p, p as u64, PAGE as u64);
        let copy = find(p as u64, PAGE as u64).unwrap();
        copy.update(p, PAGE);
        let valid = host.new_list().unwrap();
        let missed = host.new_list().unwrap();
        copy.gpu_writes(&valid, (p + 64) as u64, 4, true);
        {
            let _writer = missed.writer_uses_memory(true);
            copy.gpu_writes(&missed, (p + 256) as u64, 4, true);
        }
        let shader = copy.clone();
        valid.defer_host(move || {
            shader.write_host((p + 64) as u64, &[0x5a; 4]);
            shader.write_host((p + 256) as u64, &[0xa5; 4]);
            let n = shader.marks.unwrap() as usize;
            let bits = bitmap(false).unwrap().data as *mut u32;
            // SAFETY: the submission lock serializes access to this bitmap.
            unsafe { *bits.add(n / 32) |= 1 << (n % 32) };
        });
        missed.defer_missed(|| true);
        valid.close();
        missed.close();
        host.execute(&[&valid, &missed]);
        let mut actual = [0u8; 260];
        assert!(crate::host::read_into(p as *const u8, &mut actual));
        let retained = copy.read_host((p + 256) as u64, 1)[0];
        drop((valid, missed));
        unregister(p, p as u64);
        drop(copy);
        crate::watch::tests::free(p);
        assert_eq!(&actual[64..68], &[0x5a; 4], "valid shader write must reach title memory");
        assert_eq!(&actual[256..260], &[0; 4], "withheld title bytes");
        assert_eq!(retained, 0xa5a5a5a5, "withheld GPU bytes remain available");
    }

    /// Mark bits are handed out in runs and come back joined.
    #[test]
    fn mark_runs_are_reused_and_joined() {
        let mut m = Marks::default();
        assert_eq!((m.take(3), m.take(2)), (Some(0), Some(3)));
        m.give_back(0, 3);
        assert_eq!(m.take(1), Some(0), "first fit");
        m.give_back(3, 2);
        assert_eq!(m.free.iter().collect::<Vec<_>>(), [(&1, &4)], "3-4 joins the free 1-2");
        m.give_back(0, 1);
        assert_eq!(m.free.iter().collect::<Vec<_>>(), [(&0, &5)], "and 0 joins them");
        assert_eq!(m.take(5), Some(0));
        assert_eq!(pages(0x1_fff0, 0x20), 2, "a range across a page boundary");
        assert_eq!(pages(0x2_0000, 0x1_0000), 1);
    }
}
