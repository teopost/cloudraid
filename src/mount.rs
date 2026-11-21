//! Il repository visto come una cartella, in sola lettura (FUSE).
//!
//! Cartelle, nomi e dimensioni vengono dall'indice, tenuto in memoria: `ls` non tocca i backend.
//! Quando un programma legge un pezzo di file si scaricano solo i blocchi che servono, ognuno
//! verificato con la sua impronta come in `get`. I blocchi letti restano in una cache in memoria
//! e il blocco successivo viene chiesto in anticipo, così copie e streaming scorrono.
//! Ogni `refresh` secondi si controlla se un altro processo (interfaccia web, riga di comando)
//! ha salvato un indice più recente: il mount non prende mai il lock e non scrive niente.

use anyhow::{anyhow, bail, Context, Result};
use fuser::{
    Errno, FileAttr, FileHandle, FileType, FopenFlags, Generation, INodeNo, LockOwner, MountOption, OpenAccMode,
    OpenFlags, ReplyAttr, ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen, ReplyStatfs, Request,
    SessionACL,
};
use std::collections::{BTreeMap, HashMap};
use std::ffi::{CString, OsStr};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::thread;
use std::time::{Duration, UNIX_EPOCH};

use crate::index::{ChunkEntry, Index};
use crate::repo::Repo;

/// Per quanto il kernel può tenere per buoni nomi e attributi senza richiederli.
const TTL: Duration = Duration::from_secs(1);
const ROOT: u64 = 1;
const BLOCK: u64 = 4096;

pub struct MountOptions {
    pub mountpoint: PathBuf,
    /// Ogni quanto controllare se c'è un indice più recente.
    pub refresh: Duration,
    /// Memoria massima per i blocchi già letti.
    pub cache_bytes: usize,
    /// Proprietario di file e cartelle.
    pub uid: u32,
    pub gid: u32,
    /// Accesso anche agli altri utenti (con i permessi 0440/0550 controllati dal kernel).
    pub allow_other: bool,
    /// Richieste del kernel servite in parallelo: una lettura lenta non blocca `ls`.
    pub threads: usize,
}

// ---------------------------------------------------------------------------------------------
// L'albero delle cartelle, ricostruito dall'indice.

/// Un blocco di un file: dove comincia nel file e come leggerlo.
#[derive(Clone)]
pub struct ChunkRef {
    pub id: String,
    pub offset: u64,
    pub len: u64,
    /// `None` se il blocco manca dall'indice (indice danneggiato): leggerlo dà errore.
    pub entry: Option<Arc<ChunkEntry>>,
}

pub struct FileNode {
    pub size: u64,
    pub mtime: i64,
    pub chunks: Vec<ChunkRef>,
}

enum Node {
    Dir { parent: u64, mtime: i64, children: BTreeMap<String, u64> },
    File { file: Arc<FileNode> },
}

/// Numeri di inode stabili fra un indice e l'altro: lo stesso percorso ha sempre lo stesso numero,
/// così il kernel non confonde file diversi dopo un aggiornamento.
pub struct Inodes {
    by_key: HashMap<String, u64>,
    next: u64,
}

impl Default for Inodes {
    fn default() -> Self {
        Inodes { by_key: HashMap::new(), next: ROOT + 1 }
    }
}

impl Inodes {
    fn get(&mut self, dir: bool, path: &str) -> u64 {
        let key = format!("{}:{path}", if dir { 'd' } else { 'f' });
        let next = &mut self.next;
        *self.by_key.entry(key).or_insert_with(|| {
            *next += 1;
            *next - 1
        })
    }
}

pub struct Tree {
    pub generation: u64,
    pub files: u64,
    pub bytes: u64,
    nodes: HashMap<u64, Node>,
}

