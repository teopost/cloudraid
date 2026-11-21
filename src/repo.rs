//! Il repository: tiene insieme backend, chiavi e indice.
//!
//! Percorso di un file in scrittura: blocchi a lunghezza variabile (FastCDC) → deduplicazione
//! → cifratura → Reed-Solomon (k + m frammenti) → ogni frammento su un backend diverso.
//! Più blocchi viaggiano insieme (`transfers`) e i frammenti di un blocco vanno in parallelo.

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::backend::{self, Backend};
use crate::config::Config;
use crate::crypto::{self, random_hex, KeyFile, Keys};
use crate::erasure;
use crate::index::{ChunkEntry, FileEntry, Index, ShardLoc};

const KEYFILE: &str = "cloudraid.key";
const INDEX_DIR: &str = "index";
const LOCK_DIR: &str = "locks";
/// Generazioni dell'indice tenute su ogni backend.
const KEEP_INDEXES: usize = 3;
/// Un lock non rinnovato da così tanto appartiene a un processo morto.
pub const LOCK_STALE_SECS: u64 = 30 * 60;
const LOCK_REFRESH_SECS: u64 = 5 * 60;

pub struct Repo {
    cfg: Config,
    backends: Vec<Box<dyn Backend>>,
    keys: Keys,
    /// La chiave principale cifrata: serve per verificare la password senza rileggere i backend.
    keyfile: KeyFile,
    index: Index,
    dirty: bool,
    /// Frammenti da cancellare dopo il prossimo salvataggio riuscito dell'indice:
    /// cancellarli prima lascerebbe l'indice vecchio con riferimenti a dati spariti.
    pending_deletes: Vec<(String, String)>,
    /// Il lock che teniamo, e quando l'abbiamo rinnovato (Mutex: si rinnova anche da `upload`, con &self).
    lock: Mutex<Option<(LockInfo, u64)>>,
    /// Backend su cui abbiamo già controllato che ci sia `cloudraid.key`.
    keyfile_checked: HashSet<String>,
}

/// Un file caricato con `upload` ma non ancora registrato nell'indice.
pub struct Upload {
    pub size: u64,
    pub hash: String,
    pub mtime: Option<i64>,
    pub chunks: Vec<String>,
    /// Blocchi scritti adesso (gli altri c'erano già).
    pub new: Vec<(String, ChunkEntry)>,
}

impl Upload {
    /// Un file vuoto: niente da caricare.
    pub fn empty(mtime: Option<i64>) -> Upload {
        Upload { size: 0, hash: blake3::hash(b"").to_hex().to_string(), mtime, chunks: Vec::new(), new: Vec::new() }
    }
}

/// Statistiche di lettura.
#[derive(Default, Debug, Clone)]
pub struct ReadStats {
    pub chunks: u64,
    /// Blocchi ricostruiti con la parità perché mancava qualche frammento di dati.
    pub degraded: u64,
}

#[derive(Default, Debug, Clone)]
pub struct ScrubReport {
    pub chunks: u64,
    pub shards_ok: u64,
    pub shards_missing: u64,
    pub shards_corrupt: u64,
    /// Blocchi leggibili ma con frammenti mancanti o danneggiati.
    pub degraded_chunks: u64,
    pub repaired_shards: u64,
    /// Frammenti che non è stato possibile riscrivere (nessun backend adatto).
    pub unrepaired_shards: u64,
    /// Blocchi con meno di k frammenti validi: i dati sono persi.
    pub lost_chunks: Vec<String>,
    pub lost_files: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct BackendStatus {
    pub name: String,
    /// "local" o "rclone".
    pub kind: String,
    pub location: String,
    pub available: bool,
    pub shards: u64,
    pub bytes: u64,
}

/// Il contenuto di un file `locks/<id>.json`.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct LockInfo {
    pub id: String,
    pub host: String,
    pub pid: u32,
    /// Unix time dell'ultimo rinnovo.
    pub time: u64,
}

enum Shard {
    Ok(Vec<u8>),
    Missing,
    Corrupt,
}

fn shard_key(id: &str, i: usize) -> String {
    format!("data/{}/{}.{}", &id[..2], id, i)
}

fn index_key(generation: u64) -> String {
    format!("{INDEX_DIR}/{generation:020}.bin")
}

fn parse_index_key(key: &str) -> Option<u64> {
    key.strip_prefix(INDEX_DIR)?.strip_prefix('/')?.strip_suffix(".bin")?.parse().ok()
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn hostname() -> String {
    fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|h| h.trim().to_string())
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "sconosciuto".into())
}

/// Esegue `f` su ogni elemento in parallelo, un thread per elemento, e restituisce i risultati in ordine.
fn parallel<T: Sync, R: Send>(items: &[T], f: impl Fn(&T) -> R + Sync) -> Vec<R> {
    if items.len() <= 1 {
        return items.iter().map(&f).collect();
    }
    let f = &f;
    thread::scope(|s| {
        let handles: Vec<_> = items.iter().map(|it| s.spawn(move || f(it))).collect();
        handles.into_iter().map(|h| h.join().expect("thread di lavoro interrotto")).collect()
    })
}

/// Percorso dentro il repository: senza `/` iniziale, senza `.` né `..`.
pub fn normalize(path: &str) -> Result<String> {
    let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty() && *p != ".").collect();
    if parts.contains(&"..") {
        bail!("percorso non valido: {path}");
    }
    Ok(parts.join("/"))
}

