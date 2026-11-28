//! Compiled host pipelines kept on disk, and the title pipelines worth
//! starting early (decision 0007, "Pipeline cache").
//!
//! 1. Every host pipeline goes into the host's pipeline library, named by
//!    a hash of all that its host description holds (the translated
//!    shaders' code, which changes when a translation does, our root
//!    signature, the graphics state). The library is saved under the cache
//!    folder; the host refuses a library of another adapter or driver, and
//!    an entry whose description differs, so neither is ever used.
//! 2. The title pipelines a run drew or dispatched with (made in time or
//!    not) are listed by a key known when the title creates them (their
//!    shader containers and graphics state); the next run hands those to
//!    the workers at creation, during loading. A full list keeps the
//!    latest run's first.
//!
//! 3. The pre-build (`victory translate --cache --pipelines`) lists the
//!    title pipelines it has put in the library, by the same key, so a
//!    second pre-build skips them without translating or loading anything.
//!    The list holds only for the library bytes it was saved with and for
//!    what names the pipelines (the translator, this module and `host`).
//!
//! The library is free-threaded but for loads of one pipeline on several
//! threads (`ID3D12Device1::CreatePipelineLibrary`, "Thread Safety"), so
//! pipelines of different names load at the same time. A load is not always
//! quick: the library of an AMD driver holds about 140 bytes per pipeline,
//! and what its own cache lacks is compiled inside the load, for seconds.
//! Loaded one at a time, 650 pipelines kept the eight workers to one core
//! for minutes, while each title frame could wait out its 500 ms for
//! them (`make`). Stores and serialising keep the lock: the size asked for
//! must still be right when the bytes are written. Loads take no lock at
//! all, since a load adds nothing to the library: a save holds the lock
//! while it serialises the whole library (hundreds of megabytes in the
//! world), and loads that waited on it stalled every worker for as long.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::fs::{self, File};
use std::io::{self, Write};
#[path = "library_dirs.rs"]
mod dirs;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Condvar, Mutex, OnceLock};

use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory2, DXGI_ADAPTER_DESC1, DXGI_ADAPTER_FLAG_SOFTWARE, DXGI_CREATE_FACTORY_FLAGS,
    IDXGIAdapter1, IDXGIDevice, IDXGIFactory4,
};
use windows::core::{HSTRING, Interface, PCWSTR};

// Directory handles also pin live generations against another process's GC.
static DIR: OnceLock<PathBuf> = OnceLock::new();
static LIBRARY_DIR: OnceLock<Option<PathBuf>> = OnceLock::new();
static SOURCE_GUARD: OnceLock<File> = OnceLock::new();
static DRIVER_GUARD: OnceLock<File> = OnceLock::new();

/// Keeps the compiled library and the title's shared early list under `root`.
pub fn use_pipeline_cache(root: PathBuf) {
    let mut check_drivers = false;
    let source = DIR.get_or_init(|| {
        let source = root.join(victory_shader::SOURCE_HASH);
        if let Ok(_root) = dirs::hold(&root, false) {
            if let Ok(guard) = dirs::hold(&source, false) {
                let _ = SOURCE_GUARD.set(guard);
            }
            // Rescue legacy early lists before retiring any of their owners.
            let (lists, excess) = dirs::legacy_layout(&root, &source);
            check_drivers = excess;
            if let Err(error) = migrate_early(&root, &lists) {
                tracing::warn!(%error, "could not migrate early pipeline lists");
            } else {
                dirs::prune_sources(&root, &source);
            }
        }
        source
    });
    // A fully warmed run might never write another PSO. Retire excess
    // generations left by older releases in that case too.
    if check_drivers && SOURCE_GUARD.get().is_some()
        && let Some(adapter) = library_dir().and_then(|d| d.parent())
    {
        dirs::prune_drivers(source, adapter);
    }
    let root = source.parent().unwrap();
    let path = root.join(LIBRARY_FILE);
    if dirs::regular(&path) && let Ok(_root) = dirs::hold(root, false) {
        // Legacy blobs are opaque. Only the driver can accept them; keep the
        // existing namespaced blob if both exist. Hard-link never overwrites.
        let result = migrate_blob(&path, library_dir().map(PathBuf::as_path));
        if let Err(error) = result {
            tracing::debug!(%error, "could not retire legacy pipeline library");
        }
    }
}

fn migrate_blob(path: &Path, target: Option<&Path>) -> io::Result<()> {
    if let Some(target) = target {
        let _leaf = prepare_dir(target)?;
        if !target.join(LIBRARY_FILE).exists() {
            // Unsupported hard links or an incompatible layout just cost a
            // rebuild. Never replace an already-published library.
            if let Err(error) = fs::hard_link(path, target.join(LIBRARY_FILE)) {
                tracing::debug!(%error, "could not reuse legacy pipeline library");
            }
        }
    }
    fs::remove_file(path)
}

fn translator_dir(root: PathBuf, translator: &[u8; 32]) -> PathBuf {
    root.join(&blake3::Hash::from(*translator).to_hex()[..16])
}

fn existing_library_dir() -> Option<&'static PathBuf> {
    if let Some(resolved) = LIBRARY_DIR.get() {
        return resolved.as_ref();
    }
    SOURCE_GUARD.get().and_then(|_| library_dir())
}

fn library_dir() -> Option<&'static PathBuf> {
    let source = DIR.get()?;
    LIBRARY_DIR.get_or_init(|| {
        let device = &crate::host::host()?.device;
        let adapter = source.join(adapter_namespace(device)?);
        if SOURCE_GUARD.get().is_some() && let Ok(guard) = dirs::hold(&adapter, false) {
            let _ = DRIVER_GUARD.set(guard);
        }
        Some(translator_dir(adapter, crate::shader_cache::build(crate::shader_cache::options())))
    }).as_ref()
}

fn prepare_dir(dir: &Path) -> io::Result<File> {
    let adapter = dir.parent().unwrap();
    let source = adapter.parent().unwrap();
    let root = source.parent().unwrap();
    fs::create_dir_all(root)?;
    let _root = dirs::hold(root, false)?;
    for (path, slot) in [(source, &SOURCE_GUARD), (adapter, &DRIVER_GUARD)] {
        if slot.get().is_none() {
            fs::create_dir_all(path)?;
            let _ = slot.set(dirs::hold(path, false)?);
        }
    }
    fs::create_dir_all(dir)?;
    dirs::hold(dir, false)
}

fn adapter_namespace(device: &ID3D12Device) -> Option<String> {
    // SAFETY: reads the identifier of this live device.
    let luid = unsafe { device.GetAdapterLuid() };
    // The kernel query describes physical node zero. For linked devices use
    // the logical adapter and driver identity supplied by DXGI.
    // SAFETY: GetNodeCount queries the same live device.
    if unsafe { device.GetNodeCount() } == 1 && let Some(name) = hardware_namespace(luid) {
        return Some(name);
    }
    dxgi_namespace(luid)
}

