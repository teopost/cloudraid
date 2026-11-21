//! Backend di storage: dove finiscono i frammenti. Tutti implementati qui, senza programmi esterni.
//!
//! - `local`: una cartella o un disco montato.
//! - `webdav`: Nextcloud, ownCloud, NAS e qualsiasi server WebDAV.
//! - `s3`: AWS S3 e compatibili (Backblaze B2, Wasabi, MinIO...), firma SigV4.
//! - `dropbox`, `onedrive`: la cartella dell'app, con token OAuth rinnovati da soli.
//! - `gdrive`: Google Drive, con accesso solo ai file creati da cloudraid.
//!
//! Le chiavi degli oggetti sono percorsi come `data/ab/abcd....3` o `index/0000...12.bin`.

use anyhow::{bail, Context, Result};
use std::fs;
use std::io::{ErrorKind, Write};
use std::path::PathBuf;
use std::sync::Arc;

use crate::config::{AppsConfig, BackendConfig, BackendKind, Config};
use crate::credentials::Credentials;
use crate::crypto::random_hex;

mod dropbox;
mod gdrive;
pub mod http;
pub mod oauth;
mod onedrive;
pub mod s3;
mod webdav;

pub use gdrive::ensure_folder as gdrive_ensure_folder;

/// Spazio sul servizio, come lo dichiara il servizio stesso (vale per tutto l'account, non solo per
/// cloudraid).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Space {
    /// Byte ancora liberi.
    pub free: u64,
    /// Capienza totale, se nota.
    pub total: Option<u64>,
    /// Dispositivo (solo cartelle locali): più backend sullo stesso disco ne condividono lo spazio.
    pub device: Option<u64>,
}

pub trait Backend: Send + Sync {
    fn name(&self) -> &str;
    /// Dove sta, per i messaggi (percorso o remote).
    fn location(&self) -> String;
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
    /// Tutti gli oggetti sotto `dir`, anche nelle sottocartelle, con la dimensione in byte.
    /// Comprende gli avanzi di scritture interrotte (file temporanei): servono alla pulizia.
    fn list_all(&self, dir: &str) -> Result<Vec<(String, u64)>>;
    /// Toglie la cartella `dir` **con tutto il contenuto** (dove il servizio ha cartelle vere: per S3
    /// non serve). Usata solo per la cartella della prova, che non contiene altro.
    fn remove_dir(&self, _dir: &str) -> Result<()> {
        Ok(())
    }
    /// Spazio libero sul servizio. `Ok(None)` se il servizio non lo dice (S3): allora vale solo
    /// l'eventuale limite della configurazione.
    fn space(&self) -> Result<Option<Space>> {
        Ok(None)
    }
    /// Tipo di backend, per l'interfaccia ("local", "webdav", "s3"...).
    fn kind(&self) -> &'static str;
    /// Dimentica la disponibilità memorizzata: alla prossima domanda si verifica di nuovo.
    fn refresh(&self) {}
}

/// Apre tutti i backend della configurazione. Non tocca la rete: un backend senza credenziali o
/// irraggiungibile risulta semplicemente non disponibile.
pub fn open_all(cfg: &Config, creds: &Arc<Credentials>) -> Vec<Arc<dyn Backend>> {
    cfg.backends.iter().map(|b| open_one(b, &cfg.apps, creds)).collect()
}

/// Apre un backend.
pub fn open_one(b: &BackendConfig, apps: &AppsConfig, creds: &Arc<Credentials>) -> Arc<dyn Backend> {
    let name = b.name.clone();
    let backend: Arc<dyn Backend> = match &b.kind {
        BackendKind::Local { path } => Arc::new(Local { name, root: path.clone() }),
        BackendKind::Webdav { url, user } => Arc::new(webdav::Webdav::new(name, url, user, creds.clone())),
        BackendKind::S3 { endpoint, region, bucket, access_key, prefix, path_style } => Arc::new(s3::S3::new(
            name,
            s3::S3Params {
                endpoint: endpoint.clone(),
                region: region.clone(),
                bucket: bucket.clone(),
                access_key: access_key.clone(),
                prefix: prefix.clone(),
                path_style: *path_style,
            },
            creds.clone(),
        )),
        BackendKind::Dropbox { folder, api_base } => {
            Arc::new(dropbox::Dropbox::new(name, folder, api_base.as_deref(), apps, creds.clone()))
        }
        BackendKind::Onedrive { folder, api_base } => {
            Arc::new(onedrive::OneDrive::new(name, folder, api_base.as_deref(), apps, creds.clone()))
        }
        BackendKind::Gdrive { folder, folder_id, api_base } => Arc::new(gdrive::GDrive::new(
            name,
            folder,
            folder_id.as_deref(),
            api_base.as_deref(),
            apps,
            creds.clone(),
        )),
    };
    if b.delay_ms > 0 {
        Arc::new(Slow { inner: backend, delay: std::time::Duration::from_millis(b.delay_ms) })
    } else {
        backend
    }
}