impl Tree {
    pub fn build(index: &Index, inodes: &mut Inodes) -> Tree {
        let mut nodes: HashMap<u64, Node> = HashMap::new();
        nodes.insert(ROOT, Node::Dir { parent: ROOT, mtime: 0, children: BTreeMap::new() });
        let chunks: HashMap<&str, Arc<ChunkEntry>> =
            index.chunks.iter().map(|(id, e)| (id.as_str(), Arc::new(e.clone()))).collect();
        let (mut files, mut bytes) = (0, 0);

        'files: for (path, entry) in &index.files {
            let parts: Vec<&str> = path.split('/').collect();
            let Some((name, dirs)) = parts.split_last() else { continue };
            // le cartelle del percorso, create se mancano
            let mut cur = ROOT;
            let mut visited = vec![ROOT];
            for (j, part) in dirs.iter().enumerate() {
                let child = match &nodes[&cur] {
                    Node::Dir { children, .. } => children.get(*part).copied(),
                    Node::File { .. } => unreachable!(),
                };
                cur = match child {
                    Some(ino) if matches!(nodes[&ino], Node::Dir { .. }) => ino,
                    Some(_) => {
                        eprintln!("attenzione: {path} nascosto: {} è un file", parts[..=j].join("/"));
                        continue 'files;
                    }
                    None => {
                        let ino = inodes.get(true, &parts[..=j].join("/"));
                        nodes.insert(ino, Node::Dir { parent: cur, mtime: 0, children: BTreeMap::new() });
                        if let Some(Node::Dir { children, .. }) = nodes.get_mut(&cur) {
                            children.insert(part.to_string(), ino);
                        }
                        ino
                    }
                };
                visited.push(cur);
            }
            if let Some(Node::Dir { children, .. }) = nodes.get(&cur) {
                if children.contains_key(*name) {
                    eprintln!("attenzione: {path} nascosto: esiste già una cartella con lo stesso nome");
                    continue;
                }
            }
            // i blocchi, con la posizione di ognuno nel file
            let mut offset = 0;
            let refs: Vec<ChunkRef> = entry
                .chunks
                .iter()
                .map(|id| {
                    let entry = chunks.get(id.as_str()).cloned();
                    let len = entry.as_ref().map_or(0, |e| e.plain_len);
                    let r = ChunkRef { id: id.clone(), offset, len, entry };
                    offset += len;
                    r
                })
                .collect();
            let mtime = entry.mtime.unwrap_or(0);
            let ino = inodes.get(false, path);
            nodes.insert(
                ino,
                Node::File { file: Arc::new(FileNode { size: entry.size, mtime, chunks: refs }) },
            );
            if let Some(Node::Dir { children, .. }) = nodes.get_mut(&cur) {
                children.insert(name.to_string(), ino);
            }
            // una cartella ha la data del file più recente che contiene
            for d in visited {
                if let Some(Node::Dir { mtime: m, .. }) = nodes.get_mut(&d) {
                    *m = (*m).max(mtime);
                }
            }
            files += 1;
            bytes += entry.size;
        }
        Tree { generation: index.generation, files, bytes, nodes }
    }

    /// Il numero di inode di un percorso (`""` è la radice).
    pub fn lookup_path(&self, path: &str) -> Option<u64> {
        let mut cur = ROOT;
        for part in path.split('/').filter(|p| !p.is_empty()) {
            cur = self.child(cur, part)?;
        }
        Some(cur)
    }

    fn child(&self, parent: u64, name: &str) -> Option<u64> {
        match self.nodes.get(&parent)? {
            Node::Dir { children, .. } => children.get(name).copied(),
            Node::File { .. } => None,
        }
    }

    pub fn file(&self, ino: u64) -> Option<Arc<FileNode>> {
        match self.nodes.get(&ino)? {
            Node::File { file, .. } => Some(file.clone()),
            Node::Dir { .. } => None,
        }
    }

    /// Il contenuto di una cartella: (nome, inode, è una cartella).
    pub fn list(&self, ino: u64) -> Option<Vec<(String, u64, bool)>> {
        match self.nodes.get(&ino)? {
            Node::Dir { parent, children, .. } => {
                let mut out = vec![(".".to_string(), ino, true), ("..".to_string(), *parent, true)];
                out.extend(
                    children.iter().map(|(n, &c)| (n.clone(), c, matches!(self.nodes.get(&c), Some(Node::Dir { .. })))),
                );
                Some(out)
            }
            Node::File { .. } => None,
        }
    }

    fn attr(&self, ino: u64, uid: u32, gid: u32) -> Option<FileAttr> {
        let (kind, perm, size, mtime, nlink) = match self.nodes.get(&ino)? {
            Node::Dir { mtime, children, .. } => {
                let subdirs = children.values().filter(|c| matches!(self.nodes.get(c), Some(Node::Dir { .. }))).count();
                (FileType::Directory, 0o550, 0, *mtime, 2 + subdirs as u32)
            }
            Node::File { file, .. } => (FileType::RegularFile, 0o440, file.size, file.mtime, 1),
        };
        let t = UNIX_EPOCH + Duration::from_secs(mtime.max(0) as u64);
        Some(FileAttr {
            ino: INodeNo(ino),
            size,
            blocks: size.div_ceil(512),
            atime: t,
            mtime: t,
            ctime: t,
            crtime: t,
            kind,
            perm,
            nlink,
            uid,
            gid,
            rdev: 0,
            blksize: BLOCK as u32,
            flags: 0,
        })
    }
}