fn dxgi_namespace(luid: windows::Win32::Foundation::LUID) -> Option<String> {
    // SAFETY: query the adapter that owns the existing D3D12 device.
    let (desc, driver) = unsafe {
        let factory: IDXGIFactory4 = CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0)).ok()?;
        let adapter: IDXGIAdapter1 = factory.EnumAdapterByLuid(luid).ok()?;
        (adapter.GetDesc1().ok()?, adapter.CheckInterfaceSupport(&IDXGIDevice::IID).ok())
    };
    namespace(&desc, driver)
}

fn hardware_namespace(luid: windows::Win32::Foundation::LUID) -> Option<String> {
    use windows::Wdk::Graphics::Direct3D::*;

    let mut open = D3DKMT_OPENADAPTERFROMLUID { AdapterLuid: luid, ..Default::default() };
    // SAFETY: an initialized adapter LUID and a writable result handle.
    unsafe { D3DKMTOpenAdapterFromLuid(&mut open) }.ok().ok()?;
    let read = |kind, data, size| {
        let mut query = D3DKMT_QUERYADAPTERINFO {
            hAdapter: open.hAdapter,
            Type: kind,
            pPrivateDriverData: data,
            PrivateDriverDataSize: size,
        };
        // SAFETY: each call below supplies the live output object and size
        // prescribed by its query type.
        unsafe { D3DKMTQueryAdapterInfo(&mut query) }.ok().ok()
    };
    let result = (|| {
        let mut kind = D3DKMT_ADAPTERTYPE::default();
        read(KMTQAITYPE_ADAPTERTYPE, (&raw mut kind).cast(), size_of_val(&kind) as u32)?;
        const SOFTWARE_DEVICE: u32 = 1 << 2;
        // SAFETY: the successful adapter-type query initialized these flags, also readable as
        // Value.
        if unsafe { kind.Anonymous.Value } & SOFTWARE_DEVICE != 0 {
            return None;
        }
        let mut ids = D3DKMT_QUERY_DEVICE_IDS::default();
        read(KMTQAITYPE_PHYSICALADAPTERDEVICEIDS, (&raw mut ids).cast(), size_of_val(&ids) as u32)?;
        let ids = ids.DeviceIds;
        let mut version = D3DKMT_UMD_DRIVER_VERSION::default();
        read(KMTQAITYPE_UMD_DRIVER_VERSION, (&raw mut version).cast(), size_of_val(&version) as u32)?;
        if ids.VendorID == 0 || ids.DeviceID == 0 || version.DriverVersion == 0 {
            return None;
        }
        // The PCI subsystem DWORD contains subsystem ID then subvendor ID.
        let desc = DXGI_ADAPTER_DESC1 {
            VendorId: ids.VendorID,
            DeviceId: ids.DeviceID,
            SubSysId: (u32::from(u16::try_from(ids.SubSystemID).ok()?) << 16)
                | u32::from(u16::try_from(ids.SubVendorID).ok()?),
            Revision: ids.RevisionID,
            ..Default::default()
        };
        namespace(&desc, Some(version.DriverVersion))
    })();
    // SAFETY: this call releases the handle opened above, on all result paths.
    unsafe { D3DKMTCloseAdapter(&D3DKMT_CLOSEADAPTER { hAdapter: open.hAdapter }) }.ok().ok()?;
    result
}

fn namespace(desc: &DXGI_ADAPTER_DESC1, driver: Option<i64>) -> Option<String> {
    // If DXGI cannot identify the installed driver, use the in-memory
    // library for this run. An unknown driver cannot name a disk cache.
    let driver = driver?;
    let kind = if desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 == 0 { "hw" } else { "sw" };
    Some(format!("{kind}-{:08x}-{:08x}-{:08x}-{:08x}-{driver:016x}",
        desc.VendorId, desc.DeviceId, desc.SubSysId, desc.Revision))
}

/// Host pipelines created, loaded from the library, and title pipelines
/// started at creation, since the start.
pub(crate) static MADE: AtomicU64 = AtomicU64::new(0);
pub(crate) static LOADED: AtomicU64 = AtomicU64::new(0);
pub(crate) static EARLY: AtomicU64 = AtomicU64::new(0);

struct Library {
    library: ID3D12PipelineLibrary1,
    /// Pipelines stored since the last save.
    added: u64,
    /// Title pipelines the pre-build put in the library or found there, by
    /// key, and whether that list changed since the last save.
    prebuilt: HashSet<String>,
    prebuilt_changed: bool,
}

// SAFETY: D3D12 objects are free-threaded (a pipeline library with the
// exception `NAMES` takes care of).
unsafe impl Send for Library {}

/// The library for loads, beside the one stores and saves lock.
struct Shared {
    /// The same library as `state`'s, loaded from without its lock.
    loads: ID3D12PipelineLibrary1,
    state: Mutex<Library>,
    /// Whether a disk cache was configured when this library opened.
    /// A later configuration must not replace an unconsulted library.
    disk_configured: bool,
    // D3D borrows the driver prefix of this allocation. Drop it after both
    // library references. The optional index is in this same disk snapshot.
    image: Vec<u8>,
    driver_len: usize,
    initial_keys: OnceLock<HashSet<String>>,
}

// SAFETY: as for `Library`.
unsafe impl Send for Shared {}
// SAFETY: the library is free-threaded but for loads of one name, which
// `NAMES` keeps apart; the rest is behind `state`'s lock.
unsafe impl Sync for Shared {}

/// Names one thread at a time holds.
struct Names {
    held: Mutex<Option<HashSet<String>>>,
    released: Condvar,
}

/// A name held until this is dropped.
struct Held<'a> {
    names: &'a Names,
    name: &'a str,
}

impl Names {
    const fn new() -> Names {
        Names {
            held: Mutex::new(None),
            released: Condvar::new(),
        }
    }

    /// Holds `name`, once no other thread does.
    fn hold<'a>(&'a self, name: &'a str) -> Held<'a> {
        let mut held = self.held.lock().unwrap_or_else(|e| e.into_inner());
        while held.as_ref().is_some_and(|h| h.contains(name)) {
            held = self.released.wait(held).unwrap_or_else(|e| e.into_inner());
        }
        held.get_or_insert_default().insert(name.to_owned());
        Held { names: self, name }
    }
}

impl Drop for Held<'_> {
    fn drop(&mut self) {
        let mut held = self.names.held.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(h) = held.as_mut() {
            h.remove(self.name);
        }
        drop(held);
        self.names.released.notify_all();
    }
}

/// Pipelines being loaded, or made and stored.
static NAMES: Names = Names::new();

const LIBRARY_FILE: &str = "host-pipelines.bin";
const EARLY_FILE: &str = "early-pipelines.txt";
const PREBUILT_FILE: &str = "prebuilt-pipelines.txt";
/// Title pipelines listed for an early start, at most.
const EARLY_MAX: usize = 8192;

fn library() -> Option<&'static Shared> {
    static LIBRARY: OnceLock<Option<Shared>> = OnceLock::new();
    LIBRARY
        .get_or_init(|| {
            let device = crate::host::host()?.device.cast::<ID3D12Device1>().ok()?;
            let disk_configured = DIR.get().is_some();
            let dir = disk_configured.then(existing_library_dir).flatten();
            let read = |file| dir.and_then(|d| std::fs::read(d.join(file)).ok());
            let blob = read(LIBRARY_FILE).unwrap_or_default();
            let library = open(&device, blob, disk_configured);
            if library.is_none() {
                tracing::warn!("no host pipeline library: pipelines are not kept");
            }
            library
        })
        .as_ref()
}

