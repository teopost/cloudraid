//! Backend di storage: dove finiscono i frammenti. Per ora solo cartelle locali;
//! i provider cloud arriveranno nella fase 2 dietro la stessa interfaccia.

use anyhow::{bail, Context, Result};
use std::fs;
use std::io::{ErrorKind, Write};
use std::path::PathBuf;

use crate::config::{BackendConfig, BackendKind};

pub trait Backend: Send + Sync {
    fn name(&self) -> &str;
    /// Il backend è raggiungibile? Un backend non disponibile si tratta come un disco guasto.
    fn is_available(&self) -> bool;
    /// Prepara il backend (solo da `init`).
    fn create(&self) -> Result<()>;
    fn put(&self, key: &str, data: &[u8]) -> Result<()>;
    /// `Ok(None)` se l'oggetto non esiste.
    fn get(&self, key: &str) -> Result<Option<Vec<u8>>>;
    /// Cancellare un oggetto che non esiste non è un errore.
    fn delete(&self, key: &str) -> Result<()>;
    /// Chiavi contenute in una "cartella" (non ricorsivo), nella forma `dir/nome`.
    fn list(&self, dir: &str) -> Result<Vec<String>>;
}

pub fn open(cfg: &BackendConfig) -> Box<dyn Backend> {
    match &cfg.kind {
        BackendKind::Local { path } => Box::new(Local { name: cfg.name.clone(), root: path.clone() }),
    }
}

fn check_key(key: &str) -> Result<()> {
    if key.is_empty() || key.starts_with('/') || key.split('/').any(|p| p.is_empty() || p == "." || p == "..") {
        bail!("chiave non valida: {key:?}");
    }
    Ok(())
}

pub struct Local {
    name: String,
    root: PathBuf,
}

impl Local {
    fn path(&self, key: &str) -> Result<PathBuf> {
        check_key(key)?;
        if !self.is_available() {
            bail!("backend {} non disponibile ({} non esiste)", self.name, self.root.display());
        }
        Ok(self.root.join(key))
    }
}

impl Backend for Local {
    fn name(&self) -> &str {
        &self.name
    }

    fn is_available(&self) -> bool {
        self.root.is_dir()
    }

    fn create(&self) -> Result<()> {
        fs::create_dir_all(&self.root)
            .with_context(|| format!("backend {}: impossibile creare {}", self.name, self.root.display()))
    }

    fn put(&self, key: &str, data: &[u8]) -> Result<()> {
        let path = self.path(key)?;
        let ctx = || format!("backend {}: scrittura di {key}", self.name);
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).with_context(ctx)?;
        }
        // scrittura atomica: file temporaneo, fsync, rename
        let mut tmp = path.clone().into_os_string();
        tmp.push(".tmp");
        let tmp = PathBuf::from(tmp);
        let mut f = fs::File::create(&tmp).with_context(ctx)?;
        f.write_all(data).with_context(ctx)?;
        f.sync_all().with_context(ctx)?;
        fs::rename(&tmp, &path).with_context(ctx)?;
        Ok(())
    }

    fn get(&self, key: &str) -> Result<Option<Vec<u8>>> {
        match fs::read(self.path(key)?) {
            Ok(data) => Ok(Some(data)),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| format!("backend {}: lettura di {key}", self.name)),
        }
    }

    fn delete(&self, key: &str) -> Result<()> {
        match fs::remove_file(self.path(key)?) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e).with_context(|| format!("backend {}: cancellazione di {key}", self.name)),
        }
    }

    fn list(&self, dir: &str) -> Result<Vec<String>> {
        let entries = match fs::read_dir(self.path(dir)?) {
            Ok(it) => it,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e).with_context(|| format!("backend {}: elenco di {dir}", self.name)),
        };
        let mut keys = Vec::new();
        for entry in entries {
            let name = entry?.file_name();
            if let Some(name) = name.to_str() {
                if !name.ends_with(".tmp") {
                    keys.push(format!("{dir}/{name}"));
                }
            }
        }
        Ok(keys)
    }
}
