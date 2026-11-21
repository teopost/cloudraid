//! Interfaccia web di amministrazione: `cloudraid serve`.
//!
//! - Il server parte **bloccato**: il login usa la password del repository, che apre il repository
//!   e resta solo in memoria. Nessuna password salvata su disco.
//! - Le operazioni lunghe (scrub, riparazione, caricamenti, cancellazioni) girano una alla volta in
//!   background; la pagina ne segue l'avanzamento.
//! - Lo stato mostrato viene da un'istantanea aggiornata dopo ogni operazione, così la pagina risponde
//!   anche mentre un'operazione tiene occupato il repository.
//! - Pensato per stare dietro Traefik (HTTPS, solo LAN): cookie `Secure`, `SameSite=Strict`, e ogni
//!   richiesta che modifica qualcosa deve avere l'intestazione `X-Cloudraid: 1` (protezione CSRF).

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::fs;
use std::io::{Read, Write};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tiny_http::{Header, Method, Request, Response, Server};

use crate::backend::oauth::{Link, LinkPrompt, LinkState, Provider};
use crate::config::{BackendConfig, BackendKind};
use crate::crypto::{self, random_hex, KeyFile};
use crate::setup::{self, NewBackend};
use crate::repo::{normalize, ScrubReport, LOCK_STALE_SECS};
use crate::{Config, ReadStats, Repo};

const INDEX_HTML: &str = include_str!("../web/index.html");
const APP_JS: &str = include_str!("../web/app.js");
const APP_CSS: &str = include_str!("../web/app.css");
const FAVICON: &str = include_str!("../web/favicon.svg");

const COOKIE: &str = "cloudraid_session";
const SESSION_IDLE: Duration = Duration::from_secs(60 * 60);
const SESSION_MAX: Duration = Duration::from_secs(12 * 60 * 60);
const HISTORY_LEN: usize = 50;
const JSON_LIMIT: u64 = 64 * 1024;

pub struct ServeOptions {
    pub listen: String,
    pub config: PathBuf,
    /// File dove tenere la cronologia delle operazioni (niente segreti).
    pub state: PathBuf,
    /// Cookie senza `Secure`: solo per provare in locale senza HTTPS.
    pub insecure_cookie: bool,
}

/// Avvia il server e restituisce l'indirizzo su cui ascolta (utile ai test con la porta 0).
pub fn start(opts: ServeOptions) -> Result<SocketAddr> {
    let server = Server::http(&opts.listen).map_err(|e| anyhow!("impossibile ascoltare su {}: {e}", opts.listen))?;
    let addr = server.server_addr().to_ip().ok_or_else(|| anyhow!("indirizzo di ascolto non IP"))?;
    let app = Arc::new(App::new(opts));
    thread::spawn(move || {
        for req in server.incoming_requests() {
            let app = app.clone();
            thread::spawn(move || app.handle(req));
        }
    });
    Ok(addr)
}

/// Avvia il server e non ritorna più.
pub fn serve(opts: ServeOptions) -> Result<()> {
    let addr = start(opts)?;
    eprintln!("cloudraid web in ascolto su http://{addr}");
    loop {
        thread::park();
    }
}

// ---------------------------------------------------------------------------------------------
// Stato