/// The library in `blob`, or an empty one if the host rejects the blob (an
/// adapter or driver change, damage).
fn open(device: &ID3D12Device1, mut blob: Vec<u8>, disk_configured: bool) -> Option<Shared> {
    let make = |bytes: &[u8]| -> windows::core::Result<ID3D12PipelineLibrary1> {
        // SAFETY: the bytes outlive the library (kept beside it).
        unsafe {
            device.CreatePipelineLibrary::<ID3D12PipelineLibrary>(bytes)?.cast()
        }
    };
    let mut driver_len = driver_length(&blob);
    let loaded = if driver_len == 0 { None } else {
        let started = std::time::Instant::now();
        match make(&blob[..driver_len]) {
            Ok(library) => {
                tracing::info!(bytes = driver_len, ms = started.elapsed().as_millis(), "host pipeline library loaded");
                Some(library)
            }
            Err(error) => {
                tracing::info!(%error, "host pipeline library rejected; starting an empty one");
                None
            }
        }
    };
    let library = match loaded {
        Some(library) => library,
        None => {
            blob = Vec::new();
            driver_len = 0;
            make(&[]).ok()?
        }
    };
    Some(Shared {
        loads: library.clone(),
        state: Mutex::new(Library { library, added: 0, prebuilt: HashSet::new(), prebuilt_changed: false }),
        disk_configured,
        image: blob,
        driver_len,
        initial_keys: OnceLock::new(),
    })
}

// Legacy files consist entirely of driver bytes. New indexed files append
// the UTF-8 index, the little-endian driver length and a format signature.
const FOOTER: &[u8; 8] = b"VPSOIDX1";

fn driver_length(image: &[u8]) -> usize {
    if !image.ends_with(FOOTER) { return image.len(); }
    let Some(end) = image.len().checked_sub(16) else { return 0 };
    let n = u64::from_le_bytes(image[end..end+8].try_into().unwrap());
    usize::try_from(n).ok().filter(|n| *n <= end).unwrap_or(0)
}

impl Shared {
    // No state lock while hashing the (potentially gigabyte-sized) driver
    // bytes. Gameplay disk hits need neither the hash nor the parsed keys.
    fn initial_keys(&self) -> &HashSet<String> {
        self.initial_keys.get_or_init(|| {
            if self.driver_len == 0 { return HashSet::new(); }
            let (driver, tail) = self.image.split_at(self.driver_len);
            if let Some(index) = tail.get(..tail.len().saturating_sub(16)).filter(|s| !s.is_empty()) {
                listed(index, &tag(driver))
            } else {
                // A legacy sidecar's tag includes the sources of the writer.
                // This revised writer necessarily has a different tag.
                HashSet::new()
            }
        })
    }
}

/// What the pre-build's list holds for: the library's bytes, and what
/// names its pipelines (the translation, as the shader cache keys it; the
/// host descriptions).
fn tag(blob: &[u8]) -> String {
    let limit = victory_shader::emit::loop_limit().to_le_bytes();
    let translator = victory_shader::SOURCE_HASH.as_bytes();
    let mut h = blake3::Hasher::new();
    for part in [
        blob,
        translator,
        include_bytes!("../shader_cache.rs"),
        &limit,
        include_bytes!("host.rs"),
        include_bytes!("library.rs"),
    ] {
        h.update(&(part.len() as u64).to_le_bytes());
        h.update(part);
    }
    h.finalize().to_hex().to_string()
}

/// The pre-build's list as saved: the tag it holds for, then its keys.
fn list(tag: &str, keys: &HashSet<String>) -> String {
    let keys = keys.iter().map(String::as_str);
    std::iter::once(tag).chain(keys).collect::<Vec<_>>().join("\n")
}

/// The keys of a saved list, if it holds for `tag` (else none).
fn listed(list: &[u8], tag: &str) -> HashSet<String> {
    let list = String::from_utf8_lossy(list);
    let mut lines = list.lines();
    match lines.next() == Some(tag) {
        true => lines.map(str::to_owned).collect(),
        false => HashSet::new(),
    }
}

/// Whether the pre-build has the title pipeline `key` in the library.
pub(crate) fn prebuilt(key: &str) -> bool {
    library().is_some_and(|l| l.initial_keys().contains(key) || l.state.lock().unwrap_or_else(|e| e.into_inner()).prebuilt.contains(key))
}

/// The pre-build has the title pipeline `key` in the library now.
pub(crate) fn note_prebuilt(key: String) {
    if let Some(l) = library() && !l.initial_keys().contains(&key) {
        let mut l = l.state.lock().unwrap_or_else(|e| e.into_inner());
        if l.prebuilt.insert(key) {
            l.prebuilt_changed = true;
        }
    }
}

/// A pipeline's name in the library: the hash of what its host
/// description holds.
pub(crate) fn name(kind: &str, root: &[u8], code: &[&[u8]], state: &str) -> String {
    let parts = [kind.as_bytes(), root].into_iter().chain(code.iter().copied()).chain([state.as_bytes()]);
    // One contiguous input lets BLAKE3 hash multiple short shader chunks in
    // parallel. Keep the original streaming path for large pipeline descriptions.
    let size = parts.clone().try_fold(0usize, |n, p| n.checked_add(8)?.checked_add(p.len()));
    let digest = if let Some(size) = size.filter(|&n| n <= 8 * blake3::CHUNK_LEN) {
        let mut frame = Vec::with_capacity(size);
        for part in parts {
            frame.extend_from_slice(&(part.len() as u64).to_le_bytes());
            frame.extend_from_slice(part);
        }
        blake3::hash(&frame)
    } else {
        let mut hash = blake3::Hasher::new();
        for part in parts {
            hash.update(&(part.len() as u64).to_le_bytes());
            hash.update(part);
        }
        hash.finalize()
    };
    digest.to_hex()[..32].to_owned()
}

/// The pipeline named `name`: loaded from the library by `load` if it is
/// there with the same description, else made by `create` and stored. A
/// name asked for by several threads is loaded or made by one at a time
/// (the others then find it in the library); different names overlap.
pub(crate) fn pipeline(
    name: &str,
    load: impl FnOnce(&ID3D12PipelineLibrary1, PCWSTR) -> windows::core::Result<ID3D12PipelineState>,
    create: impl FnOnce() -> windows::core::Result<ID3D12PipelineState>,
) -> windows::core::Result<ID3D12PipelineState> {
    let wide = HSTRING::from(name);
    #[cfg(feature = "diagnostics")]
    LAST.set(Some((name.to_owned(), true)));
    let Some(library) = library() else {
        MADE.fetch_add(1, Ordering::Relaxed);
        return create();
    };
    let _held = NAMES.hold(name);
    // Outside the lock: loads take up to seconds, and a save holds the lock
    // for as long as it serialises. The library's blob lives as long as it
    // does, which is never dropped.
    if let Ok(pso) = load(&library.loads, PCWSTR(wide.as_ptr())) {
        LOADED.fetch_add(1, Ordering::Relaxed);
        return Ok(pso);
    }
    // Compiled outside the lock: compiles take up to seconds.
    #[cfg(feature = "diagnostics")]
    LAST.set(Some((name.to_owned(), false)));
    let pso = create()?;
    MADE.fetch_add(1, Ordering::Relaxed);
    let mut l = library.state.lock().unwrap_or_else(|e| e.into_inner());
    // SAFETY: a live pipeline; a name already stored (a description that
    // differs) is refused, which leaves the library as it was.
    if unsafe { l.library.StorePipeline(PCWSTR(wide.as_ptr()), &pso) }.is_ok() {
        l.added += 1;
    }
    Ok(pso)
}