impl Repo {
    /// Crea un repository nuovo su tutti i backend della configurazione.
    pub fn init(cfg: Config, password: &str) -> Result<Repo> {
        cfg.validate()?;
        let backends = backend::open_all(&cfg)?;
        for b in &backends {
            b.create()?;
            if b.get(KEYFILE)?.is_some() {
                bail!("il backend {} contiene già un repository cloudraid", b.name());
            }
        }
        let (kf, keys) = crypto::create(password)?;
        let json = serde_json::to_vec_pretty(&kf)?;
        for b in &backends {
            b.put(KEYFILE, &json)?;
        }
        let mut repo = Repo {
            cfg,
            backends,
            keys,
            keyfile: kf,
            index: Index::default(),
            dirty: true,
            pending_deletes: Vec::new(),
            lock: Mutex::new(None),
            keyfile_checked: HashSet::new(),
        };
        repo.save()?;
        Ok(repo)
    }

    /// Apre un repository esistente: basta un backend con la chiave e l'indice.
    pub fn open(cfg: Config, password: &str) -> Result<Repo> {
        cfg.validate()?;
        let backends = backend::open_all(&cfg)?;
        // verifica la disponibilità di tutti i backend in parallelo (per rclone è una chiamata di rete)
        let avail = parallel(&backends, |b| b.is_available());
        if !avail.contains(&true) {
            bail!("nessun backend disponibile");
        }

        let mut unlocked = None;
        for b in backends.iter().filter(|b| b.is_available()) {
            let Ok(Some(data)) = b.get(KEYFILE) else { continue };
            let Ok(kf) = serde_json::from_slice::<KeyFile>(&data) else {
                eprintln!("attenzione: {KEYFILE} danneggiato sul backend {}", b.name());
                continue;
            };
            unlocked = Some((crypto::unlock(&kf, password)?, kf));
            break;
        }
        let (keys, keyfile) = unlocked.ok_or_else(|| {
            anyhow!("nessun backend disponibile contiene la chiave del repository (serve `cloudraid init`?)")
        })?;

        let index = load_latest_index(&backends, &keys)?;
        Ok(Repo {
            cfg,
            backends,
            keys,
            keyfile,
            index,
            dirty: false,
            pending_deletes: Vec::new(),
            lock: Mutex::new(None),
            keyfile_checked: HashSet::new(),
        })
    }

    pub fn index(&self) -> &Index {
        &self.index
    }

    pub fn config(&self) -> &Config {
        &self.cfg
    }

    pub fn keyfile(&self) -> &KeyFile {
        &self.keyfile
    }

    /// La password apre questo repository? Non tocca i backend.
    pub fn verify_password(&self, password: &str) -> bool {
        crypto::unlock(&self.keyfile, password).is_ok()
    }

    /// Per i processi che restano aperti a lungo: verifica di nuovo i backend e rilegge l'indice
    /// se un altro processo ne ha salvato uno più recente.
    pub fn reload(&mut self) -> Result<()> {
        if self.dirty {
            bail!("ci sono modifiche non salvate: impossibile rileggere l'indice");
        }
        if let Some(index) = self.newer_index(self.index.generation)? {
            self.index = index;
        }
        Ok(())
    }

    /// Verifica di nuovo i backend e restituisce l'indice più recente, ma solo se la sua
    /// generazione è maggiore di `generation`: se nessuno ha salvato niente costa solo un elenco.
    /// Non cambia il repository: serve a chi tiene una propria copia dell'indice (il mount).
    pub fn newer_index(&self, generation: u64) -> Result<Option<Index>> {
        for b in &self.backends {
            b.refresh();
        }
        parallel(&self.backends, |b| b.is_available());
        let candidates = list_indexes(&self.backends);
        match candidates.first() {
            None => bail!("indice non trovato su nessun backend disponibile"),
            Some((latest, _, _)) if *latest <= generation => Ok(None),
            Some(_) => load_index(&self.backends, &self.keys, &candidates).map(Some),
        }
    }

    /// Abbandona le modifiche non salvate e torna all'indice dei backend.
    pub fn discard(&mut self) -> Result<()> {
        self.dirty = false;
        self.pending_deletes.clear();
        self.index.generation = 0;
        self.reload()
    }

    fn backend(&self, name: &str) -> Option<&dyn Backend> {
        self.backends.iter().find(|b| b.name() == name).map(|b| b.as_ref())
    }

    fn available(&self) -> Vec<usize> {
        (0..self.backends.len()).filter(|&i| self.backends[i].is_available()).collect()
    }

    // -----------------------------------------------------------------------------------------
    // Lock: un solo processo alla volta può modificare il repository.

    fn read_locks(&self) -> Vec<LockInfo> {
        let lists = parallel(&self.backends, |b| {
            if !b.is_available() {
                return Vec::new();
            }
            b.list(LOCK_DIR)
                .unwrap_or_default()
                .iter()
                .filter_map(|k| b.get(k).ok().flatten())
                .filter_map(|data| serde_json::from_slice::<LockInfo>(&data).ok())
                .collect()
        });
        let mut by_id: BTreeMap<String, LockInfo> = BTreeMap::new();
        for lock in lists.into_iter().flatten() {
            match by_id.get(&lock.id) {
                Some(old) if old.time >= lock.time => {}
                _ => {
                    by_id.insert(lock.id.clone(), lock);
                }
            }
        }
        by_id.into_values().collect()
    }

    /// I lock presenti sul repository (anche scaduti), il nostro escluso.
    pub fn locks(&self) -> Vec<LockInfo> {
        let mine = self.lock.lock().unwrap().as_ref().map(|(l, _)| l.id.clone());
        self.read_locks().into_iter().filter(|l| Some(&l.id) != mine.as_ref()).collect()
    }

