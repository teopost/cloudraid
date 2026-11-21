//! WebDAV (Nextcloud, ownCloud, NAS...): PUT/GET/DELETE sugli oggetti, MKCOL per le cartelle,
//! PROPFIND con profondità 1 per gli elenchi (molti server, Nextcloud compreso, non permettono la
//! profondità infinita). Autenticazione Basic; la password sta nelle credenziali cifrate.

use anyhow::{anyhow, Result};
use base64::Engine;
use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use super::http::{self, Availability, Outcome, Step};
use super::Backend;
use crate::credentials::Credentials;
use crate::xml;

pub struct Webdav {
    name: String,
    /// L'indirizzo della cartella del repository, senza `/` finale.
    base: String,
    /// Il percorso dell'indirizzo (decodificato), per riconoscere gli href delle risposte.
    base_path: String,
    user: String,
    creds: Arc<Credentials>,
    avail: Availability,
    /// Cartelle già create o viste: niente MKCOL ripetuti.
    dirs: Mutex<HashSet<String>>,
}

const PROPFIND: &[u8] = br#"<?xml version="1.0" encoding="utf-8"?><d:propfind xmlns:d="DAV:"><d:prop><d:resourcetype/><d:getcontentlength/></d:prop></d:propfind>"#;

impl Webdav {
    pub fn new(name: String, url: &str, user: &str, creds: Arc<Credentials>) -> Webdav {
        let base = url.trim_end_matches('/').to_string();
        let after_scheme = base.split_once("://").map(|(_, r)| r).unwrap_or(&base);
        let base_path = http::decode(after_scheme.find('/').map(|i| &after_scheme[i..]).unwrap_or(""));
        Webdav {
            name,
            base,
            base_path: base_path.trim_end_matches('/').to_string(),
            user: user.to_string(),
            creds,
            avail: Availability::default(),
            dirs: Mutex::new(HashSet::new()),
        }
    }

    fn url(&self, key: &str) -> String {
        if key.is_empty() {
            format!("{}/", self.base)
        } else {
            format!("{}/{}", self.base, http::encode_path(key))
        }
    }

    fn auth(&self) -> Result<String> {
        let pw = self
            .creds
            .get(&self.name, "password")
            .ok_or_else(|| anyhow!("backend {}: manca la password WebDAV nelle credenziali", self.name))?;
        let token = base64::engine::general_purpose::STANDARD.encode(format!("{}:{pw}", self.user));
        Ok(format!("Basic {token}"))
    }

    fn request(&self, method: &str, url: &str) -> Result<ureq::Request> {
        Ok(http::agent().request(method, url).set("Authorization", &self.auth()?))
    }

    /// Crea la cartella `dir` e quelle che la contengono, se servono.
    fn ensure_dir(&self, dir: &str) -> Result<()> {
        if dir.is_empty() || self.dirs.lock().unwrap().contains(dir) {
            return Ok(());
        }
        if let Some((parent, _)) = dir.rsplit_once('/') {
            self.ensure_dir(parent)?;
        }
        let url = format!("{}/", self.url(dir));
        http::retry(&format!("backend {}: creazione di {dir}", self.name), || {
            let req = match self.request("MKCOL", &url) {
                Ok(r) => r,
                Err(e) => return Step::Fail(e),
            };
            match http::send(req, None) {
                // 405: esiste già
                Outcome::Response(r) if (200..300).contains(&r.status()) || r.status() == 405 => Step::Done(()),
                Outcome::Response(r) => http::unexpected(r),
                Outcome::Transport(e) => Step::Retry(e, None),
            }
        })?;
        self.dirs.lock().unwrap().insert(dir.to_string());
        Ok(())
    }

    /// PROPFIND con profondità 1: (chiave relativa alla radice, è una cartella, dimensione).
    fn propfind(&self, dir: &str) -> Result<Option<Vec<(String, bool, u64)>>> {
        let url = if dir.is_empty() { self.url("") } else { format!("{}/", self.url(dir)) };
        let text = http::retry(&format!("backend {}: elenco di {dir}", self.name), || {
            let req = match self.request("PROPFIND", &url) {
                Ok(r) => r.set("Depth", "1").set("Content-Type", "application/xml; charset=utf-8"),
                Err(e) => return Step::Fail(e),
            };
            match http::send(req, Some(PROPFIND)) {
                Outcome::Response(r) if r.status() == 207 => match http::body(r) {
                    Ok(b) => Step::Done(Some(String::from_utf8_lossy(&b).into_owned())),
                    Err(e) => Step::Retry(e, None),
                },
                Outcome::Response(r) if r.status() == 404 => Step::Done(None),
                Outcome::Response(r) if r.status() == 401 || r.status() == 403 => {
                    Step::Fail(anyhow!("accesso negato (utente o password sbagliati?)"))
                }
                Outcome::Response(r) => http::unexpected(r),
                Outcome::Transport(e) => Step::Retry(e, None),
            }
        })?;
        let Some(text) = text else { return Ok(None) };
        let mut out = Vec::new();
        for resp in xml::elements(&text, "response") {
            let Some(href) = xml::text(resp, "href") else { continue };
            // l'href può essere un indirizzo completo o solo il percorso
            let path = match href.split_once("://") {
                Some((_, rest)) => rest.find('/').map(|i| rest[i..].to_string()).unwrap_or_default(),
                None => href,
            };
            let path = http::decode(&path);
            let Some(rel) = path.strip_prefix(&self.base_path) else { continue };
            let rel = rel.trim_matches('/');
            if rel.is_empty() || rel == dir {
                continue;
            }
            let is_dir = xml::has(resp, "collection");
            let size = xml::text(resp, "getcontentlength").and_then(|s| s.parse().ok()).unwrap_or(0);
            out.push((rel.to_string(), is_dir, size));
        }
        Ok(Some(out))
    }
}