#[cfg(feature = "diagnostics")]
thread_local! {
    /// This thread's last pipeline: its name, and whether it was loaded
    /// (else made), for the pipeline log (diagnostics).
    pub(crate) static LAST: std::cell::Cell<Option<(String, bool)>> =
        const { std::cell::Cell::new(None) };
}

/// Writes the changed early list and adapter library independently once
/// `SAVE_EVERY` has passed (or `now`). The library saves only to the adapter
/// directory selected for this adapter, driver and translator.
pub(crate) fn save(now: bool) {
    static LAST: Mutex<Option<std::time::Instant>> = Mutex::new(None);
    const SAVE_EVERY: std::time::Duration = std::time::Duration::from_secs(20);
    let Some(root) = DIR.get().and_then(|d| d.parent()) else { return };
    // Serialize complete saves so shutdown and periodic saves cannot publish
    // a newer library and then replace it with an older snapshot.
    let mut last = if now {
        LAST.lock().unwrap_or_else(|e| e.into_inner())
    } else {
        match LAST.try_lock() {
            Ok(last) => last,
            Err(std::sync::TryLockError::Poisoned(e)) => e.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) => return,
        }
    };
    if !now && last.is_some_and(|t| t.elapsed() < SAVE_EVERY) {
        return;
    }
    *last = Some(std::time::Instant::now());

    if USED_CHANGED.swap(false, Ordering::Relaxed) {
        let used = used().as_ref().map(Used::list).unwrap_or_default();
        let written = std::fs::create_dir_all(root)
            .and_then(|()| write_atomically(&root.join(EARLY_FILE), used.join("\n").as_bytes()));
        if let Err(e) = written {
            USED_CHANGED.store(true, Ordering::Relaxed);
            tracing::warn!(error = %e, "early pipeline list not saved");
        }
    }

    let Some(dir) = library_dir() else { return };
    let Some(library) = library() else { return };
    if library.disk_configured {
        save_library(dir, library);
    }
}

fn save_library(dir: &std::path::Path, library: &Shared) {
    let (blob, prebuilt, added, prebuilt_changed) = {
        let mut l = library.state.lock().unwrap_or_else(|e| e.into_inner());
        if l.added == 0 && !l.prebuilt_changed {
            return;
        }
        // SAFETY: a plain size query on a live library.
        let size = unsafe { l.library.GetSerializedSize() };
        let Some(mut bytes) = zeroed(size) else {
            // Left for a later save: the pipelines stay counted as added.
            tracing::warn!(
                bytes = size,
                "no memory to serialise the host pipeline library; saved later"
            );
            return;
        };
        // SAFETY: a buffer of the size the library asks for.
        let bytes = unsafe { l.library.Serialize(&mut bytes) }.map(|()| bytes);
        match bytes {
            Ok(bytes) => {
                let added = std::mem::take(&mut l.added);
                let prebuilt_changed = std::mem::take(&mut l.prebuilt_changed);
                // Kept beside the bytes it holds for (also when a run adds
                // pipelines).
                (bytes, l.prebuilt.clone(), added, prebuilt_changed)
            }
            Err(e) => {
                tracing::warn!(error = %e, "host pipeline library not serialised");
                return;
            }
        }
    };
    let mut keys = prebuilt;
    keys.extend(library.initial_keys().iter().cloned());
    let index = (!keys.is_empty()).then(|| list(&tag(&blob), &keys));
    let written = prepare_dir(dir).and_then(|_leaf| write_cache(dir, &blob, index.as_deref()));
    if written.is_ok() {
        static CLEANED: OnceLock<()> = OnceLock::new();
        CLEANED.get_or_init(|| {
            let adapter = dir.parent().unwrap();
            dirs::prune_drivers(adapter.parent().unwrap(), adapter);
        });
    }
    if written.is_err() {
        let mut l = library.state.lock().unwrap_or_else(|e| e.into_inner());
        l.added += added;
        l.prebuilt_changed |= prebuilt_changed;
    }
    let (created, distinct) = created();
    let (shaders_cached, shaders_translated) = crate::shader_cache::counts();
    match written {
        Ok(()) => tracing::info!(
            bytes = blob.len(),
            made = MADE.load(Ordering::Relaxed),
            loaded = LOADED.load(Ordering::Relaxed),
            started_early = EARLY.load(Ordering::Relaxed),
            created,
            distinct,
            shaders_cached,
            shaders_translated,
            "host pipeline library saved"
        ),
        Err(e) => tracing::warn!(error = %e, "host pipeline library not saved"),
    }
}

fn write_cache(dir: &std::path::Path, blob: &[u8], prebuilt: Option<&str>) -> std::io::Result<()> {
    let length = (blob.len() as u64).to_le_bytes();
    let parts: &[&[u8]] = match prebuilt {
        Some(index) => &[blob, index.as_bytes(), &length, FOOTER],
        None => &[blob],
    };
    write_parts(&dir.join(LIBRARY_FILE), parts)
}

/// `n` zero bytes, or `None` when the host has no memory for them. The
/// library is about 1.7 GB on NVIDIA (each pipeline's machine code), and an
/// infallible allocation of that size aborted the whole process mid-run
/// ("memory allocation of 2650101862 bytes failed", RTX 4050 Laptop).
fn zeroed(n: usize) -> Option<Vec<u8>> {
    let mut v = Vec::new();
    v.try_reserve_exact(n).ok()?;
    v.resize(n, 0);
    Some(v)
}

/// Writes `bytes` to a temporary file beside `path`, then renames it over
/// `path`: a reader never sees a part. Serialised so concurrent worker and
/// shutdown saves do not race on the same staging file or target rename.
fn write_atomically(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    write_parts(path, &[bytes])
}

fn write_parts(path: &Path, parts: &[&[u8]]) -> io::Result<()> {
    use std::sync::atomic::AtomicU64;
    static WRITING: Mutex<()> = Mutex::new(());
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let _guard = WRITING.lock().unwrap_or_else(|e| e.into_inner());
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".tmp-{}-{n}", std::process::id()));
    let tmp = path.with_file_name(name);
    let mut file = File::create_new(&tmp)?;
    let write = parts.iter().try_for_each(|part| file.write_all(part));
    drop(file);
    let res = write.and_then(|()| fs::rename(&tmp, path));
    if res.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    res
}

