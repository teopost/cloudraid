//! Il repository: tiene insieme backend, chiavi e indice.
//!
//! Percorso di un file in scrittura: blocchi a lunghezza variabile (FastCDC) → deduplicazione
//! → cifratura → Reed-Solomon (k + m frammenti) → ogni frammento su un backend diverso.

use anyhow::{anyhow, bail, Context, Result};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::backend::{self, Backend};
use crate::config::Config;
use crate::crypto::{self, KeyFile, Keys};
use crate::erasure;
use crate::index::{ChunkEntry, FileEntry, Index, ShardLoc};

const KEYFILE: &str = "cloudraid.key";
const INDEX_DIR: &str = "index";
/// Generazioni dell'indice tenute su ogni backend.
const KEEP_INDEXES: usize = 3;

pub struct Repo {
    cfg: Config,
    backends: Vec<Box<dyn Backend>>,
    keys: Keys,
    index: Index,
    dirty: bool,
    /// Frammenti da cancellare dopo il prossimo salvataggio riuscito dell'indice:
    /// cancellarli prima lascerebbe l'indice vecchio con riferimenti a dati spariti.
    pending_deletes: Vec<(String, String)>,
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
    pub available: bool,
    pub shards: u64,
    pub bytes: u64,
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
        let backends: Vec<_> = cfg.backends.iter().map(backend::open).collect();
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
        let mut repo = Repo { cfg, backends, keys, index: Index::default(), dirty: true, pending_deletes: Vec::new() };
        repo.save()?;
        Ok(repo)
    }

    /// Apre un repository esistente: basta un backend con la chiave e l'indice.
    pub fn open(cfg: Config, password: &str) -> Result<Repo> {
        cfg.validate()?;
        let backends: Vec<_> = cfg.backends.iter().map(backend::open).collect();
        if !backends.iter().any(|b| b.is_available()) {
            bail!("nessun backend disponibile");
        }

        let mut keys = None;
        for b in backends.iter().filter(|b| b.is_available()) {
            let Ok(Some(data)) = b.get(KEYFILE) else { continue };
            let Ok(kf) = serde_json::from_slice::<KeyFile>(&data) else {
                eprintln!("attenzione: {KEYFILE} danneggiato sul backend {}", b.name());
                continue;
            };
            keys = Some(crypto::unlock(&kf, password)?);
            break;
        }
        let keys = keys.ok_or_else(|| {
            anyhow!("nessun backend disponibile contiene la chiave del repository (serve `cloudraid init`?)")
        })?;

        // l'indice più recente che si riesce a leggere, da qualsiasi backend
        let mut candidates: Vec<(u64, usize, String)> = Vec::new();
        for (i, b) in backends.iter().enumerate().filter(|(_, b)| b.is_available()) {
            for key in b.list(INDEX_DIR).unwrap_or_default() {
                if let Some(generation) = parse_index_key(&key) {
                    candidates.push((generation, i, key));
                }
            }
        }
        candidates.sort_by(|a, b| b.0.cmp(&a.0));
        let index = candidates
            .iter()
            .find_map(|(generation, i, key)| {
                let sealed = backends[*i].get(key).ok()??;
                let plain = keys.open(&sealed).ok()?;
                let index: Index = serde_json::from_slice(&plain).ok()?;
                (index.generation == *generation).then_some(index)
            })
            .ok_or_else(|| anyhow!("indice non trovato su nessun backend disponibile"))?;

        Ok(Repo { cfg, backends, keys, index, dirty: false, pending_deletes: Vec::new() })
    }

    pub fn index(&self) -> &Index {
        &self.index
    }

    fn backend(&self, name: &str) -> Option<&dyn Backend> {
        self.backends.iter().find(|b| b.name() == name).map(|b| b.as_ref())
    }

    fn available(&self) -> Vec<usize> {
        (0..self.backends.len()).filter(|&i| self.backends[i].is_available()).collect()
    }

    /// Salva l'indice (nuova generazione) su tutti i backend disponibili, poi esegue
    /// le cancellazioni in sospeso.
    pub fn save(&mut self) -> Result<()> {
        if !self.dirty {
            return Ok(());
        }
        self.index.generation += 1;
        let sealed = self.keys.seal(&serde_json::to_vec(&self.index)?);
        let key = index_key(self.index.generation);
        let mut saved = 0;
        let mut errors = Vec::new();
        for b in self.backends.iter().filter(|b| b.is_available()) {
            match b.put(&key, &sealed) {
                Ok(()) => {
                    saved += 1;
                    if let Err(e) = prune_indexes(b.as_ref()) {
                        eprintln!("attenzione: {e:#}");
                    }
                }
                Err(e) => errors.push(format!("{e:#}")),
            }
        }
        if saved == 0 {
            self.index.generation -= 1;
            bail!("impossibile salvare l'indice: {}", errors.join("; "));
        }
        for e in errors {
            eprintln!("attenzione: indice non salvato: {e}");
        }
        self.dirty = false;
        for (name, key) in std::mem::take(&mut self.pending_deletes) {
            if let Some(b) = self.backend(&name).filter(|b| b.is_available()) {
                if let Err(e) = b.delete(&key) {
                    eprintln!("attenzione: {e:#}");
                }
            }
        }
        Ok(())
    }

    /// Carica un file locale nel repository come `dest`. Restituisce i blocchi nuovi scritti.
    pub fn put_file(&mut self, src: &Path, dest: &str) -> Result<u64> {
        let dest = normalize(dest)?;
        if dest.is_empty() {
            bail!("percorso di destinazione vuoto");
        }
        if self.index.files.keys().any(|f| f.starts_with(&format!("{dest}/"))) {
            bail!("{dest} è una cartella nel repository");
        }
        let file = fs::File::open(src).with_context(|| format!("impossibile aprire {}", src.display()))?;
        let mtime = file
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64);

        let c = self.cfg.chunk;
        let mut hasher = blake3::Hasher::new();
        let mut chunks: Vec<String> = Vec::new();
        let mut size = 0u64;
        let mut new_chunks = 0u64;
        let result: Result<()> = (|| {
            for chunk in fastcdc::v2020::StreamCDC::new(file, c.min, c.avg, c.max) {
                let chunk = chunk.map_err(|e| anyhow!("lettura di {}: {e}", src.display()))?;
                hasher.update(&chunk.data);
                size += chunk.length as u64;
                let id = self.keys.chunk_id(&chunk.data);
                if let Some(entry) = self.index.chunks.get_mut(&id) {
                    entry.refs += 1;
                } else {
                    let entry = self.store_chunk(&id, &chunk.data)?;
                    self.index.chunks.insert(id.clone(), entry);
                    new_chunks += 1;
                }
                self.dirty = true;
                chunks.push(id);
            }
            Ok(())
        })();
        if let Err(e) = result {
            self.release(&chunks); // annulla i riferimenti già presi
            return Err(e);
        }

        let entry = FileEntry { size, hash: hasher.finalize().to_hex().to_string(), mtime, chunks };
        if let Some(old) = self.index.files.insert(dest, entry) {
            self.release(&old.chunks);
        }
        self.dirty = true;
        Ok(new_chunks)
    }

    /// Cifra un blocco, lo divide in k + m frammenti e li scrive su backend tutti diversi.
    fn store_chunk(&self, id: &str, plain: &[u8]) -> Result<ChunkEntry> {
        let (k, m) = (self.cfg.data_shards, self.cfg.parity_shards);
        let sealed = self.keys.seal(plain);
        let (shard_len, shards) = erasure::encode(&sealed, k, m)?;

        let avail = self.available();
        if avail.len() < k + m {
            bail!(
                "servono {} backend disponibili ({k} dati + {m} parità), ce ne sono {}: scrittura annullata",
                k + m,
                avail.len()
            );
        }
        // si parte da un backend che dipende dall'id, così i frammenti di dati si distribuiscono
        let start = usize::from_str_radix(&id[..4], 16)? % avail.len();
        let order: Vec<usize> = (0..avail.len()).map(|j| avail[(start + j) % avail.len()]).collect();
        let mut next = 0;
        let mut written: Vec<(usize, String)> = Vec::new();
        let mut locs = Vec::with_capacity(shards.len());
        for (i, shard) in shards.iter().enumerate() {
            let key = shard_key(id, i);
            loop {
                let Some(&bi) = order.get(next) else {
                    for (bi, key) in &written {
                        let _ = self.backends[*bi].delete(key);
                    }
                    bail!("impossibile scrivere il frammento {i} del blocco {}: troppi backend in errore", &id[..12]);
                };
                next += 1;
                match self.backends[bi].put(&key, shard) {
                    Ok(()) => {
                        written.push((bi, key.clone()));
                        locs.push(ShardLoc {
                            backend: self.backends[bi].name().to_string(),
                            hash: blake3::hash(shard).to_hex().to_string(),
                        });
                        break;
                    }
                    Err(e) => eprintln!("attenzione: {e:#}; provo un altro backend"),
                }
            }
        }
        Ok(ChunkEntry {
            plain_len: plain.len() as u64,
            sealed_len: sealed.len() as u64,
            k: k as u8,
            m: m as u8,
            shard_len: shard_len as u64,
            shards: locs,
            refs: 1,
        })
    }

    /// Toglie un riferimento a ogni blocco; i blocchi non più usati vengono cancellati
    /// dopo il prossimo salvataggio dell'indice.
    fn release(&mut self, ids: &[String]) {
        for id in ids {
            let unused = match self.index.chunks.get_mut(id) {
                Some(e) => {
                    e.refs = e.refs.saturating_sub(1);
                    e.refs == 0
                }
                None => false,
            };
            if unused {
                let entry = self.index.chunks.remove(id).expect("blocco presente");
                for (i, loc) in entry.shards.iter().enumerate() {
                    self.pending_deletes.push((loc.backend.clone(), shard_key(id, i)));
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

    /// Legge un blocco: prima i frammenti di dati, la parità solo se ne manca qualcuno.
    fn load_chunk(&self, id: &str, stats: &mut ReadStats) -> Result<Vec<u8>> {
        let e = self.index.chunks.get(id).ok_or_else(|| anyhow!("blocco {} assente dall'indice", &id[..12]))?;
        let (k, m) = (e.k as usize, e.m as usize);
        let mut shards: Vec<Option<Vec<u8>>> = vec![None; k + m];
        let mut have = 0;
        for (i, slot) in shards.iter_mut().enumerate() {
            if have == k {
                break;
            }
            if let Shard::Ok(data) = self.fetch_shard(&e.shards[i], &shard_key(id, i)) {
                *slot = Some(data);
                have += 1;
            }
        }
        if have < k {
            bail!("blocco {} irrecuperabile: {have} frammenti validi su {k} necessari", &id[..12]);
        }
        if shards[..k].iter().any(Option::is_none) {
            stats.degraded += 1;
        }
        let sealed = erasure::decode(k, m, e.sealed_len as usize, shards)?;
        let plain = self.keys.open(&sealed)?;
        if self.keys.chunk_id(&plain) != id {
            bail!("il blocco {} non corrisponde alla sua impronta", &id[..12]);
        }
        stats.chunks += 1;
        Ok(plain)
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
            for id in &entry.chunks {
                let data = self.load_chunk(id, stats)?;
                hasher.update(&data);
                f.write_all(&data)?;
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
        let files = self.matching(path)?;
        for f in &files {
            if let Some(entry) = self.index.files.remove(f) {
                self.release(&entry.chunks);
            }
        }
        Ok(files)
    }

    pub fn status(&self) -> Vec<BackendStatus> {
        let mut out: Vec<BackendStatus> = self
            .backends
            .iter()
            .map(|b| BackendStatus { name: b.name().to_string(), available: b.is_available(), shards: 0, bytes: 0 })
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
        let mut report = ScrubReport::default();
        let mut load: HashMap<String, u64> = HashMap::new();
        for e in self.index.chunks.values() {
            for loc in &e.shards {
                *load.entry(loc.backend.clone()).or_default() += 1;
            }
        }
        let ids: Vec<String> = self.index.chunks.keys().cloned().collect();
        for id in ids {
            report.chunks += 1;
            let mut entry = self.index.chunks[&id].clone();
            let (k, m) = (entry.k as usize, entry.m as usize);
            let states: Vec<Shard> =
                entry.shards.iter().enumerate().map(|(i, loc)| self.fetch_shard(loc, &shard_key(&id, i))).collect();
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
                continue;
            }
            if k + m - bad.len() < k {
                report.lost_chunks.push(id);
                continue;
            }
            report.degraded_chunks += 1;
            if !repair {
                continue;
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
                let key = shard_key(&id, i);
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
                            *load.entry(original.clone()).or_default() -= 1;
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
            self.index.chunks.insert(id, entry);
            self.dirty = true;
        }

        let lost: BTreeSet<&String> = report.lost_chunks.iter().collect();
        report.lost_files =
            self.index.files.iter().filter(|(_, f)| f.chunks.iter().any(|c| lost.contains(c))).map(|(p, _)| p.clone()).collect();
        Ok(report)
    }
}

fn prune_indexes(b: &dyn Backend) -> Result<()> {
    let mut generations: Vec<u64> = b.list(INDEX_DIR)?.iter().filter_map(|k| parse_index_key(k)).collect();
    generations.sort_unstable_by(|a, b| b.cmp(a));
    for generation in generations.into_iter().skip(KEEP_INDEXES) {
        b.delete(&index_key(generation))?;
    }
    Ok(())
}
