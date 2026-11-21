//! Configurazione da programma (per l'interfaccia web): aggiungere e togliere backend e impostare
//! le applicazioni OAuth, modificando `cloudraid.toml` senza perdere i commenti. Ogni modifica viene
//! controllata prima di scrivere, il file precedente resta come `cloudraid.toml.bak`, e il
//! repository aperto passa alla configurazione nuova senza chiedere di nuovo la password.

use anyhow::{anyhow, bail, Context, Result};
use std::fs;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use toml_edit::{value, ArrayOfTables, DocumentMut, Item, Table};

use crate::backend;
use crate::config::{valid_name, BackendConfig, BackendKind, Config};
use crate::crypto::random_hex;
use crate::repo::Repo;

/// Un backend da aggiungere: la sua configurazione e le credenziali (password, chiave segreta,
/// refresh token) da cifrare.
pub struct NewBackend {
    pub config: BackendConfig,
    pub secrets: Vec<(String, String)>,
}

/// La tabella TOML di un backend.
fn backend_table(b: &BackendConfig) -> Table {
    let mut t = Table::new();
    t["name"] = value(b.name.as_str());
    t["type"] = value(b.kind.type_name());
    match &b.kind {
        BackendKind::Local { path } => {
            t["path"] = value(path.to_string_lossy().as_ref());
        }
        BackendKind::Webdav { url, user } => {
            t["url"] = value(url.as_str());
            t["user"] = value(user.as_str());
        }
        BackendKind::S3 { endpoint, region, bucket, access_key, prefix, path_style } => {
            t["endpoint"] = value(endpoint.as_str());
            t["region"] = value(region.as_str());
            t["bucket"] = value(bucket.as_str());
            t["access_key"] = value(access_key.as_str());
            if !prefix.is_empty() {
                t["prefix"] = value(prefix.as_str());
            }
            t["path_style"] = value(*path_style);
        }
        BackendKind::Dropbox { folder, api_base } | BackendKind::Onedrive { folder, api_base } => {
            if !folder.is_empty() {
                t["folder"] = value(folder.as_str());
            }
            if let Some(a) = api_base {
                t["api_base"] = value(a.as_str());
            }
        }
        BackendKind::Gdrive { folder, folder_id, api_base } => {
            t["folder"] = value(folder.as_str());
            if let Some(id) = folder_id {
                t["folder_id"] = value(id.as_str());
            }
            if let Some(a) = api_base {
                t["api_base"] = value(a.as_str());
            }
        }
    }
    if b.delay_ms > 0 {
        t["delay_ms"] = value(b.delay_ms as i64);
    }
    t
}