/// Prova un backend appena configurato: crea la cartella del repository, scrive, rilegge, elenca e
/// cancella un oggetto di prova. Restituisce un errore comprensibile se qualcosa non va.
pub fn probe(b: &dyn Backend) -> Result<()> {
    b.refresh();
    b.create().context("creazione della cartella del repository")?;
    b.refresh();
    let key = format!("{PROBE_DIR}/{}", random_hex(8));
    let data = random_hex(64).into_bytes();
    b.put(&key, &data).context("scrittura di prova")?;
    let back = b.get(&key).context("lettura di prova")?;
    let listed = b.list(PROBE_DIR).context("elenco di prova");
    let deleted = b.delete(&key).context("cancellazione di prova");
    if back.as_deref() != Some(&data[..]) {
        bail!("il file di prova riletto non corrisponde a quello scritto");
    }
    if !listed?.contains(&key) {
        bail!("il file di prova non compare nell'elenco della cartella");
    }
    deleted?;
    if b.get(&key)?.is_some() {
        bail!("il file di prova risulta ancora presente dopo la cancellazione");
    }
    // la cartella della prova non deve restare sul servizio
    b.remove_dir(PROBE_DIR).context("cancellazione della cartella di prova")?;
    Ok(())
}

/// Cartella usata solo dalla prova di un backend nuovo, cancellata alla fine.
pub const PROBE_DIR: &str = "prova";

/// Un backend rallentato apposta (`delay_ms` nella configurazione): per provare le letture con un
/// account lento senza averne uno.
struct Slow {
    inner: Arc<dyn Backend>,
    delay: std::time::Duration,
}

impl Backend for Slow {
    fn name(&self) -> &str {
        self.inner.name()
    }
    fn location(&self) -> String {
        format!("{} (rallentato di {} ms)", self.inner.location(), self.delay.as_millis())
    }
    fn is_available(&self) -> bool {
        self.inner.is_available()
    }
    fn create(&self) -> Result<()> {
        self.inner.create()
    }
    fn put(&self, key: &str, data: &[u8]) -> Result<()> {
        self.inner.put(key, data)
    }
    fn get(&self, key: &str) -> Result<Option<Vec<u8>>> {
        std::thread::sleep(self.delay);
        self.inner.get(key)
    }
    fn delete(&self, key: &str) -> Result<()> {
        self.inner.delete(key)
    }
    fn list(&self, dir: &str) -> Result<Vec<String>> {
        self.inner.list(dir)
    }
    fn remove_dir(&self, dir: &str) -> Result<()> {
        self.inner.remove_dir(dir)
    }
    fn space(&self) -> Result<Option<Space>> {
        self.inner.space()
    }
    fn list_all(&self, dir: &str) -> Result<Vec<(String, u64)>> {
        self.inner.list_all(dir)
    }
    fn kind(&self) -> &'static str {
        self.inner.kind()
    }
    fn refresh(&self) {
        self.inner.refresh()
    }
}

fn check_key(key: &str) -> Result<()> {
    if key.is_empty() || key.starts_with('/') || key.split('/').any(|p| p.is_empty() || p == "." || p == "..") {
        bail!("chiave non valida: {key:?}");
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Cartella locale

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

    fn kind(&self) -> &'static str {
        "local"
    }

    fn location(&self) -> String {
        self.root.display().to_string()
    }

    fn is_available(&self) -> bool {
        self.root.is_dir()
    }

    fn space(&self) -> Result<Option<Space>> {
        use std::os::unix::ffi::OsStrExt;
        use std::os::unix::fs::MetadataExt;
        let meta = fs::metadata(&self.root).with_context(|| format!("backend {}: {}", self.name, self.root.display()))?;
        let c = std::ffi::CString::new(self.root.as_os_str().as_bytes())?;
        // SAFETY: percorso terminato da zero e struttura inizializzata da statvfs
        let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
            bail!("backend {}: statvfs: {}", self.name, std::io::Error::last_os_error());
        }
        let unit = st.f_frsize as u64;
        Ok(Some(Space {
            free: st.f_bavail as u64 * unit,
            total: Some(st.f_blocks as u64 * unit),
            device: Some(meta.dev()),
        }))
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
        // scrittura atomica: file temporaneo con nome unico (più thread), fsync, rename
        let mut tmp = path.clone().into_os_string();
        tmp.push(format!(".{}.tmp", random_hex(4)));
        let tmp = PathBuf::from(tmp);
        let result = (|| -> std::io::Result<()> {
            let mut f = fs::File::create(&tmp)?;
            f.write_all(data)?;
            f.sync_all()?;
            fs::rename(&tmp, &path)
        })();
        if result.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        result.with_context(ctx)
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

    fn remove_dir(&self, dir: &str) -> Result<()> {
        match fs::remove_dir_all(self.path(dir)?) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e).with_context(|| format!("backend {}: cancellazione della cartella {dir}", self.name)),
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
            let entry = entry?;
            if !entry.file_type()?.is_file() {
                continue;
            }
            if let Some(name) = entry.file_name().to_str() {
                if !name.ends_with(".tmp") {
                    keys.push(format!("{dir}/{name}"));
                }
            }
        }
        Ok(keys)
    }

    fn list_all(&self, dir: &str) -> Result<Vec<(String, u64)>> {
        let base = self.path(dir)?;
        if !base.exists() {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        for entry in walkdir::WalkDir::new(&base) {
            let entry = entry.with_context(|| format!("backend {}: elenco di {dir}", self.name))?;
            if !entry.file_type().is_file() {
                continue;
            }
            let rel = entry.path().strip_prefix(&self.root).expect("sotto la radice");
            if let Some(key) = rel.to_str() {
                out.push((key.to_string(), entry.metadata().map(|m| m.len()).unwrap_or(0)));
            }
        }
        Ok(out)
    }
}
