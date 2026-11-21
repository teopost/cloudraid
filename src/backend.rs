//! Backend di storage: dove finiscono i frammenti.
//!
//! - `local`: una cartella o un disco montato.
//! - `rclone`: qualsiasi remote di rclone (Google Drive, OneDrive, Dropbox, MEGA, SFTP, WebDAV, S3…).
//!   Un solo processo `rclone rcd` serve tutti i backend: login OAuth, rinnovo dei token,
//!   limiti di traffico dei provider e cache delle cartelle restano a rclone.

use anyhow::{anyhow, bail, Context, Result};
use base64::Engine;
use serde_json::{json, Value};
use std::fs;
use std::io::{ErrorKind, Read, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::config::{BackendKind, Config, RcloneConfig};
use crate::crypto::random_hex;

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
    /// Tipo di backend, per l'interfaccia ("local", "rclone").
    fn kind(&self) -> &'static str;
    /// Dimentica la disponibilità memorizzata: alla prossima domanda si verifica di nuovo.
    fn refresh(&self) {}
}

/// Apre tutti i backend della configurazione; avvia rclone solo se serve.
pub fn open_all(cfg: &Config) -> Result<Vec<Box<dyn Backend>>> {
    let needs_rclone = cfg.backends.iter().any(|b| matches!(b.kind, BackendKind::Rclone { .. }));
    let daemon = if needs_rclone { Some(Arc::new(RcloneDaemon::start(&cfg.rclone)?)) } else { None };
    Ok(cfg
        .backends
        .iter()
        .map(|b| -> Box<dyn Backend> {
            match &b.kind {
                BackendKind::Local { path } => Box::new(Local { name: b.name.clone(), root: path.clone() }),
                BackendKind::Rclone { remote } => Box::new(Rclone {
                    name: b.name.clone(),
                    remote: remote.clone(),
                    daemon: daemon.clone().expect("rclone avviato"),
                    available: Mutex::new(None),
                }),
            }
        })
        .collect())
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
}

// ---------------------------------------------------------------------------------------------
// rclone

/// Errore di una chiamata a rclone: "non trovato" va distinto dagli altri.
enum RcError {
    NotFound,
    Other(anyhow::Error),
}

/// Il processo `rclone rcd`, raggiungibile solo da 127.0.0.1 con credenziali casuali.
pub struct RcloneDaemon {
    child: Mutex<Child>,
    base: String,
    auth: String,
    agent: ureq::Agent,
}

