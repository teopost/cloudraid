//! OAuth 2 per Dropbox, OneDrive e Google Drive.
//!
//! - Uso: il backend tiene in memoria il token d'accesso (dura un'ora circa) e lo rinnova col
//!   refresh token, che sta nelle credenziali cifrate. Se il provider ne manda uno nuovo (Microsoft
//!   lo fa a ogni rinnovo) viene salvato.
//! - Collegamento di un account, pensato per un server senza browser:
//!   - Google e Microsoft: "codice dispositivo". cloudraid mostra un indirizzo e un codice breve;
//!     la persona li usa da qualsiasi browser e cloudraid riceve il token da solo.
//!   - Dropbox: Dropbox mostra un codice dopo il consenso, da incollare in cloudraid (PKCE, senza
//!     segreti).

use anyhow::{anyhow, bail, Result};
use base64::Engine;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::http::{self, Outcome, Step};
use crate::config::AppsConfig;
use crate::credentials::Credentials;
use crate::crypto::random_hex;

/// I tre provider con OAuth.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Provider {
    Dropbox,
    OneDrive,
    Google,
}

impl Provider {
    pub fn parse(s: &str) -> Option<Provider> {
        match s {
            "dropbox" => Some(Provider::Dropbox),
            "onedrive" => Some(Provider::OneDrive),
            "gdrive" => Some(Provider::Google),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Provider::Dropbox => "Dropbox",
            Provider::OneDrive => "OneDrive",
            Provider::Google => "Google Drive",
        }
    }

    /// Dove si chiede il token (la base `auth_base` sostituisce il dominio nelle prove).
    pub fn token_url(self, auth_base: Option<&str>) -> String {
        match (self, auth_base) {
            (Provider::Dropbox, Some(b)) | (Provider::OneDrive, Some(b)) | (Provider::Google, Some(b)) => {
                format!("{}/oauth/token", b.trim_end_matches('/'))
            }
            (Provider::Dropbox, None) => "https://api.dropboxapi.com/oauth2/token".into(),
            (Provider::OneDrive, None) => "https://login.microsoftonline.com/common/oauth2/v2.0/token".into(),
            (Provider::Google, None) => "https://oauth2.googleapis.com/token".into(),
        }
    }

    fn device_url(self, auth_base: Option<&str>) -> Option<String> {
        match (self, auth_base) {
            (Provider::Dropbox, _) => None,
            (_, Some(b)) => Some(format!("{}/oauth/device", b.trim_end_matches('/'))),
            (Provider::OneDrive, None) => Some("https://login.microsoftonline.com/common/oauth2/v2.0/devicecode".into()),
            (Provider::Google, None) => Some("https://oauth2.googleapis.com/device/code".into()),
        }
    }

    pub fn scope(self) -> &'static str {
        match self {
            Provider::Dropbox => "",
            Provider::OneDrive => "Files.ReadWrite.AppFolder offline_access",
            Provider::Google => "https://www.googleapis.com/auth/drive.file",
        }
    }

    /// Identificativo (e segreto, solo Google) dell'app registrata.
    pub fn client(self, apps: &AppsConfig) -> Result<(String, Option<String>)> {
        let env = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
        let missing = || {
            anyhow!(
                "per {} serve registrare l'app presso il provider (una volta sola): vedi Impostazioni nell'interfaccia",
                self.label()
            )
        };
        match self {
            Provider::Dropbox => {
                let id = env("CLOUDRAID_DROPBOX_APP_KEY").or(apps.dropbox_app_key.clone()).ok_or_else(missing)?;
                Ok((id, None))
            }
            Provider::OneDrive => {
                let id = env("CLOUDRAID_ONEDRIVE_CLIENT_ID").or(apps.onedrive_client_id.clone()).ok_or_else(missing)?;
                Ok((id, None))
            }
            Provider::Google => {
                let id = env("CLOUDRAID_GOOGLE_CLIENT_ID").or(apps.google_client_id.clone()).ok_or_else(missing)?;
                let secret =
                    env("CLOUDRAID_GOOGLE_CLIENT_SECRET").or(apps.google_client_secret.clone()).ok_or_else(missing)?;
                Ok((id, Some(secret)))
            }
        }
    }
}

/// Una risposta JSON di un endpoint OAuth, con l'errore del provider se c'è.
fn token_call(url: &str, form: &[(&str, &str)]) -> Result<std::result::Result<Value, (String, String)>> {
    http::retry("richiesta del token", || match http::send_form(http::agent().post(url), form) {
        Outcome::Response(r) => {
            let status = r.status();
            let wait = http::retry_after(&r);
            let body = match http::body(r) {
                Ok(b) => b,
                Err(e) => return Step::Retry(e, None),
            };
            let v: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
            if (200..300).contains(&status) {
                return Step::Done(Ok(v));
            }
            if http::transient(status) && status != 429 || status == 429 && v.get("error").is_none() {
                return Step::Retry(anyhow!("risposta {status}"), wait);
            }
            let err = v.get("error").and_then(Value::as_str).unwrap_or("errore").to_string();
            let desc = v
                .get("error_description")
                .and_then(Value::as_str)
                .unwrap_or_else(|| v.get("error_summary").and_then(Value::as_str).unwrap_or(""))
                .to_string();
            Step::Done(Err((err, desc)))
        }
        Outcome::Transport(e) => Step::Retry(e, None),
    })
}

