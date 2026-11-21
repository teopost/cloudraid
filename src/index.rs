//! L'indice del repository: quali file esistono, da quali blocchi sono fatti e dove stanno
//! i frammenti di ogni blocco. Viene cifrato e copiato su tutti i backend.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Serialize, Deserialize, Default, Debug, Clone)]
pub struct Index {
    /// Aumenta a ogni salvataggio: all'apertura vince l'indice più recente fra i backend.
    pub generation: u64,
    pub files: BTreeMap<String, FileEntry>,
    pub chunks: BTreeMap<String, ChunkEntry>,
    /// Cartelle create esplicitamente (dal mount): le altre esistono perché contengono file.
    /// Assente negli indici vecchi e omessa quando è vuota, così il formato resta compatibile.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub dirs: BTreeSet<String>,
    /// Backend in dismissione: non ricevono frammenti nuovi e `retire` sposta altrove i loro.
    /// Sta nell'indice perché valga per tutti i processi (web, mount, riga di comando).
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub retired: BTreeSet<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct FileEntry {
    pub size: u64,
    /// BLAKE3 del file intero, controllato a ogni lettura.
    pub hash: String,
    /// Unix time di modifica del file originale, se noto.
    pub mtime: Option<i64>,
    pub chunks: Vec<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ChunkEntry {
    /// Lunghezza del blocco in chiaro.
    pub plain_len: u64,
    /// Lunghezza del blocco cifrato, prima della divisione in frammenti.
    pub sealed_len: u64,
    pub k: u8,
    pub m: u8,
    pub shard_len: u64,
    /// `shards[i]` è il frammento i: 0..k dati, k..k+m parità.
    pub shards: Vec<ShardLoc>,
    /// Quante volte il blocco compare nei file (deduplicazione).
    pub refs: u64,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ShardLoc {
    pub backend: String,
    /// BLAKE3 del frammento: un frammento alterato si tratta come mancante.
    pub hash: String,
}