/// Codifica percentuale di tutto tranne i caratteri non riservati.
fn enc(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

fn error_message(resp: ureq::Response) -> String {
    let body = resp.into_string().unwrap_or_default();
    serde_json::from_str::<Value>(&body)
        .ok()
        .and_then(|v| v.get("error").and_then(Value::as_str).map(str::to_string))
        .unwrap_or(body)
}

impl RcloneDaemon {
    pub fn start(cfg: &RcloneConfig) -> Result<RcloneDaemon> {
        let port = std::net::TcpListener::bind("127.0.0.1:0")?.local_addr()?.port();
        let user = "cloudraid";
        let pass = random_hex(16);
        let mut cmd = Command::new(&cfg.binary);
        cmd.arg("rcd").arg("--rc-addr").arg(format!("127.0.0.1:{port}")).arg("--rc-serve");
        if let Some(conf) = &cfg.config {
            cmd.arg("--config").arg(conf);
        }
        // credenziali nelle variabili d'ambiente, non visibili con ps
        cmd.args(&cfg.args)
            .env("RCLONE_RC_USER", user)
            .env("RCLONE_RC_PASS", &pass)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let child = cmd.spawn().with_context(|| format!("impossibile avviare {} (rclone è installato?)", cfg.binary))?;
        let daemon = RcloneDaemon {
            child: Mutex::new(child),
            base: format!("http://127.0.0.1:{port}"),
            auth: format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(format!("{user}:{pass}"))),
            agent: ureq::AgentBuilder::new()
                .timeout_connect(Duration::from_secs(5))
                .timeout(Duration::from_secs(30 * 60))
                .build(),
        };
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if daemon.call("rc/noop", &json!({})).is_ok() {
                return Ok(daemon);
            }
            if let Some(status) = daemon.child.lock().unwrap().try_wait()? {
                bail!("rclone rcd è terminato subito ({status}): prova a lanciare `{} rcd` a mano", cfg.binary);
            }
            if Instant::now() > deadline {
                bail!("rclone rcd non risponde dopo 20 secondi");
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    fn check(&self, result: Result<ureq::Response, ureq::Error>, what: &str) -> Result<ureq::Response, RcError> {
        match result {
            Ok(resp) => Ok(resp),
            Err(ureq::Error::Status(404, _)) => Err(RcError::NotFound),
            Err(ureq::Error::Status(code, resp)) => {
                Err(RcError::Other(anyhow!("rclone {what}: HTTP {code}: {}", error_message(resp))))
            }
            Err(e) => Err(RcError::Other(anyhow!("rclone {what}: {e}"))),
        }
    }

    fn call(&self, op: &str, body: &Value) -> Result<Value, RcError> {
        let resp = self.agent.post(&format!("{}/{op}", self.base)).set("Authorization", &self.auth).send_json_str(body);
        let resp = self.check(resp, op)?;
        let text = resp.into_string().map_err(|e| RcError::Other(anyhow!("rclone {op}: {e}")))?;
        serde_json::from_str(&text).map_err(|e| RcError::Other(anyhow!("rclone {op}: risposta non valida: {e}")))
    }

    fn upload(&self, fs: &str, key: &str, data: &[u8]) -> Result<(), RcError> {
        let (dir, name) = key.rsplit_once('/').unwrap_or(("", key));
        let boundary = format!("cloudraid{}", random_hex(12));
        let mut body = Vec::with_capacity(data.len() + 256);
        let _ = write!(
            body,
            "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{name}\"\r\n\
             Content-Type: application/octet-stream\r\n\r\n"
        );
        body.extend_from_slice(data);
        let _ = write!(body, "\r\n--{boundary}--\r\n");
        let url = format!("{}/operations/uploadfile?fs={}&remote={}", self.base, enc(fs), enc(dir));
        let resp = self
            .agent
            .post(&url)
            .set("Authorization", &self.auth)
            .set("Content-Type", &format!("multipart/form-data; boundary={boundary}"))
            .send_bytes(&body);
        self.check(resp, "uploadfile").map(|_| ())
    }

    fn download(&self, fs: &str, key: &str) -> Result<Vec<u8>, RcError> {
        // con --rc-serve gli oggetti sono serviti su /[remote:percorso]/chiave
        let url = format!("{}/%5B{}%5D/{}", self.base, enc(fs), key);
        let resp = self.check(self.agent.get(&url).set("Authorization", &self.auth).call(), "download")?;
        let mut data = Vec::new();
        resp.into_reader()
            .read_to_end(&mut data)
            .map_err(|e| RcError::Other(anyhow!("rclone download: {e}")))?;
        Ok(data)
    }
}

trait SendJson {
    fn send_json_str(self, body: &Value) -> Result<ureq::Response, ureq::Error>;
}

impl SendJson for ureq::Request {
    fn send_json_str(self, body: &Value) -> Result<ureq::Response, ureq::Error> {
        self.set("Content-Type", "application/json").send_string(&body.to_string())
    }
}

impl Drop for RcloneDaemon {
    fn drop(&mut self) {
        let _ = self.agent.post(&format!("{}/core/quit", self.base)).set("Authorization", &self.auth).send_json_str(&json!({}));
        let mut child = self.child.lock().unwrap();
        for _ in 0..20 {
            if let Ok(Some(_)) = child.try_wait() {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = child.kill();
        let _ = child.wait();
    }
}

pub struct Rclone {
    name: String,
    remote: String,
    daemon: Arc<RcloneDaemon>,
    /// Esito della prima verifica di disponibilità.
    available: Mutex<Option<bool>>,
}

const ATTEMPTS: u32 = 4;

impl Rclone {
    /// Ripete le operazioni fallite per errori temporanei; "non trovato" diventa `None`.
    fn retry<T>(&self, what: &str, op: impl Fn() -> Result<T, RcError>) -> Result<Option<T>> {
        let mut attempt = 0;
        loop {
            match op() {
                Ok(v) => return Ok(Some(v)),
                Err(RcError::NotFound) => return Ok(None),
                Err(RcError::Other(e)) => {
                    attempt += 1;
                    if attempt >= ATTEMPTS {
                        return Err(e.context(format!("backend {}: {what} fallito dopo {ATTEMPTS} tentativi", self.name)));
                    }
                    std::thread::sleep(Duration::from_millis(500 << attempt));
                }
            }
        }
    }

    fn ensure_available(&self) -> Result<()> {
        if !self.is_available() {
            bail!("backend {} non disponibile ({})", self.name, self.remote);
        }
        Ok(())
    }
}

impl Backend for Rclone {
    fn name(&self) -> &str {
        &self.name
    }

    fn kind(&self) -> &'static str {
        "rclone"
    }

    fn refresh(&self) {
        *self.available.lock().unwrap() = None;
    }

    fn location(&self) -> String {
        self.remote.clone()
    }

    fn is_available(&self) -> bool {
        let mut cached = self.available.lock().unwrap();
        if let Some(v) = *cached {
            return v;
        }
        // la cartella del repository deve esistere: se manca, l'account è perso o nuovo
        let probe = self.retry("verifica", || self.daemon.call("operations/list", &json!({"fs": self.remote, "remote": ""})));
        let v = match probe {
            Ok(Some(_)) => true,
            Ok(None) => false,
            Err(e) => {
                eprintln!("attenzione: {e:#}");
                false
            }
        };
        *cached = Some(v);
        v
    }

    fn create(&self) -> Result<()> {
        self.retry("creazione", || self.daemon.call("operations/mkdir", &json!({"fs": self.remote, "remote": ""})))?;
        *self.available.lock().unwrap() = Some(true);
        Ok(())
    }

    fn put(&self, key: &str, data: &[u8]) -> Result<()> {
        check_key(key)?;
        self.ensure_available()?;
        self.retry(&format!("scrittura di {key}"), || self.daemon.upload(&self.remote, key, data))?
            .ok_or_else(|| anyhow!("backend {}: scrittura di {key}: percorso non trovato", self.name))
    }

    fn get(&self, key: &str) -> Result<Option<Vec<u8>>> {
        check_key(key)?;
        self.ensure_available()?;
        self.retry(&format!("lettura di {key}"), || self.daemon.download(&self.remote, key))
    }

    fn delete(&self, key: &str) -> Result<()> {
        check_key(key)?;
        self.ensure_available()?;
        self.retry(&format!("cancellazione di {key}"), || {
            self.daemon.call("operations/deletefile", &json!({"fs": self.remote, "remote": key}))
        })?;
        Ok(())
    }

    fn list(&self, dir: &str) -> Result<Vec<String>> {
        check_key(dir)?;
        self.ensure_available()?;
        let Some(v) = self.retry(&format!("elenco di {dir}"), || {
            self.daemon.call("operations/list", &json!({"fs": self.remote, "remote": dir}))
        })?
        else {
            return Ok(Vec::new());
        };
        let items = v.get("list").and_then(Value::as_array).cloned().unwrap_or_default();
        Ok(items
            .iter()
            .filter(|it| !it.get("IsDir").and_then(Value::as_bool).unwrap_or(false))
            .filter_map(|it| it.get("Path").and_then(Value::as_str).map(str::to_string))
            .collect())
    }
}
