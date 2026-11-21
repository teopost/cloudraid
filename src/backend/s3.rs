//! Archivi compatibili S3 (AWS, Backblaze B2, Wasabi, MinIO...): PutObject, GetObject,
//! DeleteObject e ListObjectsV2, firmati con AWS Signature Version 4. La chiave segreta sta
//! nelle credenziali cifrate.

use anyhow::{anyhow, Result};
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use super::http::{self, Availability, Outcome, Step};
use super::Backend;
use crate::credentials::Credentials;
use crate::xml;

pub struct S3Params {
    pub endpoint: String,
    pub region: String,
    pub bucket: String,
    pub access_key: String,
    pub prefix: String,
    pub path_style: bool,
}

pub struct S3 {
    name: String,
    p: S3Params,
    creds: Arc<Credentials>,
    avail: Availability,
}

const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

fn sha256_hex(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

fn hmac(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut m = Hmac::<Sha256>::new_from_slice(key).expect("HMAC accetta chiavi di ogni lunghezza");
    m.update(data);
    m.finalize().into_bytes().to_vec()
}

/// Data e ora in UTC: (`YYYYMMDDTHHMMSSZ`, `YYYYMMDD`).
pub fn amz_dates(t: SystemTime) -> (String, String) {
    let secs = t.duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0) as i64;
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // da giorni dal 1970 a data civile (algoritmo di Howard Hinnant)
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    let date = format!("{y:04}{m:02}{d:02}");
    (format!("{date}T{:02}{:02}{:02}Z", rem / 3600, rem % 3600 / 60, rem % 60), date)
}

/// La firma SigV4 di una richiesta: restituisce l'intestazione `Authorization`.
/// `headers` (nomi minuscoli) deve contenere `host`, `x-amz-date` e `x-amz-content-sha256`.
#[allow(clippy::too_many_arguments)]
pub fn sign(
    method: &str,
    canonical_uri: &str,
    query: &[(String, String)],
    headers: &[(String, String)],
    payload_hash: &str,
    access_key: &str,
    secret_key: &str,
    region: &str,
    amz_date: &str,
) -> String {
    let date = &amz_date[..8];
    let mut q: Vec<(String, String)> = query.iter().map(|(k, v)| (http::encode(k), http::encode(v))).collect();
    q.sort();
    let canonical_query = q.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("&");
    let mut h: Vec<(String, String)> = headers.iter().map(|(k, v)| (k.to_lowercase(), v.trim().to_string())).collect();
    h.sort();
    let canonical_headers: String = h.iter().map(|(k, v)| format!("{k}:{v}\n")).collect();
    let signed_headers = h.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>().join(";");
    let canonical_request =
        format!("{method}\n{canonical_uri}\n{canonical_query}\n{canonical_headers}\n{signed_headers}\n{payload_hash}");
    let scope = format!("{date}/{region}/s3/aws4_request");
    let string_to_sign = format!("AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}", sha256_hex(canonical_request.as_bytes()));
    let k_date = hmac(format!("AWS4{secret_key}").as_bytes(), date.as_bytes());
    let k_region = hmac(&k_date, region.as_bytes());
    let k_service = hmac(&k_region, b"s3");
    let k_signing = hmac(&k_service, b"aws4_request");
    let signature = hex::encode(hmac(&k_signing, string_to_sign.as_bytes()));
    format!("AWS4-HMAC-SHA256 Credential={access_key}/{scope}, SignedHeaders={signed_headers}, Signature={signature}")
}

impl S3 {
    pub fn new(name: String, mut p: S3Params, creds: Arc<Credentials>) -> S3 {
        p.endpoint = p.endpoint.trim_end_matches('/').to_string();
        p.prefix = p.prefix.trim_matches('/').to_string();
        S3 { name, p, creds, avail: Availability::default() }
    }

    fn object(&self, key: &str) -> String {
        if self.p.prefix.is_empty() {
            key.to_string()
        } else if key.is_empty() {
            self.p.prefix.clone()
        } else {
            format!("{}/{key}", self.p.prefix)
        }
    }

    /// Indirizzo, host e percorso canonico per un oggetto (vuoto: il bucket).
    fn target(&self, object: &str) -> (String, String, String) {
        let (scheme, host) = self.p.endpoint.split_once("://").unwrap_or(("https", &self.p.endpoint));
        let obj = http::encode_path(object);
        if self.p.path_style {
            let uri = if object.is_empty() { format!("/{}", self.p.bucket) } else { format!("/{}/{obj}", self.p.bucket) };
            (format!("{scheme}://{host}{uri}"), host.to_string(), uri)
        } else {
            let host = format!("{}.{host}", self.p.bucket);
            let uri = format!("/{obj}");
            (format!("{scheme}://{host}{uri}"), host, uri)
        }
    }

