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
    #[serde(rename = "backend")]
    pub backends: Vec<BackendConfig>,
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

#[derive(Deserialize, Clone, Debug)]
pub struct BackendConfig {
    pub name: String,
    #[serde(flatten)]
    pub kind: BackendKind,
}

#[derive(Deserialize, Clone, Debug)]
#[serde(tag = "type", rename_all = "lowercase", deny_unknown_fields)]
pub enum BackendKind {
    /// Una cartella locale (o un disco montato).
    Local { path: PathBuf },
}

impl Config {
    pub fn load(path: &Path) -> Result<Config> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("impossibile leggere la configurazione {}", path.display()))?;
        let cfg: Config = toml::from_str(&text)
            .with_context(|| format!("configurazione {} non valida", path.display()))?;
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
        let mut names = HashSet::new();
        for b in &self.backends {
            if b.name.is_empty() {
                bail!("ogni backend deve avere un nome");
            }
            if !names.insert(b.name.as_str()) {
                bail!("nome di backend ripetuto: {}", b.name);
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
