//! Backend di rete contro server finti, in memoria, che seguono le API vere: WebDAV, S3 (con la
//! verifica della firma SigV4 di ogni richiesta), Dropbox, OneDrive e Google Drive (con i flussi
//! OAuth di collegamento e il rinnovo dei token). Poi un repository vero sopra tutti e cinque,
//! costruito come fa l'interfaccia web (setup::add_backend), con i dati spostati solo sul "cloud".

use base64::Engine;
use cloudraid::backend::oauth::{Link, LinkPrompt, LinkState, Provider};
use cloudraid::backend::s3::sign;
use cloudraid::setup::{self, NewBackend};
use cloudraid::{AppsConfig, BackendConfig, BackendKind, Config, ReadStats, Repo};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tempfile::TempDir;

const PW: &str = "password di prova";

// ---------------------------------------------------------------------------------------------
// Un server HTTP finto.

struct Req {
    method: String,
    /// Percorso senza query, non decodificato.
    path: String,
    /// La query grezza (dopo `?`).
    query: String,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

impl Req {
    fn header(&self, k: &str) -> Option<&str> {
        self.headers.get(&k.to_lowercase()).map(String::as_str)
    }

    fn q(&self, k: &str) -> Option<String> {
        self.query.split('&').filter_map(|p| p.split_once('=')).find(|(a, _)| *a == k).map(|(_, v)| dec(v))
    }

    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or(Value::Null)
    }

    fn form(&self) -> HashMap<String, String> {
        String::from_utf8_lossy(&self.body)
            .split('&')
            .filter_map(|p| p.split_once('='))
            .map(|(k, v)| (dec(k), dec(&v.replace('+', " "))))
            .collect()
    }
}

