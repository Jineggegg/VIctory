//! Translated shaders kept on disk (decision 0003): under the configured
//! cache folder, keyed by the container's content and by what the
//! translation depends on ([`build`]), so a changed translator never serves
//! an old translation, while rebuilding the rest of VIctory keeps them.
//! Each translator's entries are in a folder of their own, named by the
//! start of that key: older translators' folders can be deleted whole.
//! Entries are written beside their final path and atomically renamed.
//! Reads verify both the container key and the payload checksum. Refusals
//! are cached with their reason so they are not retried on every run.
//!
//! `victory translate --cache` fills the same cache ahead of a run, through
//! [`host_shader`].
//!
//! The entry is our own encoding of [`HostShader`]; this file's text is part
//! of the key, so changing the encoding invalidates every entry.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

use std::io::{self, Write};
use victory_shader::emit::{HostShader, StaticSampler, UserData};
use crate::pipeline::Translated;
use victory_shader::{HwStage, ResourceType, RootType, UsedResource};

static CACHE: OnceLock<Cache> = OnceLock::new();
static HITS: AtomicU64 = AtomicU64::new(0);
static MISSES: AtomicU64 = AtomicU64::new(0);

/// Keeps translated shaders in a folder of `dir` from now on: the folder
/// of this translator, `None` if it cannot be opened.
pub fn use_shader_cache(dir: PathBuf) -> Option<PathBuf> {
    let dir = dir.join(&blake3::Hash::from(*build()).to_hex()[..16]);
    match Cache::open(&dir) {
        Ok(cache) => {
            tracing::info!(dir = %dir.display(), "shader cache");
            Some(CACHE.get_or_init(|| cache).dir.clone())
        }
        Err(e) => {
            tracing::warn!(error = %e, "no shader cache");
            None
        }
    }
}

/// Shaders found in the cache and translated since the start.
pub fn counts() -> (u64, u64) {
    (HITS.load(Ordering::Relaxed), MISSES.load(Ordering::Relaxed))
}

/// What a translation depends on besides its container: the translator
/// (`victory_shader::SOURCE_HASH`: its sources, manifest, the lock file and
/// the compiler), the entry's encoding (this file's text) and the
/// translation settings (the loop limit, which a diagnostics build can
/// change at run time, and whether view heaps are paged, which the host
/// decides: [`options`]).
pub(crate) fn build() -> &'static [u8; 32] {
    static BUILD: OnceLock<[u8; 32]> = OnceLock::new();
    BUILD.get_or_init(|| {
        let limit = victory_shader::emit::loop_limit();
        if limit != victory_shader::emit::DEFAULT_LOOP_LIMIT {
            tracing::warn!(
                limit,
                "VICTORY_SHADER_LOOP_LIMIT: translated loops stop sooner"
            );
        }
        key(
            victory_shader::SOURCE_HASH,
            ENCODING,
            limit,
            options().view_pages,
        )
    })
}

/// How shaders are translated for this host: writes through views mark
/// their pages (copy-backs compare only those,
/// `crate::shadow::copy_back_marks`), and view heap indices go through the
/// page table where the host's view heaps are paged
/// (`crate::descriptor::host::view_pages`).
pub fn options() -> victory_shader::emit::Options {
    victory_shader::emit::Options {
        view_marks: true,
        view_pages: crate::descriptor::view_pages(),
    }
}

/// This file, whose `encode` and `decode` define the entries.
const ENCODING: &[u8] = include_bytes!("shader_cache.rs");

fn key(translator: &str, encoding: &[u8], limit: u32, view_pages: bool) -> [u8; 32] {
    let mut h = blake3::Hasher::new();
    h.update(translator.as_bytes());
    h.update(blake3::hash(encoding).as_bytes());
    h.update(&limit.to_le_bytes());
    h.update(&[u8::from(view_pages)]);
    *h.finalize().as_bytes()
}

/// The translation of a console shader container, from the cache when
/// there is one, and whether it came from there.
pub fn host_shader(container: &[u8]) -> (Result<HostShader, String>, bool) {
    let (result, hit) = translated(container);
    (result.map(|t| t.host), hit)
}

pub(crate) fn translated(container: &[u8]) -> (Result<Translated, String>, bool) {
    let options = options();
    let translate = |c: &[u8]| {
        victory_shader::emit::translate_with_resources(c, options)
            .map(|(host, used)| Translated { host, used })
            .map_err(|e| e.to_string())
    };
    match CACHE.get() {
        Some(cache) => cached(cache, build(), container, translate),
        None => (translate(container), false),
    }
}