#[derive(Serialize, Deserialize, Clone, Debug)]
struct Job {
    id: String,
    kind: String,
    label: String,
    started: u64,
    finished: Option<u64>,
    /// "running", "ok", "error"
    state: String,
    message: String,
    done: u64,
    total: u64,
    report: Option<ReportView>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
struct ReportView {
    repair: bool,
    chunks: u64,
    shards_ok: u64,
    shards_missing: u64,
    shards_corrupt: u64,
    degraded_chunks: u64,
    repaired_shards: u64,
    unrepaired_shards: u64,
    lost_chunks: usize,
    lost_files: Vec<String>,
}

impl ReportView {
    fn new(r: &ScrubReport, repair: bool) -> Self {
        ReportView {
            repair,
            chunks: r.chunks,
            shards_ok: r.shards_ok,
            shards_missing: r.shards_missing,
            shards_corrupt: r.shards_corrupt,
            degraded_chunks: r.degraded_chunks,
            repaired_shards: r.repaired_shards,
            unrepaired_shards: r.unrepaired_shards,
            lost_chunks: r.lost_chunks.len(),
            lost_files: r.lost_files.clone(),
        }
    }
}

#[derive(Serialize, Clone)]
struct FileRow {
    path: String,
    size: u64,
    mtime: Option<i64>,
    chunks: usize,
}

#[derive(Serialize, Clone)]
struct BackendRow {
    name: String,
    kind: String,
    location: String,
    available: bool,
    shards: u64,
    bytes: u64,
    /// Tempo medio di lettura di un frammento, misurato da questo processo.
    latency_ms: Option<f64>,
    retired: bool,
}

#[derive(Serialize, Clone)]
struct LockRow {
    host: String,
    pid: u32,
    age: u64,
    stale: bool,
}

#[derive(Serialize, Clone)]
struct Snapshot {
    taken: u64,
    data_shards: usize,
    parity_shards: usize,
    generation: u64,
    total_size: u64,
    stored_size: u64,
    chunks: usize,
    files: Vec<FileRow>,
    /// Cartelle vuote create dal mount (le altre si ricavano dai file).
    dirs: Vec<String>,
    backends: Vec<BackendRow>,
    locks: Vec<LockRow>,
    /// Applicazioni OAuth configurate (servono per collegare Dropbox, OneDrive e Google Drive).
    apps: AppsView,
    /// Dove si possono creare cartelle locali.
    local_root: String,
}

#[derive(Serialize, Clone, Default)]
struct AppsView {
    dropbox: bool,
    onedrive: bool,
    gdrive: bool,
}

/// La cartella sotto cui l'interfaccia può creare backend locali (quella montata nel container).
fn local_root() -> String {
    std::env::var("CLOUDRAID_LOCAL_ROOT").unwrap_or_else(|_| "/srv/cloudraid".into())
}

/// Un collegamento di account in corso: il flusso OAuth e, a collegamento fatto, il token.
/// Restano sul server: il browser vede solo l'identificativo.
struct PendingLink {
    link: Link,
    token: Option<String>,
}

#[derive(Serialize, Deserialize, Default)]
struct Persisted {
    history: Vec<Job>,
}

struct Session {
    created: Instant,
    seen: Instant,
}

struct App {
    opts: ServeOptions,
    repo: Mutex<Option<Repo>>,
    snapshot: Mutex<Option<Snapshot>>,
    job: Mutex<Option<Job>>,
    history: Mutex<Vec<Job>>,
    sessions: Mutex<HashMap<String, Session>>,
    failures: Mutex<u32>,
    /// Un solo login che apre il repository alla volta (può richiedere decine di secondi con rclone).
    opening: Mutex<()>,
    /// Copia della chiave cifrata, per verificare la password anche mentre un'operazione occupa il repository.
    keyfile: Mutex<Option<KeyFile>>,
    /// Collegamenti di account in corso.
    links: Mutex<HashMap<String, PendingLink>>,
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn take_snapshot(repo: &Repo) -> Snapshot {
    let ix = repo.index();
    let cfg = repo.config();
    let has = |p: Provider| p.client(&cfg.apps).is_ok();
    Snapshot {
        apps: AppsView { dropbox: has(Provider::Dropbox), onedrive: has(Provider::OneDrive), gdrive: has(Provider::Google) },
        local_root: local_root(),
        taken: now(),
        data_shards: cfg.data_shards,
        parity_shards: cfg.parity_shards,
        generation: ix.generation,
        total_size: ix.files.values().map(|f| f.size).sum(),
        stored_size: ix.chunks.values().map(|c| c.plain_len).sum(),
        chunks: ix.chunks.len(),
        files: ix
            .files
            .iter()
            .map(|(p, f)| FileRow { path: p.clone(), size: f.size, mtime: f.mtime, chunks: f.chunks.len() })
            .collect(),
        dirs: ix.dirs.iter().cloned().collect(),
        backends: repo
            .status()
            .into_iter()
            .map(|b| BackendRow {
                name: b.name,
                kind: b.kind,
                location: b.location,
                available: b.available,
                shards: b.shards,
                bytes: b.bytes,
                latency_ms: b.latency_ms,
                retired: b.retired,
            })
            .collect(),
        locks: repo
            .locks()
            .into_iter()
            .map(|l| {
                let age = now().saturating_sub(l.time);
                LockRow { host: l.host, pid: l.pid, age, stale: age >= LOCK_STALE_SECS }
            })
            .collect(),
    }
}

// ---------------------------------------------------------------------------------------------
// Risposte

type Resp = Response<std::io::Cursor<Vec<u8>>>;

fn header(name: &str, value: &str) -> Header {
    Header::from_bytes(name.as_bytes(), value.as_bytes()).expect("intestazione valida")
}

fn json_resp(code: u16, v: &Value) -> Resp {
    Response::from_string(v.to_string())
        .with_status_code(code)
        .with_header(header("Content-Type", "application/json; charset=utf-8"))
        .with_header(header("Cache-Control", "no-store"))
}

fn error(code: u16, msg: impl Into<String>) -> Resp {
    json_resp(code, &json!({ "error": msg.into() }))
}

const CSP: &str = "default-src 'self'; img-src 'self' data:; style-src 'self'; script-src 'self'; \
                   connect-src 'self'; frame-ancestors 'none'; base-uri 'self'; form-action 'self'";

fn static_resp(body: &str, mime: &str) -> Resp {
    Response::from_string(body)
        .with_header(header("Content-Type", mime))
        .with_header(header("Cache-Control", "no-cache"))
        .with_header(header("Content-Security-Policy", CSP))
        .with_header(header("X-Content-Type-Options", "nosniff"))
}

/// Codifica per `filename*=` (RFC 5987).
fn enc_filename(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

fn decode_component(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < bytes.len() => {
                match u8::from_str_radix(std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("zz"), 16) {
                    Ok(v) => {
                        out.push(v);
                        i += 2;
                    }
                    Err(_) => out.push(b'%'),
                }
            }
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn query(url: &str) -> HashMap<String, String> {
    url.split_once('?')
        .map(|(_, q)| {
            q.split('&')
                .filter(|p| !p.is_empty())
                .map(|p| {
                    let (k, v) = p.split_once('=').unwrap_or((p, ""));
                    (decode_component(k), decode_component(v))
                })
                .collect()
        })
        .unwrap_or_default()
}

fn get_header<'a>(req: &'a Request, name: &str) -> Option<&'a str> {
    req.headers().iter().find(|h| h.field.to_string().eq_ignore_ascii_case(name)).map(|h| h.value.as_str())
}

fn read_json(req: &mut Request) -> Result<Value, Resp> {
    let mut body = String::new();
    req.as_reader()
        .take(JSON_LIMIT)
        .read_to_string(&mut body)
        .map_err(|_| error(400, "richiesta non leggibile"))?;
    if body.trim().is_empty() {
        return Ok(json!({}));
    }
    serde_json::from_str(&body).map_err(|_| error(400, "JSON non valido"))
}

// ---------------------------------------------------------------------------------------------

impl App {
    fn new(opts: ServeOptions) -> App {
        let history = fs::read(&opts.state)
            .ok()
            .and_then(|d| serde_json::from_slice::<Persisted>(&d).ok())
            .map(|p| p.history)
            .unwrap_or_default();
        App {
            opts,
            repo: Mutex::new(None),
            snapshot: Mutex::new(None),
            job: Mutex::new(None),
            history: Mutex::new(history),
            sessions: Mutex::new(HashMap::new()),
            failures: Mutex::new(0),
            opening: Mutex::new(()),
            keyfile: Mutex::new(None),
            links: Mutex::new(HashMap::new()),
        }
    }