/// Title pipelines created, and their distinct keys.
static CREATED: Mutex<(u64, Option<HashSet<String>>)> = Mutex::new((0, None));

/// The title created a pipeline with `key`.
pub(crate) fn note_created(key: &str) {
    let mut created = CREATED.lock().unwrap_or_else(|e| e.into_inner());
    created.0 += 1;
    created.1.get_or_insert_with(HashSet::new).insert(key.to_owned());
}

/// Title pipelines created so far, and how many distinct ones.
fn created() -> (u64, usize) {
    let created = CREATED.lock().unwrap_or_else(|e| e.into_inner());
    (created.0, created.1.as_ref().map_or(0, HashSet::len))
}

/// Title pipeline keys drawn or dispatched with: in earlier runs (the
/// list as loaded), and in this one.
#[derive(Default)]
struct Used {
    /// Each earlier key with its place in the list as loaded: the list is
    /// kept most recent run first, so a lower place is a more recent run.
    before: HashMap<String, usize>,
    now: HashSet<String>,
}

impl Used {
    /// The list as saved: one key a line, most recent run first (a list
    /// in no order, as earlier builds saved it, reads as well).
    fn read(listed: &str) -> Used {
        let mut before = HashMap::new();
        for (place, key) in listed.lines().filter(|l| !l.is_empty()).enumerate() {
            before.entry(key.to_owned()).or_insert(place);
        }
        Used { before, now: HashSet::new() }
    }

    /// The list to keep: this run's keys, then earlier runs' others, most
    /// recent first, at most `EARLY_MAX` (the oldest go once it is full).
    fn list(&self) -> Vec<String> {
        let mut earlier: Vec<_> = self.before.iter().filter(|(k, _)| !self.now.contains(*k)).collect();
        earlier.sort_unstable_by_key(|&(_, &place)| place);
        let earlier = earlier.into_iter().map(|(k, _)| k);
        self.now.iter().chain(earlier).take(EARLY_MAX).cloned().collect()
    }
}

static USED: Mutex<Option<Used>> = Mutex::new(None);
static USED_CHANGED: AtomicBool = AtomicBool::new(false);

static UNPUBLISHED_HINTS: OnceLock<String> = OnceLock::new();

fn migrate_early(root: &Path, old: &[PathBuf]) -> io::Result<()> {
    if old.is_empty() { return Ok(()); }
    let path = root.join(EARLY_FILE);
    let mut ordered = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e),
    };
    // Pin every ancestor of each validated legacy entry until its removal.
    // A junction or a concurrent directory rename must not redirect cleanup.
    let mut guards = Vec::new();
    for file in old {
        let leaf = file.parent().unwrap();
        let adapter = leaf.parent().unwrap();
        for dir in [adapter.parent().unwrap(), adapter, leaf] {
            guards.push(dirs::hold(dir, false)?);
        }
        if !dirs::regular(file) { return Err(io::Error::other("early list changed during migration")); }
        ordered.push('\n');
        ordered.push_str(&fs::read_to_string(file)?);
    }
    let merged = Used::read(&ordered).list().join("\n");
    if let Err(error) = write_atomically(&path, merged.as_bytes()) {
        let _ = UNPUBLISHED_HINTS.set(merged);
        return Err(error);
    }
    for file in old { fs::remove_file(file)?; }
    Ok(())
}

fn used() -> std::sync::MutexGuard<'static, Option<Used>> {
    let mut used = USED.lock().unwrap_or_else(|e| e.into_inner());
    static READ_DISK: OnceLock<()> = OnceLock::new();
    if DIR.get().is_some() && READ_DISK.get().is_none() {
        let listed = DIR.get().and_then(|d| d.parent())
            .and_then(|d| std::fs::read_to_string(d.join(EARLY_FILE)).ok())
            .unwrap_or_default();
        let before = Used::read(UNPUBLISHED_HINTS.get().unwrap_or(&listed)).before;
        tracing::info!(pipelines = before.len(), "title pipelines to start early");
        used.get_or_insert_with(Used::default).before = before;
        let _ = READ_DISK.set(());
    }
    used
}

/// Reads the early list now if not yet (a pre-build asks for no title
/// pipeline).
pub(crate) fn keep_early_list() {
    drop(used());
}

/// Whether the title pipeline with `key` was drawn with in an earlier run.
pub(crate) fn wanted_early(key: &str) -> bool {
    used().as_ref().is_some_and(|u| u.before.contains_key(key))
}