/// A single directory per translator. A hit needs one file open, and a
/// miss can stage its bytes in the already existing destination directory.
struct Cache {
    dir: PathBuf,
}

impl Cache {
    fn open(dir: &Path) -> anyhow::Result<Self> {
        victory_core::ensure_outside_repo(dir)?;
        std::fs::create_dir_all(dir)?;
        Ok(Self {
            dir: dir.to_owned(),
        })
    }

    fn read(path: &Path, key: &[u8; 32]) -> Option<Result<Translated, String>> {
        let entry = std::fs::read(path).ok()?;
        let (bytes, digest) = entry.split_at(entry.len().checked_sub(32)?);
        if blake3::keyed_hash(key, bytes).as_bytes() != digest {
            return None;
        }
        decode(bytes)
    }

    fn stage(&self) -> io::Result<tempfile::NamedTempFile> {
        // Create an ordinary file: persisting a Windows temporary-attribute
        // file would also change its attributes on every publication.
        tempfile::Builder::new().make_in(&self.dir, |p| {
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(p)
        })
    }

    fn write(&self, path: &Path, key: &[u8; 32], mut bytes: Vec<u8>) -> io::Result<()> {
        let digest = blake3::keyed_hash(key, &bytes);
        bytes.extend_from_slice(digest.as_bytes());
        let mut stage = self.stage()?;
        stage.write_all(&bytes)?;
        std::fs::rename(stage.path(), path)?;
        stage.disable_cleanup(true);
        Ok(())
    }
}

fn cached(
    cache: &Cache,
    build: &[u8; 32],
    container: &[u8],
    translate: impl Fn(&[u8]) -> Result<Translated, String>,
) -> (Result<Translated, String>, bool) {
    let key = blake3::keyed_hash(build, container);
    let path = cache.dir.join(key.to_hex().as_str());
    if let Some(shader) = Cache::read(&path, key.as_bytes()) {
        HITS.fetch_add(1, Ordering::Relaxed);
        return (shader, true);
    }
    MISSES.fetch_add(1, Ordering::Relaxed);
    let shader = translate(container);
    if let Err(e) = cache.write(&path, key.as_bytes(), encode(&shader)) {
        tracing::debug!(error = %e, "shader cache entry not written");
    }
    (shader, false)
}

const MAGIC: &[u8; 4] = b"VSH1";
/// A container that does not translate: the reason follows.
const REFUSED: &[u8; 4] = b"VSHE";
const STAGES: [HwStage; 8] = [
    HwStage::Ls,
    HwStage::Hs,
    HwStage::Es,
    HwStage::Gs,
    HwStage::Vs,
    HwStage::Ps,
    HwStage::Ms,
    HwStage::Cs,
];

fn root_code(r: RootType) -> u32 {
    match r {
        RootType::Constants => 0,
        RootType::Descriptor => 1,
        RootType::Table => 2,
        RootType::ImmediateSampler => 3,
        RootType::ResourceHeap => 4,
        RootType::SamplerHeap => 5,
        RootType::Other(x) => 6 + x,
    }
}

fn root_type(code: u32) -> RootType {
    match code {
        0 => RootType::Constants,
        1 => RootType::Descriptor,
        2 => RootType::Table,
        3 => RootType::ImmediateSampler,
        4 => RootType::ResourceHeap,
        5 => RootType::SamplerHeap,
        x => RootType::Other(x - 6),
    }
}

fn encode_resource_type(out: &mut Vec<u8>, kind: ResourceType) {
    match kind {
        ResourceType::Srv => out.push(0),
        ResourceType::Sampler => out.push(1),
        ResourceType::Cbv => out.push(2),
        ResourceType::Uav => out.push(3),
        ResourceType::Other(x) => {
            out.push(4);
            out.push(x);
        }
    }
}

fn encode_root_type(out: &mut Vec<u8>, root: RootType) {
    match root {
        RootType::Constants => out.push(0),
        RootType::Descriptor => out.push(1),
        RootType::Table => out.push(2),
        RootType::ImmediateSampler => out.push(3),
        RootType::ResourceHeap => out.push(4),
        RootType::SamplerHeap => out.push(5),
        RootType::Other(x) => {
            out.push(6);
            out.extend_from_slice(&x.to_le_bytes());
        }
    }
}

