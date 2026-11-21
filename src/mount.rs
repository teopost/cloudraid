//! Il repository visto come una cartella (FUSE), in sola lettura o anche in scrittura.
//!
//! Cartelle, nomi e dimensioni vengono dall'indice, tenuto in memoria: `ls` non tocca i backend.
//! Quando un programma legge un pezzo di file si scaricano solo i blocchi che servono, ognuno
//! verificato con la sua impronta come in `get`. I blocchi letti restano in una cache in memoria
//! e il blocco successivo viene chiesto in anticipo, così copie e streaming scorrono.
//! Ogni `refresh` secondi si controlla se un altro processo (interfaccia web, riga di comando)
//! ha salvato un indice più recente.
//!
//! In scrittura (`staging`) un file aperto per scrivere vive in una copia locale nella cartella di
//! appoggio; alla chiusura (flush) viene spezzato in blocchi, cifrato e caricato come fa `put`, e la
//! chiusura aspetta la fine del caricamento, così un errore arriva al programma che scrive.
//! Rinominare, cancellare e creare cartelle cambiano solo l'indice. Ogni modifica prende il lock del
//! repository; l'indice si salva al più ogni `SAVE_DELAY` (come i metadati di ext4), subito con `fsync`.
//! Il lock resta "in prestito" finché le scritture continuano e viene lasciato dopo qualche secondo di
//! calma, così web e riga di comando possono tornare a scrivere.

use anyhow::{anyhow, bail, Context, Result};
use fuser::{
    Errno, FileAttr, FileHandle, FileType, FopenFlags, Generation, INodeNo, InitFlags, KernelConfig, LockOwner,
    MountOption, OpenAccMode, OpenFlags, Owner, RenameFlags, ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory,
    ReplyEmpty, ReplyEntry, ReplyOpen, ReplyStatfs, ReplyWrite, Request, SessionACL, TimeOrNow, WriteFlags,
};
use std::collections::{BTreeMap, HashMap};
use std::ffi::{CString, OsStr};
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::crypto::random_hex;
use crate::index::{ChunkEntry, Index};
use crate::repo::{Repo, Upload};

/// Per quanto il kernel può tenere per buoni nomi e attributi senza richiederli.
const TTL: Duration = Duration::from_secs(1);
const ROOT: u64 = 1;
const BLOCK: u64 = 4096;
/// Dopo quanto tempo senza scritture il lock del repository viene lasciato.
const LEASE: Duration = Duration::from_secs(5);
/// Ritardo massimo del salvataggio dell'indice dopo una modifica: le modifiche vicine ne fanno uno solo.
const SAVE_DELAY: Duration = Duration::from_secs(2);
/// Quante volte (una al secondo) riprovare a prendere il lock se un altro processo lo tiene.
const LOCK_TRIES: u32 = 60;

pub struct MountOptions {
    pub mountpoint: PathBuf,
    /// Ogni quanto controllare se c'è un indice più recente.
    pub refresh: Duration,
    /// Memoria massima per i blocchi già letti.
    pub cache_bytes: usize,
    /// Proprietario di file e cartelle.
    pub uid: u32,
    pub gid: u32,
    /// Accesso anche agli altri utenti (secondo i permessi, controllati dal kernel).
    pub allow_other: bool,
    /// Richieste del kernel servite in parallelo: una lettura lenta non blocca `ls`.
    pub threads: usize,
    /// Cartella di appoggio per i file in scrittura: se c'è, il mount è anche in scrittura.
    pub staging: Option<PathBuf>,
    /// File di configurazione: se cambia (backend aggiunti o tolti dall'interfaccia) si rilegge.
    pub config: Option<PathBuf>,
}

fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

