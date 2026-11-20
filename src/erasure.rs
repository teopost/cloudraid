//! Erasure coding Reed-Solomon: un blocco diventa k frammenti di dati + m di parità,
//! e bastano k frammenti qualsiasi per ricostruirlo (come un RAID 5 con m = 1, RAID 6 con m = 2).

use anyhow::{anyhow, bail, Result};

/// Divide `data` in `k` frammenti di dati della stessa lunghezza (con padding finale)
/// e calcola `m` frammenti di parità. Restituisce la lunghezza dei frammenti e i k + m frammenti.
pub fn encode(data: &[u8], k: usize, m: usize) -> Result<(usize, Vec<Vec<u8>>)> {
    if k == 0 {
        bail!("servono almeno un frammento di dati");
    }
    // reed-solomon-simd vuole frammenti di lunghezza pari e non nulla
    let mut shard_len = data.len().div_ceil(k).max(2);
    shard_len += shard_len % 2;
    let mut shards: Vec<Vec<u8>> = (0..k)
        .map(|i| {
            let start = (i * shard_len).min(data.len());
            let end = ((i + 1) * shard_len).min(data.len());
            let mut shard = data[start..end].to_vec();
            shard.resize(shard_len, 0);
            shard
        })
        .collect();
    if m > 0 {
        let parity = reed_solomon_simd::encode(k, m, &shards).map_err(|e| anyhow!("Reed-Solomon: {e}"))?;
        shards.extend(parity);
    }
    Ok((shard_len, shards))
}

/// Ricostruisce i dati originali (lunghezza `len`) da almeno `k` frammenti presenti.
/// `shards[i]` è `None` per i frammenti mancanti o danneggiati.
pub fn decode(k: usize, m: usize, len: usize, mut shards: Vec<Option<Vec<u8>>>) -> Result<Vec<u8>> {
    if shards.len() != k + m {
        bail!("numero di frammenti errato: {} invece di {}", shards.len(), k + m);
    }
    let present = shards.iter().filter(|s| s.is_some()).count();
    if present < k {
        bail!("frammenti insufficienti: {present} su {k} necessari");
    }
    if shards[..k].iter().any(Option::is_none) {
        let originals = shards[..k].iter().enumerate().filter_map(|(i, s)| s.as_ref().map(|s| (i, s)));
        let recovery = shards[k..].iter().enumerate().filter_map(|(i, s)| s.as_ref().map(|s| (i, s)));
        let restored = reed_solomon_simd::decode(k, m, originals, recovery).map_err(|e| anyhow!("Reed-Solomon: {e}"))?;
        for (i, shard) in restored {
            shards[i] = Some(shard);
        }
    }
    let mut out = Vec::with_capacity(len);
    for shard in shards.into_iter().take(k) {
        out.extend_from_slice(&shard.ok_or_else(|| anyhow!("frammento non ricostruito"))?);
    }
    if out.len() < len {
        bail!("dati ricostruiti troppo corti");
    }
    out.truncate(len);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ogni_combinazione_di_perdite_entro_m() {
        let data: Vec<u8> = (0..10_001u32).map(|i| (i * 31 % 251) as u8).collect();
        let (k, m) = (4, 2);
        let (_, shards) = encode(&data, k, m).unwrap();
        for a in 0..k + m {
            for b in a..k + m {
                let mut s: Vec<Option<Vec<u8>>> = shards.iter().cloned().map(Some).collect();
                s[a] = None;
                s[b] = None;
                assert_eq!(decode(k, m, data.len(), s).unwrap(), data, "persi {a} e {b}");
            }
        }
    }

    #[test]
    fn troppe_perdite() {
        let (_, shards) = encode(b"ciao", 2, 1).unwrap();
        let s = vec![Some(shards[0].clone()), None, None];
        assert!(decode(2, 1, 4, s).is_err());
    }

    #[test]
    fn senza_parita_e_dati_minimi() {
        for data in [&b""[..], b"x", b"xy", b"xyz"] {
            let (_, shards) = encode(data, 3, 0).unwrap();
            assert_eq!(decode(3, 0, data.len(), shards.into_iter().map(Some).collect()).unwrap(), data);
        }
    }
}
