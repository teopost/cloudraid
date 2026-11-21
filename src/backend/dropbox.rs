//! Dropbox (API v2) nella cartella dell'app: i percorsi sono relativi a `Applicazioni/<nome app>`.
//! Caricamento semplice (fino a 150 MB, i frammenti stanno ben sotto), download, cancellazione ed
//! elenchi con `list_folder` (anche ricorsivi, una sola serie di chiamate).

use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use std::sync::Arc;

use super::http::{self, Availability, Outcome, Step};
use super::oauth::{Provider, TokenSource};
use super::Backend;
use crate::config::AppsConfig;
use crate::credentials::Credentials;

pub struct Dropbox {
    name: String,
    /// Cartella dentro la cartella dell'app: `""` oppure `/nome`.
    root: String,
    api: String,
    content: String,
    tokens: TokenSource,
    avail: Availability,
}

/// L'argomento JSON nell'intestazione `Dropbox-API-Arg`: i caratteri non ASCII vanno scritti come `\uXXXX`.
fn header_arg(v: &Value) -> String {
    let s = v.to_string();
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_ascii() {
            out.push(c);
        } else {
            let mut buf = [0u16; 2];
            for u in c.encode_utf16(&mut buf) {
                out.push_str(&format!("\\u{u:04x}"));
            }
        }
    }
    out
}

impl Dropbox {
    pub fn new(name: String, folder: &str, api_base: Option<&str>, apps: &AppsConfig, creds: Arc<Credentials>) -> Dropbox {
        let folder = folder.trim_matches('/');
        let (api, content) = match api_base {
            Some(b) => (b.trim_end_matches('/').to_string(), b.trim_end_matches('/').to_string()),
            None => ("https://api.dropboxapi.com".to_string(), "https://content.dropboxapi.com".to_string()),
        };
        Dropbox {
            tokens: TokenSource::new(Provider::Dropbox, &name, apps, api_base, creds),
            name,
            root: if folder.is_empty() { String::new() } else { format!("/{folder}") },
            api,
            content,
            avail: Availability::default(),
        }
    }

    fn path(&self, key: &str) -> String {
        if key.is_empty() {
            self.root.clone()
        } else {
            format!("{}/{key}", self.root)
        }
    }

    /// Una chiamata con token, ripetuta se il token è scaduto o il server chiede di rallentare.
    /// `handle` interpreta le risposte diverse da 401 e dagli errori temporanei.
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
                Outcome::Response(r) if http::transient(r.status()) => http::unexpected(r),
                Outcome::Response(r) => handle(r),
                Outcome::Transport(e) => Step::Retry(e, None),
            }
        })
    }

    /// Una chiamata RPC con corpo JSON e risposta JSON; un 409 viene restituito come `Err(errore)`.
    fn rpc(&self, what: &str, endpoint: &str, arg: Value) -> Result<std::result::Result<Value, String>> {
        let url = format!("{}/2/{endpoint}", self.api);
        let body = arg.to_string().into_bytes();
        self.call(
            what,
            |token| {
                let req = http::agent()
                    .post(&url)
                    .set("Authorization", &format!("Bearer {token}"))
                    .set("Content-Type", "application/json");
                (req, Some(body.clone()))
            },
            |r| {
                let status = r.status();
                let text = match http::body(r) {
                    Ok(b) => String::from_utf8_lossy(&b).into_owned(),
                    Err(e) => return Step::Retry(e, None),
                };
                match status {
                    200 => Step::Done(Ok(serde_json::from_str(&text).unwrap_or(Value::Null))),
                    409 => Step::Done(Err(text)),
                    _ => Step::Fail(anyhow!("risposta {status}: {text}")),
                }
            },
        )
    }

    fn list_folder(&self, dir: &str, recursive: bool) -> Result<Vec<(String, u64)>> {
        let mut out = Vec::new();
        let prefix = format!("{}/", self.root);
        let mut res = self.rpc(
            "elenco",
            "files/list_folder",
            json!({"path": self.path(dir), "recursive": recursive, "limit": 2000}),
        )?;
        loop {
            let v = match res {
                Ok(v) => v,
                // cartella che non esiste ancora: vuota
                Err(e) if e.contains("not_found") => return Ok(out),
                Err(e) => return Err(anyhow!("backend {}: elenco di {dir}: {e}", self.name)),
            };
            for entry in v.get("entries").and_then(Value::as_array).into_iter().flatten() {
                if entry.get(".tag").and_then(Value::as_str) != Some("file") {
                    continue;
                }
                let Some(path) = entry.get("path_display").and_then(Value::as_str) else { continue };
                let size = entry.get("size").and_then(Value::as_u64).unwrap_or(0);
                if let Some(rel) = path.strip_prefix(&prefix) {
                    out.push((rel.to_string(), size));
                }
            }
            if !v.get("has_more").and_then(Value::as_bool).unwrap_or(false) {
                return Ok(out);
            }
            let cursor = v.get("cursor").and_then(Value::as_str).unwrap_or_default().to_string();
            res = self.rpc("elenco", "files/list_folder/continue", json!({ "cursor": cursor }))?;
        }
    }
}