    fn write_lock(&self, lock: &LockInfo) -> Result<()> {
        let data = serde_json::to_vec(lock)?;
        let key = format!("{LOCK_DIR}/{}.json", lock.id);
        let results = parallel(&self.backends, |b| if b.is_available() { Some(b.put(&key, &data)) } else { None });
        let ok = results.iter().filter(|r| matches!(r, Some(Ok(())))).count();
        if ok == 0 {
            let errors: Vec<String> = results.into_iter().flatten().filter_map(|r| r.err()).map(|e| format!("{e:#}")).collect();
            bail!("impossibile scrivere il lock: {}", errors.join("; "));
        }
        Ok(())
    }

    fn delete_lock(&self, id: &str) {
        let key = format!("{LOCK_DIR}/{id}.json");
        parallel(&self.backends, |b| {
            if b.is_available() {
                if let Err(e) = b.delete(&key) {
                    eprintln!("attenzione: {e:#}");
                }
            }
        });
    }

    /// Prende il lock esclusivo. Fallisce se un altro processo lo tiene (e non è scaduto).
    pub fn lock(&mut self) -> Result<()> {
        if self.holds_lock() {
            return Ok(());
        }
        let busy = |locks: Vec<LockInfo>, mine: &str| {
            locks.into_iter().find(|l| l.id != mine && now().saturating_sub(l.time) < LOCK_STALE_SECS)
        };
        let describe = |l: &LockInfo| {
            format!(
                "repository in uso da {} (pid {}, lock rinnovato {} s fa). Se quel processo non esiste più: `cloudraid unlock`",
                l.host,
                l.pid,
                now().saturating_sub(l.time)
            )
        };
        if let Some(other) = busy(self.read_locks(), "") {
            bail!("{}", describe(&other));
        }
        let mine = LockInfo { id: random_hex(8), host: hostname(), pid: std::process::id(), time: now() };
        self.write_lock(&mine)?;
        // un altro processo potrebbe aver scritto il suo lock nello stesso istante: si ricontrolla
        if let Some(other) = busy(self.read_locks(), &mine.id) {
            self.delete_lock(&mine.id);
            bail!("{}", describe(&other));
        }
        let t = mine.time;
        *self.lock.lock().unwrap() = Some((mine, t));
        // chi prende il lock lavora sull'indice più recente: un altro processo potrebbe averlo
        // cambiato dopo la nostra apertura (si eviterebbe di sovrascrivere le sue modifiche)
        if !self.dirty {
            if let Ok(index) = load_latest_index(&self.backends, &self.keys) {
                if index.generation > self.index.generation {
                    self.index = index;
                }
            }
        }
        Ok(())
    }

    /// Rinnova il lock se è passato abbastanza tempo; chiamato durante le operazioni lunghe.
    pub fn refresh_lock(&self) {
        let mut lock = {
            let held = self.lock.lock().unwrap();
            let Some((lock, refreshed)) = held.as_ref() else { return };
            if now().saturating_sub(*refreshed) < LOCK_REFRESH_SECS {
                return;
            }
            lock.clone()
        };
        lock.time = now();
        match self.write_lock(&lock) {
            Ok(()) => {
                let t = lock.time;
                let mut held = self.lock.lock().unwrap();
                // nel frattempo potrebbe essere stato rilasciato
                if held.as_ref().is_some_and(|(l, _)| l.id == lock.id) {
                    *held = Some((lock, t));
                }
            }
            Err(e) => eprintln!("attenzione: rinnovo del lock: {e:#}"),
        }
    }

    /// Teniamo il lock?
    pub fn holds_lock(&self) -> bool {
        self.lock.lock().unwrap().is_some()
    }

    /// Ci sono modifiche all'indice non ancora salvate?
    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    pub fn unlock(&mut self) {
        let taken = self.lock.lock().unwrap().take();
        if let Some((lock, _)) = taken {
            self.delete_lock(&lock.id);
        }
    }

    /// Cancella i lock degli altri processi: solo quelli scaduti, oppure tutti con `all`.
    pub fn break_locks(&self, all: bool) -> Vec<LockInfo> {
        let removed: Vec<LockInfo> =
            self.locks().into_iter().filter(|l| all || now().saturating_sub(l.time) >= LOCK_STALE_SECS).collect();
        for l in &removed {
            self.delete_lock(&l.id);
        }
        removed
    }

    // -----------------------------------------------------------------------------------------

