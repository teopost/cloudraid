//! OneDrive (Microsoft Graph) nella cartella dell'app (`/me/drive/special/approot`), indirizzando
//! gli oggetti per percorso. Caricamento semplice (fino a 250 MB), download seguendo a mano il
//! reindirizzamento verso l'indirizzo già autorizzato (senza mandargli il token), elenchi con
//! `children` cartella per cartella.

use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use std::sync::Arc;

use super::http::{self, Availability, Outcome, Step};
use super::oauth::{Provider, TokenSource};
use super::Backend;
use crate::config::AppsConfig;
use crate::credentials::Credentials;

pub struct OneDrive {
    name: String,
    /// Cartella dentro quella dell'app (vuota: la cartella dell'app stessa).
    folder: String,
    graph: String,
    tokens: TokenSource,
    avail: Availability,
}

impl OneDrive {
    pub fn new(name: String, folder: &str, api_base: Option<&str>, apps: &AppsConfig, creds: Arc<Credentials>) -> OneDrive {
        OneDrive {
            tokens: TokenSource::new(Provider::OneDrive, &name, apps, api_base, creds),
            name,
            folder: folder.trim_matches('/').to_string(),
            graph: api_base.map(|b| b.trim_end_matches('/').to_string()).unwrap_or_else(|| "https://graph.microsoft.com/v1.0".into()),
            avail: Availability::default(),
        }
    }

    /// L'elemento per un percorso relativo alla radice del repository (vuoto: la radice).
    fn item(&self, key: &str) -> String {
        let path = match (self.folder.is_empty(), key.is_empty()) {
            (true, true) => String::new(),
            (true, false) => key.to_string(),
            (false, true) => self.folder.clone(),
            (false, false) => format!("{}/{key}", self.folder),
        };
        if path.is_empty() {
            format!("{}/me/drive/special/approot", self.graph)
        } else {
            format!("{}/me/drive/special/approot:/{}:", self.graph, http::encode_path(&path))
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
                Outcome::Response(r) if http::transient(r.status()) => http::unexpected(r),
                Outcome::Response(r) => handle(r),
                Outcome::Transport(e) => Step::Retry(e, None),
            }
        })
    }

    fn get_json(&self, what: &str, url: &str) -> Result<Option<Value>> {
        self.call(
            what,
            |token| (http::agent().get(url).set("Authorization", &format!("Bearer {token}")), None),
            |r| match r.status() {
                200 => match http::body(r) {
                    Ok(b) => Step::Done(Some(serde_json::from_slice(&b).unwrap_or(Value::Null))),
                    Err(e) => Step::Retry(e, None),
                },
                404 => Step::Done(None),
                s => Step::Fail(anyhow!("risposta {s}: {}", http::body_text(r))),
            },
        )
    }

    /// I figli di una cartella: (chiave, è una cartella, dimensione). `None` se la cartella non c'è.
    fn children(&self, dir: &str) -> Result<Option<Vec<(String, bool, u64)>>> {
        let base = format!("{}/children", self.item(dir));
        let mut url = format!("{base}?$select=name,size,file,folder&$top=1000");
        let mut out = Vec::new();
        loop {
            let Some(v) = self.get_json(&format!("elenco di {dir}"), &url)? else { return Ok(None) };
            for it in v.get("value").and_then(Value::as_array).into_iter().flatten() {
                let Some(name) = it.get("name").and_then(Value::as_str) else { continue };
                let key = if dir.is_empty() { name.to_string() } else { format!("{dir}/{name}") };
                let is_dir = it.get("folder").is_some();
                out.push((key, is_dir, it.get("size").and_then(Value::as_u64).unwrap_or(0)));
            }
            match v.get("@odata.nextLink").and_then(Value::as_str) {
                Some(next) => url = next.to_string(),
                None => return Ok(Some(out)),
            }
        }
    }
}

impl Backend for OneDrive {
    fn name(&self) -> &str {
        &self.name
    }

    fn location(&self) -> String {
        let sub = if self.folder.is_empty() { String::new() } else { format!("/{}", self.folder) };
        format!("OneDrive, cartella dell'app{sub}")
    }

    fn is_available(&self) -> bool {
        self.avail.get(|| Ok(self.get_json("verifica", &self.item(""))?.is_some()), &self.name)
    }

