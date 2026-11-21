//! Google Drive (API v3) con lo scope `drive.file`: cloudraid vede solo i file che ha creato lui,
//! non il resto del Drive. Drive non ha percorsi ma identificativi, e permette più file con lo stesso
//! nome: si tiene una cache percorso → identificativo per cartelle e file, e la creazione delle
//! cartelle avviene una alla volta (due caricamenti paralleli non creano due cartelle `data/ab`).
//! Caricamento multipart fino a 4 MB, "resumable" oltre (l'indice di un repository grande).

use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use super::http::{self, Availability, Outcome, Step};
use super::oauth::{Provider, TokenSource};
use super::Backend;
use crate::config::AppsConfig;
use crate::credentials::Credentials;
use crate::crypto::random_hex;

const FOLDER_MIME: &str = "application/vnd.google-apps.folder";
/// Oltre questa dimensione si usa il caricamento "resumable".
const SIMPLE_MAX: usize = 4 << 20;

pub struct GDrive {
    name: String,
    folder: String,
    api: String,
    tokens: TokenSource,
    avail: Availability,
    /// Percorso relativo della cartella (`""` è la radice del repository) → identificativo.
    dirs: Mutex<HashMap<String, String>>,
    /// Chiave → identificativo del file.
    files: Mutex<HashMap<String, String>>,
    /// Una creazione di cartella alla volta.
    creating: Mutex<()>,
}

/// Una stringa fra apici per le query di Drive.
fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\\', "\\\\").replace('\'', "\\'"))
}

/// Il motivo di un errore di Drive (per riconoscere i limiti di velocità).
fn is_rate_limit(text: &str) -> bool {
    text.contains("rateLimitExceeded") || text.contains("userRateLimitExceeded") || text.contains("backendError")
}

/// Crea (se manca) la cartella del repository nel Drive e ne restituisce l'identificativo, da
/// scrivere nella configurazione.
pub fn ensure_folder(
    name: &str,
    folder: &str,
    api_base: Option<&str>,
    apps: &AppsConfig,
    creds: Arc<Credentials>,
) -> Result<String> {
    let g = GDrive::new(name.to_string(), folder, None, api_base, apps, creds);
    g.dir_id("", true)?.ok_or_else(|| anyhow!("cartella non creata"))
}

impl GDrive {
    pub fn new(
        name: String,
        folder: &str,
        folder_id: Option<&str>,
        api_base: Option<&str>,
        apps: &AppsConfig,
        creds: Arc<Credentials>,
    ) -> GDrive {
        let mut dirs = HashMap::new();
        if let Some(id) = folder_id.filter(|s| !s.is_empty()) {
            dirs.insert(String::new(), id.to_string());
        }
        GDrive {
            tokens: TokenSource::new(Provider::Google, &name, apps, api_base, creds),
            name,
            folder: folder.to_string(),
            api: api_base.map(|b| b.trim_end_matches('/').to_string()).unwrap_or_else(|| "https://www.googleapis.com".into()),
            avail: Availability::default(),
            dirs: Mutex::new(dirs),
            files: Mutex::new(HashMap::new()),
            creating: Mutex::new(()),
        }
    }

    fn call<T>(
        &self,
        what: &str,
        build: impl Fn(&str) -> (ureq::Request, Option<Vec<u8>>),
        mut handle: impl FnMut(ureq::Response) -> Step<T>,
    ) -> Result<T> {
        http::retry(&format!("backend {}: {what}", self.name), || {
            let token = match self.tokens.access_token() {
                Ok(t) => t,
                Err(e) => return Step::Fail(e),
            };
            let (req, body) = build(&token);
            match http::send(req, body.as_deref()) {
                Outcome::Response(r) if r.status() == 401 => {
                    self.tokens.invalidate();
                    Step::Retry(anyhow!("token scaduto"), Some(std::time::Duration::ZERO))
                }
                Outcome::Response(r) if r.status() == 403 => {
                    let wait = http::retry_after(&r);
                    let text = http::body_text(r);
                    if is_rate_limit(&text) {
                        Step::Retry(anyhow!("limite di richieste di Google: {text}"), wait)
                    } else {
                        Step::Fail(anyhow!("accesso negato: {text}"))
                    }
                }
                Outcome::Response(r) if http::transient(r.status()) => http::unexpected(r),
                Outcome::Response(r) => handle(r),
                Outcome::Transport(e) => Step::Retry(e, None),
            }
        })
    }