/// The title pipeline with `key` was drawn or dispatched with.
pub(crate) fn note_used(key: &str) {
    if used().get_or_insert_with(Used::default).now.insert(key.to_owned()) {
        USED_CHANGED.store(true, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn early_list_survives_driver_changes_and_unknown_drivers() {
        const PHASE: &str = "VICTORY_EARLY_TEST_PHASE";
        const ROOT: &str = "VICTORY_EARLY_TEST_ROOT";
        if let Ok(phase) = std::env::var(PHASE) {
            let root = PathBuf::from(std::env::var_os(ROOT).unwrap());
            let source = root.join(victory_shader::SOURCE_HASH);
            let desc = DXGI_ADAPTER_DESC1::default();
            let dir = phase.parse::<i64>().ok().and_then(|v| namespace(&desc, Some(v)))
                .map(|name| source.join(name).join("0123456789abcdef"));
            LIBRARY_DIR.set(dir).unwrap();
            use_pipeline_cache(root.clone());
            assert!(wanted_early("legacy-draw"), "{phase}");
            if phase != "1" { assert!(wanted_early("first-driver")); }
            if phase == "2" { assert!(wanted_early("unknown-driver")); }
            note_used(if phase == "unknown" { "unknown-driver" } else { "first-driver" });
            save(true);
            assert!(root.join(EARLY_FILE).is_file());
            return;
        }
        let root = tempfile::tempdir().unwrap();
        let legacy = root.path().join("0123456789abcdef")
            .join(namespace(&DXGI_ADAPTER_DESC1::default(), Some(7)).unwrap())
            .join("fedcba9876543210");
        fs::create_dir_all(&legacy).unwrap();
        fs::write(legacy.join(EARLY_FILE), "legacy-draw").unwrap();
        let source = root.path().join(victory_shader::SOURCE_HASH);
        let mut drivers = Vec::new();
        for version in 1..=4 {
            let driver = source.join(namespace(&DXGI_ADAPTER_DESC1::default(), Some(version)).unwrap());
            let leaf = driver.join("0123456789abcdef");
            fs::create_dir_all(&leaf).unwrap();
            fs::write(leaf.join(LIBRARY_FILE), b"synthetic driver blob").unwrap();
            drivers.push(driver);
        }
        for phase in ["1", "unknown", "2"] {
            assert!(std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "pipeline::library::tests::early_list_survives_driver_changes_and_unknown_drivers"])
                .env(ROOT, root.path()).env(PHASE, phase).status().unwrap().success());
            if phase == "1" {
                assert!(drivers[0].exists());
                assert_eq!(drivers.iter().filter(|dir| dir.exists()).count(), 2);
            }
        }
    }

    #[test]
    fn indexed_library_publishes_while_legacy_sidecar_is_busy() {
        use std::os::windows::fs::OpenOptionsExt;
        let root = tempfile::tempdir().unwrap();
        let held = fs::OpenOptions::new().create_new(true).write(true).share_mode(0)
            .open(root.path().join(PREBUILT_FILE)).unwrap();
        let keys = HashSet::from(["draw".to_owned()]);
        let blob = b"new driver bytes";
        let index = list(&tag(blob), &keys);
        write_cache(root.path(), blob, Some(&index)).unwrap();
        let image = fs::read(root.path().join(LIBRARY_FILE)).unwrap();
        let n = driver_length(&image);
        assert_eq!(&image[..n], blob);
        assert_eq!(listed(&image[n..image.len()-16], &tag(blob)), keys);
        drop(held);
    }

    /// A size the host cannot give is refused, not fatal; others are zeros.
    #[test]
    #[ignore]
    fn hardware_metadata_matches_the_dxgi_driver_namespace() {
        let host = crate::host::host().expect("test device");
        // SAFETY: plain identifier query on the current live device.
        let luid = unsafe { host.device.GetAdapterLuid() };
        let start = std::time::Instant::now();
        let fast = hardware_namespace(luid);
        let t1 = start.elapsed();
        let start = std::time::Instant::now();
        let reference = dxgi_namespace(luid).expect("DXGI driver identity");
        let t2 = start.elapsed();
        if reference.starts_with("hw-") {
            assert_eq!(fast.as_deref(), Some(reference.as_str()));
        } else {
            assert_eq!(adapter_namespace(&host.device).as_deref(), Some(reference.as_str()));
        }
        println!("IDENTITY kind={} kmt_us={:.3} dxgi_us={:.3}", &reference[..2],
                 t1.as_secs_f64()*1e6, t2.as_secs_f64()*1e6);
    }

    #[test]
    fn a_save_buffer_the_host_cannot_give_is_refused() {
        assert!(zeroed(usize::MAX / 2).is_none());
        assert_eq!(zeroed(100).as_deref(), Some(&[0u8; 100][..]));
    }

    #[test]
    fn library_names_preserve_the_length_framing_at_the_buffer_limit() {
        // One compute stage plus four u64 lengths and the seven-byte kind.
        const OVERHEAD: usize = 4 * 8 + 7;
        for size in [8191, 8192, 8193] {
            let shader = vec![0x5a; size - OVERHEAD];
            let mut expected = blake3::Hasher::new();
            for part in [b"compute".as_slice(), &[], shader.as_slice(), &[]] {
                expected.update(&(part.len() as u64).to_le_bytes());
                expected.update(part);
            }
            assert_eq!(name("compute", &[], &[&shader], ""), expected.finalize().to_hex()[..32]);
        }
    }

    #[test]
    fn names_change_with_what_goes_into_the_pipeline() {
        let a = name("graphics", b"root", &[b"vs", b"ps"], "state");
        // A name from the previous writer must still load from disk.
        assert_eq!(a, "9402d2bc73e94e1548e2a550a55f878f");
        assert_ne!(a, name("graphics", b"root", &[b"vs", b"ps2"], "state"));
        assert_ne!(a, name("graphics", b"root", &[b"vs", b"ps"], "state2"));
        assert_ne!(a, name("graphics", b"root2", &[b"vs", b"ps"], "state"));
        assert_ne!(a, name("mesh", b"root", &[b"vs", b"ps"], "state"));
        // Parts do not run into each other.
        assert_ne!(a, name("graphics", b"root", &[b"vsp", b"s"], "state"));
    }

    /// This run's keys stay in the list; earlier runs' fill it up to the cap.
    #[test]
    fn the_early_list_keeps_this_run_first() {
        let keys = |r: std::ops::Range<usize>| r.map(|i| format!("{i:032x}")).collect::<HashSet<_>>();
        let before = keys(0..EARLY_MAX).into_iter().enumerate().map(|(i, k)| (k, i)).collect();
        let used = Used { before, now: keys(EARLY_MAX - 10..EARLY_MAX + 10) };
        let list = used.list();
        assert_eq!(list.len(), EARLY_MAX);
        assert!(used.now.iter().all(|k| list.contains(k)));
        assert_eq!(list.iter().collect::<HashSet<_>>().len(), EARLY_MAX, "no key twice");
    }

    /// A full list makes room by the oldest runs' keys, and keeps that
    /// order across saves.
    #[test]
    fn a_full_early_list_drops_the_oldest_runs_keys() {
        let key = |i: usize| format!("{i:032x}");
        // As saved: the last run's keys, then the run before's.
        let saved: Vec<String> = (0..EARLY_MAX).map(key).collect();
        let mut used = Used::read(&saved.join("\n"));
        used.now = (EARLY_MAX..EARLY_MAX + 10).map(key).collect();
        used.now.insert(key(EARLY_MAX - 1)); // the oldest key, used again
        let list = used.list();
        assert_eq!(list.len(), EARLY_MAX);
        assert!(used.now.iter().all(|k| list.contains(k)));
        // The ten that go are the oldest ones not used again.
        let dropped: HashSet<String> = (EARLY_MAX - 11..EARLY_MAX - 1).map(key).collect();
        assert!(list.iter().all(|k| !dropped.contains(k)));
        // Read back, this run's keys come first, then the earlier ones in order.
        let again = Used::read(&list.join("\n"));
        assert!(used.now.iter().all(|k| again.before[k] < used.now.len()));
        assert!(again.before[&key(0)] < again.before[&key(1)]);
    }

    /// The pre-build's list holds only for the library bytes it was
    /// saved with.
    #[test]
    fn the_prebuilt_list_holds_for_its_library_only() {
        let keys: HashSet<String> = ["a".repeat(32), "b".repeat(32)].into();
        let saved = list(&tag(b"library"), &keys);
        assert_eq!(listed(saved.as_bytes(), &tag(b"library")), keys);
        assert!(listed(saved.as_bytes(), &tag(b"library2")).is_empty());
        assert!(listed(b"", &tag(b"library")).is_empty());
    }

    #[test]
    fn different_names_are_held_at_the_same_time() {
        static NAMES: Names = Names::new();
        // Each thread holds its name until every thread holds its own.
        let all = std::sync::Barrier::new(8);
        std::thread::scope(|s| {
            for i in 0..8 {
                let all = &all;
                s.spawn(move || {
                    let name = format!("pipeline {i}");
                    let _held = NAMES.hold(&name);
                    all.wait();
                });
            }
        });
        assert!(NAMES.held.lock().unwrap().as_ref().unwrap().is_empty());
    }

    #[test]
    fn one_name_is_held_by_one_thread_at_a_time() {
        static NAMES: Names = Names::new();
        static INSIDE: AtomicU64 = AtomicU64::new(0);
        std::thread::scope(|s| {
            for _ in 0..8 {
                s.spawn(|| {
                    for _ in 0..500 {
                        let _held = NAMES.hold("pipeline");
                        assert_eq!(INSIDE.fetch_add(1, Ordering::SeqCst), 0);
                        std::hint::spin_loop();
                        INSIDE.fetch_sub(1, Ordering::SeqCst);
                    }
                });
            }
        });
        // A holder that panics lets go of it.
        let panicked = std::thread::spawn(|| {
            let _held = NAMES.hold("pipeline");
            panic!("on purpose");
        });
        assert!(panicked.join().is_err());
        drop(NAMES.hold("pipeline"));
    }

    /// A library's pipelines load on many threads at once, each with its
    /// own description (a load with another's is refused), while others are
    /// stored and the library is serialised.
    #[test]
    fn pipelines_load_on_many_threads_at_once() {
        use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
        use windows::core::s;
        const N: usize = 48;
        let Some(device) = crate::host::host().and_then(|h| h.device.cast::<ID3D12Device1>().ok())
        else {
            return; // no host GPU here
        };
        let shader = |i: usize| {
            let source = format!(
                "#define RS \"UAV(u0)\"\nRWByteAddressBuffer b : register(u0);\n\
                 [RootSignature(RS)] [numthreads(1, 1, 1)] void main() {{ b.Store(0, b.Load(4) * {i} + {i}); }}"
            );
            let mut blob = None;
            // SAFETY: the source outlives the call; the blob is read within it.
            unsafe {
                D3DCompile(
                    source.as_ptr().cast(),
                    source.len(),
                    None,
                    None,
                    None,
                    s!("main"),
                    s!("cs_5_1"),
                    0,
                    0,
                    &mut blob,
                    None,
                )
                .ok()?;
                let blob = blob?;
                Some(
                    std::slice::from_raw_parts(
                        blob.GetBufferPointer().cast::<u8>(),
                        blob.GetBufferSize(),
                    )
                    .to_vec(),
                )
            }
        };
        let Some(codes) = (0..2 * N).map(shader).collect::<Option<Vec<_>>>() else {
            return; // no shader compiler here
        };
        // SAFETY: the code holds its root signature.
        let root: ID3D12RootSignature =
            unsafe { device.CreateRootSignature(0, &codes[0]) }.unwrap();
        struct Shared(ID3D12Device1, ID3D12RootSignature, ID3D12PipelineLibrary1);
        // SAFETY: D3D12 objects are free-threaded.
        unsafe impl Sync for Shared {}
        impl Shared {
            fn desc(&self, code: &[u8]) -> D3D12_COMPUTE_PIPELINE_STATE_DESC {
                D3D12_COMPUTE_PIPELINE_STATE_DESC {
                    // SAFETY: a borrowed pointer the description never releases.
                    pRootSignature: unsafe { std::mem::transmute_copy(&self.1) },
                    CS: D3D12_SHADER_BYTECODE {
                        pShaderBytecode: code.as_ptr().cast(),
                        BytecodeLength: code.len(),
                    },
                    ..Default::default()
                }
            }
            fn store(&self, i: usize, code: &[u8]) {
                let name = HSTRING::from(format!("p{i}"));
                // SAFETY: the description points at live data.
                unsafe {
                    let pso: ID3D12PipelineState =
                        self.0.CreateComputePipelineState(&self.desc(code)).unwrap();
                    self.2.StorePipeline(&name, &pso).unwrap();
                }
            }
            fn load(&self, i: usize, code: &[u8]) -> windows::core::Result<ID3D12PipelineState> {
                let name = HSTRING::from(format!("p{i}"));
                // SAFETY: as above.
                unsafe { self.2.LoadComputePipeline(&name, &self.desc(code)) }
            }
            fn serialized(&self) -> Vec<u8> {
                // SAFETY: a buffer of the size asked for; nothing is stored
                // meanwhile.
                unsafe {
                    let mut blob = vec![0; self.2.GetSerializedSize()];
                    self.2.Serialize(&mut blob).unwrap();
                    blob
                }
            }
        }
        let first = open(&device, Vec::new(), false).expect("an empty library");
        let made = Shared(device.clone(), root.clone(), first.loads.clone());
        (0..N).for_each(|i| made.store(i, &codes[i]));
        let again = open(&device, made.serialized(), false).expect("the library again");
        let shared = Shared(device, root, again.loads.clone());
        let stores = Mutex::new(());
        std::thread::scope(|s| {
            for t in 0..8 {
                let (shared, codes, stores) = (&shared, &codes, &stores);
                s.spawn(move || {
                    for i in (t..N).step_by(8) {
                        shared.load(i, &codes[i]).expect("a stored pipeline");
                        assert!(
                            shared.load(i, &codes[(i + 1) % N]).is_err(),
                            "another description"
                        );
                        assert!(shared.load(N + i, &codes[N + i]).is_err(), "not stored yet");
                        let _one = stores.lock().unwrap();
                        shared.store(N + i, &codes[N + i]);
                        shared.serialized();
                    }
                });
            }
        });
        for (i, code) in codes.iter().enumerate() {
            shared.load(i, code).expect("every pipeline");
        }
    }

    /// A stored pipeline loads while a save holds the library's lock (one
    /// serialises the whole library, for seconds in the world).
    #[test]
    fn a_pipeline_loads_while_a_save_holds_the_library() {
        use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
        use windows::core::s;
        let (Some(device), Some(library)) = (
            crate::host::host().and_then(|h| h.device.cast::<ID3D12Device1>().ok()),
            library(),
        ) else {
            return; // no host GPU here
        };
        // No resources, and the root signature made apart: a compiler need
        // not keep one the source names (Wine's does not).
        let source = "[numthreads(1, 1, 1)] void main() {}";
        let mut blob = None;
        // SAFETY: the source outlives the call; the blob is read within it.
        let code = unsafe {
            if D3DCompile(
                source.as_ptr().cast(),
                source.len(),
                None,
                None,
                None,
                s!("main"),
                s!("cs_5_1"),
                0,
                0,
                &mut blob,
                None,
            )
            .is_err()
            {
                return; // no shader compiler here
            }
            let blob = blob.unwrap();
            std::slice::from_raw_parts(blob.GetBufferPointer().cast::<u8>(), blob.GetBufferSize())
                .to_vec()
        };
        // SAFETY: an empty description; the blob is read while it lives.
        let root: ID3D12RootSignature = unsafe {
            let mut blob = None;
            D3D12SerializeRootSignature(
                &D3D12_ROOT_SIGNATURE_DESC::default(),
                D3D_ROOT_SIGNATURE_VERSION_1,
                &mut blob,
                None,
            )
            .unwrap();
            let blob = blob.unwrap();
            let bytes = std::slice::from_raw_parts(
                blob.GetBufferPointer().cast::<u8>(),
                blob.GetBufferSize(),
            );
            device.CreateRootSignature(0, bytes).unwrap()
        };
        struct Made(ID3D12Device1, ID3D12RootSignature, Vec<u8>);
        // SAFETY: D3D12 objects are free-threaded.
        unsafe impl Sync for Made {}
        impl Made {
            fn get(&self, made: bool) -> windows::core::Result<ID3D12PipelineState> {
                let desc = D3D12_COMPUTE_PIPELINE_STATE_DESC {
                    // SAFETY: a borrowed pointer the description never releases.
                    pRootSignature: unsafe { std::mem::transmute_copy(&self.1) },
                    CS: D3D12_SHADER_BYTECODE {
                        pShaderBytecode: self.2.as_ptr().cast(),
                        BytecodeLength: self.2.len(),
                    },
                    ..Default::default()
                };
                pipeline(
                    "test: a pipeline loads while a save holds the library",
                    // SAFETY: the description points at live data.
                    |library, name| unsafe { library.LoadComputePipeline(name, &desc) },
                    || match made {
                        // SAFETY: as above.
                        true => unsafe { self.0.CreateComputePipelineState(&desc) },
                        false => panic!("made again, not loaded"),
                    },
                )
            }
        }
        let made = Made(device, root, code);
        made.get(true).expect("made and stored");
        let saving = library.state.lock().unwrap_or_else(|e| e.into_inner());
        let (done, loaded) = std::sync::mpsc::channel();
        std::thread::scope(|s| {
            let made = &made;
            s.spawn(move || done.send(made.get(false).is_ok()));
            let loaded = loaded.recv_timeout(std::time::Duration::from_secs(30));
            drop(saving);
            assert_eq!(loaded, Ok(true), "the load waited for the save");
        });
    }

    #[test]
    fn libraries_are_separate_across_translator_driver_and_adapter() {
        let hardware = DXGI_ADAPTER_DESC1 { DeviceId: 0x2b85, ..Default::default() };
        let base = namespace(&hardware, Some(1));
        assert_ne!(base, namespace(&hardware, Some(2)));
        assert!(base.is_some());
        assert!(namespace(&hardware, None).is_none());
        assert_ne!(translator_dir("pso".into(), &[2; 32]), translator_dir("pso".into(), &[3; 32]));
        let mut other = hardware;
        other.DeviceId += 1;
        assert_ne!(base, namespace(&other, Some(1)));
        let mut warp = hardware;
        warp.Flags |= DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32;
        assert_ne!(base, namespace(&warp, Some(1)));
    }

    /// Run in separate processes so an in-memory library cannot satisfy
    /// a disk hit. The third process gets the same pipeline name and a
    /// deliberately rejected blob and must compile and dispatch it again.
    #[cfg(feature = "diagnostics")]
    #[test]
    #[ignore = "requires an SM 6.6 adapter with a persistent pipeline library"]
    fn reopened_library_loads_then_recovers_a_rejected_blob() {
        const PHASE: &str = "VICTORY_TEST_PSO_PHASE";
        const ROOT: &str = "VICTORY_TEST_PSO_ROOT";
        if let Ok(phase) = std::env::var(PHASE) {
            let root = PathBuf::from(std::env::var_os(ROOT).unwrap());
            let host = crate::host::host().expect("D3D12 device");
            if phase == "late" {
                // A run without a configured disk cache has an in-memory PSO.
                note_used("before-configuration");
                let pixels = super::super::placed_uav_tests::run_store(host, |_| {}).unwrap();
                assert_eq!(pixels, [[1.0, 0.5, -0.25, 2.0], [0.0; 4]]);
                use_pipeline_cache(root.clone());
                let dir = library_dir().unwrap();
                let blob = std::fs::read(dir.join(LIBRARY_FILE)).unwrap();
                assert!(wanted_early("old-draw"));
                note_prebuilt("late-marker".to_owned());
                save(true);
                assert_eq!(std::fs::read(dir.join(LIBRARY_FILE)).unwrap(), blob);
                let early = std::fs::read_to_string(root.join(EARLY_FILE)).unwrap();
                assert!(early.lines().any(|k| k == "old-draw"));
                assert!(early.lines().any(|k| k == "before-configuration"));
                return;
            }
            use_pipeline_cache(root.clone());
            let dir = library_dir().expect("cacheable driver version");
            let blob = dir.join(LIBRARY_FILE);
            if phase == "gameplay-save" {
                let pixels = super::super::placed_uav_tests::run_store(host, |_| {}).unwrap();
                assert_eq!(pixels, [[1.0, 0.5, -0.25, 2.0], [0.0; 4]]);
                let shared = library().unwrap();
                assert!(shared.initial_keys.get().is_none());
                let (_root, pso) = crate::draw::tests::compile_cs(
                    r#"[RootSignature("UAV(u0)")] [numthreads(1,1,1)]
                        void main() {}"#,
                );
                pipeline("gameplay-save-test", |_, _| Err(windows::core::Error::empty()), || Ok(pso)).unwrap();
                assert!(shared.initial_keys.get().is_none());
                save(true);
                assert!(shared.initial_keys().contains("old-marker"));
                return;
            }
            if phase == "hit" || phase == "legacy" {
                assert!(prebuilt("old-marker"));
            }
            if phase == "damaged" {
                assert!(blob.is_file());
                std::fs::write(&blob, b"invalid host pipeline library").unwrap();
            }
            let pixels = super::super::placed_uav_tests::run_store(host, |_| {}).unwrap();
            assert_eq!(pixels, [[1.0, 0.5, -0.25, 2.0], [0.0; 4]]);
            let hit = matches!(phase.as_str(), "hit" | "legacy" | "repaired");
            assert_eq!(LOADED.load(Ordering::Relaxed), u64::from(hit));
            assert_eq!(MADE.load(Ordering::Relaxed), u64::from(!hit));
            if phase == "empty" {
                note_used("old-draw");
                note_prebuilt("old-marker".to_owned());
            }
            save(true);
            assert!(std::fs::metadata(&blob).unwrap().len() > 32);
            if phase == "hit" {
                fs::rename(blob, root.join(LIBRARY_FILE)).unwrap();
            } else if phase == "legacy" {
                assert!(!root.join(LIBRARY_FILE).exists());
            }
            return;
        }

        let root = tempfile::tempdir().unwrap();
        for phase in ["empty", "late", "gameplay-save", "hit", "legacy", "damaged", "repaired"] {
            let child = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "pipeline::library::tests::reopened_library_loads_then_recovers_a_rejected_blob", "--ignored", "--nocapture"])
                .env(PHASE, phase)
                .env(ROOT, root.path())
                .status()
                .unwrap();
            assert!(child.success(), "{phase}: {child}");
        }
    }

    #[test]
    fn a_rejected_blob_gives_an_empty_library() {
        let Some(device) = crate::host::host().and_then(|h| h.device.cast::<ID3D12Device1>().ok()) else {
            return; // no host GPU here
        };
        let library = open(&device, b"not a pipeline library".repeat(16), false).expect("an empty library");
        // SAFETY: a plain query on a live library.
        assert!(unsafe { library.loads.GetSerializedSize() } > 0);
        assert!(library.image.is_empty());
    }

    #[test]
    fn concurrent_write_atomically_never_fails_or_truncates() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("host-pipelines.bin");
        let threads: Vec<_> = (0..4)
            .map(|t| {
                let target = target.clone();
                std::thread::spawn(move || {
                    let payload = vec![b'a' + t as u8; 4096];
                    for _ in 0..30 {
                        write_atomically(&target, &payload).unwrap();
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        let final_bytes = std::fs::read(&target).unwrap();
        assert_eq!(final_bytes.len(), 4096);
        assert!(final_bytes.iter().all(|&b| b == final_bytes[0]));
    }
}