/// I token d'accesso di un backend: rinnovati quando scadono, col refresh token delle credenziali.
pub struct TokenSource {
    pub provider: Provider,
    backend: String,
    client_id: Option<String>,
    client_secret: Option<String>,
    token_url: String,
    creds: Arc<Credentials>,
    cached: Mutex<Option<(String, Instant)>>,
    client_error: Option<String>,
}

impl TokenSource {
    pub fn new(provider: Provider, backend: &str, apps: &AppsConfig, auth_base: Option<&str>, creds: Arc<Credentials>) -> TokenSource {
        let (client_id, client_secret, client_error) = match provider.client(apps) {
            Ok((id, secret)) => (Some(id), secret, None),
            Err(e) => (None, None, Some(format!("{e:#}"))),
        };
        TokenSource {
            provider,
            backend: backend.to_string(),
            client_id,
            client_secret,
            token_url: provider.token_url(auth_base),
            creds,
            cached: Mutex::new(None),
            client_error,
        }
    }

    /// Un token d'accesso valido per almeno un altro minuto.
    pub fn access_token(&self) -> Result<String> {
        let mut cached = self.cached.lock().unwrap();
        if let Some((tok, until)) = cached.as_ref() {
            if Instant::now() + Duration::from_secs(60) < *until {
                return Ok(tok.clone());
            }
        }
        let client_id = self.client_id.as_deref().ok_or_else(|| anyhow!("{}", self.client_error.clone().unwrap_or_default()))?;
        let refresh = self.creds.get(&self.backend, "refresh_token").ok_or_else(|| {
            anyhow!("backend {}: account non collegato (manca il token): ricollegalo dall'interfaccia", self.backend)
        })?;
        let mut form = vec![("grant_type", "refresh_token"), ("refresh_token", refresh.as_str()), ("client_id", client_id)];
        if let Some(s) = &self.client_secret {
            form.push(("client_secret", s));
        }
        if self.provider == Provider::OneDrive {
            form.push(("scope", self.provider.scope()));
        }
        let v = token_call(&self.token_url, &form)?.map_err(|(err, desc)| {
            if err == "invalid_grant" {
                anyhow!("backend {}: l'accesso all'account è stato revocato o è scaduto: ricollegalo dall'interfaccia ({desc})", self.backend)
            } else {
                anyhow!("backend {}: rinnovo del token rifiutato: {err} {desc}", self.backend)
            }
        })?;
        let access = v.get("access_token").and_then(Value::as_str).ok_or_else(|| anyhow!("risposta senza access_token"))?;
        let expires = v.get("expires_in").and_then(Value::as_u64).unwrap_or(3600);
        if let Some(new_refresh) = v.get("refresh_token").and_then(Value::as_str) {
            if new_refresh != refresh {
                if let Err(e) = self.creds.set(&self.backend, "refresh_token", Some(new_refresh)) {
                    eprintln!("attenzione: backend {}: nuovo token non salvato: {e:#}", self.backend);
                }
            }
        }
        *cached = Some((access.to_string(), Instant::now() + Duration::from_secs(expires)));
        Ok(access.to_string())
    }

    /// Il provider ha rifiutato il token (401): alla prossima richiesta se ne prende uno nuovo.
    pub fn invalidate(&self) {
        *self.cached.lock().unwrap() = None;
    }
}

// ---------------------------------------------------------------------------------------------
// Collegamento di un account.

/// Che cosa mostrare alla persona per collegare l'account.
#[derive(Clone, Debug)]
pub enum LinkPrompt {
    /// Aprire `url` e inserire `code` (Google, Microsoft).
    Device { url: String, code: String, expires_in: u64 },
    /// Aprire `url`, acconsentire e incollare il codice che compare (Dropbox).
    PasteCode { url: String },
}

/// Un collegamento in corso: tiene i segreti del flusso sul server, mai nel browser.
pub struct Link {
    pub provider: Provider,
    pub prompt: LinkPrompt,
    client_id: String,
    client_secret: Option<String>,
    token_url: String,
    /// Codice dispositivo (Google, Microsoft) o verificatore PKCE (Dropbox).
    secret: String,
    interval: Duration,
    next_poll: Instant,
    pub started: Instant,
}

/// Esito di un controllo del collegamento.
pub enum LinkState {
    /// La persona non ha ancora finito.
    Pending,
    /// Fatto: il refresh token da salvare.
    Done(String),
}

fn pkce_challenge(verifier: &str) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