fn secs(t: SystemTime) -> i64 {
    t.duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
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
/// così il kernel non confonde file diversi dopo un aggiornamento. Una rinomina si porta dietro
/// il numero; un file cancellato lo perde, così uno nuovo con lo stesso nome ne ha un altro.
pub struct Inodes {
    by_key: HashMap<String, u64>,
    next: u64,
}

impl Default for Inodes {
    fn default() -> Self {
        Inodes { by_key: HashMap::new(), next: ROOT + 1 }
    }
}

fn inode_key(dir: bool, path: &str) -> String {
    format!("{}:{path}", if dir { 'd' } else { 'f' })
}

impl Inodes {
    fn get(&mut self, dir: bool, path: &str) -> u64 {
        let next = &mut self.next;
        *self.by_key.entry(inode_key(dir, path)).or_insert_with(|| {
            *next += 1;
            *next - 1
        })
    }

    pub fn forget(&mut self, dir: bool, path: &str) {
        self.by_key.remove(&inode_key(dir, path));
    }

    /// Dopo una rinomina: `to` (e per una cartella tutto quello che contiene) prende i numeri di `from`.
    pub fn rename(&mut self, dir: bool, from: &str, to: &str) {
        if !dir {
            self.by_key.remove(&inode_key(false, to));
            if let Some(ino) = self.by_key.remove(&inode_key(false, from)) {
                self.by_key.insert(inode_key(false, to), ino);
            }
            return;
        }
        let under = |key: &str, base: &str| {
            let p = &key[2..];
            p == base || p.starts_with(&format!("{base}/"))
        };
        // quello che c'era sotto `to` (una cartella vuota) sparisce
        self.by_key.retain(|k, _| !under(k, to));
        let moved: Vec<(String, u64)> =
            self.by_key.iter().filter(|(k, _)| under(k, from)).map(|(k, v)| (k.clone(), *v)).collect();
        for (k, ino) in moved {
            self.by_key.remove(&k);
            self.by_key.insert(format!("{}{to}{}", &k[..2], &k[2 + from.len()..]), ino);
        }
    }
}

pub struct Tree {
    pub generation: u64,
    pub files: u64,
    pub bytes: u64,
    nodes: HashMap<u64, Node>,
    paths: HashMap<u64, String>,
}

impl Tree {
    pub fn build(index: &Index, inodes: &mut Inodes) -> Tree {
        let mut t = Tree { generation: index.generation, files: 0, bytes: 0, nodes: HashMap::new(), paths: HashMap::new() };
        t.nodes.insert(ROOT, Node::Dir { parent: ROOT, mtime: 0, children: BTreeMap::new() });
        t.paths.insert(ROOT, String::new());
        let chunks: HashMap<&str, Arc<ChunkEntry>> =
            index.chunks.iter().map(|(id, e)| (id.as_str(), Arc::new(e.clone()))).collect();

        for dir in &index.dirs {
            let parts: Vec<&str> = dir.split('/').collect();
            if t.dirs(&parts, inodes).is_none() {
                eprintln!("attenzione: cartella {dir} nascosta: nel percorso c'è un file");
            }
        }
        for (path, entry) in &index.files {
            let parts: Vec<&str> = path.split('/').collect();
            let Some((name, dirs)) = parts.split_last() else { continue };
            let Some(visited) = t.dirs(dirs, inodes) else {
                eprintln!("attenzione: {path} nascosto: nel percorso c'è un file");
                continue;
            };
            let cur = *visited.last().expect("almeno la radice");
            if t.child(cur, name).is_some() {
                eprintln!("attenzione: {path} nascosto: esiste già una cartella con lo stesso nome");
                continue;
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
            t.nodes.insert(ino, Node::File { file: Arc::new(FileNode { size: entry.size, mtime, chunks: refs }) });
            t.paths.insert(ino, path.clone());
            if let Some(Node::Dir { children, .. }) = t.nodes.get_mut(&cur) {
                children.insert(name.to_string(), ino);
            }
            // una cartella ha la data del file più recente che contiene
            for d in visited {
                if let Some(Node::Dir { mtime: m, .. }) = t.nodes.get_mut(&d) {
                    *m = (*m).max(mtime);
                }
            }
            t.files += 1;
            t.bytes += entry.size;
        }
        t
    }

    /// Le cartelle di un percorso, create se mancano: gli inode dalla radice all'ultima.
    /// `None` se una parte del percorso è un file.
    fn dirs(&mut self, parts: &[&str], inodes: &mut Inodes) -> Option<Vec<u64>> {
        let mut cur = ROOT;
        let mut visited = vec![ROOT];
        for (j, part) in parts.iter().enumerate() {
            cur = match self.child(cur, part) {
                Some(ino) if matches!(self.nodes[&ino], Node::Dir { .. }) => ino,
                Some(_) => return None,
                None => {
                    let path = parts[..=j].join("/");
                    let ino = inodes.get(true, &path);
                    self.nodes.insert(ino, Node::Dir { parent: cur, mtime: 0, children: BTreeMap::new() });
                    self.paths.insert(ino, path);
                    if let Some(Node::Dir { children, .. }) = self.nodes.get_mut(&cur) {
                        children.insert(part.to_string(), ino);
                    }
                    ino
                }
            };
            visited.push(cur);
        }
        Some(visited)
    }

    /// Il numero di inode di un percorso (`""` è la radice).
    pub fn lookup_path(&self, path: &str) -> Option<u64> {
        let mut cur = ROOT;
        for part in path.split('/').filter(|p| !p.is_empty()) {
            cur = self.child(cur, part)?;
        }
        Some(cur)
    }

    /// Il percorso di un inode (`""` per la radice).
    pub fn path(&self, ino: u64) -> Option<&str> {
        self.paths.get(&ino).map(String::as_str)
    }

    fn child(&self, parent: u64, name: &str) -> Option<u64> {
        match self.nodes.get(&parent)? {
            Node::Dir { children, .. } => children.get(name).copied(),
            Node::File { .. } => None,
        }
    }

    fn is_dir(&self, ino: u64) -> bool {
        matches!(self.nodes.get(&ino), Some(Node::Dir { .. }))
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
                out.extend(children.iter().map(|(n, &c)| (n.clone(), c, self.is_dir(c))));
                Some(out)
            }
            Node::File { .. } => None,
        }
    }

    fn attr(&self, ino: u64, owner: (u32, u32), writable: bool) -> Option<FileAttr> {
        let (kind, perm, size, mtime, nlink) = match self.nodes.get(&ino)? {
            Node::Dir { mtime, children, .. } => {
                let subdirs = children.values().filter(|c| self.is_dir(**c)).count();
                (FileType::Directory, if writable { 0o750 } else { 0o550 }, 0, *mtime, 2 + subdirs as u32)
            }
            Node::File { file, .. } => (FileType::RegularFile, if writable { 0o640 } else { 0o440 }, file.size, file.mtime, 1),
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
            uid: owner.0,
            gid: owner.1,
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

/// La copia locale di un file aperto in scrittura, condivisa da tutti i suoi descrittori aperti.
struct Staging {
    path: PathBuf,
    file: fs::File,
    /// Contenuto cambiato rispetto al repository.
    dirty: bool,
    /// Data impostata dal programma (touch, cp -p); altrimenti quella del caricamento.
    mtime: Option<i64>,
    opens: usize,
    /// Tolta dalla tabella: chi la trova così deve crearne una nuova.
    closed: bool,
}

impl Drop for Staging {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

type StagingRef = Arc<Mutex<Staging>>;

/// Il file letto e l'ultimo blocco toccato, per la lettura anticipata del successivo.
type NextChunk = (Arc<FileNode>, usize);

#[derive(Clone)]
struct Handle {
    /// Il file com'era all'apertura (sola lettura).
    snapshot: Option<Arc<FileNode>>,
    staging: Option<StagingRef>,
}

/// Errore di un'operazione: un codice preciso per il programma, o un guasto (EIO, scritto nel log).
enum FsErr {
    Code(Errno),
    Fail(anyhow::Error),
}

impl From<anyhow::Error> for FsErr {
    fn from(e: anyhow::Error) -> Self {
        FsErr::Fail(e)
    }
}

impl From<std::io::Error> for FsErr {
    fn from(e: std::io::Error) -> Self {
        FsErr::Fail(e.into())
    }
}

type FsResult<T> = std::result::Result<T, FsErr>;

fn code(e: FsErr, what: &str) -> Errno {
    match e {
        FsErr::Code(c) => c,
        FsErr::Fail(e) => {
            eprintln!("errore ({what}): {e:#}");
            Errno::EIO
        }
    }
}

fn parent_of(path: &str) -> Option<&str> {
    path.rsplit_once('/').map(|(p, _)| p)
}

/// Dopo aver tolto o spostato `path`, la sua cartella resta anche se è rimasta vuota.
fn keep_parent(repo: &mut Repo, path: &str) -> Result<()> {
    match parent_of(path) {
        Some(p) if !repo.is_dir(p) => repo.mkdir(p),
        _ => Ok(()),
    }
}

struct Shared {
    repo: RwLock<Repo>,
    tree: RwLock<Arc<Tree>>,
    inodes: Mutex<Inodes>,
    cache: Cache,
    handles: Mutex<HashMap<u64, Handle>>,
    /// File aperti in scrittura, per inode.
    staged: Mutex<HashMap<u64, StagingRef>>,
    next_fh: AtomicU64,
    owner: (u32, u32),
    staging_dir: Option<PathBuf>,
    /// Una modifica alla volta: lock, modifica dell'indice, salvataggio.
    gate: Mutex<()>,
    last_change: Mutex<Instant>,
    /// Da quando ci sono modifiche all'indice non ancora salvate.
    unsaved_since: Mutex<Option<Instant>>,
}

impl Shared {
    fn tree(&self) -> Arc<Tree> {
        self.tree.read().unwrap().clone()
    }

    fn writable(&self) -> bool {
        self.staging_dir.is_some()
    }

    fn attr(&self, ino: u64) -> Option<FileAttr> {
        let mut attr = self.tree().attr(ino, self.owner, self.writable())?;
        // un file aperto in scrittura: dimensione e data della copia locale
        if let Some(st) = self.staged.lock().unwrap().get(&ino).cloned() {
            let st = st.lock().unwrap();
            if let Ok(meta) = st.file.metadata() {
                attr.size = meta.len();
                attr.blocks = meta.len().div_ceil(512);
                if st.dirty || st.mtime.is_some() {
                    let t = st.mtime.map(|t| UNIX_EPOCH + Duration::from_secs(t.max(0) as u64));
                    attr.mtime = t.or_else(|| meta.modified().ok()).unwrap_or(attr.mtime);
                }
            }
        }
        Some(attr)
    }

    fn chunk(&self, c: &ChunkRef) -> Result<Arc<Vec<u8>>> {
        let entry = c.entry.as_ref().ok_or_else(|| anyhow!("blocco {} assente dall'indice", c.id))?;
        self.cache.get(&c.id, || self.repo.read().unwrap().read_chunk(&c.id, entry).map(|(data, _)| data))
    }

    fn rebuild(&self, repo: &Repo) {
        let tree = Tree::build(repo.index(), &mut self.inodes.lock().unwrap());
        *self.tree.write().unwrap() = Arc::new(tree);
    }

    /// Sostituisce l'albero se sui backend c'è un indice più recente. Mentre teniamo il lock
    /// nessun altro può scrivere, e l'albero è già aggiornato.
    fn refresh(&self) -> Result<bool> {
        let generation = self.tree().generation;
        let index = {
            let repo = self.repo.read().unwrap();
            if repo.holds_lock() {
                return Ok(false);
            }
            repo.newer_index(generation)?
        };
        let Some(index) = index else { return Ok(false) };
        let tree = Tree::build(&index, &mut self.inodes.lock().unwrap());
        *self.tree.write().unwrap() = Arc::new(tree);
        Ok(true)
    }

    /// Prende il lock del repository, se non lo teniamo già; se è occupato riprova per un minuto.
    /// Va chiamata tenendo `gate`.
    fn acquire(&self) -> FsResult<()> {
        for attempt in 1..=LOCK_TRIES {
            {
                let mut repo = self.repo.write().unwrap();
                if repo.holds_lock() {
                    return Ok(());
                }
                match repo.lock() {
                    Ok(()) => {
                        // `lock` rilegge l'indice: l'albero segue
                        self.rebuild(&repo);
                        return Ok(());
                    }
                    Err(e) if attempt == 1 => eprintln!("repository occupato, aspetto: {e:#}"),
                    Err(e) if attempt == LOCK_TRIES => {
                        eprintln!("repository ancora occupato, rinuncio: {e:#}");
                        return Err(FsErr::Code(Errno::EBUSY));
                    }
                    Err(_) => {}
                }
            }
            thread::sleep(Duration::from_secs(1));
        }
        Err(FsErr::Code(Errno::EBUSY))
    }

    /// Applica `f` all'indice; il salvataggio arriva entro `SAVE_DELAY` (o subito con `sync_index`).
    /// Se `f` fallisce su un indice senza modifiche in sospeso, si torna all'indice dei backend.
    /// Va chiamata tenendo `gate`.
    fn change<T>(&self, f: impl FnOnce(&mut Repo) -> FsResult<T>) -> FsResult<T> {
        self.acquire()?;
        let mut repo = self.repo.write().unwrap();
        // modifiche in sospeso da prima: un errore di `f` (di solito un controllo, prima di cambiare
        // qualcosa) non deve buttarle via
        let pending = repo.is_dirty();
        let out = f(&mut repo);
        if out.is_err() && !pending && repo.is_dirty() {
            if let Err(e) = repo.discard() {
                eprintln!("attenzione: {e:#}");
            }
        }
        if repo.is_dirty() {
            self.unsaved_since.lock().unwrap().get_or_insert_with(Instant::now);
        }
        self.rebuild(&repo);
        *self.last_change.lock().unwrap() = Instant::now();
        out
    }

    fn modify<T>(&self, f: impl FnOnce(&mut Repo) -> FsResult<T>) -> FsResult<T> {
        let _gate = self.gate.lock().unwrap();
        self.change(f)
    }

    /// Salva l'indice se ci sono modifiche in sospeso. Va chiamata tenendo `gate`. In caso di errore
    /// le modifiche restano in memoria e si riprova più tardi: i dati sono già sui backend.
    fn save_locked(&self, repo: &mut Repo) -> FsResult<()> {
        repo.save().map_err(|e| {
            eprintln!("attenzione: salvataggio dell'indice non riuscito, riprovo: {e:#}");
            FsErr::Fail(e)
        })?;
        *self.unsaved_since.lock().unwrap() = None;
        Ok(())
    }

    /// fsync: l'indice si salva adesso.
    fn sync_index(&self) -> FsResult<()> {
        let _gate = self.gate.lock().unwrap();
        self.save_locked(&mut self.repo.write().unwrap())
    }

    /// Lavoro di sottofondo, ogni mezzo secondo: salva l'indice se le modifiche aspettano da
    /// `SAVE_DELAY`, tiene vivo il lock, e lo lascia dopo `LEASE` di calma (salvando prima).
    fn housekeeping(&self) {
        let due = self.unsaved_since.lock().unwrap().is_some_and(|t| t.elapsed() >= SAVE_DELAY);
        let idle = self.last_change.lock().unwrap().elapsed() >= LEASE;
        if !due && !(idle && self.repo.read().unwrap().holds_lock()) {
            return;
        }
        // una modifica in corso (per esempio un caricamento lungo): ci si riprova dopo
        let Ok(_gate) = self.gate.try_lock() else { return };
        let mut repo = self.repo.write().unwrap();
        repo.refresh_lock();
        if repo.is_dirty() && self.save_locked(&mut repo).is_err() {
            return;
        }
        if idle {
            repo.unlock();
        }
    }

    /// Alla fine: salva quello che resta e lascia il lock.
    fn finish(&self) {
        let _gate = self.gate.lock().unwrap();
        let mut repo = self.repo.write().unwrap();
        let _ = self.save_locked(&mut repo);
        repo.unlock();
    }

    /// Carica la copia locale nel repository, se è cambiata. Chi chiude il file aspetta qui.
    fn commit(&self, ino: u64, st: &mut Staging) -> FsResult<()> {
        if !st.dirty {
            return Ok(());
        }
        let _gate = self.gate.lock().unwrap();
        self.acquire()?;
        // il file potrebbe essere stato rinominato (l'inode lo segue) o cancellato
        let Some(path) = self.tree().path(ino).map(str::to_string) else {
            st.dirty = false;
            return Ok(());
        };
        let up = self.repo.read().unwrap().upload(&st.path, Some(st.mtime.unwrap_or_else(now)))?;
        self.change(|repo| Ok(repo.commit_upload(&path, up)?))?;
        st.dirty = false;
        Ok(())
    }

    /// Una nuova copia locale di `file`: vuota con `trunc`, altrimenti con il contenuto attuale.
    fn new_staging(&self, ino: u64, file: &FileNode, trunc: bool) -> FsResult<Staging> {
        let dir = self.staging_dir.as_ref().ok_or(FsErr::Code(Errno::EROFS))?;
        let path = dir.join(format!("{ino}-{}", random_hex(6)));
        let f = fs::OpenOptions::new().read(true).write(true).create_new(true).mode(0o600).open(&path)?;
        let st = Staging { path, file: f, dirty: trunc, mtime: None, opens: 1, closed: false };
        if !trunc {
            let mut off = 0;
            while off < file.size {
                let (piece, _) = read_range(file, off, 4 << 20, &|c| self.chunk(c))?;
                if piece.is_empty() {
                    break;
                }
                st.file.write_all_at(&piece, off)?;
                off += piece.len() as u64;
            }
        }
        Ok(st)
    }

    /// La copia locale di un file aperto in scrittura (la crea al primo descrittore).
    fn open_staging(&self, ino: u64, trunc: bool) -> FsResult<StagingRef> {
        loop {
            let existing = self.staged.lock().unwrap().get(&ino).cloned();
            if let Some(st_ref) = existing {
                let mut st = st_ref.lock().unwrap();
                if st.closed {
                    drop(st);
                    thread::sleep(Duration::from_millis(10));
                    continue;
                }
                st.opens += 1;
                if trunc {
                    st.file.set_len(0)?;
                    st.dirty = true;
                }
                drop(st);
                return Ok(st_ref);
            }
            let file = self.tree().file(ino).ok_or(FsErr::Code(Errno::ENOENT))?;
            let st_ref = Arc::new(Mutex::new(self.new_staging(ino, &file, trunc)?));
            let mut staged = self.staged.lock().unwrap();
            if staged.contains_key(&ino) {
                // un altro descrittore l'ha creata nel frattempo: si usa quella (la nostra si cancella)
                continue;
            }
            staged.insert(ino, st_ref.clone());
            return Ok(st_ref);
        }
    }

    /// Un descrittore in meno; l'ultimo carica le modifiche rimaste e cancella la copia locale.
    fn close_staging(&self, ino: u64, st_ref: &StagingRef) -> FsResult<()> {
        let result = {
            let mut st = st_ref.lock().unwrap();
            st.opens -= 1;
            if st.opens > 0 {
                return Ok(());
            }
            let r = self.commit(ino, &mut st);
            st.closed = true;
            r
        };
        let mut staged = self.staged.lock().unwrap();
        if staged.get(&ino).is_some_and(|s| Arc::ptr_eq(s, st_ref)) {
            staged.remove(&ino);
        }
        result
    }

    fn child_path(&self, parent: u64, name: &OsStr) -> FsResult<String> {
        let tree = self.tree();
        if !tree.is_dir(parent) {
            return Err(FsErr::Code(Errno::ENOENT));
        }
        let base = tree.path(parent).ok_or(FsErr::Code(Errno::ENOENT))?;
        let name = name.to_str().ok_or(FsErr::Code(Errno::EINVAL))?;
        Ok(if base.is_empty() { name.to_string() } else { format!("{base}/{name}") })
    }

    /// Alla chiusura di un descrittore (flush) o con fsync: carica le modifiche.
    fn flush_handle(&self, ino: u64, fh: FileHandle) -> FsResult<()> {
        let Some(st) = self.handle(fh).and_then(|h| h.staging) else { return Ok(()) };
        let mut st = st.lock().unwrap();
        self.commit(ino, &mut st)
    }

    fn need_writable(&self) -> FsResult<()> {
        if self.writable() { Ok(()) } else { Err(FsErr::Code(Errno::EROFS)) }
    }

    fn new_handle(&self, h: Handle) -> FileHandle {
        let fh = self.next_fh.fetch_add(1, Ordering::Relaxed);
        self.handles.lock().unwrap().insert(fh, h);
        FileHandle(fh)
    }

    fn handle(&self, fh: FileHandle) -> Option<Handle> {
        self.handles.lock().unwrap().get(&fh.0).cloned()
    }

    fn do_read(&self, ino: u64, fh: FileHandle, offset: u64, size: u32) -> FsResult<(Vec<u8>, Option<NextChunk>)> {
        // aperto in scrittura da qualcuno: si legge la copia locale
        if let Some(st) = self.staged.lock().unwrap().get(&ino).cloned() {
            let st = st.lock().unwrap();
            let mut buf = vec![0u8; size as usize];
            let mut got = 0;
            while got < buf.len() {
                let n = st.file.read_at(&mut buf[got..], offset + got as u64)?;
                if n == 0 {
                    break;
                }
                got += n;
            }
            buf.truncate(got);
            return Ok((buf, None));
        }
        let file = self.handle(fh).and_then(|h| h.snapshot).or_else(|| self.tree().file(ino));
        let file = file.ok_or(FsErr::Code(Errno::ENOENT))?;
        let (data, last) = read_range(&file, offset, size as u64, &|c| self.chunk(c))?;
        Ok((data, last.map(|i| (file, i))))
    }

    fn do_create(&self, parent: u64, name: &OsStr, flags: i32) -> FsResult<(FileAttr, FileHandle)> {
        self.need_writable()?;
        let path = self.child_path(parent, name)?;
        // il file esiste subito nell'indice, vuoto, così compare nelle cartelle
        let existed = self.modify(|repo| {
            if repo.is_dir(&path) {
                return Err(FsErr::Code(Errno::EISDIR));
            }
            if repo.index().files.contains_key(&path) {
                return if flags & libc::O_EXCL != 0 { Err(FsErr::Code(Errno::EEXIST)) } else { Ok(true) };
            }
            repo.commit_upload(&path, Upload::empty(Some(now())))?;
            Ok(false)
        })?;
        let ino = self.tree().lookup_path(&path).ok_or(FsErr::Code(Errno::ENOENT))?;
        let st = self.open_staging(ino, !existed || flags & libc::O_TRUNC != 0)?;
        if !existed {
            // il file vuoto è già nel repository: non serve ricaricarlo se nessuno scrive
            st.lock().unwrap().dirty = false;
        }
        let attr = self.attr(ino).ok_or(FsErr::Code(Errno::ENOENT))?;
        Ok((attr, self.new_handle(Handle { snapshot: None, staging: Some(st) })))
    }

    fn do_setattr(&self, ino: u64, size: Option<u64>, mtime: Option<TimeOrNow>) -> FsResult<FileAttr> {
        if size.is_some() || mtime.is_some() {
            self.need_writable()?;
        }
        let tree = self.tree();
        if let Some(size) = size {
            if tree.file(ino).is_none() {
                return Err(FsErr::Code(Errno::EISDIR));
            }
            // con o senza descrittori aperti: la copia locale tiene la modifica fino alla chiusura
            let st_ref = self.open_staging(ino, size == 0)?;
            let result = (|| -> FsResult<()> {
                let mut st = st_ref.lock().unwrap();
                st.file.set_len(size)?;
                st.dirty = true;
                Ok(())
            })();
            let closed = self.close_staging(ino, &st_ref);
            result.and(closed)?;
        }
        if let Some(m) = mtime {
            let t = match m {
                TimeOrNow::SpecificTime(t) => secs(t),
                TimeOrNow::Now => now(),
            };
            if tree.file(ino).is_some() {
                let staged = self.staged.lock().unwrap().get(&ino).cloned();
                let pending = match staged {
                    Some(st) => {
                        let mut st = st.lock().unwrap();
                        st.mtime = Some(t);
                        st.dirty
                    }
                    None => false,
                };
                // se nessuno deve ancora caricare il file, la data si scrive subito nell'indice
                if !pending {
                    let path = tree.path(ino).ok_or(FsErr::Code(Errno::ENOENT))?.to_string();
                    self.modify(|repo| Ok(repo.set_mtime(&path, t)?))?;
                }
            }
        }
        self.attr(ino).ok_or(FsErr::Code(Errno::ENOENT))
    }

    fn do_mkdir(&self, parent: u64, name: &OsStr) -> FsResult<FileAttr> {
        self.need_writable()?;
        let path = self.child_path(parent, name)?;
        self.modify(|repo| {
            if repo.is_dir(&path) || repo.index().files.contains_key(&path) {
                return Err(FsErr::Code(Errno::EEXIST));
            }
            Ok(repo.mkdir(&path)?)
        })?;
        let tree = self.tree();
        tree.lookup_path(&path).and_then(|ino| tree.attr(ino, self.owner, true)).ok_or(FsErr::Code(Errno::ENOENT))
    }

    fn do_unlink(&self, parent: u64, name: &OsStr) -> FsResult<()> {
        self.need_writable()?;
        let path = self.child_path(parent, name)?;
        self.modify(|repo| {
            if !repo.index().files.contains_key(&path) {
                return Err(FsErr::Code(if repo.is_dir(&path) { Errno::EISDIR } else { Errno::ENOENT }));
            }
            repo.remove(&path)?;
            Ok(keep_parent(repo, &path)?)
        })?;
        self.inodes.lock().unwrap().forget(false, &path);
        Ok(())
    }

    fn do_rmdir(&self, parent: u64, name: &OsStr) -> FsResult<()> {
        self.need_writable()?;
        let path = self.child_path(parent, name)?;
        self.modify(|repo| {
            if repo.index().files.contains_key(&path) {
                return Err(FsErr::Code(Errno::ENOTDIR));
            }
            if !repo.is_dir(&path) {
                return Err(FsErr::Code(Errno::ENOENT));
            }
            if !repo.index().dirs.contains(&path) {
                // una cartella che esiste solo perché contiene file
                return Err(FsErr::Code(Errno::ENOTEMPTY));
            }
            repo.rmdir(&path).map_err(|_| FsErr::Code(Errno::ENOTEMPTY))?;
            Ok(keep_parent(repo, &path)?)
        })?;
        self.inodes.lock().unwrap().forget(true, &path);
        Ok(())
    }

    fn do_rename(&self, parent: u64, name: &OsStr, newparent: u64, newname: &OsStr, flags: RenameFlags) -> FsResult<()> {
        self.need_writable()?;
        if flags.contains(RenameFlags::RENAME_EXCHANGE) || flags.contains(RenameFlags::RENAME_WHITEOUT) {
            return Err(FsErr::Code(Errno::EINVAL));
        }
        let from = self.child_path(parent, name)?;
        let to = self.child_path(newparent, newname)?;
        self.modify(|repo| {
            let from_file = repo.index().files.contains_key(&from);
            if !from_file && !repo.is_dir(&from) {
                return Err(FsErr::Code(Errno::ENOENT));
            }
            if from == to {
                return Ok(());
            }
            let to_file = repo.index().files.contains_key(&to);
            let to_dir = repo.is_dir(&to);
            if flags.contains(RenameFlags::RENAME_NOREPLACE) && (to_file || to_dir) {
                return Err(FsErr::Code(Errno::EEXIST));
            }
            if from_file && to_dir {
                return Err(FsErr::Code(Errno::EISDIR));
            }
            if !from_file && to_file {
                return Err(FsErr::Code(Errno::ENOTDIR));
            }
            if !from_file && to.starts_with(&format!("{from}/")) {
                return Err(FsErr::Code(Errno::EINVAL));
            }
            if to_dir && !repo.index().dirs.contains(&to) {
                return Err(FsErr::Code(Errno::ENOTEMPTY));
            }
            if to_dir && repo.index().dirs.iter().any(|d| d.starts_with(&format!("{to}/"))) {
                return Err(FsErr::Code(Errno::ENOTEMPTY));
            }
            repo.rename(&from, &to)?;
            keep_parent(repo, &from)?;
            // prima che l'albero venga ricostruito: gli inode seguono i file
            self.inodes.lock().unwrap().rename(!from_file, &from, &to);
            Ok(())
        })
    }
}

struct Fs(Arc<Shared>);

impl fuser::Filesystem for Fs {
    fn init(&mut self, _req: &Request, config: &mut KernelConfig) -> std::io::Result<()> {
        // O_TRUNC arriva con l'apertura invece che come troncamento separato; scritture grandi
        let _ = config.add_capabilities(InitFlags::FUSE_ATOMIC_O_TRUNC | InitFlags::FUSE_BIG_WRITES);
        let _ = config.set_max_write(1 << 20);
        Ok(())
    }

    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let found = name.to_str().and_then(|n| self.0.tree().child(parent.0, n)).and_then(|ino| self.0.attr(ino));
        match found {
            Some(attr) => reply.entry(&TTL, &attr, Generation(0)),
            None => reply.error(Errno::ENOENT),
        }
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, fh: Option<FileHandle>, reply: ReplyAttr) {
        let Some(mut attr) = self.0.attr(ino.0) else { return reply.error(Errno::ENOENT) };
        // un file aperto in lettura mantiene la dimensione che aveva all'apertura
        if let Some(file) = fh.and_then(|fh| self.0.handle(fh)).and_then(|h| h.snapshot) {
            attr.size = file.size;
            attr.blocks = file.size.div_ceil(512);
        }
        reply.attr(&TTL, &attr)
    }

    fn setattr(
        &self,
        _req: &Request,
        ino: INodeNo,
        _mode: Option<u32>,
        _uid: Option<u32>,
        _gid: Option<u32>,
        size: Option<u64>,
        _atime: Option<TimeOrNow>,
        mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        _fh: Option<FileHandle>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<fuser::BsdFileFlags>,
        _kill_suid_gid: bool,
        reply: ReplyAttr,
    ) {
        // permessi e proprietario non si salvano: vengono accettati e ignorati (come rclone mount)
        match self.0.do_setattr(ino.0, size, mtime) {
            Ok(attr) => reply.attr(&TTL, &attr),
            Err(e) => reply.error(code(e, "setattr")),
        }
    }

    fn open(&self, _req: &Request, ino: INodeNo, flags: OpenFlags, _kill: bool, reply: ReplyOpen) {
        let write = flags.acc_mode() != OpenAccMode::O_RDONLY;
        let trunc = flags.0 & libc::O_TRUNC != 0;
        if !write && !trunc {
            let Some(file) = self.0.tree().file(ino.0) else { return reply.error(Errno::ENOENT) };
            let fh = self.0.new_handle(Handle { snapshot: Some(file), staging: None });
            return reply.opened(fh, FopenFlags::empty());
        }
        if !self.0.writable() {
            return reply.error(Errno::EROFS);
        }
        if self.0.tree().file(ino.0).is_none() {
            return reply.error(Errno::EISDIR);
        }
        match self.0.open_staging(ino.0, trunc) {
            Ok(st) => reply.opened(self.0.new_handle(Handle { snapshot: None, staging: Some(st) }), FopenFlags::empty()),
            Err(e) => reply.error(code(e, "apertura in scrittura")),
        }
    }

    fn create(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        flags: i32,
        _kill_suid_gid: bool,
        _owner: Owner,
        reply: ReplyCreate,
    ) {
        match self.0.do_create(parent.0, name, flags) {
            Ok((attr, fh)) => reply.created(&TTL, &attr, Generation(0), fh, FopenFlags::empty()),
            Err(e) => reply.error(code(e, "creazione")),
        }
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
        match self.0.do_read(ino.0, fh, offset, size) {
            Ok((data, last)) => {
                reply.data(&data);
                // lettura anticipata del blocco successivo, in sottofondo
                if let Some((file, i)) = last {
                    if let Some(next) = file.chunks.get(i + 1) {
                        if !self.0.cache.contains(&next.id) {
                            let (shared, next) = (self.0.clone(), next.clone());
                            thread::spawn(move || {
                                let _ = shared.chunk(&next);
                            });
                        }
                    }
                }
            }
            Err(e) => reply.error(code(e, "lettura")),
        }
    }

    fn write(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        data: &[u8],
        _write_flags: WriteFlags,
        _flags: OpenFlags,
        _lock: Option<LockOwner>,
        reply: ReplyWrite,
    ) {
        let Some(st) = self.0.handle(fh).and_then(|h| h.staging) else { return reply.error(Errno::EBADF) };
        let mut st = st.lock().unwrap();
        match st.file.write_all_at(data, offset) {
            Ok(()) => {
                st.dirty = true;
                reply.written(data.len() as u32)
            }
            Err(e) => reply.error(code(FsErr::from(e), "scrittura locale")),
        }
    }

    fn flush(&self, _req: &Request, ino: INodeNo, fh: FileHandle, _lock: LockOwner, reply: ReplyEmpty) {
        match self.0.flush_handle(ino.0, fh) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(code(e, "caricamento")),
        }
    }

    fn fsync(&self, _req: &Request, ino: INodeNo, fh: FileHandle, _datasync: bool, reply: ReplyEmpty) {
        match self.0.flush_handle(ino.0, fh).and_then(|()| self.0.sync_index()) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(code(e, "fsync")),
        }
    }

    fn fsyncdir(&self, _req: &Request, _ino: INodeNo, _fh: FileHandle, _datasync: bool, reply: ReplyEmpty) {
        match self.0.sync_index() {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(code(e, "fsync")),
        }
    }

    fn release(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        _flags: OpenFlags,
        _lock: Option<LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        let handle = self.0.handles.lock().unwrap().remove(&fh.0);
        if let Some(st) = handle.and_then(|h| h.staging) {
            if let Err(e) = self.0.close_staging(ino.0, &st) {
                let _ = code(e, "chiusura");
            }
        }
        reply.ok();
    }

    fn mkdir(&self, _req: &Request, parent: INodeNo, name: &OsStr, _mode: u32, _umask: u32, _owner: Owner, reply: ReplyEntry) {
        match self.0.do_mkdir(parent.0, name) {
            Ok(attr) => reply.entry(&TTL, &attr, Generation(0)),
            Err(e) => reply.error(code(e, "mkdir")),
        }
    }

    fn unlink(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        match self.0.do_unlink(parent.0, name) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(code(e, "cancellazione")),
        }
    }

    fn rmdir(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        match self.0.do_rmdir(parent.0, name) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(code(e, "rmdir")),
        }
    }

    fn rename(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        newparent: INodeNo,
        newname: &OsStr,
        flags: RenameFlags,
        _owner: Option<Owner>,
        reply: ReplyEmpty,
    ) {
        match self.0.do_rename(parent.0, name, newparent.0, newname, flags) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(code(e, "rinomina")),
        }
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
        // in scrittura lo spazio libero è quello della cartella di appoggio, dove passa ogni file scritto
        let free = self.0.staging_dir.as_deref().and_then(free_bytes).unwrap_or(0) / BLOCK;
        let used = tree.bytes.div_ceil(BLOCK);
        reply.statfs(used + free, free, free, tree.files, 0, BLOCK as u32, 255, BLOCK as u32);
    }
}