/// Little-endian words and length-prefixed lists; a refusal is its
/// reason's text.
fn encode(translation: &Result<Translated, String>) -> Vec<u8> {
    let s = match translation {
        Ok(s) => s,
        Err(reason) => return [REFUSED.as_slice(), reason.as_bytes()].concat(),
    };
    // Every field by name: a field added to `HostShader` must be added here
    // (and to `decode`).
    let HostShader {
        stage,
        dxil,
        user_data,
        samplers,
        static_samplers,
        root_cbvs,
        root_srvs,
        root_uavs,
        memory,
        memory_writes,
        view_addresses,
        heap,
        heap_writes,
        view_marks,
    } = &s.host;
    let mut out = MAGIC.to_vec();
    let word = |out: &mut Vec<u8>, v: u32| out.extend(v.to_le_bytes());
    let stage = STAGES.iter().position(|x| x == stage).unwrap_or(0) as u32;
    word(&mut out, stage);
    word(&mut out, dxil.len() as u32);
    out.extend(dxil);
    word(&mut out, user_data.len() as u32);
    for u in user_data {
        for v in [u.index, u32::from(u.slot), root_code(u.root), u.dword] {
            word(&mut out, v);
        }
    }
    word(&mut out, samplers.len() as u32);
    for sampler in samplers {
        for &v in sampler {
            word(&mut out, v);
        }
    }
    word(&mut out, static_samplers.len() as u32);
    for t in static_samplers {
        let [u, v, w] = t.address;
        let words = [
            t.filter,
            u,
            v,
            w,
            t.mip_lod_bias.to_bits(),
            t.max_anisotropy,
            t.comparison,
            t.border,
            t.min_lod.to_bits(),
            t.max_lod.to_bits(),
        ];
        for v in words {
            word(&mut out, v);
        }
    }
    for slots in [root_cbvs, root_srvs, root_uavs] {
        word(&mut out, slots.len() as u32);
        out.extend(slots);
    }
    word(
        &mut out,
        u32::from(*memory)
            | u32::from(*memory_writes) << 1
            | u32::from(*view_addresses) << 2
            | u32::from(*heap) << 3
            | u32::from(*heap_writes) << 4
            | u32::from(*view_marks) << 5,
    );
    word(&mut out, s.used.len() as u32);
    for res in &s.used {
        encode_resource_type(&mut out, res.kind);
        encode_root_type(&mut out, res.root);
        out.extend_from_slice(&res.table_offset_dwords.to_le_bytes());
        out.extend_from_slice(&res.lower_bound.to_le_bytes());
        out.extend_from_slice(&res.stride.to_le_bytes());
        out.push(res.dwords);
        out.push(res.user_data);
        out.push(res.slot);
    }
    out
}