    fn create(&self) -> Result<()> {
        // la cartella dell'app nasce da sola al primo accesso; la sottocartella si crea se serve
        let approot = format!("{}/me/drive/special/approot", self.graph);
        self.get_json("apertura della cartella dell'app", &approot)?;
        if !self.folder.is_empty() {
            let parent = format!("{}/me/drive/special/approot/children", self.graph);
            let body = json!({"name": self.folder, "folder": {}, "@microsoft.graph.conflictBehavior": "fail"}).to_string().into_bytes();
            self.call(
                "creazione della cartella",
                |token| {
                    let req = http::agent()
                        .post(&parent)
                        .set("Authorization", &format!("Bearer {token}"))
                        .set("Content-Type", "application/json");
                    (req, Some(body.clone()))
                },
                |r| match r.status() {
                    200 | 201 | 409 => {
                        let _ = http::body(r);
                        Step::Done(())
                    }
                    s => Step::Fail(anyhow!("risposta {s}: {}", http::body_text(r))),
                },
            )?;
        }
        self.avail.reset();
        Ok(())
    }

    fn put(&self, key: &str, data: &[u8]) -> Result<()> {
        super::check_key(key)?;
        let url = format!("{}/content", self.item(key));
        self.call(
            &format!("scrittura di {key}"),
            |token| {
                let req = http::agent()
                    .put(&url)
                    .set("Authorization", &format!("Bearer {token}"))
                    .set("Content-Type", "application/octet-stream");
                (req, Some(data.to_vec()))
            },
            |r| match r.status() {
                200 | 201 => {
                    let _ = http::body(r);
                    Step::Done(())
                }
                // 409: conflitto momentaneo nella creazione delle cartelle intermedie
                409 => Step::Retry(anyhow!("conflitto: {}", http::body_text(r)), None),
                s => Step::Fail(anyhow!("risposta {s}: {}", http::body_text(r))),
            },
        )
    }

    fn get(&self, key: &str) -> Result<Option<Vec<u8>>> {
        super::check_key(key)?;
        let url = format!("{}/content", self.item(key));
        self.call(
            &format!("lettura di {key}"),
            |token| (http::agent_no_redirect().get(&url).set("Authorization", &format!("Bearer {token}")), None),
            |r| match r.status() {
                200 => match http::body(r) {
                    Ok(b) => Step::Done(Some(b)),
                    Err(e) => Step::Retry(e, None),
                },
                // il contenuto sta su un indirizzo già autorizzato: lo si scarica senza token
                301 | 302 | 303 | 307 | 308 => {
                    let Some(loc) = r.header("Location").map(str::to_string) else {
                        return Step::Fail(anyhow!("reindirizzamento senza indirizzo"));
                    };
                    match http::send(http::agent().get(&loc), None) {
                        Outcome::Response(d) if d.status() == 200 => match http::body(d) {
                            Ok(b) => Step::Done(Some(b)),
                            Err(e) => Step::Retry(e, None),
                        },
                        Outcome::Response(d) => http::unexpected(d),
                        Outcome::Transport(e) => Step::Retry(e, None),
                    }
                }
                404 => Step::Done(None),
                s => Step::Fail(anyhow!("risposta {s}: {}", http::body_text(r))),
            },
        )
    }

    fn delete(&self, key: &str) -> Result<()> {
        super::check_key(key)?;
        let url = self.item(key);
        self.call(
            &format!("cancellazione di {key}"),
            |token| (http::agent().delete(&url).set("Authorization", &format!("Bearer {token}")), None),
            |r| match r.status() {
                200 | 204 | 404 => Step::Done(()),
                s => Step::Fail(anyhow!("risposta {s}: {}", http::body_text(r))),
            },
        )
    }

    fn remove_dir(&self, dir: &str) -> Result<()> {
        // la cancellazione per percorso vale anche per le cartelle (con il contenuto)
        self.delete(dir)
    }

    fn list(&self, dir: &str) -> Result<Vec<String>> {
        super::check_key(dir)?;
        Ok(self.children(dir)?.unwrap_or_default().into_iter().filter(|(_, d, _)| !d).map(|(k, _, _)| k).collect())
    }

    fn list_all(&self, dir: &str) -> Result<Vec<(String, u64)>> {
        super::check_key(dir)?;
        let mut out = Vec::new();
        let mut todo = vec![dir.to_string()];
        while let Some(d) = todo.pop() {
            for (key, is_dir, size) in self.children(&d)?.unwrap_or_default() {
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
        "onedrive"
    }

    fn refresh(&self) {
        self.avail.reset();
    }
}
