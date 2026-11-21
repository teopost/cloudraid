//! Cifratura. Una chiave principale casuale, protetta dalla password con Argon2id e
//! salvata (cifrata) su ogni backend. Da questa derivano la chiave dei dati e quella
//! degli identificativi dei blocchi, così gli id non rivelano il contenuto.

use anyhow::{anyhow, bail, Result};
use argon2::{Algorithm, Argon2, Params, Version};
use chacha20poly1305::aead::rand_core::RngCore;
use chacha20poly1305::aead::{Aead, AeadCore, KeyInit, OsRng};
use chacha20poly1305::{Key, XChaCha20Poly1305, XNonce};
use serde::{Deserialize, Serialize};

const NONCE_LEN: usize = 24;
const TAG_LEN: usize = 16;

/// Il file `cloudraid.key` presente su ogni backend.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct KeyFile {
    pub version: u32,
    pub kdf: String,
    pub m_cost: u32,
    pub t_cost: u32,
    pub p_cost: u32,
    pub salt: String,
    /// Chiave principale cifrata con la chiave derivata dalla password.
    pub wrapped: String,
}

pub struct Keys {
    data: [u8; 32],
    id: [u8; 32],
    /// Chiave delle credenziali locali (password e token degli account), mai usata sui backend.
    secrets: [u8; 32],
}

impl Keys {
    fn from_master(master: &[u8; 32]) -> Keys {
        Keys {
            data: blake3::derive_key("cloudraid v1 data encryption", master),
            id: blake3::derive_key("cloudraid v1 chunk id", master),
            secrets: blake3::derive_key("cloudraid v1 local credentials", master),
        }
    }

    /// Chiavi casuali usa e getta (per strutture che ne chiedono una ma non cifreranno niente).
    pub fn ephemeral() -> Keys {
        let mut master = [0u8; 32];
        OsRng.fill_bytes(&mut master);
        Keys::from_master(&master)
    }

    /// Cifra con XChaCha20-Poly1305: il risultato è nonce || testo cifrato || tag.
    pub fn seal(&self, plain: &[u8]) -> Vec<u8> {
        seal(&self.data, plain)
    }

    pub fn open(&self, sealed: &[u8]) -> Result<Vec<u8>> {
        open(&self.data, sealed)
    }

    /// Cifra le credenziali locali (chiave separata da quella dei dati).
    pub fn seal_secret(&self, plain: &[u8]) -> Vec<u8> {
        seal(&self.secrets, plain)
    }

    pub fn open_secret(&self, sealed: &[u8]) -> Result<Vec<u8>> {
        open(&self.secrets, sealed)
    }

    /// Identificativo di un blocco: hash con chiave del contenuto in chiaro.
    pub fn chunk_id(&self, plain: &[u8]) -> String {
        blake3::keyed_hash(&self.id, plain).to_hex().to_string()
    }
}

fn seal(key: &[u8; 32], plain: &[u8]) -> Vec<u8> {
    let cipher = XChaCha20Poly1305::new(Key::from_slice(key));
    let nonce = XChaCha20Poly1305::generate_nonce(&mut OsRng);
    let ct = cipher.encrypt(&nonce, plain).expect("la cifratura in memoria non può fallire");
    let mut out = Vec::with_capacity(NONCE_LEN + ct.len());
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&ct);
    out
}

fn open(key: &[u8; 32], sealed: &[u8]) -> Result<Vec<u8>> {
    if sealed.len() < NONCE_LEN + TAG_LEN {
        bail!("dati cifrati troppo corti");
    }
    let (nonce, ct) = sealed.split_at(NONCE_LEN);
    XChaCha20Poly1305::new(Key::from_slice(key))
        .decrypt(XNonce::from_slice(nonce), ct)
        .map_err(|_| anyhow!("decifratura fallita: dati danneggiati o chiave sbagliata"))
}

fn kdf(password: &str, salt: &[u8], m: u32, t: u32, p: u32) -> Result<[u8; 32]> {
    let params = Params::new(m, t, p, Some(32)).map_err(|e| anyhow!("argon2: {e}"))?;
    let mut out = [0u8; 32];
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
        .hash_password_into(password.as_bytes(), salt, &mut out)
        .map_err(|e| anyhow!("argon2: {e}"))?;
    Ok(out)
}

/// Crea una chiave principale nuova protetta da `password`.
pub fn create(password: &str) -> Result<(KeyFile, Keys)> {
    let mut master = [0u8; 32];
    OsRng.fill_bytes(&mut master);
    let mut salt = [0u8; 16];
    OsRng.fill_bytes(&mut salt);
    let (m, t, p) = (Params::DEFAULT_M_COST, Params::DEFAULT_T_COST, Params::DEFAULT_P_COST);
    let kek = kdf(password, &salt, m, t, p)?;
    let kf = KeyFile {
        version: 1,
        kdf: "argon2id".into(),
        m_cost: m,
        t_cost: t,
        p_cost: p,
        salt: hex::encode(salt),
        wrapped: hex::encode(seal(&kek, &master)),
    };
    Ok((kf, Keys::from_master(&master)))
}

/// Apre la chiave principale con la password.
pub fn unlock(kf: &KeyFile, password: &str) -> Result<Keys> {
    if kf.version != 1 || kf.kdf != "argon2id" {
        bail!("formato della chiave non supportato (versione {}, {})", kf.version, kf.kdf);
    }
    let salt = hex::decode(&kf.salt).map_err(|_| anyhow!("file della chiave danneggiato"))?;
    let wrapped = hex::decode(&kf.wrapped).map_err(|_| anyhow!("file della chiave danneggiato"))?;
    let kek = kdf(password, &salt, kf.m_cost, kf.t_cost, kf.p_cost)?;
    let master = open(&kek, &wrapped).map_err(|_| anyhow!("password errata"))?;
    let master: [u8; 32] = master.try_into().map_err(|_| anyhow!("file della chiave danneggiato"))?;
    Ok(Keys::from_master(&master))
}

/// Stringa esadecimale casuale di `bytes` byte.
pub fn random_hex(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    OsRng.fill_bytes(&mut buf);
    hex::encode(buf)
}
