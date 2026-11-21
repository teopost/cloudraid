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
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::backend::{self, Backend};
use crate::config::Config;
use crate::credentials::Credentials;
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
    backends: Vec<Arc<dyn Backend>>,
    keys: Arc<Keys>,
    /// Credenziali degli account, cifrate accanto alla configurazione.
    creds: Arc<Credentials>,
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
    /// Tempo medio (ms, media mobile) per leggere un frammento da ogni backend: le letture chiedono
    /// prima ai più rapidi. Condiviso con le richieste ancora in viaggio.
    latency: Arc<Mutex<HashMap<String, f64>>>,
    /// Data di modifica del file di configurazione quando l'abbiamo letto: se cambia (un altro
    /// processo ha aggiunto, tolto o rinominato un backend), chi prende il lock lo rilegge.
    config_seen: Option<std::time::SystemTime>,
}

/// Data di modifica del file da cui viene la configurazione.
fn config_mtime(cfg: &Config) -> Option<std::time::SystemTime> {
    std::fs::metadata(cfg.file.as_ref()?).ok()?.modified().ok()
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

/// Byte in forma leggibile (KiB, MiB, ...).
pub fn human(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = bytes as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{bytes} B")
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

/// Risultato della pulizia dei frammenti orfani.
#[derive(Default, Debug, Clone)]
pub struct GcReport {
    /// Oggetti trovati nelle cartelle dei dati, su tutti i backend controllati.
    pub scanned: u64,
    /// Oggetti che l'indice non usa: frammenti di scritture interrotte, copie rimaste dopo una
    /// riparazione, file temporanei.
    pub orphans: u64,
    pub orphan_bytes: u64,
    pub deleted: u64,
    pub failed: u64,
    /// Backend non raggiungibili: i loro orfani restano per la prossima volta.
    pub skipped: Vec<String>,
}

/// Risultato del ribilanciamento.
#[derive(Default, Debug, Clone)]
pub struct RebalanceReport {
    /// Spostamenti decisi, e i byte da copiare.
    pub planned: u64,
    pub planned_bytes: u64,
    pub moved: u64,
    pub moved_bytes: u64,
    /// Spostamenti non riusciti (frammento d'origine illeggibile o backend in errore).
    pub failed: u64,
    /// Byte di frammenti per backend prima e dopo (dopo = previsto, in simulazione).
    pub before: Vec<(String, u64)>,
    pub after: Vec<(String, u64)>,
}

impl GcReport {
    pub fn summary(&self, dry_run: bool) -> String {
        let mut msg = if self.orphans == 0 {
            format!("Nessun frammento orfano su {} oggetti controllati", self.scanned)
        } else if dry_run {
            format!("{} frammenti orfani ({}) su {} oggetti: nessuno cancellato (simulazione)", self.orphans, human(self.orphan_bytes), self.scanned)
        } else {
            format!("{} frammenti orfani cancellati, {} liberati", self.deleted, human(self.orphan_bytes))
        };
        if self.failed > 0 {
            msg += &format!("; {} non cancellati", self.failed);
        }
        if !self.skipped.is_empty() {
            msg += &format!("; backend non controllati: {}", self.skipped.join(", "));
        }
        msg
    }
}

impl RebalanceReport {
    pub fn summary(&self, dry_run: bool) -> String {
        let mut msg = if self.planned == 0 {
            "Frammenti già distribuiti in modo equilibrato: niente da spostare".to_string()
        } else if dry_run {
            format!("{} frammenti da spostare ({}): nessuno spostato (simulazione)", self.planned, human(self.planned_bytes))
        } else {
            format!("{} frammenti spostati ({})", self.moved, human(self.moved_bytes))
        };
        if self.failed > 0 {
            msg += &format!("; {} non spostati (serve una verifica con riparazione)", self.failed);
        }
        msg
    }
}

/// Risultato di una dismissione.
#[derive(Default, Debug, Clone)]
pub struct RetireReport {
    pub name: String,
    pub moved: u64,
    pub moved_bytes: u64,
    /// Frammenti non spostati (illeggibili e non ricostruibili, o nessun backend adatto).
    pub failed: u64,
    /// Frammenti ancora sul backend: zero vuol dire che si può togliere dalla configurazione.
    pub remaining: u64,
}

impl RetireReport {
    pub fn summary(&self) -> String {
        let mut msg = format!("{}: {} frammenti spostati ({})", self.name, self.moved, human(self.moved_bytes));
        if self.remaining == 0 {
            msg += ". Il backend è vuoto: ora si può togliere dalla configurazione o sostituire";
        } else {
            msg += &format!(". Restano {} frammenti: rilancia la dismissione", self.remaining);
        }
        if self.failed > 0 {
            msg += &format!(" ({} non spostati)", self.failed);
        }
        msg
    }
}

/// Uno spostamento deciso dal ribilanciamento: il frammento `shard` del blocco `id` da `from` a `to`.
struct Move {
    id: String,
    shard: usize,
    from: usize,
    to: usize,
    len: u64,
}

#[derive(Debug, Clone)]
pub struct BackendStatus {
    /// Tempo medio per leggere un frammento, se misurato.
    pub latency_ms: Option<f64>,
    /// In dismissione (vedi `retire`).
    pub retired: bool,
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
        let (kf, keys) = crypto::create(password)?;
        let keys = Arc::new(keys);
        let creds = Credentials::load(cfg.dir.as_deref(), keys.clone())?;
        let backends = backend::open_all(&cfg, &creds);
        for b in &backends {
            b.create()?;
            if b.get(KEYFILE)?.is_some() {
                bail!("il backend {} contiene già un repository cloudraid", b.name());
            }
        }
        let json = serde_json::to_vec_pretty(&kf)?;
        for b in &backends {
            b.put(KEYFILE, &json)?;
        }
        save_local_keyfile(&cfg, &kf)?;
        let mut repo = Repo {
            cfg,
            backends,
            keys,
            creds,
            keyfile: kf,
            index: Index::default(),
            dirty: true,
            pending_deletes: Vec::new(),
            lock: Mutex::new(None),
            keyfile_checked: HashSet::new(),
            latency: Arc::default(),
            config_seen: None,
        };
        repo.config_seen = config_mtime(&repo.cfg);
        repo.save()?;
        Ok(repo)
    }

    /// Apre un repository esistente. La chiave cifrata si prende dalla copia accanto alla
    /// configurazione (serve per aprire le credenziali degli account, e quindi i backend cloud);
    /// se manca, dal primo backend che risponde, e poi se ne salva la copia.
    pub fn open(cfg: Config, password: &str) -> Result<Repo> {
        cfg.validate()?;
        let (keyfile, from_backends) = match load_local_keyfile(&cfg) {
            Some(kf) => (kf, false),
            None => {
                // senza credenziali: rispondono solo i backend che non ne hanno bisogno (cartelle locali)
                let none = Credentials::load(None, Arc::new(Keys::ephemeral()))?;
                let backends = backend::open_all(&cfg, &none);
                parallel(&backends, |b| b.is_available());
                let avail: Vec<usize> = (0..backends.len()).filter(|&i| backends[i].is_available()).collect();
                let kf = race_get(&backends, &avail, KEYFILE, |i, data| match serde_json::from_slice::<KeyFile>(&data) {
                    Ok(kf) => Some(kf),
                    Err(_) => {
                        eprintln!("attenzione: {KEYFILE} danneggiato sul backend {}", backends[i].name());
                        None
                    }
                })
                .ok_or_else(|| {
                    anyhow!(
                        "chiave del repository non trovata: né accanto alla configurazione né su un backend raggiungibile \
                         (serve `cloudraid init`?)"
                    )
                })?;
                (kf, true)
            }
        };
        let keys = Arc::new(crypto::unlock(&keyfile, password)?);
        if from_backends {
            save_local_keyfile(&cfg, &keyfile)?;
        }
        let creds = Credentials::load(cfg.dir.as_deref(), keys.clone())?;
        let backends = backend::open_all(&cfg, &creds);
        // verifica la disponibilità di tutti i backend in parallelo (per quelli di rete è una chiamata)
        let avail = parallel(&backends, |b| b.is_available());
        if !avail.contains(&true) {
            bail!("nessun backend disponibile");
        }
        let index = load_latest_index(&backends, &keys)?;
        let config_seen = config_mtime(&cfg);
        Ok(Repo {
            cfg,
            backends,
            keys,
            creds,
            keyfile,
            index,
            dirty: false,
            pending_deletes: Vec::new(),
            lock: Mutex::new(None),
            keyfile_checked: HashSet::new(),
            latency: Arc::default(),
            config_seen,
        })
    }

    /// Passa a una configurazione nuova (backend aggiunti o tolti) senza chiedere di nuovo la
    /// password: la chiave e le credenziali restano quelle aperte.
    pub fn reconfigure(&mut self, cfg: Config) -> Result<()> {
        if self.dirty {
            bail!("ci sono modifiche all'indice non salvate: impossibile cambiare configurazione adesso");
        }
        cfg.validate()?;
        let creds = Credentials::load(cfg.dir.as_deref(), self.keys.clone())?;
        let backends = backend::open_all(&cfg, &creds);
        parallel(&backends, |b| b.is_available());
        self.config_seen = config_mtime(&cfg);
        self.cfg = cfg;
        self.creds = creds;
        self.backends = backends;
        self.keyfile_checked.clear();
        if let Some(index) = self.newer_index(self.index.generation)? {
            self.index = index;
        }
        Ok(())
    }

    /// Le credenziali degli account (per aggiungerne o toglierne).
    pub fn credentials(&self) -> &Arc<Credentials> {
        &self.creds
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

    /// I backend disponibili che possono ricevere frammenti (non in dismissione).
    fn writable(&self) -> Vec<usize> {
        self.available().into_iter().filter(|&i| !self.index.retired.contains(self.backends[i].name())).collect()
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
        self.reload_config_if_changed();
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

    /// Se il file di configurazione è cambiato dopo la lettura (per esempio un backend aggiunto o
    /// rinominato dall'interfaccia web mentre gira il mount), lo rilegge: chi scrive deve usare gli
    /// stessi nomi dei backend che ci sono nell'indice.
    fn reload_config_if_changed(&mut self) {
        if self.dirty {
            return;
        }
        let Some(path) = self.cfg.file.clone() else { return };
        let now = config_mtime(&self.cfg);
        if now.is_none() || now == self.config_seen {
            return;
        }
        match Config::load(&path).and_then(|cfg| self.reconfigure(cfg)) {
            Ok(()) => eprintln!("configurazione riletta: {} backend", self.cfg.backends.len()),
            Err(e) => eprintln!("attenzione: configurazione nuova non applicata: {e:#}"),
        }
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

        let avail = self.writable();
        if avail.len() < n {
            bail!(
                "servono {n} backend disponibili e non in dismissione ({k} dati + {m} parità), ce ne sono {}: scrittura annullata",
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
        fetch_from(self.backend(&loc.backend), loc, key)
    }

    /// Legge in parallelo i frammenti `idx` di un blocco.
    fn fetch_shards(&self, id: &str, entry: &ChunkEntry, idx: &[usize]) -> Vec<Shard> {
        parallel(idx, |&i| self.fetch_shard(&entry.shards[i], &shard_key(id, i)))
    }

    /// Legge un blocco dell'indice (vedi `read_chunk`). Restituisce i dati e se la ridondanza era ridotta.
    fn load_chunk(&self, id: &str) -> Result<(Vec<u8>, bool)> {
        let e = self.index.chunks.get(id).ok_or_else(|| anyhow!("blocco {} assente dall'indice", &id[..12]))?;
        self.read_chunk(id, e)
    }

    /// Legge un blocco descritto da `e`, che può venire da una copia dell'indice tenuta altrove
    /// (il mount). Il contenuto è verificato con la sua impronta. Restituisce i dati e se è
    /// servita la parità.
    pub fn read_chunk(&self, id: &str, e: &ChunkEntry) -> Result<(Vec<u8>, bool)> {
        let (k, m) = (e.k as usize, e.m as usize);
        let n = k + m;
        if e.shards.len() != n {
            bail!("blocco {}: numero di frammenti incoerente nell'indice", &id[..12]);
        }
        // Lettura "a gara": bastano k frammenti qualsiasi, dati o parità. Si chiedono prima ai backend
        // più rapidi secondo le misure (a parità, i frammenti di dati); se una risposta tarda oltre il
        // previsto se ne chiede un altro, e vince chi arriva prima. Le richieste rimaste indietro
        // finiscono in sottofondo e servono solo ad aggiornare le misure.
        let lat = self.latency.lock().unwrap().clone();
        let expected: Vec<f64> = e
            .shards
            .iter()
            .map(|loc| match self.backend(&loc.backend) {
                Some(b) if b.is_available() => lat.get(&loc.backend).copied().unwrap_or(0.0),
                _ => f64::INFINITY,
            })
            .collect();
        let mut order: Vec<usize> = (0..n).collect();
        order.sort_by(|&a, &b| expected[a].total_cmp(&expected[b]).then((a >= k).cmp(&(b >= k))).then(a.cmp(&b)));
        // un backend irraggiungibile toglie ridondanza anche se non serve chiedergli niente
        let mut degraded = expected.iter().any(|t| t.is_infinite());
        // attesa prima di chiedere un frammento in più: il doppio del tempo previsto per il k-esimo
        let kth = expected[order[k - 1]];
        let hedge = if kth > 0.0 && kth.is_finite() {
            Duration::from_secs_f64((kth * 2.0 + 100.0).clamp(250.0, 5000.0) / 1000.0)
        } else {
            Duration::from_secs(1)
        };

        let (tx, rx) = mpsc::channel::<(usize, Shard)>();
        let launch = |i: usize| {
            let b = self.backends.iter().find(|b| b.name() == e.shards[i].backend).cloned();
            let (loc, key, tx, lat) = (e.shards[i].clone(), shard_key(id, i), tx.clone(), self.latency.clone());
            thread::spawn(move || {
                let start = Instant::now();
                let shard = fetch_from(b.as_deref(), &loc, &key);
                if matches!(shard, Shard::Ok(_)) {
                    let ms = start.elapsed().as_secs_f64() * 1000.0;
                    let mut l = lat.lock().unwrap();
                    let v = l.entry(loc.backend.clone()).or_insert(ms);
                    *v = 0.7 * *v + 0.3 * ms;
                }
                let _ = tx.send((i, shard));
            });
        };
        let mut shards: Vec<Option<Vec<u8>>> = vec![None; n];
        let (mut have, mut pending, mut next) = (0usize, 0usize, 0usize);
        let started = Instant::now();
        let mut deadline = started + hedge;
        // richieste in viaggio: frammento e momento della richiesta
        let mut flying: HashMap<usize, Instant> = HashMap::new();
        while have < k {
            // quelli che servono, oltre a quelli già in viaggio (all'inizio k, poi uno per ogni errore)
            while have + pending < k && next < n {
                launch(order[next]);
                flying.insert(order[next], Instant::now());
                next += 1;
                pending += 1;
                deadline = Instant::now() + hedge;
            }
            if pending == 0 {
                break;
            }
            let wait = if next < n { deadline.saturating_duration_since(Instant::now()) } else { Duration::from_secs(24 * 3600) };
            match rx.recv_timeout(wait) {
                Ok((i, Shard::Ok(d))) => {
                    shards[i] = Some(d);
                    have += 1;
                    pending -= 1;
                    flying.remove(&i);
                    // chi è già arrivato dice quanto ci si mette: chi tarda tre volte tanto è lento
                    // (vale anche quando le misure non ci sono ancora, per esempio appena avviati)
                    let t = started.elapsed();
                    deadline = deadline.min(started + (t * 3).max(t + Duration::from_millis(100)));
                }
                Ok((i, _)) => {
                    pending -= 1;
                    flying.remove(&i);
                    degraded = true;
                }
                Err(RecvTimeoutError::Timeout) => {
                    // chi è ancora in viaggio è lento almeno quanto l'attesa: lo si segna subito,
                    // così le letture successive non gli chiedono più niente finché non migliora
                    {
                        let mut l = self.latency.lock().unwrap();
                        for (&i, since) in &flying {
                            let ms = since.elapsed().as_secs_f64() * 1000.0;
                            let v = l.entry(e.shards[i].backend.clone()).or_insert(ms);
                            *v = v.max(ms);
                        }
                    }
                    if next < n {
                        launch(order[next]);
                        flying.insert(order[next], Instant::now());
                        next += 1;
                        pending += 1;
                    }
                    deadline = Instant::now() + hedge;
                }
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
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

    /// Prepara pulizia e ribilanciamento: servono il lock (nessun altro processo sta scrivendo
    /// frammenti non ancora nell'indice) e un indice senza modifiche in sospeso.
    fn need_exclusive(&self, what: &str) -> Result<()> {
        if !self.holds_lock() {
            bail!("{what}: serve il lock del repository");
        }
        if self.dirty {
            bail!("{what}: ci sono modifiche all'indice non salvate");
        }
        Ok(())
    }

    /// Cancella dai backend gli oggetti in `data/` che l'indice non usa: frammenti di scritture
    /// interrotte, vecchie copie rimaste su un backend tornato dopo una riparazione, file
    /// temporanei. Con `dry_run` conta soltanto. Va chiamata tenendo il lock.
    pub fn gc(&mut self, dry_run: bool, progress: &mut dyn FnMut(u64, u64)) -> Result<GcReport> {
        self.need_exclusive("pulizia")?;
        let used: HashSet<(String, String)> = self
            .index
            .chunks
            .iter()
            .flat_map(|(id, c)| c.shards.iter().enumerate().map(move |(i, loc)| (loc.backend.clone(), shard_key(id, i))))
            .collect();
        let mut report = GcReport::default();
        let listed = parallel(&self.backends, |b| if b.is_available() { Some(b.list_all("data")) } else { None });
        let mut orphans: Vec<(usize, Vec<String>)> = Vec::new();
        for (bi, res) in listed.into_iter().enumerate() {
            let name = self.backends[bi].name().to_string();
            let objects = match res {
                Some(Ok(objects)) => objects,
                Some(Err(e)) => {
                    eprintln!("attenzione: {e:#}");
                    report.skipped.push(name);
                    continue;
                }
                None => {
                    report.skipped.push(name);
                    continue;
                }
            };
            report.scanned += objects.len() as u64;
            let mut mine = Vec::new();
            for (key, size) in objects {
                if !used.contains(&(name.clone(), key.clone())) {
                    report.orphans += 1;
                    report.orphan_bytes += size;
                    mine.push(key);
                }
            }
            if !mine.is_empty() {
                orphans.push((bi, mine));
            }
        }
        if dry_run {
            return Ok(report);
        }
        // cartelle della prova di un backend nuovo rimaste dalle versioni fino alla 0.9 (contengono
        // solo file di prova): una richiesta per backend, e se non ci sono non succede niente
        parallel(&self.backends, |b| {
            if b.is_available() {
                if let Err(e) = b.remove_dir(backend::PROBE_DIR) {
                    eprintln!("attenzione: {e:#}");
                }
            }
        });
        if report.orphans == 0 {
            return Ok(report);
        }
        // a lotti, in parallelo; l'avanzamento fra un lotto e l'altro
        let all: Vec<(usize, String)> =
            orphans.into_iter().flat_map(|(bi, keys)| keys.into_iter().map(move |k| (bi, k))).collect();
        let batch_size = (self.cfg.transfers * self.backends.len()).max(1);
        for batch in all.chunks(batch_size) {
            for res in parallel(batch, |(bi, key)| self.backends[*bi].delete(key)) {
                match res {
                    Ok(()) => report.deleted += 1,
                    Err(e) => {
                        eprintln!("attenzione: {e:#}");
                        report.failed += 1;
                    }
                }
            }
            progress(report.deleted + report.failed, report.orphans);
            self.refresh_lock();
        }
        self.refresh_lock();
        Ok(report)
    }

    /// Byte di frammenti per backend secondo l'indice.
    fn shard_load(&self) -> HashMap<String, u64> {
        let mut load: HashMap<String, u64> = self.backends.iter().map(|b| (b.name().to_string(), 0)).collect();
        for c in self.index.chunks.values() {
            for loc in &c.shards {
                *load.entry(loc.backend.clone()).or_insert(0) += c.shard_len;
            }
        }
        load
    }

    /// Decide gli spostamenti: da un backend sopra la media al più scarico fra quelli sotto la media che
    /// non hanno già un frammento dello stesso blocco, solo se lo squilibrio diminuisce davvero.
    fn plan_rebalance(&self) -> (Vec<Move>, HashMap<String, u64>) {
        let mut load = self.shard_load();
        // i backend in dismissione non partecipano: li svuota `retire`
        let avail = self.writable();
        let names: Vec<String> = self.backends.iter().map(|b| b.name().to_string()).collect();
        let pos: HashMap<&str, usize> = names.iter().enumerate().map(|(i, n)| (n.as_str(), i)).collect();
        if avail.len() < 2 {
            return (Vec::new(), load);
        }
        let total: u64 = avail.iter().map(|&i| load[&names[i]]).sum();
        let target = total / avail.len() as u64;
        // ruoli fissati all'inizio: chi è sopra la media cede soltanto, chi è sotto riceve soltanto.
        // Così nessun byte fa giri inutili (un backend che cede troppo non si fa ridare frammenti).
        // Uno scarto entro il 5% della media non vale il traffico: ripetere il comando non sposta niente
        let tolerance = target / 20;
        let donors: Vec<usize> = avail.iter().copied().filter(|&b| load[&names[b]] > target + tolerance).collect();
        let receivers: Vec<usize> =
            avail.iter().copied().filter(|&b| load[&names[b]] + tolerance < target).collect();
        let mut moves = Vec::new();
        for (id, c) in &self.index.chunks {
            let mut holders: HashSet<usize> = c.shards.iter().filter_map(|l| pos.get(l.backend.as_str()).copied()).collect();
            for (i, loc) in c.shards.iter().enumerate() {
                let Some(&from) = pos.get(loc.backend.as_str()) else { continue };
                if !avail.contains(&from) || load[&names[from]] <= target {
                    continue;
                }
                if !donors.contains(&from) {
                    continue;
                }
                let best = receivers.iter().copied().filter(|b| !holders.contains(b)).min_by_key(|&b| (load[&names[b]], b));
                let Some(to) = best else { continue };
                if load[&names[to]] >= target || load[&names[to]] + c.shard_len >= load[&names[from]] {
                    continue;
                }
                *load.get_mut(&names[from]).unwrap() -= c.shard_len;
                *load.get_mut(&names[to]).unwrap() += c.shard_len;
                holders.remove(&from);
                holders.insert(to);
                moves.push(Move { id: id.clone(), shard: i, from, to, len: c.shard_len });
            }
        }
        (moves, load)
    }

    /// Sposta frammenti dai backend più pieni a quelli più vuoti (per esempio dopo aver aggiunto un
    /// backend), sempre con i frammenti di un blocco su backend diversi. Ogni frammento viene
    /// verificato prima di copiarlo; la copia vecchia si cancella solo dopo il salvataggio
    /// dell'indice. Con `dry_run` mostra soltanto il piano. Va chiamata tenendo il lock.
    pub fn rebalance(&mut self, dry_run: bool, progress: &mut dyn FnMut(u64, u64)) -> Result<RebalanceReport> {
        self.need_exclusive("ribilanciamento")?;
        let sorted = |m: HashMap<String, u64>| {
            let mut v: Vec<(String, u64)> = m.into_iter().collect();
            v.sort();
            v
        };
        let before = sorted(self.shard_load());
        let (moves, planned_load) = self.plan_rebalance();
        let mut report = RebalanceReport {
            planned: moves.len() as u64,
            planned_bytes: moves.iter().map(|m| m.len).sum(),
            before,
            after: sorted(planned_load),
            ..Default::default()
        };
        if dry_run || moves.is_empty() {
            return Ok(report);
        }
        let (moved, moved_bytes, failed) = self.execute_moves(&moves, progress)?;
        (report.moved, report.moved_bytes, report.failed) = (moved, moved_bytes, failed);
        report.after = sorted(self.shard_load());
        Ok(report)
    }

    /// Esegue gli spostamenti a gruppi di `transfers`: copia il frammento (o lo ricostruisce dalla
    /// parità se l'origine è illeggibile), aggiorna l'indice e cancella la copia vecchia dopo il
    /// salvataggio. Restituisce spostati, byte spostati, falliti.
    fn execute_moves(&mut self, moves: &[Move], progress: &mut dyn FnMut(u64, u64)) -> Result<(u64, u64, u64)> {
        let total = moves.len() as u64;
        let (mut moved, mut moved_bytes, mut failed) = (0u64, 0u64, 0u64);
        let mut since_save = 0;
        for batch in moves.chunks(self.cfg.transfers.max(1)) {
            let results = parallel(batch, |mv| -> Result<()> {
                let entry = &self.index.chunks[&mv.id];
                let key = shard_key(&mv.id, mv.shard);
                let data = match self.fetch_shard(&entry.shards[mv.shard], &key) {
                    Shard::Ok(data) => data,
                    _ => self.rebuild_shard(&mv.id, entry, mv.shard)?,
                };
                self.backends[mv.to].put(&key, &data)
            });
            for (mv, res) in batch.iter().zip(results) {
                match res {
                    Ok(()) => {
                        let to = self.backends[mv.to].name().to_string();
                        let key = shard_key(&mv.id, mv.shard);
                        self.index.chunks.get_mut(&mv.id).expect("blocco presente").shards[mv.shard].backend = to;
                        // la copia vecchia si cancella dopo il salvataggio (se il backend c'è ancora)
                        if self.backends[mv.from].is_available() {
                            self.pending_deletes.push((self.backends[mv.from].name().to_string(), key));
                        }
                        self.dirty = true;
                        moved += 1;
                        moved_bytes += mv.len;
                        since_save += 1;
                    }
                    Err(e) => {
                        eprintln!("attenzione: {e:#}");
                        failed += 1;
                    }
                }
            }
            progress(moved + failed, total);
            // ogni tanto si salva: un'interruzione perde poco lavoro e le copie vecchie si cancellano
            if since_save >= 64 {
                self.save()?;
                since_save = 0;
            }
            self.refresh_lock();
        }
        self.save()?;
        Ok((moved, moved_bytes, failed))
    }

    /// Ricostruisce il frammento `i` di un blocco dagli altri (serve quando il suo backend è
    /// irraggiungibile o la copia è danneggiata). Il risultato ha la stessa impronta dell'originale.
    fn rebuild_shard(&self, id: &str, e: &ChunkEntry, i: usize) -> Result<Vec<u8>> {
        let (k, m) = (e.k as usize, e.m as usize);
        let others: Vec<usize> = (0..k + m).filter(|&j| j != i).collect();
        let mut shards: Vec<Option<Vec<u8>>> = vec![None; k + m];
        for (j, s) in others.iter().zip(self.fetch_shards(id, e, &others)) {
            if let Shard::Ok(d) = s {
                shards[*j] = Some(d);
            }
        }
        let have = shards.iter().filter(|s| s.is_some()).count();
        if have < k {
            bail!("blocco {}: solo {have} frammenti validi, impossibile ricostruire il frammento {i}", &id[..12]);
        }
        let sealed = erasure::decode(k, m, e.sealed_len as usize, shards)?;
        let (_, mut all) = erasure::encode(&sealed, k, m)?;
        let rebuilt = std::mem::take(&mut all[i]);
        if blake3::hash(&rebuilt).to_hex().as_str() != e.shards[i].hash {
            bail!("blocco {}: il frammento {i} ricostruito non corrisponde all'impronta", &id[..12]);
        }
        Ok(rebuilt)
    }

    /// I backend in dismissione.
    pub fn retired(&self) -> &BTreeSet<String> {
        &self.index.retired
    }

    /// Mette in dismissione un backend e sposta tutti i suoi frammenti sugli altri, scegliendo ogni volta
    /// il meno carico fra quelli che non hanno già un frammento dello stesso blocco: la ridondanza resta
    /// piena per tutto il tempo. Se il backend è irraggiungibile i frammenti vengono ricostruiti dalla
    /// parità. Si può rilanciare: riprende da dove era arrivato. Va chiamata tenendo il lock.
    pub fn retire(&mut self, name: &str, progress: &mut dyn FnMut(u64, u64)) -> Result<RetireReport> {
        self.need_exclusive("dismissione")?;
        let Some(bi) = self.backends.iter().position(|b| b.name() == name) else {
            bail!("backend {name} non presente nella configurazione");
        };
        let n = self.cfg.data_shards + self.cfg.parity_shards;
        let others = self.backends.iter().filter(|b| b.name() != name && !self.index.retired.contains(b.name())).count();
        if others < n {
            bail!(
                "senza {name} restano {others} backend, ne servono almeno {n} ({} dati + {} parità): \
                 aggiungi prima alla configurazione il backend che lo sostituisce",
                self.cfg.data_shards,
                self.cfg.parity_shards
            );
        }
        if self.index.retired.insert(name.to_string()) {
            self.dirty = true;
            self.save()?;
        }
        // il piano: ogni frammento sul backend va al meno carico fra quelli che possono riceverlo
        let mut load = self.shard_load();
        let targets = self.writable();
        let mut moves = Vec::new();
        let mut stuck = 0u64;
        for (id, c) in &self.index.chunks {
            let mut holders: HashSet<&str> = c.shards.iter().map(|l| l.backend.as_str()).collect();
            for (i, loc) in c.shards.iter().enumerate() {
                if loc.backend != name {
                    continue;
                }
                let best = targets
                    .iter()
                    .copied()
                    .filter(|&b| !holders.contains(self.backends[b].name()))
                    .min_by_key(|&b| (load[self.backends[b].name()], b));
                let Some(to) = best else {
                    stuck += 1;
                    continue;
                };
                let to_name = self.backends[to].name();
                *load.get_mut(to_name).unwrap() += c.shard_len;
                holders.insert(to_name);
                moves.push(Move { id: id.clone(), shard: i, from: bi, to, len: c.shard_len });
            }
        }
        let (moved, moved_bytes, failed) = self.execute_moves(&moves, progress)?;
        let remaining = self.index.chunks.values().flat_map(|c| &c.shards).filter(|l| l.backend == name).count() as u64;
        Ok(RetireReport { name: name.to_string(), moved, moved_bytes, failed: failed + stuck, remaining })
    }

    /// Rimette in servizio un backend in dismissione (i frammenti già spostati restano dove sono:
    /// `rebalance` li ridistribuisce).
    /// Cambia il nome di un backend nell'indice (frammenti e dismissioni). Va chiamata tenendo il
    /// lock, insieme al cambio di nome nella configurazione; poi `save`.
    pub fn rename_backend_refs(&mut self, old: &str, new: &str) -> Result<u64> {
        if !self.holds_lock() {
            bail!("serve il lock del repository");
        }
        let mut n = 0;
        for chunk in self.index.chunks.values_mut() {
            for s in chunk.shards.iter_mut().filter(|s| s.backend == old) {
                s.backend = new.to_string();
                n += 1;
            }
        }
        if self.index.retired.remove(old) {
            self.index.retired.insert(new.to_string());
        }
        let mut lat = self.latency.lock().unwrap();
        if let Some(v) = lat.remove(old) {
            lat.insert(new.to_string(), v);
        }
        self.dirty = true;
        Ok(n)
    }

    pub fn unretire(&mut self, name: &str) -> Result<()> {
        if !self.holds_lock() {
            bail!("serve il lock del repository");
        }
        if self.index.retired.remove(name) {
            self.dirty = true;
        }
        Ok(())
    }

    pub fn status(&self) -> Vec<BackendStatus> {
        let mut out: Vec<BackendStatus> = self
            .backends
            .iter()
            .map(|b| BackendStatus {
                latency_ms: self.latency.lock().unwrap().get(b.name()).copied(),
                retired: self.index.retired.contains(b.name()),
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
                .filter(|b| b.is_available() && !holders.contains(b.name()) && !self.index.retired.contains(b.name()))
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
const LOCAL_KEYFILE: &str = "cloudraid.key";

/// La copia della chiave cifrata accanto alla configurazione, se c'è.
fn load_local_keyfile(cfg: &Config) -> Option<KeyFile> {
    let path = cfg.dir.as_ref()?.join(LOCAL_KEYFILE);
    let data = fs::read(&path).ok()?;
    match serde_json::from_slice(&data) {
        Ok(kf) => Some(kf),
        Err(_) => {
            eprintln!("attenzione: {} danneggiato: uso la chiave dei backend", path.display());
            None
        }
    }
}

/// Salva la copia della chiave cifrata accanto alla configurazione (solo il proprietario la legge;
/// comunque senza password non serve a niente).
fn save_local_keyfile(cfg: &Config, kf: &KeyFile) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let Some(dir) = &cfg.dir else { return Ok(()) };
    let path = dir.join(LOCAL_KEYFILE);
    let tmp = dir.join(format!(".{LOCAL_KEYFILE}.{}.tmp", random_hex(4)));
    let result = (|| -> std::io::Result<()> {
        let mut f = fs::OpenOptions::new().create_new(true).write(true).mode(0o600).open(&tmp)?;
        f.write_all(&serde_json::to_vec_pretty(kf)?)?;
        f.sync_all()?;
        fs::rename(&tmp, &path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result.with_context(|| format!("impossibile salvare {}", path.display()))
}

/// Legge un frammento e ne controlla l'impronta.
fn fetch_from(b: Option<&dyn Backend>, loc: &ShardLoc, key: &str) -> Shard {
    let Some(b) = b.filter(|b| b.is_available()) else {
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

fn load_latest_index(backends: &[Arc<dyn Backend>], keys: &Keys) -> Result<Index> {
    load_index(backends, keys, &list_indexes(backends))
}

/// Le generazioni dell'indice presenti sui backend disponibili, dalla più recente:
/// (generazione, backend, chiave).
fn list_indexes(backends: &[Arc<dyn Backend>]) -> Vec<(u64, usize, String)> {
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
fn load_index(backends: &[Arc<dyn Backend>], keys: &Keys, candidates: &[(u64, usize, String)]) -> Result<Index> {
    // dalla generazione più recente: la stessa copia sta su più backend, vale la prima integra che arriva
    let mut generations: Vec<u64> = candidates.iter().map(|c| c.0).collect();
    generations.dedup();
    for generation in generations {
        let holders: Vec<usize> = candidates.iter().filter(|c| c.0 == generation).map(|c| c.1).collect();
        let found = race_get(backends, &holders, &index_key(generation), |_, sealed| {
            let plain = keys.open(&sealed).ok()?;
            let index: Index = serde_json::from_slice(&plain).ok()?;
            (index.generation == generation).then_some(index)
        });
        if let Some(index) = found {
            return Ok(index);
        }
    }
    bail!("indice non trovato su nessun backend disponibile")
}

/// Legge `key` da più backend insieme e restituisce il primo risultato che `accept` approva; le
/// richieste rimaste indietro finiscono in sottofondo. Così un backend lento non rallenta l'apertura.
fn race_get<T>(
    backends: &[Arc<dyn Backend>],
    idx: &[usize],
    key: &str,
    mut accept: impl FnMut(usize, Vec<u8>) -> Option<T>,
) -> Option<T> {
    let (tx, rx) = mpsc::channel();
    for &i in idx {
        let (b, key, tx) = (backends[i].clone(), key.to_string(), tx.clone());
        thread::spawn(move || {
            let _ = tx.send((i, b.get(&key).ok().flatten()));
        });
    }
    drop(tx);
    while let Ok((i, data)) = rx.recv() {
        if let Some(v) = data.and_then(|d| accept(i, d)) {
            return Some(v);
        }
    }
    None
}

fn prune_indexes(b: &dyn Backend) -> Result<()> {
    let mut generations: Vec<u64> = b.list(INDEX_DIR)?.iter().filter_map(|k| parse_index_key(k)).collect();
    generations.sort_unstable_by(|a, b| b.cmp(a));
    for generation in generations.into_iter().skip(KEEP_INDEXES) {
        b.delete(&index_key(generation))?;
    }
    Ok(())
}