struct Resp {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

fn resp(status: u16, body: impl Into<Vec<u8>>) -> Resp {
    Resp { status, headers: Vec::new(), body: body.into() }
}

fn json_resp(status: u16, v: Value) -> Resp {
    Resp { status, headers: vec![("Content-Type".into(), "application/json".into())], body: v.to_string().into_bytes() }
}

fn dec(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Some(v) = std::str::from_utf8(&b[i + 1..i + 3]).ok().and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn enc(s: &str) -> String {
    s.bytes()
        .map(|b| if b.is_ascii_alphanumeric() || b"-_.~/".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") })
        .collect()
}

/// Avvia un server su una porta libera; restituisce l'indirizzo di base.
fn serve(handler: impl Fn(Req, &str) -> Resp + Send + Sync + 'static) -> String {
    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let base = format!("http://{}", server.server_addr().to_ip().unwrap());
    let handler = Arc::new(handler);
    let b = base.clone();
    std::thread::spawn(move || {
        for mut r in server.incoming_requests() {
            let mut body = Vec::new();
            std::io::Read::read_to_end(r.as_reader(), &mut body).unwrap();
            let url = r.url().to_string();
            let (path, query) = url.split_once('?').map(|(p, q)| (p.to_string(), q.to_string())).unwrap_or((url, String::new()));
            let headers = r.headers().iter().map(|h| (h.field.to_string().to_lowercase(), h.value.to_string())).collect();
            let req = Req { method: r.method().to_string(), path, query, headers, body };
            let out = handler(req, &b);
            let mut response = tiny_http::Response::from_data(out.body).with_status_code(out.status);
            for (k, v) in out.headers {
                response = response.with_header(tiny_http::Header::from_bytes(k.as_bytes(), v.as_bytes()).unwrap());
            }
            let _ = r.respond(response);
        }
    });
    base
}

// ---------------------------------------------------------------------------------------------
// WebDAV

fn webdav_server(user: &'static str, pass: &'static str) -> String {
    let files: Arc<Mutex<BTreeMap<String, Vec<u8>>>> = Arc::default();
    let dirs: Arc<Mutex<std::collections::BTreeSet<String>>> = Arc::default();
    dirs.lock().unwrap().insert("/dav".into());
    let auth = format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(format!("{user}:{pass}")));
    serve(move |r, _| {
        // solo per i test: le cartelle che esistono
        if r.path == "/__cartelle" {
            return resp(200, dirs.lock().unwrap().iter().cloned().collect::<Vec<_>>().join("\n"));
        }
        if r.header("authorization") != Some(&auth) {
            return resp(401, "auth");
        }
        let path = dec(&r.path).trim_end_matches('/').to_string();
        let parent = path.rsplit_once('/').map(|(p, _)| p.to_string()).unwrap_or_default();
        let mut files = files.lock().unwrap();
        let mut dirs = dirs.lock().unwrap();
        match r.method.as_str() {
            "MKCOL" if dirs.contains(&path) => resp(405, ""),
            "MKCOL" if !dirs.contains(&parent) => resp(409, "manca la cartella madre"),
            "MKCOL" => {
                dirs.insert(path);
                resp(201, "")
            }
            "PUT" if !dirs.contains(&parent) => resp(409, "manca la cartella madre"),
            "PUT" => {
                files.insert(path, r.body);
                resp(201, "")
            }
            "GET" => match files.get(&path) {
                Some(d) => resp(200, d.clone()),
                None => resp(404, ""),
            },
            // una cartella si cancella con tutto il contenuto, come sui server veri
            "DELETE" if dirs.contains(&path) => {
                let prefix = format!("{path}/");
                dirs.retain(|d| *d != path && !d.starts_with(&prefix));
                files.retain(|f, _| !f.starts_with(&prefix));
                resp(204, "")
            }
            "DELETE" => match files.remove(&path) {
                Some(_) => resp(204, ""),
                None => resp(404, ""),
            },
            "PROPFIND" => {
                assert_eq!(r.header("depth"), Some("1"), "solo profondità 1");
                if !dirs.contains(&path) {
                    return resp(404, "");
                }
                let mut x = String::from(r#"<?xml version="1.0"?><d:multistatus xmlns:d="DAV:">"#);
                let entry = |href: &str, dir: bool, size: usize| {
                    let rt = if dir { "<d:resourcetype><d:collection/></d:resourcetype>".to_string() } else {
                        format!("<d:resourcetype/><d:getcontentlength>{size}</d:getcontentlength>")
                    };
                    format!("<d:response><d:href>{}</d:href><d:propstat><d:prop>{rt}</d:prop></d:propstat></d:response>", enc(href))
                };
                x.push_str(&entry(&format!("{path}/"), true, 0));
                for d in dirs.iter().filter(|d| d.rsplit_once('/').map(|(p, _)| p) == Some(path.as_str())) {
                    x.push_str(&entry(&format!("{d}/"), true, 0));
                }
                for (f, data) in files.iter().filter(|(f, _)| f.rsplit_once('/').map(|(p, _)| p) == Some(path.as_str())) {
                    x.push_str(&entry(f, false, data.len()));
                }
                x.push_str("</d:multistatus>");
                resp(207, x)
            }
            _ => resp(405, ""),
        }
    })
}

// ---------------------------------------------------------------------------------------------
// S3 (path-style), con la firma di ogni richiesta ricalcolata e confrontata.

fn s3_server(access: &'static str, secret: &'static str) -> String {
    let objects: Arc<Mutex<BTreeMap<String, Vec<u8>>>> = Arc::default();
    serve(move |r, _| {
        // firma: dai valori decodificati, come fa AWS
        let payload = hex::encode(Sha256::digest(&r.body));
        let claimed = r.header("x-amz-content-sha256").unwrap_or_default().to_string();
        if claimed != payload {
            return resp(400, "<Error><Code>XAmzContentSHA256Mismatch</Code></Error>");
        }
        let date = r.header("x-amz-date").unwrap_or_default().to_string();
        let headers = vec![
            ("host".to_string(), r.header("host").unwrap_or_default().to_string()),
            ("x-amz-content-sha256".to_string(), claimed.clone()),
            ("x-amz-date".to_string(), date.clone()),
        ];
        let query: Vec<(String, String)> = r
            .query
            .split('&')
            .filter(|p| !p.is_empty())
            .map(|p| p.split_once('=').map(|(k, v)| (dec(k), dec(v))).unwrap_or((dec(p), String::new())))
            .collect();
        let expected = sign(&r.method, &r.path, &query, &headers, &claimed, access, secret, "eu-test-1", &date);
        if r.header("authorization") != Some(expected.as_str()) {
            return resp(403, "<Error><Code>SignatureDoesNotMatch</Code><Message>firma sbagliata</Message></Error>");
        }
        let path = dec(&r.path);
        let mut objects = objects.lock().unwrap();
        let Some(rest) = path.strip_prefix("/bucket") else { return resp(404, "<Error><Code>NoSuchBucket</Code></Error>") };
        let key = rest.trim_start_matches('/').to_string();
        match (r.method.as_str(), key.is_empty()) {
            ("PUT", true) => resp(409, "<Error><Code>BucketAlreadyOwnedByYou</Code></Error>"),
            ("GET", true) => {
                let prefix = r.q("prefix").unwrap_or_default();
                let delim = r.q("delimiter").is_some();
                let start = r.q("continuation-token").unwrap_or_default();
                let keys: Vec<(&String, &Vec<u8>)> = objects
                    .iter()
                    .filter(|(k, _)| k.starts_with(&prefix) && (!delim || !k[prefix.len()..].contains('/')))
                    .filter(|(k, _)| k.as_str() > start.as_str())
                    .collect();
                // pagine piccole, per provare il seguito
                let page: Vec<_> = keys.iter().take(3).collect();
                let mut x = String::from("<ListBucketResult>");
                for (k, v) in &page {
                    x.push_str(&format!("<Contents><Key>{}</Key><Size>{}</Size></Contents>", k.replace('&', "&amp;"), v.len()));
                }
                if keys.len() > 3 {
                    x.push_str(&format!("<IsTruncated>true</IsTruncated><NextContinuationToken>{}</NextContinuationToken>", page[2].0));
                } else {
                    x.push_str("<IsTruncated>false</IsTruncated>");
                }
                x.push_str("</ListBucketResult>");
                resp(200, x)
            }
            ("PUT", false) => {
                objects.insert(key, r.body);
                resp(200, "")
            }
            ("GET", false) => match objects.get(&key) {
                Some(d) => resp(200, d.clone()),
                None => resp(404, "<Error><Code>NoSuchKey</Code></Error>"),
            },
            ("DELETE", false) => {
                objects.remove(&key);
                resp(204, "")
            }
            _ => resp(405, ""),
        }
    })
}

// ---------------------------------------------------------------------------------------------
// OAuth finto, comune ai tre provider.

#[derive(Default)]
struct OAuthState {
    /// Token d'accesso validi.
    access: Vec<String>,
    /// Il refresh token valido (cambia a ogni rinnovo se `rotate`).
    refresh: String,
    rotate: bool,
    issued: u32,
    /// Codici Dropbox: codice → sfida PKCE.
    codes: HashMap<String, String>,
    device_polls: u32,
    refreshes: u32,
}

impl OAuthState {
    fn issue(&mut self) -> Value {
        self.issued += 1;
        let at = format!("AT{}", self.issued);
        self.access.push(at.clone());
        if self.rotate || self.refresh.is_empty() {
            self.refresh = format!("refresh-finto-{}", self.issued);
        }
        json!({"access_token": at, "token_type": "bearer", "expires_in": 3600, "refresh_token": self.refresh})
    }

    /// Gestisce /oauth/token e /oauth/device. `None` se la richiesta non è OAuth.
    fn handle(&mut self, r: &Req, secret: Option<&str>) -> Option<Resp> {
        if !r.path.starts_with("/oauth/") {
            return None;
        }
        let f = r.form();
        if let Some(s) = secret {
            if r.path.starts_with("/oauth/token") && f.get("client_secret").map(String::as_str) != Some(s) {
                return Some(json_resp(401, json!({"error": "invalid_client"})));
            }
        }
        match r.path.as_str() {
            "/oauth/device" => Some(json_resp(
                200,
                json!({"device_code": "DC", "user_code": "ABCD-EFGH", "verification_uri": "https://esempio/device", "interval": 1, "expires_in": 900}),
            )),
            "/oauth/token" => Some(match f.get("grant_type").map(String::as_str) {
                Some("urn:ietf:params:oauth:grant-type:device_code") => {
                    self.device_polls += 1;
                    if self.device_polls < 2 {
                        json_resp(400, json!({"error": "authorization_pending"}))
                    } else {
                        json_resp(200, self.issue())
                    }
                }
                Some("authorization_code") => {
                    let code = f.get("code").cloned().unwrap_or_default();
                    let verifier = f.get("code_verifier").cloned().unwrap_or_default();
                    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
                    if self.codes.get(&code) == Some(&challenge) {
                        json_resp(200, self.issue())
                    } else {
                        json_resp(400, json!({"error": "invalid_grant", "error_description": "PKCE"}))
                    }
                }
                Some("refresh_token") if f.get("refresh_token") == Some(&self.refresh) => {
                    self.refreshes += 1;
                    json_resp(200, self.issue())
                }
                _ => json_resp(400, json!({"error": "invalid_grant"})),
            }),
            _ => None,
        }
    }

    fn authorized(&self, r: &Req) -> bool {
        r.header("authorization").and_then(|a| a.strip_prefix("Bearer ")).is_some_and(|t| self.access.iter().any(|x| x == t))
    }
}

/// Collega un account col flusso vero (codice dispositivo o PKCE) contro il server finto.
fn link(provider: Provider, base: &str, oauth: &Arc<Mutex<OAuthState>>, apps: &AppsConfig) -> String {
    let mut l = Link::start(provider, apps, Some(base)).unwrap();
    let pasted = match &l.prompt {
        LinkPrompt::PasteCode { url } => {
            // la persona acconsente: Dropbox le mostra un codice legato alla sfida PKCE
            let challenge = url.split("code_challenge=").nth(1).unwrap().split('&').next().unwrap().to_string();
            oauth.lock().unwrap().codes.insert("CODICE".into(), challenge);
            Some("CODICE")
        }
        LinkPrompt::Device { code, .. } => {
            assert_eq!(code, "ABCD-EFGH");
            None
        }
    };
    for _ in 0..50 {
        match l.check(pasted).unwrap() {
            LinkState::Done(token) => return token,
            LinkState::Pending => std::thread::sleep(Duration::from_millis(200)),
        }
    }
    panic!("collegamento non concluso");
}

// ---------------------------------------------------------------------------------------------
// Dropbox

fn dropbox_server(oauth: Arc<Mutex<OAuthState>>) -> String {
    let files: Arc<Mutex<BTreeMap<String, Vec<u8>>>> = Arc::default();
    let folders: Arc<Mutex<std::collections::BTreeSet<String>>> = Arc::default();
    serve(move |r, _| {
        if r.path == "/__cartelle" {
            return resp(200, folders.lock().unwrap().iter().cloned().collect::<Vec<_>>().join("\n"));
        }
        if let Some(out) = oauth.lock().unwrap().handle(&r, None) {
            return out;
        }
        if !oauth.lock().unwrap().authorized(&r) {
            return resp(401, "expired_access_token");
        }
        let mut files = files.lock().unwrap();
        let arg: Value = r.header("dropbox-api-arg").map(|a| serde_json::from_str(a).unwrap()).unwrap_or_else(|| r.json());
        let path = arg.get("path").and_then(Value::as_str).unwrap_or_default().to_lowercase();
        let not_found = || json_resp(409, json!({"error_summary": "path/not_found/.."}));
        match r.path.as_str() {
            "/2/files/upload" => {
                assert_eq!(arg["mode"], "overwrite");
                // come Dropbox: le cartelle che contengono il file nascono e restano anche quando si svuotano
                let mut d = path.as_str();
                while let Some((parent, _)) = d.rsplit_once('/') {
                    if !parent.is_empty() {
                        folders.lock().unwrap().insert(parent.to_string());
                    }
                    d = parent;
                }
                files.insert(path, r.body);
                json_resp(200, json!({"name": "x"}))
            }
            "/2/files/download" => match files.get(&path) {
                Some(d) => resp(200, d.clone()),
                None => not_found(),
            },
            "/2/files/delete_v2" if folders.lock().unwrap().contains(&path) => {
                let prefix = format!("{path}/");
                folders.lock().unwrap().retain(|d| *d != path && !d.starts_with(&prefix));
                files.retain(|f, _| !f.starts_with(&prefix));
                json_resp(200, json!({}))
            }
            "/2/files/delete_v2" => match files.remove(&path) {
                Some(_) => json_resp(200, json!({})),
                None => json_resp(409, json!({"error_summary": "path_lookup/not_found/"})),
            },
            "/2/files/create_folder_v2" => {
                if folders.lock().unwrap().insert(path) {
                    json_resp(200, json!({}))
                } else {
                    json_resp(409, json!({"error_summary": "path/conflict/folder/"}))
                }
            }
            "/2/files/list_folder" | "/2/files/list_folder/continue" => {
                let (path, recursive, skip) = if r.path.ends_with("continue") {
                    let c = arg["cursor"].as_str().unwrap().to_string();
                    let (p, rest) = c.split_once('|').unwrap();
                    let (rec, skip) = rest.split_once('|').unwrap();
                    (p.to_string(), rec == "1", skip.parse::<usize>().unwrap())
                } else {
                    (path, arg["recursive"].as_bool().unwrap_or(false), 0)
                };
                let prefix = format!("{path}/");
                if !path.is_empty() && !files.keys().any(|k| k.starts_with(&prefix)) && !folders.lock().unwrap().contains(&path) {
                    return not_found();
                }
                let mut entries: Vec<Value> = Vec::new();
                let mut folders = std::collections::BTreeSet::new();
                for (k, v) in files.iter().filter(|(k, _)| k.starts_with(&prefix)) {
                    let rest = &k[prefix.len()..];
                    match rest.split_once('/') {
                        Some((d, _)) if !recursive => {
                            folders.insert(d.to_string());
                        }
                        _ => entries.push(json!({".tag": "file", "path_display": k, "size": v.len()})),
                    }
                }
                for d in folders {
                    entries.push(json!({".tag": "folder", "path_display": format!("{prefix}{d}")}));
                }
                let page: Vec<Value> = entries.iter().skip(skip).take(4).cloned().collect();
                let more = entries.len() > skip + 4;
                json_resp(
                    200,
                    json!({"entries": page, "has_more": more, "cursor": format!("{path}|{}|{}", if recursive { 1 } else { 0 }, skip + 4)}),
                )
            }
            _ => resp(404, ""),
        }
    })
}

// ---------------------------------------------------------------------------------------------
// OneDrive (Graph)

fn onedrive_server(oauth: Arc<Mutex<OAuthState>>) -> String {
    let files: Arc<Mutex<BTreeMap<String, Vec<u8>>>> = Arc::default();
    serve(move |r, base| {
        if let Some(out) = oauth.lock().unwrap().handle(&r, None) {
            return out;
        }
        let mut files = files.lock().unwrap();
        // il contenuto si scarica da un indirizzo già autorizzato: niente token
        if let Some(p) = r.path.strip_prefix("/download/") {
            assert!(r.header("authorization").is_none(), "il token non va mandato all'indirizzo di download");
            return files.get(&dec(p)).map(|d| resp(200, d.clone())).unwrap_or(resp(404, ""));
        }
        if !oauth.lock().unwrap().authorized(&r) {
            return json_resp(401, json!({"error": {"code": "InvalidAuthenticationToken"}}));
        }
        let children = |files: &BTreeMap<String, Vec<u8>>, dir: &str, skip: usize, next: String| {
            let prefix = if dir.is_empty() { String::new() } else { format!("{dir}/") };
            let mut items: Vec<Value> = Vec::new();
            let mut folders = std::collections::BTreeSet::new();
            for (k, v) in files.iter().filter(|(k, _)| k.starts_with(&prefix)) {
                match k[prefix.len()..].split_once('/') {
                    Some((d, _)) => {
                        folders.insert(d.to_string());
                    }
                    None => items.push(json!({"name": &k[prefix.len()..], "size": v.len(), "file": {}})),
                }
            }
            for d in folders {
                items.push(json!({"name": d, "size": 0, "folder": {"childCount": 1}}));
            }
            let page: Vec<Value> = items.iter().skip(skip).take(3).cloned().collect();
            let mut v = json!({"value": page});
            if items.len() > skip + 3 {
                v["@odata.nextLink"] = json!(format!("{next}?skip={}", skip + 3));
            }
            json_resp(200, v)
        };
        let skip = r.q("skip").and_then(|s| s.parse().ok()).unwrap_or(0);
        match r.path.as_str() {
            "/me/drive/special/approot" => return json_resp(200, json!({"id": "approot"})),
            "/me/drive/special/approot/children" if r.method == "POST" => return json_resp(201, json!({"id": "f"})),
            "/me/drive/special/approot/children" => {
                return children(&files, "", skip, format!("{base}/me/drive/special/approot/children"))
            }
            _ => {}
        }
        let Some(rest) = r.path.strip_prefix("/me/drive/special/approot:/") else { return resp(404, "") };
        let (p, suffix) = rest.split_once(':').unwrap();
        let path = dec(p);
        match (r.method.as_str(), suffix) {
            ("PUT", "/content") => {
                files.insert(path, r.body);
                json_resp(201, json!({"id": "x"}))
            }
            ("GET", "/content") => match files.contains_key(&path) {
                true => Resp { status: 302, headers: vec![("Location".into(), format!("{base}/download/{}", enc(&path)))], body: vec![] },
                false => json_resp(404, json!({"error": {"code": "itemNotFound"}})),
            },
            ("DELETE", "") => match files.remove(&path) {
                Some(_) => resp(204, ""),
                None => json_resp(404, json!({"error": {"code": "itemNotFound"}})),
            },
            ("GET", "/children") => {
                if !files.keys().any(|k| k.starts_with(&format!("{path}/"))) {
                    return json_resp(404, json!({"error": {"code": "itemNotFound"}}));
                }
                children(&files, &path, skip, format!("{base}/me/drive/special/approot:/{p}:/children"))
            }
            ("GET", "") => match files.keys().any(|k| k.starts_with(&format!("{path}/"))) {
                true => json_resp(200, json!({"id": "d"})),
                false => json_resp(404, json!({"error": {"code": "itemNotFound"}})),
            },
            _ => resp(405, ""),
        }
    })
}

// ---------------------------------------------------------------------------------------------
// Google Drive

#[derive(Clone)]
struct Item {
    name: String,
    parent: String,
    folder: bool,
    data: Vec<u8>,
}

#[derive(Default)]
struct DriveState {
    items: BTreeMap<String, Item>,
    next: u32,
    sessions: HashMap<String, (Option<String>, Value)>,
    /// Quante richieste di caricamento rifiutare col limite di velocità (per provare i nuovi tentativi).
    rate_limit: u32,
}

impl DriveState {
    fn add(&mut self, item: Item) -> String {
        self.next += 1;
        let id = format!("id{}", self.next);
        self.items.insert(id.clone(), item);
        id
    }
}

/// Il testo fra apici che segue `after` nella query di Drive.
fn quoted(q: &str, after: &str) -> Option<String> {
    let start = q.find(after)? + after.len();
    let rest = &q[start..];
    let rest = rest.strip_prefix('\'')?;
    let mut out = String::new();
    let mut chars = rest.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => out.push(chars.next()?),
            '\'' => return Some(out),
            c => out.push(c),
        }
    }
    None
}

fn gdrive_server(oauth: Arc<Mutex<OAuthState>>) -> String {
    let st: Arc<Mutex<DriveState>> = Arc::default();
    st.lock().unwrap().rate_limit = 2;
    serve(move |r, base| {
        if r.path == "/__cartelle" {
            let st = st.lock().unwrap();
            let names: Vec<String> = st.items.values().filter(|it| it.folder).map(|it| it.name.clone()).collect();
            return resp(200, names.join("\n"));
        }
        if let Some(out) = oauth.lock().unwrap().handle(&r, Some("segreto-google")) {
            return out;
        }
        if !oauth.lock().unwrap().authorized(&r) {
            return json_resp(401, json!({"error": {"code": 401}}));
        }
        let mut st = st.lock().unwrap();
        let file_json = |id: &str, it: &Item| {
            json!({"id": id, "name": it.name, "size": it.data.len().to_string(),
                   "mimeType": if it.folder { "application/vnd.google-apps.folder" } else { "application/octet-stream" }})
        };
        let path = r.path.clone();
        if (path.starts_with("/upload/") || path.starts_with("/session/")) && st.rate_limit > 0 {
            st.rate_limit -= 1;
            return json_resp(403, json!({"error": {"errors": [{"reason": "userRateLimitExceeded"}]}}));
        }
        if let Some(sid) = path.strip_prefix("/session/") {
            let (id, meta) = st.sessions.remove(sid).unwrap();
            let id = match id {
                Some(id) => {
                    st.items.get_mut(&id).unwrap().data = r.body;
                    id
                }
                None => {
                    let parent = meta["parents"][0].as_str().unwrap().to_string();
                    st.add(Item { name: meta["name"].as_str().unwrap().into(), parent, folder: false, data: r.body })
                }
            };
            return json_resp(200, json!({ "id": id }));
        }
        let upload_type = r.q("uploadType");
        match (r.method.as_str(), path.as_str()) {
            ("GET", "/drive/v3/files") => {
                let q = r.q("q").unwrap();
                let page: usize = r.q("pageToken").and_then(|p| p.parse().ok()).unwrap_or(0);
                let found: Vec<Value> = if q.starts_with("name = ") {
                    let name = quoted(&q, "name = ").unwrap();
                    let parent = quoted(&q, "and ").unwrap();
                    let folder = q.contains("mimeType = ");
                    st.items
                        .iter()
                        .filter(|(_, it)| it.name == name && it.parent == parent && it.folder == folder)
                        .map(|(id, it)| file_json(id, it))
                        .collect()
                } else {
                    let parent = quoted(&q, "").unwrap();
                    st.items.iter().filter(|(_, it)| it.parent == parent).map(|(id, it)| file_json(id, it)).collect()
                };
                let mut v = json!({"files": found.iter().skip(page).take(3).cloned().collect::<Vec<_>>()});
                if found.len() > page + 3 {
                    v["nextPageToken"] = json!((page + 3).to_string());
                }
                json_resp(200, v)
            }
            ("POST", "/drive/v3/files") => {
                let m = r.json();
                assert_eq!(m["mimeType"], "application/vnd.google-apps.folder");
                let parent = m["parents"][0].as_str().unwrap().to_string();
                let id = st.add(Item { name: m["name"].as_str().unwrap().into(), parent, folder: true, data: vec![] });
                json_resp(200, json!({ "id": id }))
            }
            ("POST", "/upload/drive/v3/files") if upload_type.as_deref() == Some("multipart") => {
                let ct = r.header("content-type").unwrap();
                let sep = format!("--{}", ct.split("boundary=").nth(1).unwrap()).into_bytes();
                let b = &r.body;
                // parte 1: metadati JSON; parte 2: contenuto (binario)
                let find = |from: usize, needle: &[u8]| b[from..].windows(needle.len()).position(|w| w == needle).map(|p| p + from);
                let blank: &[u8] = b"\r\n\r\n";
                let p1 = find(0, &sep).unwrap() + sep.len();
                let m_start = find(p1, blank).unwrap() + 4;
                let p2 = find(m_start, &sep).unwrap();
                let meta: Value = serde_json::from_slice(b[m_start..p2].trim_ascii()).unwrap();
                let d_start = find(p2, blank).unwrap() + 4;
                let mut closing = b"\r\n".to_vec();
                closing.extend_from_slice(&sep);
                closing.extend_from_slice(b"--");
                let d_end = b.windows(closing.len()).rposition(|w| w == closing.as_slice()).unwrap();
                let data = b[d_start..d_end].to_vec();
                let parent = meta["parents"][0].as_str().unwrap().to_string();
                let id = st.add(Item { name: meta["name"].as_str().unwrap().into(), parent, folder: false, data });
                json_resp(200, json!({ "id": id }))
            }
            ("POST", "/upload/drive/v3/files") | ("PATCH", _) if upload_type.as_deref() == Some("resumable") => {
                assert!(r.header("x-upload-content-length").is_some());
                let id = path.strip_prefix("/upload/drive/v3/files/").map(str::to_string);
                let sid = format!("s{}", st.next + 1000);
                st.next += 1;
                st.sessions.insert(sid.clone(), (id, r.json()));
                Resp { status: 200, headers: vec![("Location".into(), format!("{base}/session/{sid}"))], body: vec![] }
            }
            ("PATCH", p) if upload_type.as_deref() == Some("media") => {
                let id = p.strip_prefix("/upload/drive/v3/files/").unwrap();
                match st.items.get_mut(id) {
                    Some(it) => {
                        it.data = r.body;
                        json_resp(200, json!({ "id": id }))
                    }
                    None => json_resp(404, json!({})),
                }
            }
            ("GET", p) if r.q("alt").as_deref() == Some("media") => {
                let id = p.strip_prefix("/drive/v3/files/").unwrap();
                match st.items.get(id) {
                    Some(it) => resp(200, it.data.clone()),
                    None => json_resp(404, json!({})),
                }
            }
            ("DELETE", p) => {
                let id = p.strip_prefix("/drive/v3/files/").unwrap();
                match st.items.remove(id) {
                    Some(_) => resp(204, ""),
                    None => json_resp(404, json!({})),
                }
            }
            _ => resp(404, format!("{} {}", r.method, path)),
        }
    })
}

// ---------------------------------------------------------------------------------------------

fn random(seed: &str, len: usize) -> Vec<u8> {
    let mut out = vec![0u8; len];
    blake3::Hasher::new().update(seed.as_bytes()).finalize_xof().fill(&mut out);
    out
}

fn write_config(dir: &Path, locals: usize) -> std::path::PathBuf {
    let mut toml = String::from("# prova\ndata_shards = 4\nparity_shards = 2\n\n[chunk]\nmin = 65536\navg = 262144\nmax = 1048576\n");
    for i in 0..locals {
        toml.push_str(&format!("\n[[backend]]\nname = \"loc{i}\"\ntype = \"local\"\npath = \"{}\"\n", dir.join(format!("loc{i}")).display()));
    }
    let path = dir.join("cloudraid.toml");
    fs::write(&path, toml).unwrap();
    path
}

#[test]
fn repository_su_cinque_servizi_cloud() {
    let tmp = TempDir::new().unwrap();
    let dav = webdav_server("stefano", "pw-dav");
    let s3 = s3_server("AKTEST", "segreto-s3");
    let dbx_oauth = Arc::new(Mutex::new(OAuthState::default()));
    let dbx = dropbox_server(dbx_oauth.clone());
    let od_oauth = Arc::new(Mutex::new(OAuthState { rotate: true, ..Default::default() }));
    let od = onedrive_server(od_oauth.clone());
    let gd_oauth = Arc::new(Mutex::new(OAuthState::default()));
    let gd = gdrive_server(gd_oauth.clone());
    let apps = AppsConfig {
        dropbox_app_key: Some("app-dropbox".into()),
        onedrive_client_id: Some("app-onedrive".into()),
        google_client_id: Some("app-google".into()),
        google_client_secret: Some("segreto-google".into()),
    };

    // repository con 6 cartelle locali, poi i cinque servizi aggiunti come fa l'interfaccia
    let config_path = write_config(tmp.path(), 6);
    let mut repo = Repo::init(Config::load(&config_path).unwrap(), PW).unwrap();
    assert!(tmp.path().join("cloudraid.key").exists(), "copia locale della chiave");
    setup::set_apps(
        &config_path,
        &mut repo,
        &[
            ("dropbox_app_key", "app-dropbox"),
            ("onedrive_client_id", "app-onedrive"),
            ("google_client_id", "app-google"),
            ("google_client_secret", "segreto-google"),
        ],
    )
    .unwrap();
    let add = |repo: &mut Repo, name: &str, kind: BackendKind, secrets: Vec<(&str, String)>| {
        let nb = NewBackend {
            config: BackendConfig { name: name.into(), kind, delay_ms: 0 },
            secrets: secrets.into_iter().map(|(k, v)| (k.to_string(), v)).collect(),
        };
        setup::add_backend(&config_path, repo, nb).unwrap_or_else(|e| panic!("{name}: {e:#}"));
    };
    // una password sbagliata viene scoperta dalla prova, e non resta niente
    let bad = NewBackend {
        config: BackendConfig { name: "dav".into(), kind: BackendKind::Webdav { url: format!("{dav}/dav/repo"), user: "stefano".into() }, delay_ms: 0 },
        secrets: vec![("password".into(), "sbagliata".into())],
    };
    let err = setup::add_backend(&config_path, &mut repo, bad).unwrap_err();
    assert!(format!("{err:#}").contains("accesso negato"), "{err:#}");
    assert!(!fs::read_to_string(&config_path).unwrap().contains("webdav"));

    add(&mut repo, "dav", BackendKind::Webdav { url: format!("{dav}/dav/repo"), user: "stefano".into() }, vec![("password", "pw-dav".into())]);
    add(
        &mut repo,
        "s3",
        BackendKind::S3 {
            endpoint: s3.clone(),
            region: "eu-test-1".into(),
            bucket: "bucket".into(),
            access_key: "AKTEST".into(),
            prefix: "cr".into(),
            path_style: true,
        },
        vec![("secret_key", "segreto-s3".into())],
    );
    let t = link(Provider::Dropbox, &dbx, &dbx_oauth, &apps);
    add(&mut repo, "dbx", BackendKind::Dropbox { folder: "cr".into(), api_base: Some(dbx.clone()) }, vec![("refresh_token", t)]);
    let t = link(Provider::OneDrive, &od, &od_oauth, &apps);
    add(&mut repo, "od", BackendKind::Onedrive { folder: String::new(), api_base: Some(od.clone()) }, vec![("refresh_token", t)]);
    let t = link(Provider::Google, &gd, &gd_oauth, &apps);
    add(&mut repo, "gd", BackendKind::Gdrive { folder: "cloudraid".into(), folder_id: None, api_base: Some(gd.clone()) }, vec![("refresh_token", t)]);

    // niente segreti in chiaro né nella configurazione né nel file delle credenziali
    let cfg_text = fs::read_to_string(&config_path).unwrap();
    assert!(cfg_text.contains("# prova"), "commenti conservati");
    assert!(cfg_text.contains("folder_id"), "cartella di Google registrata");
    let creds = fs::read(tmp.path().join("credenziali.bin")).unwrap();
    for secret in ["pw-dav", "segreto-s3", "refresh-finto"] {
        assert!(!cfg_text.contains(secret) && !String::from_utf8_lossy(&creds).contains(secret), "{secret} in chiaro");
    }

    // file, poi tutte le cartelle locali tranne una dismesse: i dati finiscono sul "cloud"
    let big = random("grande", 3_000_000);
    let small = b"ciao dal cloud".to_vec();
    for (name, data) in [("grande.bin", &big), ("cartella/piccolo.txt", &small)] {
        let p = tmp.path().join("src");
        fs::write(&p, data).unwrap();
        repo.lock().unwrap();
        repo.put_file(&p, name).unwrap();
        repo.save().unwrap();
        repo.unlock();
    }
    repo.lock().unwrap();
    for i in 1..6 {
        let r = repo.retire(&format!("loc{i}"), &mut |_, _| {}).unwrap();
        assert_eq!((r.remaining, r.failed), (0, 0), "{r:?}");
    }
    repo.unlock();
    // un token d'accesso revocato dal provider: si rinnova da solo; OneDrive cambia il refresh token
    dbx_oauth.lock().unwrap().access.clear();
    od_oauth.lock().unwrap().access.clear();
    drop(repo);

    // riaperto da zero con la password: chiave locale, credenziali cifrate, cinque servizi
    let repo = Repo::open(Config::load(&config_path).unwrap(), PW).unwrap();
    assert_eq!(repo.status().iter().filter(|b| b.available).count(), 11, "{:?}", repo.status());
    let mut stats = ReadStats::default();
    for (name, data) in [("grande.bin", &big), ("cartella/piccolo.txt", &small)] {
        let out = tmp.path().join("out");
        repo.get_file(name, &out, &mut stats).unwrap();
        assert!(fs::read(&out).unwrap() == *data, "{name}");
    }
    assert_eq!(stats.degraded, 0);
    assert!(od_oauth.lock().unwrap().refreshes > 0 && dbx_oauth.lock().unwrap().refreshes > 0);
    // il refresh token nuovo di OneDrive è stato salvato: una terza apertura funziona ancora
    drop(repo);
    let mut repo = Repo::open(Config::load(&config_path).unwrap(), PW).unwrap();
    let s = repo.scrub(false).unwrap();
    assert_eq!((s.shards_missing, s.shards_corrupt), (0, 0), "{s:?}");
    repo.lock().unwrap();
    assert_eq!(repo.gc(true, &mut |_, _| {}).unwrap().orphans, 0, "elenchi ricorsivi coerenti su tutti i servizi");
    // un servizio si toglie solo se è vuoto
    assert!(setup::remove_backend(&config_path, &mut repo, "gd").is_err());
    setup::remove_backend(&config_path, &mut repo, "loc3").unwrap();
    assert!(!fs::read_to_string(&config_path).unwrap().contains("loc3"));

    // rinomina: un servizio con dati e token (od) e uno dismesso (loc1). Un secondo processo aperto
    // prima (come il mount) prende i nomi nuovi quando prende il lock.
    repo.unlock();
    let mut other = Repo::open(Config::load(&config_path).unwrap(), PW).unwrap();
    repo.lock().unwrap();
    let od_shards = repo.status().into_iter().find(|b| b.name == "od").unwrap().shards;
    assert!(od_shards > 0);
    assert!(setup::rename_backend(&config_path, &mut repo, "od", "dav").is_err(), "nome già usato");
    assert!(setup::rename_backend(&config_path, &mut repo, "od", "One Drive").is_err(), "nome non valido");
    // config.toml riscritto nello stesso secondo: la data di modifica deve comunque cambiare
    std::thread::sleep(std::time::Duration::from_millis(20));
    setup::rename_backend(&config_path, &mut repo, "od", "onedrive").unwrap();
    setup::rename_backend(&config_path, &mut repo, "loc1", "vecchio").unwrap();
    repo.unlock();
    let cfg_text = fs::read_to_string(&config_path).unwrap();
    assert!(cfg_text.contains("name = \"onedrive\"") && !cfg_text.contains("name = \"od\""), "{cfg_text}");
    assert!(cfg_text.contains("# prova"), "commenti conservati");
    assert!(repo.retired().contains("vecchio") && !repo.retired().contains("loc1"));
    let st = repo.status();
    assert_eq!(st.iter().find(|b| b.name == "onedrive").map(|b| b.shards), Some(od_shards), "{st:?}");
    assert!(st.iter().all(|b| b.name != "od"));
    assert!(repo.credentials().get("onedrive", "refresh_token").is_some());
    assert!(repo.credentials().get("od", "refresh_token").is_none());
    other.lock().unwrap();
    assert!(other.config().backends.iter().any(|b| b.name == "onedrive"), "configurazione riletta prendendo il lock");
    assert_eq!(other.status().iter().find(|b| b.name == "onedrive").map(|b| (b.available, b.shards)), Some((true, od_shards)));
    other.unlock();
    drop((repo, other));
    // riaperto da zero: tutto leggibile, nessun frammento mancante
    let mut repo = Repo::open(Config::load(&config_path).unwrap(), PW).unwrap();
    let s = repo.scrub(false).unwrap();
    assert_eq!((s.shards_missing, s.shards_corrupt), (0, 0), "{s:?}");
    let out = tmp.path().join("out");
    repo.get_file("grande.bin", &out, &mut ReadStats::default()).unwrap();
    assert!(fs::read(&out).unwrap() == big);
}

#[test]
fn backend_singoli_operazioni_di_base() {
    use cloudraid::backend::{open_one, probe};
    use cloudraid::credentials::Credentials;
    let tmp = TempDir::new().unwrap();
    let keys = Arc::new(cloudraid::crypto::Keys::ephemeral());
    let creds = Credentials::load(Some(tmp.path()), keys).unwrap();
    let dav = webdav_server("u", "p");
    let s3 = s3_server("AKTEST", "segreto-s3");
    creds.set("dav", "password", Some("p")).unwrap();
    creds.set("s3", "secret_key", Some("segreto-s3")).unwrap();
    let backends = [
        BackendConfig { name: "dav".into(), kind: BackendKind::Webdav { url: format!("{dav}/dav/a b"), user: "u".into() }, delay_ms: 0 },
        BackendConfig {
            name: "s3".into(),
            kind: BackendKind::S3 {
                endpoint: s3,
                region: "eu-test-1".into(),
                bucket: "bucket".into(),
                access_key: "AKTEST".into(),
                prefix: String::new(),
                path_style: true,
            },
            delay_ms: 0,
        },
    ];
    // i tre servizi con OAuth, collegati col flusso vero
    let apps = AppsConfig {
        dropbox_app_key: Some("k".into()),
        onedrive_client_id: Some("k".into()),
        google_client_id: Some("k".into()),
        google_client_secret: Some("segreto-google".into()),
    };
    let (o1, o2, o3) = (Arc::new(Mutex::new(OAuthState::default())), Arc::new(Mutex::new(OAuthState::default())), Arc::new(Mutex::new(OAuthState::default())));
    let (dbx, od, gd) = (dropbox_server(o1.clone()), onedrive_server(o2.clone()), gdrive_server(o3.clone()));
    let mock_base: HashMap<&str, String> = [("dav", dav.clone()), ("dbx", dbx.clone()), ("gd", gd.clone())].into();
    creds.set("dbx", "refresh_token", Some(&link(Provider::Dropbox, &dbx, &o1, &apps))).unwrap();
    creds.set("od", "refresh_token", Some(&link(Provider::OneDrive, &od, &o2, &apps))).unwrap();
    creds.set("gd", "refresh_token", Some(&link(Provider::Google, &gd, &o3, &apps))).unwrap();
    let gd_id = cloudraid::backend::gdrive_ensure_folder("gd", "cloudraid", Some(&gd), &apps, creds.clone()).unwrap();
    let mut backends = backends.to_vec();
    backends.push(BackendConfig { name: "dbx".into(), kind: BackendKind::Dropbox { folder: String::new(), api_base: Some(dbx) }, delay_ms: 0 });
    backends.push(BackendConfig { name: "od".into(), kind: BackendKind::Onedrive { folder: "sotto".into(), api_base: Some(od) }, delay_ms: 0 });
    backends.push(BackendConfig {
        name: "gd".into(),
        kind: BackendKind::Gdrive { folder: "cloudraid".into(), folder_id: Some(gd_id), api_base: Some(gd) },
        delay_ms: 0,
    });
    for bc in &backends {
        let b = open_one(bc, &apps, &creds);
        assert!(b.is_available() || bc.name != "gd", "{}", bc.name);
        probe(b.as_ref()).unwrap_or_else(|e| panic!("{}: {e:#}", bc.name));
        // la prova non lascia la sua cartella (dove il servizio ha cartelle vere)
        if let Some(base) = mock_base.get(bc.name.as_str()) {
            let dirs = ureq::get(&format!("{base}/__cartelle")).call().unwrap().into_string().unwrap();
            assert!(dirs.lines().all(|d| !d.ends_with("prova")), "{}: cartella di prova rimasta: {dirs}", bc.name);
        }
        for i in 0..7 {
            b.put(&format!("data/ab/k{i}"), &random(&i.to_string(), 100 + i)).unwrap();
        }
        b.put("index/1.bin", b"indice").unwrap();
        let mut l = b.list("data/ab").unwrap();
        l.sort();
        assert_eq!(l.len(), 7, "{}: {l:?}", bc.name);
        assert_eq!(l[0], "data/ab/k0");
        let all = b.list_all("data").unwrap();
        assert_eq!(all.iter().map(|(_, s)| *s).sum::<u64>(), (100..107).sum::<u64>(), "{}", bc.name);
        assert_eq!(b.get("data/ab/k3").unwrap().unwrap(), random("3", 103));
        assert!(b.get("data/ab/niente").unwrap().is_none());
        b.delete("data/ab/k3").unwrap();
        b.delete("data/ab/k3").unwrap();
        assert!(b.get("data/ab/k3").unwrap().is_none());
        assert!(b.list("vuota").unwrap().is_empty());
        // un oggetto grande (l'indice di un repository grande): Google passa al caricamento "resumable"
        let big = random("grande", 5 << 20);
        b.put("index/2.bin", &big).unwrap();
        b.put("index/2.bin", &big[..(5 << 20) - 7]).unwrap();
        assert!(b.get("index/2.bin").unwrap().unwrap() == big[..(5 << 20) - 7], "{}", bc.name);
    }
}