fn free_bytes(path: &Path) -> Option<u64> {
    let c = CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: c è una stringa C valida e st una struttura della dimensione giusta
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
        return None;
    }
    Some(st.f_bavail as u64 * st.f_frsize as u64)
}

// ---------------------------------------------------------------------------------------------

/// Smonta un mount rimasto appeso (processo morto: "Transport endpoint is not connected").
fn clear_stale(path: &Path) -> Result<()> {
    match fs::metadata(path) {
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

/// Prepara la cartella di appoggio: solo per il proprietario, senza avanzi di un'esecuzione interrotta
/// (file mai caricati: il programma che li scriveva non ha ricevuto conferma).
fn prepare_staging(dir: &Path) -> Result<()> {
    fs::create_dir_all(dir).with_context(|| format!("impossibile creare {}", dir.display()))?;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    let mut removed = 0;
    for e in fs::read_dir(dir)? {
        let e = e?;
        if e.file_type()?.is_file() {
            fs::remove_file(e.path())?;
            removed += 1;
        }
    }
    if removed > 0 {
        eprintln!("cartella di appoggio: tolti {removed} file mai caricati da un'esecuzione interrotta");
    }
    Ok(())
}

/// Monta il repository e resta in attesa finché non arriva SIGTERM/SIGINT/SIGHUP
/// o qualcuno smonta la cartella.
pub fn mount(repo: Repo, opts: MountOptions) -> Result<()> {
    let mountpoint = &opts.mountpoint;
    clear_stale(mountpoint)?;
    fs::create_dir_all(mountpoint).with_context(|| format!("impossibile creare {}", mountpoint.display()))?;
    if let Some(dir) = &opts.staging {
        prepare_staging(dir)?;
    }

    let mut inodes = Inodes::default();
    let tree = Tree::build(repo.index(), &mut inodes);
    eprintln!("{} file ({} MB), indice generazione {}", tree.files, tree.bytes / 1_000_000, tree.generation);
    let writable = opts.staging.is_some();
    let shared = Arc::new(Shared {
        repo: RwLock::new(repo),
        tree: RwLock::new(Arc::new(tree)),
        inodes: Mutex::new(inodes),
        cache: Cache::new(opts.cache_bytes),
        handles: Mutex::new(HashMap::new()),
        staged: Mutex::new(HashMap::new()),
        next_fh: AtomicU64::new(1),
        owner: (opts.uid, opts.gid),
        staging_dir: opts.staging.clone(),
        gate: Mutex::new(()),
        last_change: Mutex::new(Instant::now()),
        unsaved_since: Mutex::new(None),
    });

    let mut cfg = fuser::Config::default();
    cfg.mount_options = vec![
        MountOption::FSName("cloudraid".into()),
        MountOption::Subtype("cloudraid".into()),
        if writable { MountOption::RW } else { MountOption::RO },
        MountOption::NoDev,
        MountOption::NoSuid,
        MountOption::DefaultPermissions,
    ];
    cfg.acl = if opts.allow_other { SessionACL::All } else { SessionACL::Owner };
    cfg.n_threads = Some(opts.threads.max(2));
    let mut session = fuser::Session::new(Fs(shared.clone()), mountpoint, &cfg)
        .with_context(|| format!("impossibile montare {} (serve /dev/fuse)", mountpoint.display()))?;

    // aggiornamento dell'indice (e della configurazione, se è cambiata)
    let refresher = shared.clone();
    let every = opts.refresh;
    let config_path = opts.config.clone();
    let modified = |p: &Option<PathBuf>| p.as_ref().and_then(|p| fs::metadata(p).ok()).and_then(|m| m.modified().ok());
    let mut config_seen = modified(&config_path);
    thread::spawn(move || loop {
        thread::sleep(every);
        let now_modified = modified(&config_path);
        if now_modified != config_seen {
            if let Some(path) = &config_path {
                let _gate = refresher.gate.lock().unwrap();
                let mut repo = refresher.repo.write().unwrap();
                match crate::Config::load(path).and_then(|cfg| repo.reconfigure(cfg)) {
                    Ok(()) => {
                        config_seen = now_modified;
                        refresher.rebuild(&repo);
                        eprintln!("configurazione riletta: {} backend", repo.config().backends.len());
                    }
                    Err(e) => eprintln!("attenzione: configurazione nuova non applicata: {e:#}"),
                }
            }
        }
        match refresher.refresh() {
            Ok(true) => eprintln!("indice aggiornato alla generazione {}", refresher.tree().generation),
            Ok(false) => {}
            Err(e) => eprintln!("attenzione: aggiornamento dell'indice: {e:#}"),
        }
    });
    // salvataggi dell'indice e lock preso per scrivere
    if writable {
        let keeper = shared.clone();
        thread::spawn(move || loop {
            thread::sleep(Duration::from_millis(500));
            keeper.housekeeping();
        });
    }

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

    eprintln!("montato su {} ({})", mountpoint.display(), if writable { "lettura e scrittura" } else { "sola lettura" });
    session.run().context("sessione FUSE interrotta")?;
    // le modifiche in sospeso si salvano e il lock si lascia
    shared.finish();
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
        let attr = t.attr(foto, (1000, 1000), false).unwrap();
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