    fn persist(&self) {
        let data = Persisted { history: lock(&self.history).clone() };
        let tmp = self.opts.state.with_extension("tmp");
        let ok = serde_json::to_vec_pretty(&data)
            .ok()
            .and_then(|json| fs::write(&tmp, json).ok())
            .and_then(|_| fs::rename(&tmp, &self.opts.state).ok());
        if ok.is_none() {
            eprintln!("attenzione: impossibile salvare {}", self.opts.state.display());
        }
    }

    fn session_from(&self, req: &Request) -> Option<String> {
        let cookies = get_header(req, "Cookie")?;
        let token = cookies.split(';').filter_map(|c| c.trim().split_once('=')).find(|(k, _)| *k == COOKIE)?.1;
        let mut sessions = lock(&self.sessions);
        let now = Instant::now();
        sessions.retain(|_, s| now.duration_since(s.seen) < SESSION_IDLE && now.duration_since(s.created) < SESSION_MAX);
        let s = sessions.get_mut(token)?;
        s.seen = now;
        Some(token.to_string())
    }

    fn cookie(&self, value: &str, max_age: u64) -> Header {
        let secure = if self.opts.insecure_cookie { "" } else { "; Secure" };
        header("Set-Cookie", &format!("{COOKIE}={value}; Path=/; HttpOnly; SameSite=Strict; Max-Age={max_age}{secure}"))
    }

    fn handle(self: Arc<Self>, mut req: Request) {
        let url = req.url().to_string();
        let path = url.split('?').next().unwrap_or("/").to_string();
        let method = req.method().clone();

        // pagine statiche e controllo di salute: senza login
        let static_page = match (&method, path.as_str()) {
            (Method::Get, "/healthz") => Some(Response::from_string("ok").with_header(header("Content-Type", "text/plain"))),
            (Method::Get, "/") | (Method::Get, "/index.html") => {
                // dietro Traefik con StripPrefix (es. /cloudraid) la pagina deve sapere da dove è servita
                let prefix = get_header(&req, "X-Forwarded-Prefix")
                    .unwrap_or("")
                    .chars()
                    .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '-' | '_'))
                    .collect::<String>();
                let base = format!("{}/", prefix.trim_end_matches('/'));
                Some(static_resp(&INDEX_HTML.replace("{{BASE}}", &base), "text/html; charset=utf-8"))
            }
            (Method::Get, "/app.js") => Some(static_resp(APP_JS, "text/javascript; charset=utf-8")),
            (Method::Get, "/app.css") => Some(static_resp(APP_CSS, "text/css; charset=utf-8")),
            (Method::Get, "/favicon.svg") => Some(static_resp(FAVICON, "image/svg+xml")),
            _ => None,
        };
        if let Some(resp) = static_page {
            let _ = req.respond(resp);
            return;
        }
        if !path.starts_with("/api/") {
            let _ = req.respond(error(404, "pagina non trovata"));
            return;
        }