    /// Una richiesta firmata (il corpo non è ancora allegato).
    fn request(&self, method: &str, object: &str, query: &[(&str, &str)], body: &[u8]) -> Result<ureq::Request> {
        let secret = self
            .creds
            .get(&self.name, "secret_key")
            .ok_or_else(|| anyhow!("backend {}: manca la chiave segreta S3 nelle credenziali", self.name))?;
        let (url, host, uri) = self.target(object);
        let (amz_date, _) = amz_dates(SystemTime::now());
        let payload = if body.is_empty() { EMPTY_SHA256.to_string() } else { sha256_hex(body) };
        let headers = vec![
            ("host".to_string(), host),
            ("x-amz-content-sha256".to_string(), payload.clone()),
            ("x-amz-date".to_string(), amz_date.clone()),
        ];
        let q: Vec<(String, String)> = query.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        let auth = sign(method, &uri, &q, &headers, &payload, &self.p.access_key, &secret, &self.p.region, &amz_date);
        // la query va sul filo codificata esattamente come nella firma
        let mut qs: Vec<String> = q.iter().map(|(k, v)| format!("{}={}", http::encode(k), http::encode(v))).collect();
        qs.sort();
        let url = if qs.is_empty() { url } else { format!("{url}?{}", qs.join("&")) };
        Ok(http::agent()
            .request(method, &url)
            .set("x-amz-date", &amz_date)
            .set("x-amz-content-sha256", &payload)
            .set("Authorization", &auth))
    }

    /// Un errore S3 leggibile (il codice XML, per esempio `AccessDenied` o `NoSuchBucket`).
    fn failure<T>(&self, r: ureq::Response) -> Step<T> {
        let status = r.status();
        let wait = http::retry_after(&r);
        let text = http::body(r).map(|b| String::from_utf8_lossy(&b).into_owned()).unwrap_or_default();
        let code = xml::text(&text, "Code").unwrap_or_default();
        let message = xml::text(&text, "Message").unwrap_or_default();
        let err = anyhow!("risposta {status} {code}: {message}");
        if http::transient(status) || code == "SlowDown" || code == "RequestTimeTooSkewed" {
            Step::Retry(err, wait)
        } else {
            Step::Fail(err)
        }
    }

    /// ListObjectsV2: (chiave relativa al prefisso, dimensione). Con `delimiter` solo il primo livello.
    fn list_objects(&self, dir: &str, delimiter: bool) -> Result<Vec<(String, u64)>> {
        let prefix = format!("{}/", self.object(dir));
        let strip = if self.p.prefix.is_empty() { String::new() } else { format!("{}/", self.p.prefix) };
        let mut out = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let text = http::retry(&format!("backend {}: elenco di {dir}", self.name), || {
                let mut q: Vec<(&str, &str)> = vec![("list-type", "2"), ("prefix", &prefix)];
                if delimiter {
                    q.push(("delimiter", "/"));
                }
                if let Some(t) = &token {
                    q.push(("continuation-token", t));
                }
                let req = match self.request("GET", "", &q, &[]) {
                    Ok(r) => r,
                    Err(e) => return Step::Fail(e),
                };
                match http::send(req, None) {
                    Outcome::Response(r) if r.status() == 200 => match http::body(r) {
                        Ok(b) => Step::Done(String::from_utf8_lossy(&b).into_owned()),
                        Err(e) => Step::Retry(e, None),
                    },
                    Outcome::Response(r) => self.failure(r),
                    Outcome::Transport(e) => Step::Retry(e, None),
                }
            })?;
            for c in xml::elements(&text, "Contents") {
                let Some(key) = xml::text(c, "Key") else { continue };
                let size = xml::text(c, "Size").and_then(|s| s.parse().ok()).unwrap_or(0);
                if let Some(rel) = key.strip_prefix(&strip) {
                    out.push((rel.to_string(), size));
                }
            }
            if xml::text(&text, "IsTruncated").as_deref() != Some("true") {
                break;
            }
            token = xml::text(&text, "NextContinuationToken");
            if token.is_none() {
                break;
            }
        }
        Ok(out)
    }
}

impl Backend for S3 {
    fn name(&self) -> &str {
        &self.name
    }

    fn location(&self) -> String {
        let prefix = if self.p.prefix.is_empty() { String::new() } else { format!("/{}", self.p.prefix) };
        format!("{} bucket {}{prefix}", self.p.endpoint, self.p.bucket)
    }

    fn is_available(&self) -> bool {
        self.avail.get(
            || {
                http::retry(&format!("backend {}: verifica del bucket", self.name), || {
                    let req = match self.request("GET", "", &[("list-type", "2"), ("max-keys", "1")], &[]) {
                        Ok(r) => r,
                        Err(e) => return Step::Fail(e),
                    };
                    match http::send(req, None) {
                        Outcome::Response(r) if r.status() == 200 => Step::Done(true),
                        Outcome::Response(r) if r.status() == 404 => Step::Done(false),
                        Outcome::Response(r) => self.failure(r),
                        Outcome::Transport(e) => Step::Retry(e, None),
                    }
                })
            },
            &self.name,
        )
    }