// ---------------------------------------------------------------------------------------------
// Cache dei blocchi in chiaro.

struct Slot {
    /// Chi scarica il blocco tiene il mutex: le altre richieste dello stesso blocco aspettano
    /// e poi trovano i dati, invece di scaricarlo di nuovo.
    data: Mutex<Option<Arc<Vec<u8>>>>,
    used: AtomicU64,
}

pub struct Cache {
    capacity: usize,
    inner: Mutex<CacheInner>,
}

struct CacheInner {
    slots: HashMap<String, Arc<Slot>>,
    size: usize,
    tick: u64,
}

impl Cache {
    pub fn new(capacity: usize) -> Cache {
        Cache { capacity, inner: Mutex::new(CacheInner { slots: HashMap::new(), size: 0, tick: 0 }) }
    }

    /// Il blocco `id`, dalla cache o da `load`.
    pub fn get(&self, id: &str, load: impl FnOnce() -> Result<Vec<u8>>) -> Result<Arc<Vec<u8>>> {
        let slot = {
            let mut g = self.inner.lock().unwrap();
            g.tick += 1;
            let tick = g.tick;
            let slot = g
                .slots
                .entry(id.to_string())
                .or_insert_with(|| Arc::new(Slot { data: Mutex::new(None), used: AtomicU64::new(0) }))
                .clone();
            slot.used.store(tick, Ordering::Relaxed);
            slot
        };
        let mut data = slot.data.lock().unwrap();
        if let Some(d) = data.as_ref() {
            return Ok(d.clone());
        }
        match load() {
            Ok(d) => {
                let d = Arc::new(d);
                *data = Some(d.clone());
                drop(data);
                let mut g = self.inner.lock().unwrap();
                g.size += d.len();
                self.evict(&mut g, id);
                Ok(d)
            }
            Err(e) => {
                drop(data);
                let mut g = self.inner.lock().unwrap();
                if g.slots.get(id).is_some_and(|s| Arc::ptr_eq(s, &slot)) {
                    g.slots.remove(id);
                }
                Err(e)
            }
        }
    }

    /// C'è (o si sta scaricando)?
    pub fn contains(&self, id: &str) -> bool {
        self.inner.lock().unwrap().slots.contains_key(id)
    }

    /// Byte in cache.
    pub fn size(&self) -> usize {
        self.inner.lock().unwrap().size
    }

    /// Toglie i blocchi usati meno di recente finché si sta nella capacità (mai `keep`, appena letto).
    fn evict(&self, g: &mut CacheInner, keep: &str) {
        while g.size > self.capacity {
            let victim = g
                .slots
                .iter()
                .filter(|(id, _)| id.as_str() != keep)
                .filter_map(|(id, s)| {
                    let len = s.data.try_lock().ok()?.as_ref()?.len();
                    Some((s.used.load(Ordering::Relaxed), id.clone(), len))
                })
                .min();
            let Some((_, id, len)) = victim else { break };
            g.slots.remove(&id);
            g.size -= len;
        }
    }
}