    /// Copia `cloudraid.key` sui backend disponibili che non ce l'hanno: succede con un backend
    /// aggiunto alla configurazione dopo `init`. Senza la chiave, se sparissero i backend originali
    /// il repository non si aprirebbe più. Ogni backend si controlla una volta sola per processo.
    fn sync_keyfile(&mut self) {
        let todo: Vec<usize> = self
            .available()
            .into_iter()
            .filter(|&i| !self.keyfile_checked.contains(self.backends[i].name()))
            .collect();
        if todo.is_empty() {
            return;
        }
        let ours = match serde_json::to_value(&self.keyfile) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("attenzione: {e:#}");
                return;
            }
        };
        let results = parallel(&todo, |&i| -> Result<()> {
            let b = &self.backends[i];
            match b.get(KEYFILE)? {
                Some(data) => {
                    if serde_json::from_slice::<serde_json::Value>(&data).ok().as_ref() != Some(&ours) {
                        bail!("il backend {} contiene la chiave di un altro repository", b.name());
                    }
                    Ok(())
                }
                None => b.put(KEYFILE, &serde_json::to_vec_pretty(&ours)?),
            }
        });
        for (&i, res) in todo.iter().zip(results) {
            match res {
                Ok(()) => {
                    self.keyfile_checked.insert(self.backends[i].name().to_string());
                }
                Err(e) => eprintln!("attenzione: {e:#}"),
            }
        }
    }

    /// Salva l'indice (nuova generazione) su tutti i backend disponibili, poi esegue
    /// le cancellazioni in sospeso.
    pub fn save(&mut self) -> Result<()> {
        if !self.dirty {
            return Ok(());
        }
        self.sync_keyfile();
        self.index.generation += 1;
        let sealed = self.keys.seal(&serde_json::to_vec(&self.index)?);
        let key = index_key(self.index.generation);
        let results = parallel(&self.backends, |b| {
            if !b.is_available() {
                return None;
            }
            Some(b.put(&key, &sealed).map(|()| {
                if let Err(e) = prune_indexes(b.as_ref()) {
                    eprintln!("attenzione: {e:#}");
                }
            }))
        });
        let saved = results.iter().filter(|r| matches!(r, Some(Ok(())))).count();
        let errors: Vec<String> = results.into_iter().flatten().filter_map(|r| r.err()).map(|e| format!("{e:#}")).collect();
        if saved == 0 {
            self.index.generation -= 1;
            bail!("impossibile salvare l'indice: {}", errors.join("; "));
        }
        for e in errors {
            eprintln!("attenzione: indice non salvato: {e}");
        }
        self.dirty = false;
        let deletes = std::mem::take(&mut self.pending_deletes);
        parallel(&deletes, |(name, key)| {
            if let Some(b) = self.backend(name).filter(|b| b.is_available()) {
                if let Err(e) = b.delete(key) {
                    eprintln!("attenzione: {e:#}");
                }
            }
        });
        Ok(())
    }

    /// Carica un file locale nel repository come `dest`. Restituisce i blocchi nuovi scritti.
    pub fn put_file(&mut self, src: &Path, dest: &str) -> Result<u64> {
        let dest = self.check_file_dest(dest)?;
        let up = self.upload(src, None)?;
        let new_chunks = up.new.len() as u64;
        self.commit_upload(&dest, up)?;
        Ok(new_chunks)
    }

    /// `dest` normalizzato, se lì può stare un file (non la radice, non una cartella).
    fn check_file_dest(&self, dest: &str) -> Result<String> {
        let dest = normalize(dest)?;
        if dest.is_empty() {
            bail!("percorso di destinazione vuoto");
        }
        if self.is_dir(&dest) {
            bail!("{dest} è una cartella nel repository");
        }
        Ok(dest)
    }

    /// Spezza `src` in blocchi e scrive quelli che il repository non ha ancora, senza toccare
    /// l'indice: il risultato si registra con `commit_upload`. Va chiamata tenendo il lock
    /// (`lock`), così nessun altro processo scrive gli stessi blocchi nello stesso momento.
    /// `mtime` sostituisce la data del file sorgente. In caso di errore i frammenti già scritti
    /// vengono cancellati.
    pub fn upload(&self, src: &Path, mtime: Option<i64>) -> Result<Upload> {
        let file = fs::File::open(src).with_context(|| format!("impossibile aprire {}", src.display()))?;
        let mtime = mtime.or_else(|| {
            file.metadata()
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64)
        });

        let c = self.cfg.chunk;
        let mut cdc = fastcdc::v2020::StreamCDC::new(file, c.min, c.avg, c.max);
        let mut hasher = blake3::Hasher::new();
        let mut up = Upload { size: 0, hash: String::new(), mtime, chunks: Vec::new(), new: Vec::new() };
        let mut stored: HashSet<String> = HashSet::new();
        let result: Result<()> = (|| loop {
            // un gruppo di `transfers` blocchi alla volta
            let mut batch: Vec<(String, Vec<u8>)> = Vec::new();
            for chunk in cdc.by_ref().take(self.cfg.transfers) {
                let chunk = chunk.map_err(|e| anyhow!("lettura di {}: {e}", src.display()))?;
                hasher.update(&chunk.data);
                up.size += chunk.length as u64;
                batch.push((self.keys.chunk_id(&chunk.data), chunk.data));
            }
            if batch.is_empty() {
                return Ok(());
            }
            let mut seen = HashSet::new();
            let to_store: Vec<&(String, Vec<u8>)> = batch
                .iter()
                .filter(|(id, _)| !self.index.chunks.contains_key(id) && !stored.contains(id) && seen.insert(id.clone()))
                .collect();
            let results = parallel(&to_store, |(id, data)| self.store_chunk(id, data));
            let mut first_error = None;
            for (item, res) in to_store.iter().zip(results) {
                match res {
                    Ok(entry) => {
                        stored.insert(item.0.clone());
                        up.new.push((item.0.clone(), entry));
                    }
                    Err(e) => {
                        first_error.get_or_insert(e);
                    }
                }
            }
            if let Some(e) = first_error {
                return Err(e);
            }
            up.chunks.extend(batch.into_iter().map(|(id, _)| id));
            self.refresh_lock();
        })();
        if let Err(e) = result {
            self.delete_shards(&up.new);
            return Err(e);
        }
        up.hash = hasher.finalize().to_hex().to_string();
        Ok(up)
    }

    /// Cancella subito i frammenti di blocchi mai entrati nell'indice.
    fn delete_shards(&self, chunks: &[(String, ChunkEntry)]) {
        for (id, entry) in chunks {
            for (i, loc) in entry.shards.iter().enumerate() {
                if let Some(b) = self.backend(&loc.backend) {
                    if let Err(e) = b.delete(&shard_key(id, i)) {
                        eprintln!("attenzione: {e:#}");
                    }
                }
            }
        }
    }

    /// Registra nell'indice un file caricato con `upload` (sostituisce quello che c'era).
    pub fn commit_upload(&mut self, dest: &str, up: Upload) -> Result<()> {
        let dest = match self.check_file_dest(dest) {
            Ok(d) => d,
            Err(e) => {
                self.delete_shards(&up.new);
                return Err(e);
            }
        };
        // un blocco già noto doveva esserci al momento del caricamento: se intanto è sparito
        // (cancellato da un processo che non rispettava il lock) il file sarebbe incompleto
        let new_ids: HashSet<&str> = up.new.iter().map(|(id, _)| id.as_str()).collect();
        if let Some(missing) = up.chunks.iter().find(|id| !new_ids.contains(id.as_str()) && !self.index.chunks.contains_key(*id)) {
            self.delete_shards(&up.new);
            bail!("il blocco {} è sparito durante il caricamento: riprova", &missing[..12]);
        }
        for (id, entry) in up.new {
            self.index.chunks.entry(id).or_insert(entry);
        }
        for id in &up.chunks {
            self.index.chunks.get_mut(id).expect("blocco presente").refs += 1;
        }
        let entry = FileEntry { size: up.size, hash: up.hash, mtime: up.mtime, chunks: up.chunks };
        if let Some(old) = self.index.files.insert(dest.clone(), entry) {
            self.release(&old.chunks);
        }
        self.dirty = true;
        Ok(())
    }

    /// `path` è una cartella (esplicita o perché contiene file)? La radice lo è sempre.
    pub fn is_dir(&self, path: &str) -> bool {
        if path.is_empty() || self.index.dirs.contains(path) {
            return true;
        }
        let prefix = format!("{path}/");
        self.index.files.range(prefix.clone()..).next().is_some_and(|(f, _)| f.starts_with(&prefix))
            || self.index.dirs.range(prefix.clone()..).next().is_some_and(|d| d.starts_with(&prefix))
    }

    /// Crea una cartella vuota (anche le cartelle intermedie, implicitamente).
    pub fn mkdir(&mut self, path: &str) -> Result<()> {
        let path = normalize(path)?;
        if path.is_empty() || self.is_dir(&path) {
            return Ok(());
        }
        if self.index.files.contains_key(&path) {
            bail!("{path} è un file");
        }
        // nessun antenato può essere un file
        let parts: Vec<&str> = path.split('/').collect();
        for i in 1..parts.len() {
            let anc = parts[..i].join("/");
            if self.index.files.contains_key(&anc) {
                bail!("{anc} è un file");
            }
        }
        self.index.dirs.insert(path);
        self.dirty = true;
        Ok(())
    }

    /// Toglie una cartella vuota (la cartella che la contiene resta).
    pub fn rmdir(&mut self, path: &str) -> Result<()> {
        let path = normalize(path)?;
        if path.is_empty() {
            bail!("la radice non si può togliere");
        }
        let prefix = format!("{path}/");
        if self.index.files.keys().any(|f| f.starts_with(&prefix)) || self.index.dirs.iter().any(|d| d.starts_with(&prefix)) {
            bail!("{path}: la cartella non è vuota");
        }
        if !self.index.dirs.remove(&path) {
            bail!("{path}: cartella non presente");
        }
        // la cartella madre resta, anche se ora è vuota
        if let Some((parent, _)) = path.rsplit_once('/') {
            if !self.is_dir(parent) {
                self.index.dirs.insert(parent.to_string());
            }
        }
        self.dirty = true;
        Ok(())
    }

    /// Cambia la data di modifica di un file, senza riscriverlo.
    pub fn set_mtime(&mut self, path: &str, mtime: i64) -> Result<()> {
        let path = normalize(path)?;
        let f = self.index.files.get_mut(&path).ok_or_else(|| anyhow!("{path}: file non presente"))?;
        if f.mtime != Some(mtime) {
            f.mtime = Some(mtime);
            self.dirty = true;
        }
        Ok(())
    }

    /// Rinomina o sposta un file o una cartella: cambia solo l'indice, i dati restano dove sono.
    /// Un file di destinazione esistente viene sostituito; una cartella di destinazione solo se vuota.
    pub fn rename(&mut self, from: &str, to: &str) -> Result<()> {
        let (from, to) = (normalize(from)?, normalize(to)?);
        if from.is_empty() || to.is_empty() {
            bail!("la radice non si può rinominare");
        }
        if from == to {
            return Ok(());
        }
        if to.starts_with(&format!("{from}/")) {
            bail!("impossibile spostare {from} dentro sé stessa");
        }
        if self.index.files.contains_key(&from) {
            if self.is_dir(&to) {
                bail!("{to} è una cartella");
            }
            let entry = self.index.files.remove(&from).expect("presente");
            if let Some(old) = self.index.files.insert(to.clone(), entry) {
                self.release(&old.chunks);
            }
        } else if self.is_dir(&from) {
            if self.index.files.contains_key(&to) {
                bail!("{to} è un file");
            }
            let to_prefix = format!("{to}/");
            if self.index.files.keys().any(|f| f.starts_with(&to_prefix)) || self.index.dirs.iter().any(|d| d.starts_with(&to_prefix)) {
                bail!("{to}: la cartella di destinazione non è vuota");
            }
            self.index.dirs.remove(&to);
            let from_prefix = format!("{from}/");
            let moved = |p: &str| format!("{to}{}", &p[from.len()..]);
            let files: Vec<String> = self.index.files.keys().filter(|f| f.starts_with(&from_prefix)).cloned().collect();
            for f in files {
                let entry = self.index.files.remove(&f).expect("presente");
                self.index.files.insert(moved(&f), entry);
            }
            let dirs: Vec<String> =
                self.index.dirs.iter().filter(|d| **d == from || d.starts_with(&from_prefix)).cloned().collect();
            for d in dirs {
                self.index.dirs.remove(&d);
                self.index.dirs.insert(moved(&d));
            }
        } else {
            bail!("{from}: non presente");
        }
        self.dirty = true;
        Ok(())
    }

    /// Cifra un blocco, lo divide in k + m frammenti e li scrive in parallelo su backend tutti diversi.
    fn store_chunk(&self, id: &str, plain: &[u8]) -> Result<ChunkEntry> {
        let (k, m) = (self.cfg.data_shards, self.cfg.parity_shards);
        let n = k + m;
        let sealed = self.keys.seal(plain);
        let (shard_len, shards) = erasure::encode(&sealed, k, m)?;

        let avail = self.available();
        if avail.len() < n {
            bail!(
                "servono {n} backend disponibili ({k} dati + {m} parità), ce ne sono {}: scrittura annullata",
                avail.len()
            );
        }
        // si parte da un backend che dipende dall'id, così i frammenti di dati si distribuiscono
        let start = usize::from_str_radix(&id[..4], 16)? % avail.len();
        let order: Vec<usize> = (0..avail.len()).map(|j| avail[(start + j) % avail.len()]).collect();

        let jobs: Vec<usize> = (0..n).collect();
        let first = parallel(&jobs, |&i| self.backends[order[i]].put(&shard_key(id, i), &shards[i]));
        let mut placed: Vec<Option<usize>> = vec![None; n];
        for (i, res) in first.into_iter().enumerate() {
            match res {
                Ok(()) => placed[i] = Some(order[i]),
                Err(e) => eprintln!("attenzione: {e:#}; provo un altro backend"),
            }
        }
        // i frammenti falliti vanno sui backend di riserva, se ce ne sono
        let mut spares = order[n..].iter().copied();
        for i in 0..n {
            while placed[i].is_none() {
                let Some(bi) = spares.next() else {
                    for (j, bi) in placed.iter().enumerate() {
                        if let Some(bi) = bi {
                            let _ = self.backends[*bi].delete(&shard_key(id, j));
                        }
                    }
                    bail!("impossibile scrivere il blocco {}: troppi backend in errore", &id[..12]);
                };
                match self.backends[bi].put(&shard_key(id, i), &shards[i]) {
                    Ok(()) => placed[i] = Some(bi),
                    Err(e) => eprintln!("attenzione: {e:#}"),
                }
            }
        }
        let locs = placed
            .iter()
            .zip(&shards)
            .map(|(bi, shard)| ShardLoc {
                backend: self.backends[bi.expect("frammento piazzato")].name().to_string(),
                hash: blake3::hash(shard).to_hex().to_string(),
            })
            .collect();
        Ok(ChunkEntry {
            plain_len: plain.len() as u64,
            sealed_len: sealed.len() as u64,
            k: k as u8,
            m: m as u8,
            shard_len: shard_len as u64,
            shards: locs,
            refs: 0,
        })
    }

    /// Toglie un blocco dall'indice; i suoi frammenti si cancellano dopo il prossimo salvataggio.
    fn forget_chunk(&mut self, id: &str) {
        if let Some(entry) = self.index.chunks.remove(id) {
            for (i, loc) in entry.shards.iter().enumerate() {
                self.pending_deletes.push((loc.backend.clone(), shard_key(id, i)));
            }
            self.dirty = true;
        }
    }

    /// Toglie un riferimento a ogni blocco e dimentica quelli non più usati.
    fn release(&mut self, ids: &[String]) {
        for id in ids {
            if let Some(e) = self.index.chunks.get_mut(id) {
                e.refs = e.refs.saturating_sub(1);
                if e.refs == 0 {
                    self.forget_chunk(id);
                }
            }
            self.dirty = true;
        }
    }

    fn fetch_shard(&self, loc: &ShardLoc, key: &str) -> Shard {
        let Some(b) = self.backend(&loc.backend).filter(|b| b.is_available()) else {
            return Shard::Missing;
        };
        match b.get(key) {
            Ok(Some(data)) if blake3::hash(&data).to_hex().as_str() == loc.hash => Shard::Ok(data),
            Ok(Some(_)) => Shard::Corrupt,
            Ok(None) => Shard::Missing,
            Err(e) => {
                eprintln!("attenzione: {e:#}");
                Shard::Missing
            }
        }
    }

    /// Legge in parallelo i frammenti `idx` di un blocco.
    fn fetch_shards(&self, id: &str, entry: &ChunkEntry, idx: &[usize]) -> Vec<Shard> {
        parallel(idx, |&i| self.fetch_shard(&entry.shards[i], &shard_key(id, i)))
    }

    /// Legge un blocco: prima i frammenti di dati, la parità solo se ne manca qualcuno.
    /// Restituisce i dati e se è servita la parità.
    fn load_chunk(&self, id: &str) -> Result<(Vec<u8>, bool)> {
        let e = self.index.chunks.get(id).ok_or_else(|| anyhow!("blocco {} assente dall'indice", &id[..12]))?;
        self.read_chunk(id, e)
    }

    /// Legge un blocco descritto da `e`, che può venire da una copia dell'indice tenuta altrove
    /// (il mount). Il contenuto è verificato con la sua impronta. Restituisce i dati e se è
    /// servita la parità.
    pub fn read_chunk(&self, id: &str, e: &ChunkEntry) -> Result<(Vec<u8>, bool)> {
        let (k, m) = (e.k as usize, e.m as usize);
        let mut shards: Vec<Option<Vec<u8>>> = vec![None; k + m];
        let data_idx: Vec<usize> = (0..k).collect();
        for (i, s) in data_idx.iter().zip(self.fetch_shards(id, e, &data_idx)) {
            if let Shard::Ok(d) = s {
                shards[*i] = Some(d);
            }
        }
        let degraded = shards[..k].iter().any(Option::is_none);
        if degraded {
            let parity_idx: Vec<usize> = (k..k + m).collect();
            for (i, s) in parity_idx.iter().zip(self.fetch_shards(id, e, &parity_idx)) {
                if let Shard::Ok(d) = s {
                    shards[*i] = Some(d);
                }
            }
        }
        let have = shards.iter().filter(|s| s.is_some()).count();
        if have < k {
            bail!("blocco {} irrecuperabile: {have} frammenti validi su {k} necessari", &id[..12]);
        }
        let sealed = erasure::decode(k, m, e.sealed_len as usize, shards)?;
        let plain = self.keys.open(&sealed)?;
        if self.keys.chunk_id(&plain) != id {
            bail!("il blocco {} non corrisponde alla sua impronta", &id[..12]);
        }
        Ok((plain, degraded))
    }

    /// Estrae un file del repository in `out` (scrittura atomica, impronta verificata).
    pub fn get_file(&self, path: &str, out: &Path, stats: &mut ReadStats) -> Result<()> {
        let path = normalize(path)?;
        let entry = self.index.files.get(&path).ok_or_else(|| anyhow!("{path}: file non presente"))?;
        if let Some(dir) = out.parent().filter(|d| !d.as_os_str().is_empty()) {
            fs::create_dir_all(dir).with_context(|| format!("impossibile creare {}", dir.display()))?;
        }
        let mut tmp = out.as_os_str().to_owned();
        tmp.push(".cloudraid-tmp");
        let tmp = PathBuf::from(tmp);
        let result: Result<()> = (|| {
            let mut f = fs::File::create(&tmp).with_context(|| format!("impossibile creare {}", tmp.display()))?;
            let mut hasher = blake3::Hasher::new();
            for group in entry.chunks.chunks(self.cfg.transfers) {
                for loaded in parallel(group, |id| self.load_chunk(id)) {
                    let (data, degraded) = loaded?;
                    stats.chunks += 1;
                    stats.degraded += degraded as u64;
                    hasher.update(&data);
                    f.write_all(&data)?;
                }
            }
            f.sync_all()?;
            if hasher.finalize().to_hex().as_str() != entry.hash {
                bail!("{path}: l'impronta del file ricostruito non corrisponde");
            }
            Ok(())
        })();
        match result {
            Ok(()) => fs::rename(&tmp, out).with_context(|| format!("impossibile scrivere {}", out.display())),
            Err(e) => {
                let _ = fs::remove_file(&tmp);
                Err(e.context(format!("estrazione di {path} non riuscita")))
            }
        }
    }

    /// File del repository uguali a `path` o contenuti nella cartella `path`.
    pub fn matching(&self, path: &str) -> Result<Vec<String>> {
        let path = normalize(path)?;
        if self.index.files.contains_key(&path) {
            return Ok(vec![path]);
        }
        let prefix = if path.is_empty() { String::new() } else { format!("{path}/") };
        Ok(self.index.files.keys().filter(|f| f.starts_with(&prefix)).cloned().collect())
    }

    /// Toglie un file (o una cartella) dal repository. Restituisce i file tolti.
    pub fn remove(&mut self, path: &str) -> Result<Vec<String>> {
        let mut files = self.matching(path)?;
        for f in &files {
            if let Some(entry) = self.index.files.remove(f) {
                self.release(&entry.chunks);
            }
        }
        // anche le cartelle vuote esplicite, dentro o uguali a `path`
        let path = normalize(path)?;
        let prefix = if path.is_empty() { String::new() } else { format!("{path}/") };
        let dirs: Vec<String> = self.index.dirs.iter().filter(|d| **d == path || d.starts_with(&prefix)).cloned().collect();
        for d in &dirs {
            self.index.dirs.remove(d);
            self.dirty = true;
        }
        if files.is_empty() && !dirs.is_empty() {
            files.push(format!("{path}/"));
        }
        Ok(files)
    }

    pub fn status(&self) -> Vec<BackendStatus> {
        let mut out: Vec<BackendStatus> = self
            .backends
            .iter()
            .map(|b| BackendStatus {
                name: b.name().to_string(),
                kind: b.kind().to_string(),
                location: b.location(),
                available: b.is_available(),
                shards: 0,
                bytes: 0,
            })
            .collect();
        for e in self.index.chunks.values() {
            for loc in &e.shards {
                if let Some(s) = out.iter_mut().find(|s| s.name == loc.backend) {
                    s.shards += 1;
                    s.bytes += e.shard_len;
                }
            }
        }
        out
    }

    /// Controlla tutti i frammenti. Con `repair` ricostruisce quelli mancanti o danneggiati,
    /// sul backend originale se è disponibile, altrimenti su un altro che non abbia già
    /// un frammento dello stesso blocco.
    pub fn scrub(&mut self, repair: bool) -> Result<ScrubReport> {
        self.scrub_with_progress(repair, &mut |_, _| {})
    }

    /// Come `scrub`, chiamando `progress(blocchi controllati, totale)` dopo ogni gruppo.
    pub fn scrub_with_progress(&mut self, repair: bool, progress: &mut dyn FnMut(u64, u64)) -> Result<ScrubReport> {
        let mut report = ScrubReport::default();
        let mut load: HashMap<String, u64> = HashMap::new();
        for e in self.index.chunks.values() {
            for loc in &e.shards {
                *load.entry(loc.backend.clone()).or_default() += 1;
            }
        }
        let ids: Vec<String> = self.index.chunks.keys().cloned().collect();
        let total = ids.len() as u64;
        progress(0, total);
        for group in ids.chunks(self.cfg.transfers) {
            let checked = parallel(group, |id| {
                let e = &self.index.chunks[id];
                let all: Vec<usize> = (0..e.shards.len()).collect();
                self.fetch_shards(id, e, &all)
            });
            for (id, states) in group.iter().zip(checked) {
                self.scrub_chunk(id, states, repair, &mut load, &mut report)?;
            }
            if repair {
                self.refresh_lock();
            }
            progress(report.chunks, total);
        }

        let lost: BTreeSet<&String> = report.lost_chunks.iter().collect();
        report.lost_files = self
            .index
            .files
            .iter()
            .filter(|(_, f)| f.chunks.iter().any(|c| lost.contains(c)))
            .map(|(p, _)| p.clone())
            .collect();
        Ok(report)
    }

    fn scrub_chunk(
        &mut self,
        id: &str,
        states: Vec<Shard>,
        repair: bool,
        load: &mut HashMap<String, u64>,
        report: &mut ScrubReport,
    ) -> Result<()> {
        report.chunks += 1;
        let mut entry = self.index.chunks[id].clone();
        let (k, m) = (entry.k as usize, entry.m as usize);
        let mut bad = Vec::new();
        for (i, s) in states.iter().enumerate() {
            match s {
                Shard::Ok(_) => report.shards_ok += 1,
                Shard::Missing => {
                    report.shards_missing += 1;
                    bad.push(i);
                }
                Shard::Corrupt => {
                    report.shards_corrupt += 1;
                    bad.push(i);
                }
            }
        }
        if bad.is_empty() {
            return Ok(());
        }
        if k + m - bad.len() < k {
            report.lost_chunks.push(id.to_string());
            return Ok(());
        }
        report.degraded_chunks += 1;
        if !repair {
            return Ok(());
        }

        let present: Vec<Option<Vec<u8>>> = states
            .into_iter()
            .map(|s| match s {
                Shard::Ok(d) => Some(d),
                _ => None,
            })
            .collect();
        let sealed = erasure::decode(k, m, entry.sealed_len as usize, present)?;
        let (_, rebuilt) = erasure::encode(&sealed, k, m)?;
        for &i in &bad {
            if blake3::hash(&rebuilt[i]).to_hex().as_str() != entry.shards[i].hash {
                bail!("blocco {}: il frammento {i} ricostruito non corrisponde all'impronta", &id[..12]);
            }
            let holders: HashSet<&str> =
                entry.shards.iter().enumerate().filter(|(j, _)| *j != i).map(|(_, l)| l.backend.as_str()).collect();
            let original = entry.shards[i].backend.clone();
            // prima il backend originale, poi gli altri liberi, dal meno carico
            let mut candidates: Vec<&dyn Backend> = self
                .backends
                .iter()
                .map(|b| b.as_ref())
                .filter(|b| b.is_available() && !holders.contains(b.name()))
                .collect();
            candidates.sort_by_key(|b| (b.name() != original, load.get(b.name()).copied().unwrap_or(0)));
            let key = shard_key(id, i);
            let target = candidates.into_iter().find(|b| match b.put(&key, &rebuilt[i]) {
                Ok(()) => true,
                Err(e) => {
                    eprintln!("attenzione: {e:#}");
                    false
                }
            });
            match target {
                Some(b) => {
                    let name = b.name().to_string();
                    if name != original {
                        if let Some(n) = load.get_mut(&original) {
                            *n = n.saturating_sub(1);
                        }
                        *load.entry(name.clone()).or_default() += 1;
                        // l'eventuale copia danneggiata sul backend originale non serve più
                        self.pending_deletes.push((original, key));
                        entry.shards[i].backend = name;
                    }
                    report.repaired_shards += 1;
                }
                None => report.unrepaired_shards += 1,
            }
        }
        self.index.chunks.insert(id.to_string(), entry);
        self.dirty = true;
        Ok(())
    }
}