        // protezione CSRF: le modifiche arrivano solo dalla pagina (intestazione impossibile da
        // aggiungere da un altro sito senza un preflight CORS, che qui non viene mai autorizzato)
        if method != Method::Get && get_header(&req, "X-Cloudraid") != Some("1") {
            let _ = req.respond(error(403, "richiesta non autorizzata"));
            return;
        }

        if (method.clone(), path.as_str()) == (Method::Post, "/api/login") {
            let resp = self.login(&mut req);
            let _ = req.respond(resp);
            return;
        }

        let session = self.session_from(&req);
        if (method.clone(), path.as_str()) == (Method::Get, "/api/session") {
            let unlocked = lock(&self.keyfile).is_some();
            let _ = req.respond(json_resp(200, &json!({ "logged": session.is_some(), "unlocked": unlocked })));
            return;
        }
        let Some(session) = session else {
            let _ = req.respond(error(401, "accesso richiesto"));
            return;
        };

        let q = query(&url);
        match (method, path.as_str()) {
            (Method::Post, "/api/logout") => {
                lock(&self.sessions).remove(&session);
                let _ = req.respond(json_resp(200, &json!({"ok": true})).with_header(self.cookie("", 0)));
            }
            (Method::Get, "/api/status") => {
                let _ = req.respond(self.status());
            }
            (Method::Post, "/api/refresh") => {
                let r = self.start_job("refresh", "Aggiornamento dello stato".into(), move |repo, _| {
                    repo.reload()?;
                    Ok(("Stato aggiornato".into(), None))
                });
                let _ = req.respond(r);
            }
            (Method::Post, "/api/scrub") => {
                let repair = match read_json(&mut req) {
                    Ok(v) => v.get("repair").and_then(Value::as_bool).unwrap_or(false),
                    Err(resp) => {
                        let _ = req.respond(resp);
                        return;
                    }
                };
                let (kind, label) =
                    if repair { ("repair", "Verifica e riparazione") } else { ("scrub", "Verifica dei frammenti") };
                let r = self.start_job(kind, label.into(), move |repo, progress| {
                    if repair {
                        repo.lock()?;
                    } else {
                        repo.reload()?;
                    }
                    let report = repo.scrub_with_progress(repair, &mut |d, t| progress(d, t))?;
                    let msg = if !report.lost_chunks.is_empty() {
                        format!("{} blocchi irrecuperabili, {} file danneggiati", report.lost_chunks.len(), report.lost_files.len())
                    } else if repair && report.repaired_shards > 0 {
                        format!("{} frammenti ricostruiti, {} non riparabili", report.repaired_shards, report.unrepaired_shards)
                    } else if report.degraded_chunks > 0 {
                        format!("{} blocchi con frammenti mancanti o danneggiati: serve la riparazione", report.degraded_chunks)
                    } else {
                        format!("Tutti i {} frammenti sono integri", report.shards_ok)
                    };
                    let lost = !report.lost_chunks.is_empty();
                    let view = ReportView::new(&report, repair);
                    if lost {
                        return Err(JobError::WithReport(msg, view));
                    }
                    Ok((msg, Some(view)))
                });
                let _ = req.respond(r);
            }
            (Method::Post, "/api/gc") | (Method::Post, "/api/rebalance") => {
                let dry_run = match read_json(&mut req) {
                    Ok(v) => v.get("dry_run").and_then(Value::as_bool).unwrap_or(false),
                    Err(resp) => {
                        let _ = req.respond(resp);
                        return;
                    }
                };
                let gc = path == "/api/gc";
                let (kind, label) = match (gc, dry_run) {
                    (true, true) => ("gc-check", "Ricerca dei frammenti orfani"),
                    (true, false) => ("gc", "Pulizia dei frammenti orfani"),
                    (false, true) => ("rebalance-check", "Simulazione del ribilanciamento"),
                    (false, false) => ("rebalance", "Ribilanciamento"),
                };
                let r = self.start_job(kind, label.into(), move |repo, progress| {
                    repo.lock()?;
                    let msg = if gc {
                        repo.gc(dry_run, &mut |d, t| progress(d, t))?.summary(dry_run)
                    } else {
                        repo.rebalance(dry_run, &mut |d, t| progress(d, t))?.summary(dry_run)
                    };
                    Ok((msg, None))
                });
                let _ = req.respond(r);
            }
            (Method::Post, "/api/retire") => {
                let (name, undo) = match read_json(&mut req) {
                    Ok(v) => (
                        v.get("name").and_then(Value::as_str).unwrap_or_default().to_string(),
                        v.get("undo").and_then(Value::as_bool).unwrap_or(false),
                    ),
                    Err(resp) => {
                        let _ = req.respond(resp);
                        return;
                    }
                };
                let label = if undo { format!("Rimessa in servizio di {name}") } else { format!("Dismissione di {name}") };
                let r = self.start_job(if undo { "unretire" } else { "retire" }, label, move |repo, progress| {
                    repo.lock()?;
                    if undo {
                        repo.unretire(&name)?;
                        return Ok((format!("{name} di nuovo in servizio: il ribilanciamento gli ridà una parte dei frammenti"), None));
                    }
                    Ok((repo.retire(&name, &mut |d, t| progress(d, t))?.summary(), None))
                });
                let _ = req.respond(r);
            }
            (Method::Post, "/api/link/start") => {
                let resp = self.link_start(&mut req);
                let _ = req.respond(resp);
            }
            (Method::Post, "/api/link/check") => {
                let resp = self.link_check(&mut req);
                let _ = req.respond(resp);
            }
            (Method::Post, "/api/backends") => {
                let resp = self.add_backend(&mut req);
                let _ = req.respond(resp);
            }
            (Method::Delete, "/api/backends") => {
                let name = q.get("name").cloned().unwrap_or_default();
                let config = self.opts.config.clone();
                let r = self.start_job("remove-backend", format!("Rimozione di {name}"), move |repo, _| {
                    repo.lock()?;
                    setup::remove_backend(&config, repo, &name)?;
                    Ok((format!("{name} tolto dalla configurazione"), None))
                });
                let _ = req.respond(r);
            }
            (Method::Post, "/api/apps") => {
                let v = match read_json(&mut req) {
                    Ok(v) => v,
                    Err(resp) => {
                        let _ = req.respond(resp);
                        return;
                    }
                };
                let config = self.opts.config.clone();
                let r = self.start_job("apps", "Impostazioni delle applicazioni".into(), move |repo, _| {
                    let keys = ["dropbox_app_key", "onedrive_client_id", "google_client_id", "google_client_secret"];
                    let values: Vec<(&str, String)> = keys
                        .iter()
                        .filter_map(|k| v.get(*k).and_then(Value::as_str).map(|s| (*k, s.to_string())))
                        .collect();
                    let refs: Vec<(&str, &str)> = values.iter().map(|(k, v)| (*k, v.as_str())).collect();
                    setup::set_apps(&config, repo, &refs)?;
                    Ok(("Impostazioni salvate".into(), None))
                });
                let _ = req.respond(r);
            }
            (Method::Post, "/api/unlock") => {
                let all = match read_json(&mut req) {
                    Ok(v) => v.get("all").and_then(Value::as_bool).unwrap_or(false),
                    Err(resp) => {
                        let _ = req.respond(resp);
                        return;
                    }
                };
                let r = self.start_job("unlock", "Cancellazione dei lock".into(), move |repo, _| {
                    let removed = repo.break_locks(all);
                    Ok((format!("{} lock cancellati", removed.len()), None))
                });
                let _ = req.respond(r);
            }
            (Method::Delete, "/api/files") => {
                let target = q.get("path").cloned().unwrap_or_default();
                let r = match normalize(&target) {
                    Ok(p) if !p.is_empty() => {
                        self.start_job("delete", format!("Eliminazione di {p}"), move |repo, _| {
                            repo.lock()?;
                            let removed = repo.remove(&p)?;
                            if removed.is_empty() {
                                return Err(anyhow!("{p}: niente da eliminare").into());
                            }
                            Ok((format!("{} file eliminati", removed.len()), None))
                        })
                    }
                    _ => error(400, "percorso non valido"),
                };
                let _ = req.respond(r);
            }
            (Method::Put, "/api/upload") => {
                let r = self.upload(&mut req, q.get("path").cloned().unwrap_or_default());
                let _ = req.respond(r);
            }
            (Method::Get, "/api/download") => {
                self.download(req, q.get("path").cloned().unwrap_or_default());
            }
            _ => {
                let _ = req.respond(error(404, "operazione sconosciuta"));
            }
        }
    }

