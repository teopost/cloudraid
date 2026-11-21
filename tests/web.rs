//! Test dell'interfaccia web attraverso HTTP, con cartelle locali al posto degli account.

use cloudraid::web::{start, ServeOptions};
use cloudraid::{AppsConfig, BackendConfig, BackendKind, ChunkConfig, Config, Repo};
use serde_json::{json, Value};
use std::fs;
use std::io::Read;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tempfile::TempDir;

const PW: &str = "password del server di prova";

struct Lab {
    tmp: TempDir,
    cfg: Config,
    base: String,
}

fn config(tmp: &TempDir) -> Config {
    Config {
        data_shards: 4,
        parity_shards: 2,
        chunk: ChunkConfig { min: 16 << 10, avg: 64 << 10, max: 256 << 10 },
        transfers: 4,
        apps: AppsConfig::default(),
        dir: None,
        backends: (0..6)
            .map(|i| BackendConfig {
                name: format!("acc{i}"),
                kind: BackendKind::Local { path: tmp.path().join(format!("acc{i}")) },
                delay_ms: 0,
            })
            .collect(),
    }
}

impl Lab {
    fn new() -> Lab {
        let tmp = TempDir::new().unwrap();
        let cfg = config(&tmp);
        Repo::init(cfg.clone(), PW).unwrap();
        let toml = format!(
            "data_shards = 4\nparity_shards = 2\n[chunk]\nmin = 16384\navg = 65536\nmax = 262144\n{}",
            (0..6)
                .map(|i| format!("[[backend]]\nname = \"acc{i}\"\ntype = \"local\"\npath = \"{}\"\n", tmp.path().join(format!("acc{i}")).display()))
                .collect::<String>()
        );
        let cfg_path = tmp.path().join("cloudraid.toml");
        fs::write(&cfg_path, toml).unwrap();
        let addr = start(ServeOptions {
            listen: "127.0.0.1:0".into(),
            config: cfg_path,
            state: tmp.path().join("web-state.json"),
            insecure_cookie: true,
        })
        .unwrap();
        Lab { tmp, cfg, base: format!("http://{addr}") }
    }

    fn dir(&self, i: usize) -> PathBuf {
        self.tmp.path().join(format!("acc{i}"))
    }
}

struct Resp {
    code: u16,
    body: Vec<u8>,
    cookie: Option<String>,
    headers: Vec<(String, String)>,
}

impl Resp {
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or(Value::Null)
    }
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
}

fn call(lab: &Lab, method: &str, path: &str, cookie: Option<&str>, body: Option<&[u8]>, extra: &[(&str, &str)]) -> Resp {
    let mut req = ureq::request(method, &format!("{}/{path}", lab.base));
    if method != "GET" {
        req = req.set("X-Cloudraid", "1");
    }
    if let Some(c) = cookie {
        req = req.set("Cookie", &format!("cloudraid_session={c}"));
    }
    for (k, v) in extra {
        req = req.set(k, v);
    }
    let result = match body {
        Some(b) => req.send_bytes(b),
        None => req.call(),
    };
    let resp = match result {
        Ok(r) => r,
        Err(ureq::Error::Status(_, r)) => r,
        Err(e) => panic!("{method} {path}: {e}"),
    };
    let code = resp.status();
    let headers: Vec<(String, String)> =
        resp.headers_names().iter().map(|n| (n.clone(), resp.header(n).unwrap_or("").to_string())).collect();
    let cookie = resp
        .header("Set-Cookie")
        .and_then(|c| c.split(';').next())
        .and_then(|c| c.strip_prefix("cloudraid_session="))
        .map(str::to_string);
    let mut body = Vec::new();
    resp.into_reader().read_to_end(&mut body).unwrap();
    Resp { code, body, cookie, headers }
}

fn login(lab: &Lab) -> String {
    let r = call(lab, "POST", "api/login", None, Some(json!({"password": PW}).to_string().as_bytes()), &[]);
    assert_eq!(r.code, 200, "{}", r.text());
    r.cookie.expect("cookie di sessione")
}