/// Legge `len` byte da `offset` di un file fatto di `chunks`, chiedendo i blocchi a `load`.
/// Restituisce anche l'indice dell'ultimo blocco toccato (per la lettura anticipata).
pub fn read_range(
    file: &FileNode,
    offset: u64,
    len: u64,
    load: &dyn Fn(&ChunkRef) -> Result<Arc<Vec<u8>>>,
) -> Result<(Vec<u8>, Option<usize>)> {
    let end = offset.saturating_add(len).min(file.size);
    if offset >= end {
        return Ok((Vec::new(), None));
    }
    let mut out = Vec::with_capacity((end - offset) as usize);
    let mut i = file.chunks.partition_point(|c| c.offset + c.len <= offset);
    let mut last = None;
    while out.len() < (end - offset) as usize {
        let Some(c) = file.chunks.get(i) else {
            bail!("il file è più corto di quanto dice l'indice");
        };
        let data = load(c)?;
        if data.len() as u64 != c.len {
            bail!("blocco {} di lunghezza inattesa", &c.id[..12.min(c.id.len())]);
        }
        let pos = offset + out.len() as u64;
        let from = (pos - c.offset) as usize;
        let to = ((end - c.offset) as usize).min(data.len());
        out.extend_from_slice(&data[from..to]);
        last = Some(i);
        i += 1;
    }
    Ok((out, last))
}

// ---------------------------------------------------------------------------------------------
// Il filesystem.

struct Shared {
    repo: Repo,
    tree: RwLock<Arc<Tree>>,
    inodes: Mutex<Inodes>,
    cache: Cache,
    /// I file aperti: chi legge vede il file com'era all'apertura anche se l'indice cambia.
    handles: Mutex<HashMap<u64, Arc<FileNode>>>,
    next_fh: AtomicU64,
    uid: u32,
    gid: u32,
}

impl Shared {
    fn tree(&self) -> Arc<Tree> {
        self.tree.read().unwrap().clone()
    }

    fn chunk(&self, c: &ChunkRef) -> Result<Arc<Vec<u8>>> {
        let entry = c.entry.as_ref().ok_or_else(|| anyhow!("blocco {} assente dall'indice", c.id))?;
        self.cache.get(&c.id, || self.repo.read_chunk(&c.id, entry).map(|(data, _)| data))
    }

    /// Sostituisce l'albero se sui backend c'è un indice più recente.
    fn refresh(&self) -> Result<bool> {
        let generation = self.tree().generation;
        let Some(index) = self.repo.newer_index(generation)? else { return Ok(false) };
        let tree = Tree::build(&index, &mut self.inodes.lock().unwrap());
        *self.tree.write().unwrap() = Arc::new(tree);
        Ok(true)
    }
}

struct Fs(Arc<Shared>);

fn errno(e: &anyhow::Error) -> Errno {
    eprintln!("errore di lettura: {e:#}");
    Errno::EIO
}