impl Backend for Webdav {
    fn name(&self) -> &str {
        &self.name
    }

    fn location(&self) -> String {
        format!("{} (utente {})", self.base, self.user)
    }

    fn is_available(&self) -> bool {
        self.avail.get(|| Ok(self.propfind("")?.is_some()), &self.name)
    }

    fn create(&self) -> Result<()> {
        // la cartella del repository e quelle che la contengono, a partire dalla radice del server
        let url = format!("{}/", self.base);
        http::retry(&format!("backend {}: creazione della cartella", self.name), || {
            let req = match self.request("MKCOL", &url) {
                Ok(r) => r,
                Err(e) => return Step::Fail(e),
            };
            match http::send(req, None) {
                Outcome::Response(r) if (200..300).contains(&r.status()) || r.status() == 405 => Step::Done(()),
                Outcome::Response(r) if r.status() == 409 => {
                    Step::Fail(anyhow!("la cartella che deve contenere {} non esiste sul server", self.base))
                }
                Outcome::Response(r) if r.status() == 401 || r.status() == 403 => {
                    Step::Fail(anyhow!("accesso negato (utente o password sbagliati?)"))
                }
                Outcome::Response(r) => http::unexpected(r),
                Outcome::Transport(e) => Step::Retry(e, None),
            }
        })?;
        self.avail.set(true);
        Ok(())
    }

    fn put(&self, key: &str, data: &[u8]) -> Result<()> {
        super::check_key(key)?;
        if let Some((dir, _)) = key.rsplit_once('/') {
            self.ensure_dir(dir)?;
        }
        let url = self.url(key);
        let mut fixed_dirs = false;
        http::retry(&format!("backend {}: scrittura di {key}", self.name), || {
            let req = match self.request("PUT", &url) {
                Ok(r) => r.set("Content-Type", "application/octet-stream"),
                Err(e) => return Step::Fail(e),
            };
            match http::send(req, Some(data)) {
                Outcome::Response(r) if (200..300).contains(&r.status()) => Step::Done(()),
                // cartella sparita (cancellata da fuori): si ricrea una volta
                Outcome::Response(r) if (r.status() == 409 || r.status() == 404) && !fixed_dirs => {
                    fixed_dirs = true;
                    self.dirs.lock().unwrap().clear();
                    match key.rsplit_once('/').map(|(d, _)| self.ensure_dir(d)).transpose() {
                        Ok(_) => Step::Retry(anyhow!("cartella mancante, ricreata"), Some(std::time::Duration::ZERO)),
                        Err(e) => Step::Fail(e),
                    }
                }
                Outcome::Response(r) => http::unexpected(r),
                Outcome::Transport(e) => Step::Retry(e, None),
            }
        })
    }

    fn get(&self, key: &str) -> Result<Option<Vec<u8>>> {
        super::check_key(key)?;
        let url = self.url(key);
        http::retry(&format!("backend {}: lettura di {key}", self.name), || {
            let req = match self.request("GET", &url) {
                Ok(r) => r,
                Err(e) => return Step::Fail(e),
            };
            match http::send(req, None) {
                Outcome::Response(r) if r.status() == 200 => match http::body(r) {
                    Ok(b) => Step::Done(Some(b)),
                    Err(e) => Step::Retry(e, None),
                },
                Outcome::Response(r) if r.status() == 404 => Step::Done(None),
                Outcome::Response(r) => http::unexpected(r),
                Outcome::Transport(e) => Step::Retry(e, None),
            }
        })
    }

    fn delete(&self, key: &str) -> Result<()> {
        super::check_key(key)?;
        let url = self.url(key);
        http::retry(&format!("backend {}: cancellazione di {key}", self.name), || {
            let req = match self.request("DELETE", &url) {
                Ok(r) => r,
                Err(e) => return Step::Fail(e),
            };
            match http::send(req, None) {
                Outcome::Response(r) if (200..300).contains(&r.status()) || r.status() == 404 => Step::Done(()),
                Outcome::Response(r) => http::unexpected(r),
                Outcome::Transport(e) => Step::Retry(e, None),
            }
        })
    }

    fn list(&self, dir: &str) -> Result<Vec<String>> {
        super::check_key(dir)?;
        Ok(self.propfind(dir)?.unwrap_or_default().into_iter().filter(|(_, d, _)| !d).map(|(k, _, _)| k).collect())
    }

    fn list_all(&self, dir: &str) -> Result<Vec<(String, u64)>> {
        super::check_key(dir)?;
        let mut out = Vec::new();
        let mut todo = vec![dir.to_string()];
        while let Some(d) = todo.pop() {
            for (key, is_dir, size) in self.propfind(&d)?.unwrap_or_default() {
                if is_dir {
                    todo.push(key);
                } else {
                    out.push((key, size));
                }
            }
        }
        Ok(out)
    }

    fn kind(&self) -> &'static str {
        "webdav"
    }

    fn refresh(&self) {
        self.avail.reset();
        self.dirs.lock().unwrap().clear();
    }
}