    fn create(&self) -> Result<()> {
        // crea il bucket se manca (dove le chiavi lo permettono); se esiste già va bene
        http::retry(&format!("backend {}: creazione del bucket", self.name), || {
            let req = match self.request("PUT", "", &[], &[]) {
                Ok(r) => r,
                Err(e) => return Step::Fail(e),
            };
            match http::send(req, None) {
                Outcome::Response(r) if r.status() == 200 => Step::Done(()),
                Outcome::Response(r) if r.status() == 409 => Step::Done(()), // BucketAlreadyOwnedByYou
                Outcome::Response(r) if r.status() == 403 => {
                    // niente permesso di creare bucket: basta che quello indicato esista
                    let _ = http::body(r);
                    Step::Done(())
                }
                Outcome::Response(r) => self.failure(r),
                Outcome::Transport(e) => Step::Retry(e, None),
            }
        })?;
        self.avail.reset();
        if !self.is_available() {
            anyhow::bail!("il bucket {} non esiste o le chiavi non permettono di usarlo", self.p.bucket);
        }
        Ok(())
    }

    fn put(&self, key: &str, data: &[u8]) -> Result<()> {
        super::check_key(key)?;
        let object = self.object(key);
        http::retry(&format!("backend {}: scrittura di {key}", self.name), || {
            let req = match self.request("PUT", &object, &[], data) {
                Ok(r) => r.set("Content-Type", "application/octet-stream"),
                Err(e) => return Step::Fail(e),
            };
            match http::send(req, Some(data)) {
                Outcome::Response(r) if r.status() == 200 => Step::Done(()),
                Outcome::Response(r) => self.failure(r),
                Outcome::Transport(e) => Step::Retry(e, None),
            }
        })
    }

    fn get(&self, key: &str) -> Result<Option<Vec<u8>>> {
        super::check_key(key)?;
        let object = self.object(key);
        http::retry(&format!("backend {}: lettura di {key}", self.name), || {
            let req = match self.request("GET", &object, &[], &[]) {
                Ok(r) => r,
                Err(e) => return Step::Fail(e),
            };
            match http::send(req, None) {
                Outcome::Response(r) if r.status() == 200 => match http::body(r) {
                    Ok(b) => Step::Done(Some(b)),
                    Err(e) => Step::Retry(e, None),
                },
                Outcome::Response(r) if r.status() == 404 => Step::Done(None),
                Outcome::Response(r) => self.failure(r),
                Outcome::Transport(e) => Step::Retry(e, None),
            }
        })
    }

    fn delete(&self, key: &str) -> Result<()> {
        super::check_key(key)?;
        let object = self.object(key);
        http::retry(&format!("backend {}: cancellazione di {key}", self.name), || {
            let req = match self.request("DELETE", &object, &[], &[]) {
                Ok(r) => r,
                Err(e) => return Step::Fail(e),
            };
            match http::send(req, None) {
                Outcome::Response(r) if (200..300).contains(&r.status()) || r.status() == 404 => Step::Done(()),
                Outcome::Response(r) => self.failure(r),
                Outcome::Transport(e) => Step::Retry(e, None),
            }
        })
    }

    fn list(&self, dir: &str) -> Result<Vec<String>> {
        super::check_key(dir)?;
        Ok(self.list_objects(dir, true)?.into_iter().map(|(k, _)| k).collect())
    }

    fn list_all(&self, dir: &str) -> Result<Vec<(String, u64)>> {
        super::check_key(dir)?;
        self.list_objects(dir, false)
    }

    fn kind(&self) -> &'static str {
        "s3"
    }

    fn refresh(&self) {
        self.avail.reset();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// L'esempio "GET Object" della documentazione AWS (Signature Version 4, Authorization header).
    #[test]
    fn firma_come_nella_documentazione_aws() {
        let headers = vec![
            ("Host".to_string(), "examplebucket.s3.amazonaws.com".to_string()),
            ("Range".to_string(), "bytes=0-9".to_string()),
            ("x-amz-content-sha256".to_string(), EMPTY_SHA256.to_string()),
            ("x-amz-date".to_string(), "20130524T000000Z".to_string()),
        ];
        let auth = sign(
            "GET",
            "/test.txt",
            &[],
            &headers,
            EMPTY_SHA256,
            "AKIAIOSFODNN7EXAMPLE",
            "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
            "us-east-1",
            "20130524T000000Z",
        );
        assert_eq!(
            auth,
            "AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/20130524/us-east-1/s3/aws4_request, \
             SignedHeaders=host;range;x-amz-content-sha256;x-amz-date, \
             Signature=f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41"
        );
    }

    #[test]
    fn date_utc() {
        assert_eq!(amz_dates(UNIX_EPOCH).0, "19700101T000000Z");
        assert_eq!(amz_dates(UNIX_EPOCH + std::time::Duration::from_secs(1_369_353_600)).0, "20130524T000000Z");
        assert_eq!(amz_dates(UNIX_EPOCH + std::time::Duration::from_secs(1_709_251_199)).0, "20240229T235959Z");
    }
}
