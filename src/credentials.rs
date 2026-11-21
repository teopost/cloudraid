//! Credenziali degli account (password WebDAV, chiavi segrete S3, token OAuth), cifrate con una
//! chiave derivata dalla chiave principale del repository: senza la password del repository il file
//! non serve a niente. Stanno nella cartella della configurazione, file `credenziali.bin`.
//!
//! Più processi (interfaccia web, mount, riga di comando) possono aggiornare lo stesso file, per
//! esempio quando un provider rinnova un token: ogni scrittura rilegge il file e cambia solo la voce
//! interessata, con un lock sul file.

use anyhow::{anyhow, bail, Context, Result};
use base64::Engine;
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::crypto::{random_hex, Keys};

const FILE: &str = "credenziali.bin";

pub struct Credentials {
    path: Option<PathBuf>,
    keys: Arc<Keys>,
    map: Mutex<BTreeMap<String, Map<String, Value>>>,
}

impl Credentials {
    /// Le credenziali della cartella `dir` (nessuna se la cartella non c'è o il file manca).
    pub fn load(dir: Option<&Path>, keys: Arc<Keys>) -> Result<Arc<Credentials>> {
        let path = dir.map(|d| d.join(FILE));
        let map = match &path {
            Some(p) => read(p, &keys)?,
            None => BTreeMap::new(),
        };
        Ok(Arc::new(Credentials { path, keys, map: Mutex::new(map) }))
    }

    /// Un campo delle credenziali di un backend.
    pub fn get(&self, backend: &str, field: &str) -> Option<String> {
        self.map.lock().unwrap().get(backend)?.get(field)?.as_str().map(str::to_string)
    }

    /// Imposta (o, con `None`, toglie) un campo e salva.
    pub fn set(&self, backend: &str, field: &str, value: Option<&str>) -> Result<()> {
        self.update(|map| {
            let entry = map.entry(backend.to_string()).or_default();
            match value {
                Some(v) => {
                    entry.insert(field.to_string(), Value::String(v.to_string()));
                }
                None => {
                    entry.remove(field);
                }
            }
            if entry.is_empty() {
                map.remove(backend);
            }
        })
    }

    /// Imposta tutte le credenziali di un backend (sostituisce quelle che c'erano) e salva.
    pub fn set_all(&self, backend: &str, fields: &[(&str, &str)]) -> Result<()> {
        self.update(|map| {
            let entry: Map<String, Value> =
                fields.iter().map(|(k, v)| (k.to_string(), Value::String(v.to_string()))).collect();
            if entry.is_empty() {
                map.remove(backend);
            } else {
                map.insert(backend.to_string(), entry);
            }
        })
    }

    /// Sposta le credenziali di un backend sotto un nome nuovo.
    pub fn rename(&self, old: &str, new: &str) -> Result<()> {
        self.update(|map| {
            if let Some(entry) = map.remove(old) {
                map.insert(new.to_string(), entry);
            }
        })
    }

    pub fn remove(&self, backend: &str) -> Result<()> {
        self.update(|map| {
            map.remove(backend);
        })
    }

    /// Rilegge il file (un altro processo può averlo cambiato), applica `f` e lo riscrive, tenendo
    /// un lock sul file per tutto il tempo.
    fn update(&self, f: impl FnOnce(&mut BTreeMap<String, Map<String, Value>>)) -> Result<()> {
        let mut mem = self.map.lock().unwrap();
        let Some(path) = &self.path else {
            // senza cartella (prove): solo in memoria
            f(&mut mem);
            return Ok(());
        };
        let lock_path = path.with_extension("lock");
        let lock = fs::OpenOptions::new().create(true).write(true).truncate(false).mode(0o600).open(&lock_path)?;
        // SAFETY: descrittore valido per tutta la durata di `lock`
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } != 0 {
            bail!("impossibile bloccare {}", lock_path.display());
        }
        let mut map = read(path, &self.keys)?;
        f(&mut map);
        write(path, &self.keys, &map)?;
        *mem = map;
        Ok(())
    }
}

fn read(path: &Path, keys: &Keys) -> Result<BTreeMap<String, Map<String, Value>>> {
    let data = match fs::read(path) {
        Ok(d) => d,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(e) => return Err(e).with_context(|| format!("impossibile leggere {}", path.display())),
    };
    let wrapper: Value = serde_json::from_slice(&data).with_context(|| format!("{} danneggiato", path.display()))?;
    let sealed = wrapper
        .get("sealed")
        .and_then(Value::as_str)
        .and_then(|s| base64::engine::general_purpose::STANDARD.decode(s).ok())
        .ok_or_else(|| anyhow!("{} danneggiato", path.display()))?;
    let plain = keys
        .open_secret(&sealed)
        .map_err(|_| anyhow!("{}: credenziali di un altro repository o danneggiate", path.display()))?;
    Ok(serde_json::from_slice(&plain)?)
}

fn write(path: &Path, keys: &Keys, map: &BTreeMap<String, Map<String, Value>>) -> Result<()> {
    let sealed = keys.seal_secret(&serde_json::to_vec(map)?);
    let body = serde_json::json!({
        "version": 1,
        "about": "credenziali degli account di cloudraid, cifrate con la chiave del repository",
        "sealed": base64::engine::general_purpose::STANDARD.encode(sealed),
    });
    let tmp = path.with_extension(format!("{}.tmp", random_hex(4)));
    let result = (|| -> std::io::Result<()> {
        let mut f = fs::OpenOptions::new().create_new(true).write(true).mode(0o600).open(&tmp)?;
        f.write_all(&serde_json::to_vec_pretty(&body)?)?;
        f.sync_all()?;
        fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result.with_context(|| format!("impossibile scrivere {}", path.display()))
}