    fn json_or_fail(r: ureq::Response) -> Step<Value> {
        match r.status() {
            200 | 201 => match http::body(r) {
                Ok(b) => Step::Done(serde_json::from_slice(&b).unwrap_or(Value::Null)),
                Err(e) => Step::Retry(e, None),
            },
            s => Step::Fail(anyhow!("risposta {s}: {}", http::body_text(r))),
        }
    }

    /// Cerca per nome dentro una cartella: i figli (id, nome, dimensione, è cartella), a pagine.
    fn query(&self, q: &str) -> Result<Vec<(String, String, u64, bool)>> {
        let mut out = Vec::new();
        let mut page: Option<String> = None;
        loop {
            let mut url = format!(
                "{}/drive/v3/files?q={}&fields=nextPageToken,files(id,name,size,mimeType)&pageSize=1000&spaces=drive",
                self.api,
                http::encode(q)
            );
            if let Some(p) = &page {
                url.push_str(&format!("&pageToken={}", http::encode(p)));
            }
            let v = self.call(
                "ricerca",
                |token| (http::agent().get(&url).set("Authorization", &format!("Bearer {token}")), None),
                Self::json_or_fail,
            )?;
            for f in v.get("files").and_then(Value::as_array).into_iter().flatten() {
                let id = f.get("id").and_then(Value::as_str).unwrap_or_default().to_string();
                let name = f.get("name").and_then(Value::as_str).unwrap_or_default().to_string();
                // la dimensione arriva come stringa
                let size = f.get("size").and_then(Value::as_str).and_then(|s| s.parse().ok()).unwrap_or(0);
                let is_dir = f.get("mimeType").and_then(Value::as_str) == Some(FOLDER_MIME);
                out.push((id, name, size, is_dir));
            }
            page = v.get("nextPageToken").and_then(Value::as_str).map(str::to_string);
            if page.is_none() {
                return Ok(out);
            }
        }
    }

    fn find(&self, parent: &str, name: &str, folder: bool) -> Result<Option<String>> {
        let kind = if folder { format!("mimeType = '{FOLDER_MIME}'") } else { format!("mimeType != '{FOLDER_MIME}'") };
        let q = format!("name = {} and {} in parents and {kind} and trashed = false", quote(name), quote(parent));
        Ok(self.query(&q)?.into_iter().next().map(|(id, ..)| id))
    }

    fn create_folder(&self, parent: &str, name: &str) -> Result<String> {
        let url = format!("{}/drive/v3/files?fields=id", self.api);
        let body = json!({"name": name, "mimeType": FOLDER_MIME, "parents": [parent]}).to_string().into_bytes();
        let v = self.call(
            &format!("creazione della cartella {name}"),
            |token| {
                let req = http::agent()
                    .post(&url)
                    .set("Authorization", &format!("Bearer {token}"))
                    .set("Content-Type", "application/json; charset=UTF-8");
                (req, Some(body.clone()))
            },
            Self::json_or_fail,
        )?;
        v.get("id").and_then(Value::as_str).map(str::to_string).ok_or_else(|| anyhow!("cartella creata senza identificativo"))
    }

    /// L'identificativo di una cartella del repository (`""`: la radice), creandola se `create`.
    fn dir_id(&self, dir: &str, create: bool) -> Result<Option<String>> {
        if let Some(id) = self.dirs.lock().unwrap().get(dir) {
            return Ok(Some(id.clone()));
        }
        let (parent_id, name) = if dir.is_empty() {
            ("root".to_string(), self.folder.as_str())
        } else {
            let (parent, name) = dir.rsplit_once('/').unwrap_or(("", dir));
            match self.dir_id(parent, create)? {
                Some(p) => (p, name),
                None => return Ok(None),
            }
        };
        let mut found = self.find(&parent_id, name, true)?;
        if found.is_none() && create {
            let _one = self.creating.lock().unwrap();
            // ricontrollo: un altro thread potrebbe averla appena creata
            if let Some(id) = self.dirs.lock().unwrap().get(dir) {
                return Ok(Some(id.clone()));
            }
            found = match self.find(&parent_id, name, true)? {
                Some(id) => Some(id),
                None => Some(self.create_folder(&parent_id, name)?),
            };
            if let Some(id) = &found {
                self.dirs.lock().unwrap().insert(dir.to_string(), id.clone());
            }
        }
        if let Some(id) = &found {
            self.dirs.lock().unwrap().insert(dir.to_string(), id.clone());
        }
        Ok(found)
    }