    fn login(self: &Arc<Self>, req: &mut Request) -> Resp {
        let body = match read_json(req) {
            Ok(v) => v,
            Err(r) => return r,
        };
        let password = body.get("password").and_then(Value::as_str).unwrap_or("").to_string();
        let ok = self.check_password(&password);
        match ok {
            Ok(()) => {
                *lock(&self.failures) = 0;
                let token = random_hex(32);
                lock(&self.sessions).insert(token.clone(), Session { created: Instant::now(), seen: Instant::now() });
                json_resp(200, &json!({"ok": true})).with_header(self.cookie(&token, SESSION_MAX.as_secs()))
            }
            Err((code, msg)) => {
                if code == 401 {
                    // rallenta i tentativi a raffica: 0,5 s, 1 s, 2 s… fino a 10 s
                    let n = {
                        let mut f = lock(&self.failures);
                        *f += 1;
                        *f
                    };
                    thread::sleep(Duration::from_millis((250u64 << n.min(6)).min(10_000)));
                }
                error(code, msg)
            }
        }
    }

    /// Verifica la password; al primo accesso apre il repository.
    fn check_password(self: &Arc<Self>, password: &str) -> Result<(), (u16, String)> {
        if password.is_empty() {
            return Err((401, "password errata".into()));
        }
        let _opening = lock(&self.opening);
        if let Some(kf) = lock(&self.keyfile).as_ref() {
            return if crypto::unlock(kf, password).is_ok() { Ok(()) } else { Err((401, "password errata".into())) };
        }
        let cfg = Config::load(&self.opts.config).map_err(|e| (500, format!("{e:#}")))?;
        match Repo::open(cfg, password) {
            Ok(repo) => {
                *lock(&self.snapshot) = Some(take_snapshot(&repo));
                *lock(&self.keyfile) = Some(repo.keyfile().clone());
                *lock(&self.repo) = Some(repo);
                Ok(())
            }
            Err(e) => {
                let msg = format!("{e:#}");
                if msg.contains("password errata") {
                    Err((401, "password errata".into()))
                } else {
                    Err((500, msg))
                }
            }
        }
    }