impl Drop for Repo {
    fn drop(&mut self) {
        self.unlock();
    }
}

/// L'indice più recente che si riesce a leggere, da qualsiasi backend disponibile.
fn load_latest_index(backends: &[Box<dyn Backend>], keys: &Keys) -> Result<Index> {
    load_index(backends, keys, &list_indexes(backends))
}

/// Le generazioni dell'indice presenti sui backend disponibili, dalla più recente:
/// (generazione, backend, chiave).
fn list_indexes(backends: &[Box<dyn Backend>]) -> Vec<(u64, usize, String)> {
    let listed = parallel(backends, |b| if b.is_available() { b.list(INDEX_DIR).unwrap_or_default() } else { Vec::new() });
    let mut candidates: Vec<(u64, usize, String)> = listed
        .into_iter()
        .enumerate()
        .flat_map(|(i, keys)| keys.into_iter().filter_map(move |k| parse_index_key(&k).map(|g| (g, i, k))))
        .collect();
    candidates.sort_by_key(|c| std::cmp::Reverse(c.0));
    candidates
}

/// Il primo indice leggibile fra i candidati (in ordine).
fn load_index(backends: &[Box<dyn Backend>], keys: &Keys, candidates: &[(u64, usize, String)]) -> Result<Index> {
    candidates
        .iter()
        .find_map(|(generation, i, key)| {
            let sealed = backends[*i].get(key).ok()??;
            let plain = keys.open(&sealed).ok()?;
            let index: Index = serde_json::from_slice(&plain).ok()?;
            (index.generation == *generation).then_some(index)
        })
        .ok_or_else(|| anyhow!("indice non trovato su nessun backend disponibile"))
}

fn prune_indexes(b: &dyn Backend) -> Result<()> {
    let mut generations: Vec<u64> = b.list(INDEX_DIR)?.iter().filter_map(|k| parse_index_key(k)).collect();
    generations.sort_unstable_by(|a, b| b.cmp(a));
    for generation in generations.into_iter().skip(KEEP_INDEXES) {
        b.delete(&index_key(generation))?;
    }
    Ok(())
}