    fn file_id(&self, key: &str) -> Result<Option<String>> {
        if let Some(id) = self.files.lock().unwrap().get(key) {
            return Ok(Some(id.clone()));
        }
        let (dir, name) = key.rsplit_once('/').unwrap_or(("", key));
        let Some(parent) = self.dir_id(dir, false)? else { return Ok(None) };
        let found = self.find(&parent, name, false)?;
        if let Some(id) = &found {
            self.files.lock().unwrap().insert(key.to_string(), id.clone());
        }
        Ok(found)
    }

    /// Caricamento "resumable": si apre la sessione, poi si manda il contenuto in un colpo.
    fn upload_resumable(&self, method: &str, url: &str, meta: &Value, data: &[u8]) -> Result<Value> {
        let meta = meta.to_string().into_bytes();
        let session = self.call(
            "apertura del caricamento",
            |token| {
                let req = http::agent()
                    .request(method, url)
                    .set("Authorization", &format!("Bearer {token}"))
                    .set("Content-Type", "application/json; charset=UTF-8")
                    .set("X-Upload-Content-Type", "application/octet-stream")
                    .set("X-Upload-Content-Length", &data.len().to_string());
                (req, Some(meta.clone()))
            },
            |r| match (r.status(), r.header("Location").map(str::to_string)) {
                (200, Some(loc)) => Step::Done(loc),
                (s, _) => Step::Fail(anyhow!("risposta {s}: {}", http::body_text(r))),
            },
        )?;
        self.call(
            "caricamento",
            |token| (http::agent().put(&session).set("Authorization", &format!("Bearer {token}")), Some(data.to_vec())),
            Self::json_or_fail,
        )
    }
}

impl Backend for GDrive {
    fn name(&self) -> &str {
        &self.name
    }

    fn location(&self) -> String {
        format!("Google Drive, cartella {}", self.folder)
    }

    fn is_available(&self) -> bool {
        self.avail.get(|| Ok(self.dir_id("", false)?.is_some()), &self.name)
    }

    fn create(&self) -> Result<()> {
        self.dir_id("", true)?;
        self.avail.reset();
        Ok(())
    }

    fn put(&self, key: &str, data: &[u8]) -> Result<()> {
        super::check_key(key)?;
        let (dir, name) = key.rsplit_once('/').unwrap_or(("", key));
        let parent = self.dir_id(dir, true)?.ok_or_else(|| anyhow!("cartella {dir} non creata"))?;
        let existing = self.file_id(key)?;
        let v = match (&existing, data.len() > SIMPLE_MAX) {
            (Some(id), false) => {
                let url = format!("{}/upload/drive/v3/files/{id}?uploadType=media&fields=id", self.api);
                self.call(
                    &format!("scrittura di {key}"),
                    |token| {
                        let req = http::agent()
                            .request("PATCH", &url)
                            .set("Authorization", &format!("Bearer {token}"))
                            .set("Content-Type", "application/octet-stream");
                        (req, Some(data.to_vec()))
                    },
                    Self::json_or_fail,
                )?
            }
            (Some(id), true) => {
                let url = format!("{}/upload/drive/v3/files/{id}?uploadType=resumable&fields=id", self.api);
                self.upload_resumable("PATCH", &url, &json!({}), data)?
            }
            (None, false) => {
                let boundary = format!("cloudraid{}", random_hex(12));
                let meta = json!({"name": name, "parents": [parent]}).to_string();
                let mut body = Vec::with_capacity(data.len() + 512);
                body.extend_from_slice(
                    format!("--{boundary}\r\nContent-Type: application/json; charset=UTF-8\r\n\r\n{meta}\r\n--{boundary}\r\nContent-Type: application/octet-stream\r\n\r\n")
                        .as_bytes(),
                );
                body.extend_from_slice(data);
                body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
                let url = format!("{}/upload/drive/v3/files?uploadType=multipart&fields=id", self.api);
                self.call(
                    &format!("scrittura di {key}"),
                    |token| {
                        let req = http::agent()
                            .post(&url)
                            .set("Authorization", &format!("Bearer {token}"))
                            .set("Content-Type", &format!("multipart/related; boundary={boundary}"));
                        (req, Some(body.clone()))
                    },
                    Self::json_or_fail,
                )?
            }
            (None, true) => {
                let url = format!("{}/upload/drive/v3/files?uploadType=resumable&fields=id", self.api);
                self.upload_resumable("POST", &url, &json!({"name": name, "parents": [parent]}), data)?
            }
        };
        let id = v.get("id").and_then(Value::as_str).ok_or_else(|| anyhow!("file caricato senza identificativo"))?;
        self.files.lock().unwrap().insert(key.to_string(), id.to_string());
        Ok(())
    }