    fn status(&self) -> Resp {
        let job = lock(&self.job).clone();
        let history = lock(&self.history).clone();
        let snapshot = lock(&self.snapshot).clone();
        let busy = job.as_ref().is_some_and(|j| j.state == "running");
        json_resp(200, &json!({ "busy": busy, "job": job, "history": history, "snapshot": snapshot, "now": now() }))
    }

    /// Avvia un'operazione in background. `work` riceve il repository e una funzione per l'avanzamento.
    /// Le impostazioni delle app, senza aspettare un'operazione in corso.
    fn apps_config(&self) -> Result<crate::config::AppsConfig, Resp> {
        match self.repo.try_lock() {
            Ok(g) => g.as_ref().map(|r| r.config().apps.clone()).ok_or_else(|| error(409, "repository non aperto")),
            Err(_) => Err(error(409, "c'è un'operazione in corso: riprova quando finisce")),
        }
    }

    /// Comincia il collegamento di un account Dropbox, OneDrive o Google.
    fn link_start(&self, req: &mut Request) -> Resp {
        let v = match read_json(req) {
            Ok(v) => v,
            Err(r) => return r,
        };
        let Some(provider) = v.get("provider").and_then(Value::as_str).and_then(Provider::parse) else {
            return error(400, "servizio sconosciuto");
        };
        let apps = match self.apps_config() {
            Ok(a) => a,
            Err(r) => return r,
        };
        // solo per le prove: indirizzi finti al posto dei provider (mai in produzione)
        let auth_base = v.get("auth_base").and_then(Value::as_str).filter(|_| std::env::var("CLOUDRAID_TEST_AUTH").is_ok());
        match Link::start(provider, &apps, auth_base) {
            Ok(link) => {
                let id = random_hex(12);
                let body = match &link.prompt {
                    LinkPrompt::Device { url, code, expires_in } => {
                        json!({"id": id, "kind": "device", "url": url, "code": code, "expires_in": expires_in})
                    }
                    LinkPrompt::PasteCode { url } => json!({"id": id, "kind": "paste", "url": url}),
                };
                let mut links = lock(&self.links);
                // i collegamenti lasciati a metà scadono dopo mezz'ora
                links.retain(|_, l| l.link.started.elapsed() < Duration::from_secs(1800));
                links.insert(id, PendingLink { link, token: None });
                json_resp(200, &body)
            }
            Err(e) => error(400, format!("{e:#}")),
        }
    }

    /// Controlla un collegamento: per Dropbox con il codice incollato, per gli altri chiedendo al provider.
    fn link_check(&self, req: &mut Request) -> Resp {
        let v = match read_json(req) {
            Ok(v) => v,
            Err(r) => return r,
        };
        let id = v.get("id").and_then(Value::as_str).unwrap_or_default().to_string();
        let code = v.get("code").and_then(Value::as_str).map(str::to_string);
        // il controllo può fare una richiesta al provider: senza tenere bloccata la tabella
        let mut pending = match lock(&self.links).remove(&id) {
            Some(p) => p,
            None => return error(404, "collegamento scaduto: ricomincia"),
        };
        if pending.token.is_some() {
            lock(&self.links).insert(id, pending);
            return json_resp(200, &json!({"state": "done"}));
        }
        let result = pending.link.check(code.as_deref());
        let resp = match &result {
            Ok(LinkState::Pending) => json_resp(200, &json!({"state": "pending"})),
            Ok(LinkState::Done(_)) => json_resp(200, &json!({"state": "done"})),
            Err(e) => json_resp(200, &json!({"state": "error", "message": format!("{e:#}")})),
        };
        match result {
            Ok(LinkState::Done(token)) => {
                pending.token = Some(token);
                lock(&self.links).insert(id, pending);
            }
            Ok(LinkState::Pending) => {
                lock(&self.links).insert(id, pending);
            }
            Err(_) => {}
        }
        resp
    }

