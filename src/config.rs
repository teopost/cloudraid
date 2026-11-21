//! Configurazione del repository: schema Reed-Solomon, dimensione dei blocchi, backend.

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

#[derive(Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Frammenti di dati per ogni blocco (k).
    pub data_shards: usize,
    /// Frammenti di parità per ogni blocco (m): si possono perdere fino a m backend.
    pub parity_shards: usize,
    #[serde(default)]
    pub chunk: ChunkConfig,
    /// Blocchi caricati o scaricati in parallelo (ognuno con tutti i suoi frammenti in parallelo).
    #[serde(default = "default_transfers")]
    pub transfers: usize,
    #[serde(default)]
    pub apps: AppsConfig,
    #[serde(rename = "backend")]
    pub backends: Vec<BackendConfig>,
    /// Cartella della configurazione: lì stanno anche la copia della chiave e le credenziali cifrate.
    #[serde(skip)]
    pub dir: Option<PathBuf>,
}

/// Dimensioni dei blocchi a lunghezza variabile (FastCDC), in byte.
#[derive(Deserialize, Clone, Copy, Debug)]
#[serde(deny_unknown_fields)]
pub struct ChunkConfig {
    pub min: u32,
    pub avg: u32,
    pub max: u32,
}

impl Default for ChunkConfig {
    fn default() -> Self {
        ChunkConfig { min: 1 << 20, avg: 4 << 20, max: 16 << 20 }
    }
}

fn default_transfers() -> usize {
    4
}

/// Le applicazioni registrate presso i provider con login OAuth (una volta sola, vedi il README).
/// Non sono segreti veri: per le applicazioni installate i provider li considerano pubblici.
#[derive(Deserialize, Clone, Debug, Default)]
#[serde(deny_unknown_fields)]
pub struct AppsConfig {
    /// App key di un'app Dropbox con accesso "App folder".
    #[serde(default)]
    pub dropbox_app_key: Option<String>,
    /// Application (client) ID di un'app Microsoft Entra con i flussi client pubblici abilitati.
    #[serde(default)]
    pub onedrive_client_id: Option<String>,
    /// Client OAuth Google di tipo "TV e dispositivi con input limitato".
    #[serde(default)]
    pub google_client_id: Option<String>,
    #[serde(default)]
    pub google_client_secret: Option<String>,
}

#[derive(Deserialize, Clone, Debug)]
pub struct BackendConfig {
    pub name: String,
    #[serde(flatten)]
    pub kind: BackendKind,
    /// Solo per prove: ritardo artificiale in millisecondi su ogni lettura, per simulare un account lento.
    #[serde(default)]
    pub delay_ms: u64,
}

/// I tipi di backend. Password, chiavi segrete e token non stanno qui ma nelle credenziali
/// cifrate accanto alla configurazione (vedi `credentials`).
#[derive(Deserialize, Clone, Debug)]
#[serde(tag = "type", rename_all = "lowercase", deny_unknown_fields)]
pub enum BackendKind {
    /// Una cartella locale (o un disco montato).
    Local { path: PathBuf },
    /// Un server WebDAV (Nextcloud, ownCloud, NAS...): l'indirizzo della cartella del repository.
    Webdav { url: String, user: String },
    /// Un archivio compatibile S3 (AWS, Backblaze B2, Wasabi, MinIO...).
    S3 {
        /// Per esempio `https://s3.eu-central-003.backblazeb2.com`.
        endpoint: String,
        #[serde(default = "default_region")]
        region: String,
        bucket: String,
        access_key: String,
        /// Prefisso degli oggetti nel bucket (vuoto: tutto il bucket).
        #[serde(default)]
        prefix: String,
        /// Indirizzi `endpoint/bucket/oggetto` (vero) o `bucket.endpoint/oggetto` (falso).
        #[serde(default = "yes")]
        path_style: bool,
    },
    /// Dropbox, nella cartella dell'app (`Applicazioni/<nome app>`).
    Dropbox {
        /// Sottocartella dentro la cartella dell'app (vuota: la cartella dell'app stessa).
        #[serde(default)]
        folder: String,
        /// Solo per prove: indirizzo alternativo delle API.
        #[serde(default)]
        api_base: Option<String>,
    },
    /// OneDrive, nella cartella dell'app (`Applicazioni/<nome app>`).
    Onedrive {
        #[serde(default)]
        folder: String,
        #[serde(default)]
        api_base: Option<String>,
    },
    /// Google Drive: una cartella nel Drive, visibile a cloudraid solo per i file che crea lui.
    Gdrive {
        #[serde(default = "default_gdrive_folder")]
        folder: String,
        /// Identificativo della cartella, scritto da cloudraid quando la crea.
        #[serde(default)]
        folder_id: Option<String>,
        #[serde(default)]
        api_base: Option<String>,
    },
}