impl fuser::Filesystem for Fs {
    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let tree = self.0.tree();
        let found = name.to_str().and_then(|n| tree.child(parent.0, n)).and_then(|ino| tree.attr(ino, self.0.uid, self.0.gid));
        match found {
            Some(attr) => reply.entry(&TTL, &attr, Generation(0)),
            None => reply.error(Errno::ENOENT),
        }
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, fh: Option<FileHandle>, reply: ReplyAttr) {
        // un file aperto mantiene la dimensione che aveva all'apertura
        if let Some(file) = fh.and_then(|fh| self.0.handles.lock().unwrap().get(&fh.0).cloned()) {
            if let Some(mut attr) = self.0.tree().attr(ino.0, self.0.uid, self.0.gid) {
                attr.size = file.size;
                attr.blocks = file.size.div_ceil(512);
                return reply.attr(&TTL, &attr);
            }
        }
        match self.0.tree().attr(ino.0, self.0.uid, self.0.gid) {
            Some(attr) => reply.attr(&TTL, &attr),
            None => reply.error(Errno::ENOENT),
        }
    }

    fn open(&self, _req: &Request, ino: INodeNo, flags: OpenFlags, _kill: bool, reply: ReplyOpen) {
        if flags.acc_mode() != OpenAccMode::O_RDONLY {
            return reply.error(Errno::EROFS);
        }
        let Some(file) = self.0.tree().file(ino.0) else {
            return reply.error(Errno::ENOENT);
        };
        let fh = self.0.next_fh.fetch_add(1, Ordering::Relaxed);
        self.0.handles.lock().unwrap().insert(fh, file);
        reply.opened(FileHandle(fh), FopenFlags::empty());
    }

    fn read(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock: Option<LockOwner>,
        reply: ReplyData,
    ) {
        let file = self.0.handles.lock().unwrap().get(&fh.0).cloned().or_else(|| self.0.tree().file(ino.0));
        let Some(file) = file else { return reply.error(Errno::ENOENT) };
        match read_range(&file, offset, size as u64, &|c| self.0.chunk(c)) {
            Ok((data, last)) => {
                reply.data(&data);
                // lettura anticipata del blocco successivo, in sottofondo
                if let Some(next) = last.and_then(|i| file.chunks.get(i + 1)) {
                    if !self.0.cache.contains(&next.id) {
                        let (shared, next) = (self.0.clone(), next.clone());
                        thread::spawn(move || {
                            let _ = shared.chunk(&next);
                        });
                    }
                }
            }
            Err(e) => reply.error(errno(&e)),
        }
    }

    fn release(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _flags: OpenFlags,
        _lock: Option<LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        self.0.handles.lock().unwrap().remove(&fh.0);
        reply.ok();
    }

    fn readdir(&self, _req: &Request, ino: INodeNo, _fh: FileHandle, offset: u64, mut reply: ReplyDirectory) {
        let Some(entries) = self.0.tree().list(ino.0) else {
            return reply.error(Errno::ENOTDIR);
        };
        for (i, (name, child, dir)) in entries.into_iter().enumerate().skip(offset as usize) {
            let kind = if dir { FileType::Directory } else { FileType::RegularFile };
            if reply.add(INodeNo(child), i as u64 + 1, kind, name) {
                break;
            }
        }
        reply.ok();
    }

    fn statfs(&self, _req: &Request, _ino: INodeNo, reply: ReplyStatfs) {
        let tree = self.0.tree();
        reply.statfs(tree.bytes.div_ceil(BLOCK), 0, 0, tree.files, 0, BLOCK as u32, 255, BLOCK as u32);
    }
}

// ---------------------------------------------------------------------------------------------

/// Smonta un mount rimasto appeso (processo morto: "Transport endpoint is not connected").
fn clear_stale(path: &Path) -> Result<()> {
    match std::fs::metadata(path) {
        Err(e) if e.raw_os_error() == Some(libc::ENOTCONN) => {
            eprintln!("{}: mount precedente rimasto appeso, lo smonto", path.display());
            lazy_umount(path)
        }
        _ => Ok(()),
    }
}

fn lazy_umount(path: &Path) -> Result<()> {
    let c = CString::new(path.as_os_str().as_bytes())?;
    // SAFETY: c è una stringa C valida per tutta la chiamata
    if unsafe { libc::umount2(c.as_ptr(), libc::MNT_DETACH) } != 0 {
        bail!("impossibile smontare {}: {}", path.display(), std::io::Error::last_os_error());
    }
    Ok(())
}