    /// Aggiunge un backend (operazione in sottofondo: prova di scrittura e lettura compresa).
    fn add_backend(self: &Arc<Self>, req: &mut Request) -> Resp {
        let v = match read_json(req) {
            Ok(v) => v,
            Err(r) => return r,
        };
        let s = |k: &str| v.get(k).and_then(Value::as_str).map(str::trim).unwrap_or("").to_string();
        let name = s("name");
        let kind = s("type");
        let mut secrets: Vec<(String, String)> = Vec::new();
        let kind = match kind.as_str() {
            "local" => {
                let root = local_root();
                let path = s("path");
                let ok = path.starts_with(&format!("{}/", root.trim_end_matches('/')))
                    && !path.split('/').any(|p| p == ".." || p == ".")
                    && path.len() > root.len() + 1;
                if !ok {
                    return error(400, format!("la cartella deve stare dentro {root}"));
                }
                BackendKind::Local { path: path.into() }
            }
            "webdav" => {
                secrets.push(("password".into(), s("password")));
                BackendKind::Webdav { url: s("url"), user: s("user") }
            }
            "s3" => {
                secrets.push(("secret_key".into(), s("secret_key")));
                let region = if s("region").is_empty() { "us-east-1".to_string() } else { s("region") };
                BackendKind::S3 {
                    endpoint: s("endpoint"),
                    region,
                    bucket: s("bucket"),
                    access_key: s("access_key"),
                    prefix: s("prefix"),
                    path_style: v.get("path_style").and_then(Value::as_bool).unwrap_or(true),
                }
            }
            "dropbox" | "onedrive" | "gdrive" => {
                let link_id = s("link_id");
                let token = lock(&self.links).get(&link_id).and_then(|l| l.token.clone());
                let Some(token) = token else {
                    return error(400, "prima collega l'account");
                };
                secrets.push(("refresh_token".into(), token));
                let api_base = Some(s("api_base")).filter(|a| !a.is_empty() && std::env::var("CLOUDRAID_TEST_AUTH").is_ok());
                match kind.as_str() {
                    "dropbox" => BackendKind::Dropbox { folder: s("folder"), api_base },
                    "onedrive" => BackendKind::Onedrive { folder: s("folder"), api_base },
                    _ => BackendKind::Gdrive {
                        folder: if s("folder").is_empty() { "cloudraid".into() } else { s("folder") },
                        folder_id: None,
                        api_base,
                    },
                }
            }
            _ => return error(400, "tipo di spazio sconosciuto"),
        };
        let config = self.opts.config.clone();
        let link_id = s("link_id");
        let app = self.clone();
        let label = format!("Aggiunta di {name}");
        self.start_job("add-backend", label, move |repo, _| {
            let nb = NewBackend { config: BackendConfig { name: name.clone(), kind, delay_ms: 0 }, secrets };
            setup::add_backend(&config, repo, nb)?;
            lock(&app.links).remove(&link_id);
            Ok((format!("{name} aggiunto e provato: ora riceve i frammenti nuovi. Con \"Ribilancia\" gli passi anche una parte di quelli esistenti"), None))
        })
    }