fn default_region() -> String {
    "us-east-1".into()
}

fn yes() -> bool {
    true
}

fn default_gdrive_folder() -> String {
    "cloudraid".into()
}

impl BackendKind {
    /// Il nome del tipo, come nella configurazione.
    pub fn type_name(&self) -> &'static str {
        match self {
            BackendKind::Local { .. } => "local",
            BackendKind::Webdav { .. } => "webdav",
            BackendKind::S3 { .. } => "s3",
            BackendKind::Dropbox { .. } => "dropbox",
            BackendKind::Onedrive { .. } => "onedrive",
            BackendKind::Gdrive { .. } => "gdrive",
        }
    }
}

/// Un nome di backend valido: lettere minuscole, cifre, `-` e `_`, fino a 40 caratteri.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 40
        && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

impl Config {
    pub fn load(path: &Path) -> Result<Config> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("impossibile leggere la configurazione {}", path.display()))?;
        let mut cfg = Config::parse(&text).with_context(|| format!("configurazione {} non valida", path.display()))?;
        cfg.dir = Some(path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new(".")).to_path_buf());
        Ok(cfg)
    }

    /// Legge e controlla una configurazione dal testo TOML (senza cartella).
    pub fn parse(text: &str) -> Result<Config> {
        if text.lines().any(|l| l.trim_start().starts_with("[rclone]") || l.replace(' ', "") == "type=\"rclone\"") {
            bail!(
                "rclone non è più usato: cloudraid parla direttamente con Google Drive, OneDrive, Dropbox, \
                 WebDAV e S3. Togli la sezione [rclone] e i backend di tipo \"rclone\", poi riaggiungili \
                 dall'interfaccia web"
            );
        }
        let cfg: Config = toml::from_str(text)?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn shards(&self) -> usize {
        self.data_shards + self.parity_shards
    }

    pub fn validate(&self) -> Result<()> {
        use fastcdc::v2020::{AVERAGE_MAX, AVERAGE_MIN, MAXIMUM_MAX, MAXIMUM_MIN, MINIMUM_MAX, MINIMUM_MIN};
        if self.data_shards == 0 {
            bail!("data_shards deve essere almeno 1");
        }
        if self.shards() > 255 {
            bail!("data_shards + parity_shards non può superare 255");
        }
        if self.backends.len() < self.shards() {
            bail!(
                "servono almeno {} backend ({} dati + {} parità), ne sono configurati {}",
                self.shards(),
                self.data_shards,
                self.parity_shards,
                self.backends.len()
            );
        }
        if !(1..=64).contains(&self.transfers) {
            bail!("transfers deve essere fra 1 e 64");
        }
        let mut names = HashSet::new();
        for b in &self.backends {
            if b.name.is_empty() {
                bail!("ogni backend deve avere un nome");
            }
            if !names.insert(b.name.as_str()) {
                bail!("nome di backend ripetuto: {}", b.name);
            }
            match &b.kind {
                BackendKind::Webdav { url, .. } if !(url.starts_with("https://") || url.starts_with("http://")) => {
                    bail!("backend {}: l'indirizzo WebDAV deve cominciare con https:// o http://", b.name)
                }
                BackendKind::S3 { endpoint, bucket, .. } => {
                    if !(endpoint.starts_with("https://") || endpoint.starts_with("http://")) {
                        bail!("backend {}: l'endpoint S3 deve cominciare con https:// o http://", b.name);
                    }
                    if bucket.is_empty() {
                        bail!("backend {}: bucket vuoto", b.name);
                    }
                }
                _ => {}
            }
        }
        let c = self.chunk;
        if !(MINIMUM_MIN..=MINIMUM_MAX).contains(&c.min)
            || !(AVERAGE_MIN..=AVERAGE_MAX).contains(&c.avg)
            || !(MAXIMUM_MIN..=MAXIMUM_MAX).contains(&c.max)
            || !(c.min <= c.avg && c.avg <= c.max)
        {
            bail!(
                "dimensioni dei blocchi non valide: serve min <= avg <= max, con min {MINIMUM_MIN}-{MINIMUM_MAX}, \
                 avg {AVERAGE_MIN}-{AVERAGE_MAX}, max {MAXIMUM_MIN}-{MAXIMUM_MAX}"
            );
        }
        Ok(())
    }
}