/// Legge il file, applica `edit` al documento, controlla il risultato e lo scrive (con copia di
/// sicurezza del precedente). Restituisce la configurazione nuova.
fn edit_config(path: &Path, edit: impl FnOnce(&mut DocumentMut) -> Result<()>) -> Result<Config> {
    let old = fs::read_to_string(path).with_context(|| format!("impossibile leggere {}", path.display()))?;
    let mut doc: DocumentMut = old.parse().with_context(|| format!("{} non è TOML valido", path.display()))?;
    edit(&mut doc)?;
    let new = doc.to_string();
    Config::parse(&new).context("la configurazione risultante non è valida")?;
    let dir = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let backup = path.with_extension("toml.bak");
    let _ = fs::remove_file(&backup);
    fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(&backup)
        .and_then(|mut f| f.write_all(old.as_bytes()))
        .context("copia di sicurezza della configurazione")?;
    let tmp = dir.join(format!(".cloudraid.toml.{}.tmp", random_hex(4)));
    let result = (|| -> std::io::Result<()> {
        let mut f = fs::OpenOptions::new().create_new(true).write(true).mode(0o600).open(&tmp)?;
        f.write_all(new.as_bytes())?;
        f.sync_all()?;
        fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result.with_context(|| format!("impossibile scrivere {}", path.display()))?;
    Config::load(path)
}

fn backends_mut(doc: &mut DocumentMut) -> Result<&mut ArrayOfTables> {
    if doc.get("backend").is_none() {
        doc["backend"] = Item::ArrayOfTables(ArrayOfTables::new());
    }
    doc["backend"].as_array_of_tables_mut().ok_or_else(|| anyhow!("[[backend]] nella configurazione ha una forma inattesa"))
}

/// Aggiunge un backend: salva le credenziali, crea la cartella del repository, prova scrittura,
/// lettura, elenco e cancellazione, e solo se tutto va bene lo scrive nella configurazione e lo
/// rende attivo. Se la prova fallisce non resta niente (né configurazione né credenziali).
pub fn add_backend(config_path: &Path, repo: &mut Repo, mut nb: NewBackend) -> Result<()> {
    let name = nb.config.name.clone();
    if !valid_name(&name) {
        bail!("nome non valido: usa lettere minuscole, cifre, - e _ (al massimo 40 caratteri)");
    }
    if repo.config().backends.iter().any(|b| b.name == name) {
        bail!("esiste già un backend che si chiama {name}");
    }
    let creds = repo.credentials().clone();
    let secrets: Vec<(&str, &str)> = nb.secrets.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    creds.set_all(&name, &secrets)?;
    let result = (|| -> Result<()> {
        let apps = repo.config().apps.clone();
        // Google Drive: la cartella si crea subito e il suo identificativo va nella configurazione
        if let BackendKind::Gdrive { folder, folder_id, api_base } = &mut nb.config.kind {
            if folder_id.is_none() {
                *folder_id = Some(backend::gdrive_ensure_folder(&name, folder, api_base.as_deref(), &apps, creds.clone())?);
            }
        }
        let b = backend::open_one(&nb.config, &apps, &creds);
        backend::probe(b.as_ref()).with_context(|| format!("prova di {name} non riuscita"))?;
        let table = backend_table(&nb.config);
        let cfg = edit_config(config_path, |doc| {
            backends_mut(doc)?.push(table);
            Ok(())
        })?;
        repo.reconfigure(cfg)
    })();
    if result.is_err() {
        let _ = creds.remove(&name);
    }
    result
}

/// Toglie un backend dalla configurazione. Solo se non contiene frammenti (dismesso e svuotato, o
/// mai usato): così non si perde mai ridondanza. Serve il lock del repository.
pub fn remove_backend(config_path: &Path, repo: &mut Repo, name: &str) -> Result<()> {
    if !repo.config().backends.iter().any(|b| b.name == name) {
        bail!("{name} non è nella configurazione");
    }
    let shards = repo.status().into_iter().find(|s| s.name == name).map(|s| s.shards).unwrap_or(0);
    if shards > 0 {
        bail!("{name} contiene ancora {shards} frammenti: prima dismettilo, così vengono spostati sugli altri");
    }
    let cfg = edit_config(config_path, |doc| {
        let arr = backends_mut(doc)?;
        let pos = (0..arr.len())
            .find(|&i| arr.get(i).and_then(|t| t.get("name")).and_then(|v| v.as_str()) == Some(name))
            .ok_or_else(|| anyhow!("{name} non trovato nel file"))?;
        arr.remove(pos);
        Ok(())
    })?;
    // non è più in dismissione (un backend futuro con lo stesso nome partirà pulito)
    if repo.retired().contains(name) {
        repo.unretire(name)?;
        repo.save()?;
    }
    repo.reconfigure(cfg)?;
    repo.credentials().remove(name)?;
    Ok(())
}

/// Cambia il nome di un backend: nella configurazione, nelle credenziali e nell'indice (i frammenti
/// restano dove sono: sui servizi il nome non compare). Serve il lock del repository. Gli altri
/// processi (mount, riga di comando) rileggono la configurazione quando prendono il lock.
pub fn rename_backend(config_path: &Path, repo: &mut Repo, old: &str, new: &str) -> Result<()> {
    if !valid_name(new) {
        bail!("nome non valido: usa lettere minuscole, cifre, - e _ (al massimo 40 caratteri)");
    }
    if old == new {
        return Ok(());
    }
    if !repo.config().backends.iter().any(|b| b.name == old) {
        bail!("{old} non è nella configurazione");
    }
    if repo.config().backends.iter().any(|b| b.name == new) {
        bail!("esiste già un backend che si chiama {new}");
    }
    if !repo.holds_lock() {
        bail!("serve il lock del repository");
    }
    let set_name = |from: &str, to: &str| {
        let (from, to) = (from.to_string(), to.to_string());
        edit_config(config_path, move |doc| {
            let arr = backends_mut(doc)?;
            let t = arr
                .iter_mut()
                .find(|t| t.get("name").and_then(|v| v.as_str()) == Some(from.as_str()))
                .ok_or_else(|| anyhow!("{from} non trovato nel file"))?;
            t["name"] = value(to.as_str());
            Ok(())
        })
    };
    let creds = repo.credentials().clone();
    let cfg = set_name(old, new)?;
    creds.rename(old, new)?;
    let result = (|| -> Result<()> {
        repo.reconfigure(cfg)?;
        repo.rename_backend_refs(old, new)?;
        repo.save()
    })();
    if let Err(e) = result {
        // si torna al nome vecchio: configurazione e credenziali di nuovo coerenti con l'indice
        let back = set_name(new, old);
        let _ = creds.rename(new, old);
        if let Ok(cfg) = back {
            let _ = repo.reconfigure(cfg);
        }
        return Err(e);
    }
    Ok(())
}

/// Imposta (o, con stringa vuota, toglie) i valori della sezione `[apps]`.
pub fn set_apps(config_path: &Path, repo: &mut Repo, values: &[(&str, &str)]) -> Result<()> {
    const KEYS: [&str; 4] = ["dropbox_app_key", "onedrive_client_id", "google_client_id", "google_client_secret"];
    let cfg = edit_config(config_path, |doc| {
        if doc.get("apps").is_none() {
            let mut t = Table::new();
            t.set_implicit(false);
            t.decor_mut().set_prefix("\n# Applicazioni registrate presso i provider (vedi Impostazioni nell'interfaccia)\n");
            doc["apps"] = Item::Table(t);
        }
        let apps = doc["apps"].as_table_mut().ok_or_else(|| anyhow!("[apps] ha una forma inattesa"))?;
        for (k, v) in values {
            if !KEYS.contains(k) {
                bail!("impostazione sconosciuta: {k}");
            }
            let v = v.trim();
            if v.is_empty() {
                apps.remove(k);
            } else {
                apps[*k] = value(v);
            }
        }
        Ok(())
    })?;
    repo.reconfigure(cfg)
}