    fn start_job<F>(self: &Arc<Self>, kind: &str, label: String, work: F) -> Resp
    where
        F: FnOnce(&mut Repo, &dyn Fn(u64, u64)) -> Result<(String, Option<ReportView>), JobError> + Send + 'static,
    {
        let job = {
            let mut cur = lock(&self.job);
            if cur.as_ref().is_some_and(|j| j.state == "running") {
                return error(409, "c'è già un'operazione in corso");
            }
            let job = Job {
                id: random_hex(6),
                kind: kind.into(),
                label,
                started: now(),
                finished: None,
                state: "running".into(),
                message: String::new(),
                done: 0,
                total: 0,
                report: None,
            };
            *cur = Some(job.clone());
            job
        };
        let app = self.clone();
        thread::spawn(move || {
            let mut guard = lock(&app.repo);
            let result = match guard.as_mut() {
                None => Err(JobError::Plain(anyhow!("repository non aperto"))),
                Some(repo) => {
                    let progress = |done: u64, total: u64| {
                        if let Some(j) = lock(&app.job).as_mut() {
                            j.done = done;
                            j.total = total;
                        }
                    };
                    let r = work(repo, &progress);
                    // come la riga di comando: si salva comunque il lavoro fatto, poi si rilascia il lock
                    if let Err(e) = repo.save() {
                        eprintln!("attenzione: salvataggio dell'indice: {e:#}");
                        if let Err(e) = repo.discard() {
                            eprintln!("attenzione: {e:#}");
                        }
                    }
                    repo.unlock();
                    *lock(&app.snapshot) = Some(take_snapshot(repo));
                    r
                }
            };
            drop(guard);
            let finished = {
                let mut cur = lock(&app.job);
                let j = cur.as_mut().expect("operazione in corso");
                j.finished = Some(now());
                match result {
                    Ok((msg, report)) => {
                        j.state = "ok".into();
                        j.message = msg;
                        j.report = report;
                    }
                    Err(JobError::WithReport(msg, report)) => {
                        j.state = "error".into();
                        j.message = msg;
                        j.report = Some(report);
                    }
                    Err(JobError::Plain(e)) => {
                        j.state = "error".into();
                        j.message = format!("{e:#}");
                    }
                }
                j.clone()
            };
            {
                let mut h = lock(&app.history);
                h.insert(0, finished);
                h.truncate(HISTORY_LEN);
            }
            app.persist();
        });
        json_resp(202, &json!({ "job": job }))
    }

    fn upload(self: &Arc<Self>, req: &mut Request, dest: String) -> Resp {
        let dest = match normalize(&dest) {
            Ok(d) if !d.is_empty() => d,
            _ => return error(400, "percorso di destinazione non valido"),
        };
        if lock(&self.job).as_ref().is_some_and(|j| j.state == "running") {
            return error(409, "c'è già un'operazione in corso");
        }
        let tmp = std::env::temp_dir().join(format!("cloudraid-upload-{}", random_hex(8)));
        let received = (|| -> std::io::Result<u64> {
            let mut f = fs::File::create(&tmp)?;
            let n = std::io::copy(req.as_reader(), &mut f)?;
            f.flush()?;
            Ok(n)
        })();
        if let Err(e) = received {
            let _ = fs::remove_file(&tmp);
            return error(400, format!("caricamento interrotto: {e}"));
        }
        let label = format!("Caricamento di {dest}");
        let tmp2 = tmp.clone();
        let resp = self.start_job("upload", label, move |repo, _| {
            let r = (|| {
                repo.lock()?;
                let new = repo.put_file(&tmp2, &dest)?;
                Ok::<_, anyhow::Error>(format!("{dest} caricato ({new} blocchi nuovi)"))
            })();
            let _ = fs::remove_file(&tmp2);
            Ok((r?, None))
        });
        if resp.status_code().0 != 202 {
            let _ = fs::remove_file(&tmp);
        }
        resp
    }

    fn download(&self, req: Request, path: String) {
        let path = match normalize(&path) {
            Ok(p) if !p.is_empty() => p,
            _ => {
                let _ = req.respond(error(400, "percorso non valido"));
                return;
            }
        };
        let tmp = std::env::temp_dir().join(format!("cloudraid-download-{}", random_hex(8)));
        let result = {
            let guard = match self.repo.try_lock() {
                Ok(g) => g,
                Err(_) => {
                    let _ = req.respond(error(409, "operazione in corso: riprova quando è finita"));
                    return;
                }
            };
            match guard.as_ref() {
                None => Err(anyhow!("repository non aperto")),
                Some(repo) => {
                    if !repo.index().files.contains_key(&path) {
                        let _ = req.respond(error(404, format!("{path}: file non presente")));
                        return;
                    }
                    repo.get_file(&path, &tmp, &mut ReadStats::default())
                }
            }
        };
        match result.and_then(|()| fs::File::open(&tmp).map_err(Into::into)) {
            Ok(file) => {
                let _ = fs::remove_file(&tmp); // il file aperto resta leggibile fino alla fine dell'invio
                let name = path.rsplit('/').next().unwrap_or("file");
                let resp = Response::from_file(file)
                    .with_header(header("Content-Type", "application/octet-stream"))
                    .with_header(header("Content-Disposition", &format!("attachment; filename*=UTF-8''{}", enc_filename(name))))
                    .with_header(header("Cache-Control", "no-store"));
                let _ = req.respond(resp);
            }
            Err(e) => {
                let _ = fs::remove_file(&tmp);
                let _ = req.respond(error(500, format!("{e:#}")));
            }
        }
    }
}

/// Errore di un'operazione, eventualmente con il rapporto dello scrub.
enum JobError {
    Plain(anyhow::Error),
    WithReport(String, ReportView),
}

impl From<anyhow::Error> for JobError {
    fn from(e: anyhow::Error) -> Self {
        JobError::Plain(e)
    }
}