impl Link {
    /// Comincia il collegamento. `auth_base` sostituisce i domini dei provider nelle prove.
    pub fn start(provider: Provider, apps: &AppsConfig, auth_base: Option<&str>) -> Result<Link> {
        let (client_id, client_secret) = provider.client(apps)?;
        let token_url = provider.token_url(auth_base);
        match provider.device_url(auth_base) {
            None => {
                // Dropbox: PKCE senza indirizzo di ritorno, il codice viene mostrato da Dropbox
                let verifier = format!("{}{}", random_hex(32), random_hex(16));
                let base = auth_base.map(|b| format!("{}/oauth2/authorize", b.trim_end_matches('/')));
                let url = format!(
                    "{}?client_id={}&response_type=code&token_access_type=offline&code_challenge={}&code_challenge_method=S256",
                    base.as_deref().unwrap_or("https://www.dropbox.com/oauth2/authorize"),
                    http::encode(&client_id),
                    pkce_challenge(&verifier)
                );
                Ok(Link {
                    provider,
                    prompt: LinkPrompt::PasteCode { url },
                    client_id,
                    client_secret,
                    token_url,
                    secret: verifier,
                    interval: Duration::ZERO,
                    next_poll: Instant::now(),
                    started: Instant::now(),
                })
            }
            Some(device_url) => {
                let form = [("client_id", client_id.as_str()), ("scope", provider.scope())];
                let v = token_call(&device_url, &form)?
                    .map_err(|(err, desc)| anyhow!("{}: richiesta del codice rifiutata: {err} {desc}", provider.label()))?;
                let field = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
                let device_code = field("device_code").ok_or_else(|| anyhow!("risposta senza device_code"))?;
                let code = field("user_code").ok_or_else(|| anyhow!("risposta senza user_code"))?;
                let url = field("verification_uri")
                    .or_else(|| field("verification_url"))
                    .ok_or_else(|| anyhow!("risposta senza indirizzo di verifica"))?;
                let interval = Duration::from_secs(v.get("interval").and_then(Value::as_u64).unwrap_or(5).max(1));
                let expires_in = v.get("expires_in").and_then(Value::as_u64).unwrap_or(900);
                Ok(Link {
                    provider,
                    prompt: LinkPrompt::Device { url, code, expires_in },
                    client_id,
                    client_secret,
                    token_url,
                    secret: device_code,
                    interval,
                    next_poll: Instant::now() + interval,
                    started: Instant::now(),
                })
            }
        }
    }

    /// Controlla il collegamento. Per Dropbox serve `pasted`, il codice incollato dalla persona;
    /// per gli altri si interroga il provider (al più una volta per intervallo concesso).
    pub fn check(&mut self, pasted: Option<&str>) -> Result<LinkState> {
        let v = match &self.prompt {
            LinkPrompt::PasteCode { .. } => {
                let code = pasted.map(str::trim).filter(|c| !c.is_empty()).ok_or_else(|| anyhow!("incolla il codice mostrato da Dropbox"))?;
                let form = [
                    ("grant_type", "authorization_code"),
                    ("code", code),
                    ("client_id", self.client_id.as_str()),
                    ("code_verifier", self.secret.as_str()),
                ];
                token_call(&self.token_url, &form)?.map_err(|(err, desc)| {
                    anyhow!("codice non accettato da Dropbox ({err} {desc}): riprova dall'inizio")
                })?
            }
            LinkPrompt::Device { .. } => {
                if Instant::now() < self.next_poll {
                    return Ok(LinkState::Pending);
                }
                let mut form = vec![
                    ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                    ("device_code", self.secret.as_str()),
                    ("client_id", self.client_id.as_str()),
                ];
                if let Some(s) = &self.client_secret {
                    form.push(("client_secret", s));
                }
                match token_call(&self.token_url, &form)? {
                    Ok(v) => v,
                    Err((err, _)) if err == "authorization_pending" => {
                        self.next_poll = Instant::now() + self.interval;
                        return Ok(LinkState::Pending);
                    }
                    Err((err, _)) if err == "slow_down" => {
                        self.interval += Duration::from_secs(5);
                        self.next_poll = Instant::now() + self.interval;
                        return Ok(LinkState::Pending);
                    }
                    Err((err, _)) if err == "expired_token" => bail!("il codice è scaduto: riprova dall'inizio"),
                    Err((err, _)) if err == "access_denied" => bail!("accesso negato sull'account: riprova dall'inizio"),
                    Err((err, desc)) => bail!("{}: {err} {desc}", self.provider.label()),
                }
            }
        };
        let refresh = v
            .get("refresh_token")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("{} non ha dato un token permanente (refresh token)", self.provider.label()))?;
        Ok(LinkState::Done(refresh.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// L'esempio della RFC 7636 (appendice B).
    #[test]
    fn pkce_come_nella_rfc() {
        assert_eq!(pkce_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"), "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
    }
}