impl Backend for Dropbox {
    fn name(&self) -> &str {
        &self.name
    }

    fn location(&self) -> String {
        format!("Dropbox, cartella dell'app{}", self.root)
    }

    fn is_available(&self) -> bool {
        self.avail.get(
            || match self.rpc("verifica", "files/list_folder", json!({"path": self.root, "limit": 1}))? {
                Ok(_) => Ok(true),
                Err(e) if e.contains("not_found") => Ok(false),
                Err(e) => Err(anyhow!("{e}")),
            },
            &self.name,
        )
    }

    fn create(&self) -> Result<()> {
        if !self.root.is_empty() {
            match self.rpc("creazione della cartella", "files/create_folder_v2", json!({"path": self.root}))? {
                Ok(_) => {}
                Err(e) if e.contains("conflict") => {}
                Err(e) => return Err(anyhow!("backend {}: creazione della cartella: {e}", self.name)),
            }
        }
        self.avail.reset();
        Ok(())
    }

    fn put(&self, key: &str, data: &[u8]) -> Result<()> {
        super::check_key(key)?;
        let url = format!("{}/2/files/upload", self.content);
        let arg = header_arg(&json!({"path": self.path(key), "mode": "overwrite", "mute": true, "autorename": false}));
        self.call(
            &format!("scrittura di {key}"),
            |token| {
                let req = http::agent()
                    .post(&url)
                    .set("Authorization", &format!("Bearer {token}"))
                    .set("Dropbox-API-Arg", &arg)
                    .set("Content-Type", "application/octet-stream");
                (req, Some(data.to_vec()))
            },
            |r| match r.status() {
                200 => {
                    let _ = http::body(r);
                    Step::Done(())
                }
                // too_many_write_operations e simili: si riprova
                409 => {
                    let text = http::body_text(r);
                    if text.contains("too_many_write_operations") {
                        Step::Retry(anyhow!("{text}"), None)
                    } else {
                        Step::Fail(anyhow!("risposta 409: {text}"))
                    }
                }
                s => Step::Fail(anyhow!("risposta {s}: {}", http::body_text(r))),
            },
        )
    }

    fn get(&self, key: &str) -> Result<Option<Vec<u8>>> {
        super::check_key(key)?;
        let url = format!("{}/2/files/download", self.content);
        let arg = header_arg(&json!({"path": self.path(key)}));
        self.call(
            &format!("lettura di {key}"),
            |token| {
                let req = http::agent()
                    .post(&url)
                    .set("Authorization", &format!("Bearer {token}"))
                    .set("Dropbox-API-Arg", &arg);
                (req, Some(Vec::new()))
            },
            |r| match r.status() {
                200 => match http::body(r) {
                    Ok(b) => Step::Done(Some(b)),
                    Err(e) => Step::Retry(e, None),
                },
                409 => {
                    let text = http::body_text(r);
                    if text.contains("not_found") {
                        Step::Done(None)
                    } else {
                        Step::Fail(anyhow!("risposta 409: {text}"))
                    }
                }
                s => Step::Fail(anyhow!("risposta {s}: {}", http::body_text(r))),
            },
        )
    }

    fn delete(&self, key: &str) -> Result<()> {
        super::check_key(key)?;
        match self.rpc(&format!("cancellazione di {key}"), "files/delete_v2", json!({"path": self.path(key)}))? {
            Ok(_) => Ok(()),
            Err(e) if e.contains("not_found") => Ok(()),
            Err(e) => Err(anyhow!("backend {}: cancellazione di {key}: {e}", self.name)),
        }
    }

    fn remove_dir(&self, dir: &str) -> Result<()> {
        // la cancellazione per percorso vale anche per le cartelle (con il contenuto)
        self.delete(dir)
    }

    fn list(&self, dir: &str) -> Result<Vec<String>> {
        super::check_key(dir)?;
        Ok(self.list_folder(dir, false)?.into_iter().map(|(k, _)| k).collect())
    }

    fn list_all(&self, dir: &str) -> Result<Vec<(String, u64)>> {
        super::check_key(dir)?;
        self.list_folder(dir, true)
    }

    fn kind(&self) -> &'static str {
        "dropbox"
    }

    fn refresh(&self) {
        self.avail.reset();
    }
}