/// Aspetta la fine dell'operazione `id` e ne restituisce il risultato.
fn wait_job(lab: &Lab, cookie: &str, id: &str) -> Value {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let st = call(lab, "GET", "api/status", Some(cookie), None, &[]).json();
        if let Some(j) = st["history"].as_array().unwrap().iter().find(|j| j["id"] == id) {
            return j.clone();
        }
        assert!(Instant::now() < deadline, "operazione {id} non finita");
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn run_job(lab: &Lab, cookie: &str, method: &str, path: &str, body: Option<&[u8]>) -> Value {
    let r = call(lab, method, path, Some(cookie), body, &[]);
    assert_eq!(r.code, 202, "{method} {path}: {}", r.text());
    let id = r.json()["job"]["id"].as_str().unwrap().to_string();
    wait_job(lab, cookie, &id)
}

fn status(lab: &Lab, cookie: &str) -> Value {
    call(lab, "GET", "api/status", Some(cookie), None, &[]).json()
}

fn random(seed: &str, len: usize) -> Vec<u8> {
    let mut out = vec![0u8; len];
    blake3::Hasher::new().update(seed.as_bytes()).finalize_xof().fill(&mut out);
    out
}

#[test]
fn pagine_statiche_e_prefisso() {
    let lab = Lab::new();
    let r = call(&lab, "GET", "", None, None, &[]);
    assert_eq!(r.code, 200);
    assert!(r.text().contains(r#"<base href="/">"#));
    assert!(r.header("Content-Security-Policy").unwrap().contains("default-src 'self'"));
    // dietro Traefik con StripPrefix
    let r = call(&lab, "GET", "", None, None, &[("X-Forwarded-Prefix", "/cloudraid")]);
    assert!(r.text().contains(r#"<base href="/cloudraid/">"#));
    // niente iniezioni attraverso il prefisso
    let r = call(&lab, "GET", "", None, None, &[("X-Forwarded-Prefix", "/x\"><script>")]);
    assert!(r.text().contains(r#"<base href="/xscript/">"#), "{}", &r.text()[..300]);
    for p in ["app.js", "app.css", "favicon.svg", "healthz"] {
        assert_eq!(call(&lab, "GET", p, None, None, &[]).code, 200, "{p}");
    }
}

#[test]
fn accesso_e_protezioni() {
    let lab = Lab::new();
    // senza sessione
    assert_eq!(call(&lab, "GET", "api/status", None, None, &[]).code, 401);
    let s = call(&lab, "GET", "api/session", None, None, &[]).json();
    assert_eq!(s["logged"], false);
    assert_eq!(s["unlocked"], false);
    // senza l'intestazione anti-CSRF
    let r = ureq::post(&format!("{}/api/login", lab.base)).send_string(&json!({"password": PW}).to_string());
    assert!(matches!(r, Err(ureq::Error::Status(403, _))));
    // password errata
    let r = call(&lab, "POST", "api/login", None, Some(br#"{"password":"sbagliata"}"#), &[]);
    assert_eq!(r.code, 401);
    assert!(r.cookie.is_none());
    // password giusta: apre il repository
    let cookie = login(&lab);
    let st = status(&lab, &cookie);
    assert_eq!(st["snapshot"]["backends"].as_array().unwrap().len(), 6);
    assert_eq!(st["snapshot"]["data_shards"], 4);
    // dopo l'apertura la password si verifica senza riaprire
    assert_eq!(call(&lab, "POST", "api/login", None, Some(br#"{"password":"sbagliata"}"#), &[]).code, 401);
    let second = login(&lab);
    assert_ne!(second, cookie);
    // cookie inventato
    assert_eq!(call(&lab, "GET", "api/status", Some("abc"), None, &[]).code, 401);
    // uscita
    assert_eq!(call(&lab, "POST", "api/logout", Some(&cookie), None, &[]).code, 200);
    assert_eq!(call(&lab, "GET", "api/status", Some(&cookie), None, &[]).code, 401);
    assert_eq!(call(&lab, "GET", "api/status", Some(&second), None, &[]).code, 200);
}

#[test]
fn carica_scarica_verifica_ripara_elimina() {
    let lab = Lab::new();
    let cookie = login(&lab);
    let data = random("web", 900_000);

    let j = run_job(&lab, &cookie, "PUT", "api/upload?path=cartella%20uno/dati.bin", Some(&data));
    assert_eq!(j["state"], "ok", "{j}");
    let st = status(&lab, &cookie);
    assert_eq!(st["snapshot"]["files"][0]["path"], "cartella uno/dati.bin");
    assert_eq!(st["snapshot"]["files"][0]["size"], 900_000);
    assert!(st["snapshot"]["locks"].as_array().unwrap().is_empty(), "lock rilasciato");

    let r = call(&lab, "GET", "api/download?path=cartella%20uno/dati.bin", Some(&cookie), None, &[]);
    assert_eq!(r.code, 200);
    assert!(r.body == data, "contenuto scaricato diverso");
    assert!(r.header("Content-Disposition").unwrap().contains("dati.bin"));
    assert_eq!(call(&lab, "GET", "api/download?path=non/esiste", Some(&cookie), None, &[]).code, 404);

    let j = run_job(&lab, &cookie, "POST", "api/scrub", Some(br#"{"repair":false}"#));
    assert_eq!(j["state"], "ok", "{j}");
    assert_eq!(j["report"]["shards_missing"], 0);

    // due account persi e sostituiti con cartelle vuote
    for i in [1, 4] {
        fs::remove_dir_all(lab.dir(i)).unwrap();
        fs::create_dir_all(lab.dir(i)).unwrap();
    }
    let j = run_job(&lab, &cookie, "POST", "api/scrub", Some(br#"{"repair":true}"#));
    assert_eq!(j["state"], "ok", "{j}");
    assert!(j["report"]["repaired_shards"].as_u64().unwrap() > 0);
    assert_eq!(j["report"]["unrepaired_shards"], 0);
    let j = run_job(&lab, &cookie, "POST", "api/scrub", Some(br#"{"repair":false}"#));
    assert_eq!(j["report"]["shards_missing"], 0, "{j}");

    // un account sparito: dopo il ricontrollo risulta non disponibile, i dati si leggono lo stesso
    fs::remove_dir_all(lab.dir(2)).unwrap();
    let j = run_job(&lab, &cookie, "POST", "api/refresh", None);
    assert_eq!(j["state"], "ok");
    let st = status(&lab, &cookie);
    assert_eq!(st["snapshot"]["backends"][2]["available"], false);
    let r = call(&lab, "GET", "api/download?path=cartella%20uno/dati.bin", Some(&cookie), None, &[]);
    assert!(r.body == data);

    // le scritture con un backend mancante vengono rifiutate e l'errore arriva all'interfaccia
    let j = run_job(&lab, &cookie, "PUT", "api/upload?path=altro.bin", Some(b"ciao"));
    assert_eq!(j["state"], "error");
    assert!(j["message"].as_str().unwrap().contains("servono 6 backend"), "{j}");
    fs::create_dir_all(lab.dir(2)).unwrap();
    run_job(&lab, &cookie, "POST", "api/refresh", None);
    run_job(&lab, &cookie, "POST", "api/scrub", Some(br#"{"repair":true}"#));

    let j = run_job(&lab, &cookie, "DELETE", "api/files?path=cartella%20uno", None);
    assert_eq!(j["state"], "ok", "{j}");
    let st = status(&lab, &cookie);
    assert!(st["snapshot"]["files"].as_array().unwrap().is_empty());
    assert_eq!(st["snapshot"]["chunks"], 0);

    // la cronologia sopravvive al riavvio
    let saved: Value = serde_json::from_slice(&fs::read(lab.tmp.path().join("web-state.json")).unwrap()).unwrap();
    assert!(saved["history"].as_array().unwrap().len() >= 8);
}

#[test]
fn modifiche_dalla_riga_di_comando_non_vanno_perse() {
    let lab = Lab::new();
    let cookie = login(&lab);
    run_job(&lab, &cookie, "PUT", "api/upload?path=dal-web-1.bin", Some(&random("a", 100_000)));

    // un altro processo (la riga di comando) aggiunge un file mentre il server è acceso
    {
        let mut cli = Repo::open(lab.cfg.clone(), PW).unwrap();
        cli.lock().unwrap();
        let p = lab.tmp.path().join("cli.bin");
        fs::write(&p, random("cli", 120_000)).unwrap();
        cli.put_file(&p, "dalla-cli.bin").unwrap();
        // una cartella vuota, come quelle create dal mount
        cli.mkdir("vuota/dentro").unwrap();
        cli.save().unwrap();
    }

    // il server non ha ancora riletto l'indice, ma prima di scrivere prende il lock e lo rilegge
    let j = run_job(&lab, &cookie, "PUT", "api/upload?path=dal-web-2.bin", Some(&random("b", 100_000)));
    assert_eq!(j["state"], "ok", "{j}");
    let files: Vec<String> = status(&lab, &cookie)["snapshot"]["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["path"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(files, ["dal-web-1.bin", "dal-web-2.bin", "dalla-cli.bin"]);
    assert_eq!(status(&lab, &cookie)["snapshot"]["dirs"], serde_json::json!(["vuota/dentro"]));

    // pulizia e ribilanciamento dall'interfaccia (qui non c'è niente da fare)
    let j = run_job(&lab, &cookie, "POST", "api/gc", Some(br#"{"dry_run":true}"#));
    assert_eq!((j["state"].as_str(), j["kind"].as_str()), (Some("ok"), Some("gc-check")), "{j}");
    assert!(j["message"].as_str().unwrap().starts_with("Nessun frammento orfano"), "{j}");
    let j = run_job(&lab, &cookie, "POST", "api/rebalance", Some(br#"{"dry_run":false}"#));
    assert_eq!(j["state"], "ok", "{j}");
    assert!(j["message"].as_str().unwrap().contains("niente da spostare"), "{j}");

    // dismissione: con 6 backend e 4+2 frammenti non si può, e lo si dice chiaramente
    let j = run_job(&lab, &cookie, "POST", "api/retire", Some(br#"{"name":"acc0"}"#));
    assert_eq!(j["state"], "error", "{j}");
    assert!(j["message"].as_str().unwrap().contains("aggiungi prima"), "{j}");

    // e la riga di comando vede tutto
    let cli = Repo::open(lab.cfg.clone(), PW).unwrap();
    assert_eq!(cli.index().files.len(), 3);
}

#[test]
fn un_operazione_alla_volta() {
    let lab = Lab::new();
    let cookie = login(&lab);
    // abbastanza dati da rendere lo scrub non istantaneo
    run_job(&lab, &cookie, "PUT", "api/upload?path=grande.bin", Some(&random("g", 12_000_000)));
    let first = call(&lab, "POST", "api/scrub", Some(&cookie), Some(br#"{"repair":false}"#), &[]);
    assert_eq!(first.code, 202);
    let second = call(&lab, "POST", "api/scrub", Some(&cookie), Some(br#"{"repair":false}"#), &[]);
    let busy = status(&lab, &cookie)["busy"].as_bool().unwrap();
    if busy || second.code == 409 {
        assert_eq!(second.code, 409, "{}", second.text());
    }
    let id = first.json()["job"]["id"].as_str().unwrap().to_string();
    assert_eq!(wait_job(&lab, &cookie, &id)["state"], "ok");
}