fn decode(bytes: &[u8]) -> Option<Result<Translated, String>> {
    struct Reader<'a>(&'a [u8]);
    impl<'a> Reader<'a> {
        fn take(&mut self, n: usize) -> Option<&'a [u8]> {
            let (head, rest) = self.0.split_at_checked(n)?;
            self.0 = rest;
            Some(head)
        }
        fn word(&mut self) -> Option<u32> {
            Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
        }
        fn u8(&mut self) -> Option<u8> { Some(self.take(1)?[0]) }
        fn resource_type(&mut self) -> Option<ResourceType> {
            match self.u8()? {
                0 => Some(ResourceType::Srv),
                1 => Some(ResourceType::Sampler),
                2 => Some(ResourceType::Cbv),
                3 => Some(ResourceType::Uav),
                4 => Some(ResourceType::Other(self.u8()?)),
                _ => None,
            }
        }

        fn root_type(&mut self) -> Option<RootType> {
            match self.u8()? {
                0 => Some(RootType::Constants),
                1 => Some(RootType::Descriptor),
                2 => Some(RootType::Table),
                3 => Some(RootType::ImmediateSampler),
                4 => Some(RootType::ResourceHeap),
                5 => Some(RootType::SamplerHeap),
                6 => Some(RootType::Other(self.word()?)),
                _ => None,
            }
        }
        fn list(&mut self) -> Option<Vec<u8>> {
            let n = self.word()? as usize;
            Some(self.take(n)?.to_vec())
        }
    }
    if let Some(reason) = bytes.strip_prefix(REFUSED) {
        return Some(Err(String::from_utf8(reason.to_vec()).ok()?));
    }
    let mut r = Reader(bytes);
    if r.take(4)? != MAGIC {
        return None;
    }
    let stage = *STAGES.get(r.word()? as usize)?;
    let dxil = r.list()?;
    let n = r.word()? as usize;
    let mut user_data = Vec::with_capacity(n.min(256));
    for _ in 0..n {
        user_data.push(UserData {
            index: r.word()?,
            slot: u8::try_from(r.word()?).ok()?,
            root: root_type(r.word()?),
            dword: r.word()?,
        });
    }
    let n = r.word()? as usize;
    let mut samplers = Vec::with_capacity(n.min(256));
    for _ in 0..n {
        samplers.push([r.word()?, r.word()?, r.word()?, r.word()?]);
    }
    let n = r.word()? as usize;
    let mut static_samplers = Vec::with_capacity(n.min(256));
    for _ in 0..n {
        static_samplers.push(StaticSampler {
            filter: r.word()?,
            address: [r.word()?, r.word()?, r.word()?],
            mip_lod_bias: f32::from_bits(r.word()?),
            max_anisotropy: r.word()?,
            comparison: r.word()?,
            border: r.word()?,
            min_lod: f32::from_bits(r.word()?),
            max_lod: f32::from_bits(r.word()?),
        });
    }
    let root_cbvs = r.list()?;
    let root_srvs = r.list()?;
    let root_uavs = r.list()?;
    let memory = r.word()?;
    let count = r.word()? as usize;
    let mut used = Vec::with_capacity(count.min(256));
    for _ in 0..count {
        used.push(UsedResource {
            kind: r.resource_type()?,
            root: r.root_type()?,
            table_offset_dwords: r.word()?,
            lower_bound: r.word()?,
            stride: r.word()?,
            dwords: r.u8()?,
            user_data: r.u8()?,
            slot: r.u8()?,
        });
    }
    let host = HostShader {
        stage,
        dxil,
        user_data,
        samplers,
        static_samplers,
        root_cbvs,
        root_srvs,
        root_uavs,
        memory: memory & 1 != 0,
        memory_writes: memory & 2 != 0,
        view_addresses: memory & 4 != 0,
        heap: memory & 8 != 0,
        heap_writes: memory & 16 != 0,
        view_marks: memory & 32 != 0,
    };
    r.0.is_empty().then_some(Ok(Translated { host, used }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shader() -> Translated {
        let host = HostShader {
            stage: HwStage::Ps,
            dxil: b"DXBC and the rest".to_vec(),
            user_data: vec![UserData {
                index: 8,
                slot: 1,
                root: RootType::Table,
                dword: 0,
            }],
            samplers: vec![[0x400, 0xc0_0000, 0x18f0_0000, 0]],
            static_samplers: vec![StaticSampler::from_words([
                0x400,
                0xc0_0000,
                0x18f0_0000,
                0,
            ])],
            root_cbvs: vec![2],
            root_srvs: vec![0, 3],
            root_uavs: vec![4],
            memory: true,
            memory_writes: true,
            view_addresses: true,
            heap: true,
            heap_writes: true,
            view_marks: true,
        };
        let used = vec![UsedResource {
            kind: ResourceType::Other(255),
            root: RootType::Other(u32::MAX),
            table_offset_dwords: 7,
            lower_bound: 2,
            stride: 32,
            dwords: 8,
            user_data: 3,
            slot: 1,
        }];
        Translated { host, used }
    }

    #[test]
    fn translations_come_back_from_the_cache() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::open(dir.path()).unwrap();
        let build = [7u8; 32];
        let container = b"a console container".as_slice();
        let runs = AtomicU64::new(0);
        let translate = |_: &[u8]| {
            runs.fetch_add(1, Ordering::Relaxed);
            Ok(shader())
        };
        assert_eq!(
            cached(&cache, &build, container, translate),
            (Ok(shader()), false)
        );
        assert_eq!(
            cached(&cache, &build, container, translate),
            (Ok(shader()), true)
        );
        assert_eq!(runs.load(Ordering::Relaxed), 1, "the second came from disk");

        // Another build translates again.
        assert_eq!(
            cached(&cache, &[8; 32], container, translate).0,
            Ok(shader())
        );
        assert_eq!(runs.load(Ordering::Relaxed), 2);

        // A damaged entry is a miss, translated and stored again.
        let path = cache
            .dir
            .join(blake3::keyed_hash(&build, container).to_hex().as_str());
        let mut damaged = std::fs::read(&path).unwrap();
        damaged[12] ^= 1; // DXIL payload: still decodes structurally.
        std::fs::write(&path, damaged).unwrap();
        assert_eq!(
            cached(&cache, &build, container, translate),
            (Ok(shader()), false)
        );
        assert_eq!(runs.load(Ordering::Relaxed), 3);
        assert_eq!(
            cached(&cache, &build, container, translate),
            (Ok(shader()), true)
        );
        assert_eq!(runs.load(Ordering::Relaxed), 3);

        // Refusals are stored with their reason, and not tried again.
        let fails = AtomicU64::new(0);
        let fail = |_: &[u8]| {
            fails.fetch_add(1, Ordering::Relaxed);
            Err::<Translated, _>("unsupported".to_owned())
        };
        let refused = (Err("unsupported".to_owned()), false);
        assert_eq!(cached(&cache, &build, b"other", fail), refused);
        assert_eq!(cached(&cache, &build, b"other", fail), (refused.0, true));
        assert_eq!(fails.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn concurrent_publication_of_one_key_survives_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let caches: Vec<_> = (0..4).map(|_| Cache::open(dir.path()).unwrap()).collect();
        let start = std::sync::Barrier::new(caches.len());
        std::thread::scope(|scope| {
            for cache in &caches {
                scope.spawn(|| {
                    let (result, hit) = cached(cache, &[7; 32], b"same key", |_| {
                        start.wait(); // every connection observed the initial miss
                        Ok(shader())
                    });
                    assert!(!hit);
                    assert_eq!(result, Ok(shader()));
                });
            }
        });
        drop(caches);
        let cache = Cache::open(dir.path()).unwrap();
        assert_eq!(
            cached(&cache, &[7; 32], b"same key", |_| panic!()),
            (Ok(shader()), true)
        );
    }

    #[test]
    fn interrupted_writer_preserves_only_published_entries() {
        const CHILD: &str = "VICTORY_TEST_INTERRUPTED_SHADER_WRITER";
        if let Some(dir) = std::env::var_os(CHILD) {
            let cache = Cache::open(Path::new(&dir)).unwrap();
            assert!(!cached(&cache, &[7; 32], b"committed", |_| Ok(shader())).1);
            let mut partial = cache.stage().unwrap();
            partial.write_all(b"incomplete shader").unwrap();
            // Exit before the temporary file is published at a cache key.
            std::process::exit(71);
        }

        let dir = tempfile::tempdir().unwrap();
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "shader_cache::tests::interrupted_writer_preserves_only_published_entries",
            ])
            .env(CHILD, dir.path())
            .status()
            .unwrap();
        assert_eq!(status.code(), Some(71));
        let cache = Cache::open(dir.path()).unwrap();
        assert_eq!(
            cached(&cache, &[7; 32], b"committed", |_| panic!()),
            (Ok(shader()), true)
        );
        assert_eq!(
            cached(&cache, &[7; 32], b"unfinished", |_| Ok(shader())),
            (Ok(shader()), false)
        );
    }

    #[test]
    fn a_translation_is_made_once_when_its_entry_cannot_be_written() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("cache");
        let cache = Cache::open(&root).unwrap();
        std::fs::remove_dir(&root).unwrap();
        std::fs::write(&root, b"not a directory").unwrap();
        let runs = AtomicU64::new(0);
        let translate = |_: &[u8]| {
            runs.fetch_add(1, Ordering::Relaxed);
            Ok(shader())
        };
        assert_eq!(
            cached(&cache, &[7; 32], b"a container", translate),
            (Ok(shader()), false)
        );
        assert_eq!(runs.load(Ordering::Relaxed), 1);
    }

    /// The key's parts: a change to any of them (the translator's sources,
    /// this file's encoding, the loop limit) makes another key, so no entry
    /// written before it is read.
    #[test]
    fn the_key_covers_translator_encoding_and_settings() {
        let k = key("0123456789abcdef", b"encoding", 1 << 20, false);
        assert_eq!(k, key("0123456789abcdef", b"encoding", 1 << 20, false));
        assert_ne!(k, key("0123456789abcdee", b"encoding", 1 << 20, false));
        assert_ne!(k, key("0123456789abcdef", b"encoding.", 1 << 20, false));
        assert_ne!(k, key("0123456789abcdef", b"encoding", 1 << 19, false));
        assert_ne!(k, key("0123456789abcdef", b"encoding", 1 << 20, true));
        assert_eq!(
            *build(),
            key(
                victory_shader::SOURCE_HASH,
                ENCODING,
                victory_shader::emit::loop_limit(),
                options().view_pages
            )
        );
        // The encoding part is this file as built.
        assert!(
            std::str::from_utf8(ENCODING)
                .unwrap()
                .contains("fn encode(translation: &Result<Translated, String>)")
        );
    }

    #[test]
    fn entries_round_trip_and_reject_damage() {
        let bytes = encode(&Ok(shader()));
        assert_eq!(decode(&bytes), Some(Ok(shader())));
        assert_eq!(decode(&bytes[..bytes.len() - 1]), None);
        assert_eq!(decode(&[bytes.as_slice(), &[0]].concat()), None);
        assert_eq!(decode(b"VSH0"), None);
        let refusal = Err("GFX10 code at 0x10: x is not supported".to_owned());
        assert_eq!(decode(&encode(&refusal)), Some(refusal));
        assert_eq!(decode(&[b"VSHE".as_slice(), &[0xff]].concat()), None);
    }
}