    fn get(&self, key: &str) -> Result<Option<Vec<u8>>> {
        super::check_key(key)?;
        let Some(id) = self.file_id(key)? else { return Ok(None) };
        let url = format!("{}/drive/v3/files/{id}?alt=media", self.api);
        let data = self.call(
            &format!("lettura di {key}"),
            |token| (http::agent().get(&url).set("Authorization", &format!("Bearer {token}")), None),
            |r| match r.status() {
                200 => match http::body(r) {
                    Ok(b) => Step::Done(Some(b)),
                    Err(e) => Step::Retry(e, None),
                },
                404 => Step::Done(None),
                s => Step::Fail(anyhow!("risposta {s}: {}", http::body_text(r))),
            },
        )?;
        if data.is_none() {
            // l'identificativo in cache non vale più (file cancellato da fuori)
            self.files.lock().unwrap().remove(key);
        }
        Ok(data)
    }

    fn delete(&self, key: &str) -> Result<()> {
        super::check_key(key)?;
        let Some(id) = self.file_id(key)? else { return Ok(()) };
        let url = format!("{}/drive/v3/files/{id}", self.api);
        self.call(
            &format!("cancellazione di {key}"),
            |token| (http::agent().delete(&url).set("Authorization", &format!("Bearer {token}")), None),
            |r| match r.status() {
                200 | 204 | 404 => Step::Done(()),
                s => Step::Fail(anyhow!("risposta {s}: {}", http::body_text(r))),
            },
        )?;
        self.files.lock().unwrap().remove(key);
        Ok(())
    }

    fn list(&self, dir: &str) -> Result<Vec<String>> {
        super::check_key(dir)?;
        let Some(id) = self.dir_id(dir, false)? else { return Ok(Vec::new()) };
        let mut out = Vec::new();
        for (fid, name, _, is_dir) in self.query(&format!("{} in parents and trashed = false", quote(&id)))? {
            if !is_dir {
                let key = format!("{dir}/{name}");
                self.files.lock().unwrap().insert(key.clone(), fid);
                out.push(key);
            }
        }
        Ok(out)
    }

    fn list_all(&self, dir: &str) -> Result<Vec<(String, u64)>> {
        super::check_key(dir)?;
        let Some(id) = self.dir_id(dir, false)? else { return Ok(Vec::new()) };
        let mut out = Vec::new();
        let mut todo = vec![(dir.to_string(), id)];
        while let Some((d, id)) = todo.pop() {
            for (fid, name, size, is_dir) in self.query(&format!("{} in parents and trashed = false", quote(&id)))? {
                let key = format!("{d}/{name}");
                if is_dir {
                    self.dirs.lock().unwrap().insert(key.clone(), fid.clone());
                    todo.push((key, fid));
                } else {
                    self.files.lock().unwrap().insert(key.clone(), fid);
                    out.push((key, size));
                }
            }
        }
        Ok(out)
    }

    fn kind(&self) -> &'static str {
        "gdrive"
    }

    fn refresh(&self) {
        self.avail.reset();
        // la radice resta: viene dalla configurazione o da una ricerca già fatta
        let root = self.dirs.lock().unwrap().get("").cloned();
        let mut dirs = self.dirs.lock().unwrap();
        dirs.clear();
        if let Some(r) = root {
            dirs.insert(String::new(), r);
        }
        self.files.lock().unwrap().clear();
    }
}