/// Monta il repository e resta in attesa finché non arriva SIGTERM/SIGINT/SIGHUP
/// o qualcuno smonta la cartella.
pub fn mount(repo: Repo, opts: MountOptions) -> Result<()> {
    let mountpoint = &opts.mountpoint;
    clear_stale(mountpoint)?;
    std::fs::create_dir_all(mountpoint).with_context(|| format!("impossibile creare {}", mountpoint.display()))?;

    let mut inodes = Inodes::default();
    let tree = Tree::build(repo.index(), &mut inodes);
    eprintln!(
        "{} file ({} MB), indice generazione {}",
        tree.files,
        tree.bytes / 1_000_000,
        tree.generation
    );
    let shared = Arc::new(Shared {
        repo,
        tree: RwLock::new(Arc::new(tree)),
        inodes: Mutex::new(inodes),
        cache: Cache::new(opts.cache_bytes),
        handles: Mutex::new(HashMap::new()),
        next_fh: AtomicU64::new(1),
        uid: opts.uid,
        gid: opts.gid,
    });

    let mut cfg = fuser::Config::default();
    cfg.mount_options = vec![
        MountOption::FSName("cloudraid".into()),
        MountOption::Subtype("cloudraid".into()),
        MountOption::RO,
        MountOption::NoDev,
        MountOption::NoSuid,
        MountOption::DefaultPermissions,
    ];
    cfg.acl = if opts.allow_other { SessionACL::All } else { SessionACL::Owner };
    cfg.n_threads = Some(opts.threads.max(1));
    let mut session = fuser::Session::new(Fs(shared.clone()), mountpoint, &cfg)
        .with_context(|| format!("impossibile montare {} (serve /dev/fuse)", mountpoint.display()))?;

    // aggiornamento dell'indice
    let refresher = shared.clone();
    let every = opts.refresh;
    thread::spawn(move || loop {
        thread::sleep(every);
        match refresher.refresh() {
            Ok(true) => eprintln!("indice aggiornato alla generazione {}", refresher.tree().generation),
            Ok(false) => {}
            Err(e) => eprintln!("attenzione: aggiornamento dell'indice: {e:#}"),
        }
    });

    // allo stop del container si smonta in modo pulito; se la cartella è occupata (una shell
    // ci sta dentro) si stacca comunque, invece di lasciare un mount appeso
    let mut unmounter = session.unmount_callable();
    let path = mountpoint.clone();
    let mut signals = signal_hook::iterator::Signals::new([libc::SIGTERM, libc::SIGINT, libc::SIGHUP])?;
    thread::spawn(move || {
        if let Some(sig) = signals.forever().next() {
            eprintln!("segnale {sig}: smonto {}", path.display());
            if let Err(e) = unmounter.unmount() {
                eprintln!("smontaggio normale non riuscito ({e}), stacco la cartella");
                if let Err(e) = lazy_umount(&path) {
                    eprintln!("{e:#}");
                }
            }
        }
    });

    eprintln!("montato su {} (sola lettura)", mountpoint.display());
    session.run().context("sessione FUSE interrotta")?;
    eprintln!("smontato");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::FileEntry;

    fn chunk(len: u64) -> ChunkEntry {
        ChunkEntry { plain_len: len, sealed_len: 0, k: 1, m: 1, shard_len: 0, shards: Vec::new(), refs: 1 }
    }

    fn file(chunks: &[&str], size: u64, mtime: i64) -> FileEntry {
        FileEntry { size, hash: String::new(), mtime: Some(mtime), chunks: chunks.iter().map(|c| c.to_string()).collect() }
    }

    fn index(generation: u64, files: &[(&str, FileEntry)]) -> Index {
        let mut idx = Index { generation, ..Index::default() };
        idx.chunks.insert("aaaa".into(), chunk(10));
        idx.chunks.insert("bbbb".into(), chunk(5));
        for (p, f) in files {
            idx.files.insert(p.to_string(), f.clone());
        }
        idx
    }

    #[test]
    fn albero_cartelle_e_conflitti() {
        let idx = index(
            1,
            &[
                ("a.txt", file(&["aaaa"], 10, 100)),
                ("foto/2026/x.jpg", file(&["aaaa", "bbbb"], 15, 300)),
                ("foto/y.jpg", file(&["bbbb"], 5, 200)),
                // "a.txt" è un file: questo non si può mostrare
                ("a.txt/dentro", file(&["bbbb"], 5, 1)),
            ],
        );
        let mut inodes = Inodes::default();
        let t = Tree::build(&idx, &mut inodes);
        assert_eq!(t.files, 3);
        assert_eq!(t.bytes, 30);
        let names: Vec<String> = t.list(ROOT).unwrap().into_iter().map(|e| e.0).collect();
        assert_eq!(names, [".", "..", "a.txt", "foto"]);
        let foto = t.lookup_path("foto").unwrap();
        let attr = t.attr(foto, 1000, 1000).unwrap();
        assert_eq!(attr.kind, FileType::Directory);
        assert_eq!(attr.mtime, UNIX_EPOCH + Duration::from_secs(300), "data del file più recente");
        assert_eq!(attr.nlink, 3);
        let x = t.file(t.lookup_path("foto/2026/x.jpg").unwrap()).unwrap();
        assert_eq!((x.chunks[1].offset, x.chunks[1].len), (10, 5));
        assert!(t.lookup_path("a.txt/dentro").is_none());

        // stessi percorsi, stessi inode anche dopo un nuovo indice
        let idx2 = index(2, &[("nuovo.txt", file(&["aaaa"], 10, 1)), ("foto/y.jpg", file(&["bbbb"], 5, 200))]);
        let t2 = Tree::build(&idx2, &mut inodes);
        assert_eq!(t2.lookup_path("foto"), Some(foto));
        assert_eq!(t2.lookup_path("foto/y.jpg"), t.lookup_path("foto/y.jpg"));
        assert!(t2.lookup_path("a.txt").is_none());
        assert_ne!(t2.lookup_path("nuovo.txt"), t.lookup_path("a.txt"));
    }

    #[test]
    fn lettura_a_cavallo_dei_blocchi() {
        let data: Vec<u8> = (0..=255u8).cycle().take(1000).collect();
        let lens = [300u64, 1, 450, 249];
        let mut chunks = Vec::new();
        let mut offset = 0;
        for (i, len) in lens.iter().enumerate() {
            chunks.push(ChunkRef { id: format!("{i:04}"), offset, len: *len, entry: None });
            offset += len;
        }
        let f = FileNode { size: 1000, mtime: 0, chunks };
        let load = |c: &ChunkRef| Ok(Arc::new(data[c.offset as usize..(c.offset + c.len) as usize].to_vec()));
        for (off, len) in [(0, 1000), (0, 5000), (299, 3), (300, 1), (301, 450), (999, 10), (1000, 10), (5, 0)] {
            let (got, _) = read_range(&f, off, len, &load).unwrap();
            let end = (off + len).min(1000) as usize;
            let expected = if off as usize >= end { &[][..] } else { &data[off as usize..end] };
            assert_eq!(got, expected, "offset {off} len {len}");
        }
        assert_eq!(read_range(&f, 299, 3, &load).unwrap().1, Some(2), "ultimo blocco toccato");
        // l'indice dice 1000 byte ma i blocchi finiscono prima: errore, non dati inventati
        let corto = FileNode { size: 1200, mtime: 0, chunks: f.chunks.clone() };
        assert!(read_range(&corto, 900, 200, &load).is_err());
    }

    #[test]
    fn cache_un_solo_download_e_sfratto() {
        use std::sync::atomic::AtomicUsize;
        let cache = Arc::new(Cache::new(250));
        let loads = Arc::new(AtomicUsize::new(0));
        // dieci richieste contemporanee dello stesso blocco: un solo download
        let handles: Vec<_> = (0..10)
            .map(|_| {
                let (cache, loads) = (cache.clone(), loads.clone());
                thread::spawn(move || {
                    cache
                        .get("x", || {
                            loads.fetch_add(1, Ordering::SeqCst);
                            thread::sleep(Duration::from_millis(50));
                            Ok(vec![1; 100])
                        })
                        .unwrap()
                        .len()
                })
            })
            .collect();
        for h in handles {
            assert_eq!(h.join().unwrap(), 100);
        }
        assert_eq!(loads.load(Ordering::SeqCst), 1);
        // un errore non resta in cache
        assert!(cache.get("e", || bail!("rete giù")).is_err());
        assert!(!cache.contains("e"));
        // oltre la capacità esce il blocco usato meno di recente
        cache.get("y", || Ok(vec![2; 100])).unwrap();
        cache.get("x", || unreachable!()).unwrap();
        cache.get("z", || Ok(vec![3; 100])).unwrap();
        assert!(cache.contains("x") && cache.contains("z") && !cache.contains("y"));
        assert_eq!(cache.size(), 200);
    }
}
