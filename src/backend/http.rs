//! Strumenti HTTP comuni ai backend di rete: un client con TLS (rustls, certificati di Mozilla
//! inclusi nel binario), i nuovi tentativi con attesa crescente che rispettano `Retry-After`, la
//! lettura delle risposte e la codifica degli indirizzi.

use anyhow::{anyhow, Result};
use std::io::Read;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

/// Tentativi per ogni operazione (il primo più i nuovi tentativi).
const ATTEMPTS: u32 = 5;
/// Dimensione massima di una risposta letta in memoria (frammenti e indice stanno ben sotto).
const MAX_BODY: u64 = 1 << 30;

/// Il client condiviso: tiene aperte le connessioni fra una richiesta e l'altra.
pub fn agent() -> &'static ureq::Agent {
    static AGENT: OnceLock<ureq::Agent> = OnceLock::new();
    AGENT.get_or_init(|| {
        ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(20))
            .timeout_read(Duration::from_secs(300))
            .timeout_write(Duration::from_secs(300))
            .user_agent(concat!("cloudraid/", env!("CARGO_PKG_VERSION")))
            .build()
    })
}

/// Lo stesso client, ma senza seguire i reindirizzamenti (per gestirli a mano, senza mandare il
/// token a un altro sito).
pub fn agent_no_redirect() -> &'static ureq::Agent {
    static AGENT: OnceLock<ureq::Agent> = OnceLock::new();
    AGENT.get_or_init(|| {
        ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(20))
            .timeout_read(Duration::from_secs(300))
            .timeout_write(Duration::from_secs(300))
            .redirects(0)
            .user_agent(concat!("cloudraid/", env!("CARGO_PKG_VERSION")))
            .build()
    })
}

/// L'esito di una richiesta: risposta (anche 4xx/5xx, da interpretare) o errore di trasporto.
// vive solo il tempo di un `match`: la differenza di dimensione fra le varianti non conta
#[allow(clippy::large_enum_variant)]
pub enum Outcome {
    Response(ureq::Response),
    Transport(anyhow::Error),
}

/// Manda una richiesta senza trasformare i codici 4xx/5xx in errori.
pub fn send(req: ureq::Request, body: Option<&[u8]>) -> Outcome {
    let res = match body {
        Some(b) => req.send_bytes(b),
        None => req.call(),
    };
    match res {
        Ok(r) => Outcome::Response(r),
        Err(ureq::Error::Status(_, r)) => Outcome::Response(r),
        Err(ureq::Error::Transport(t)) => Outcome::Transport(anyhow!("{t}")),
    }
}

pub fn send_form(req: ureq::Request, form: &[(&str, &str)]) -> Outcome {
    match req.send_form(form) {
        Ok(r) => Outcome::Response(r),
        Err(ureq::Error::Status(_, r)) => Outcome::Response(r),
        Err(ureq::Error::Transport(t)) => Outcome::Transport(anyhow!("{t}")),
    }
}

/// Legge tutto il corpo di una risposta.
pub fn body(resp: ureq::Response) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    resp.into_reader().take(MAX_BODY).read_to_end(&mut out)?;
    Ok(out)
}

/// Il corpo come testo, accorciato per i messaggi di errore.
pub fn body_text(resp: ureq::Response) -> String {
    let data = body(resp).unwrap_or_default();
    let text = String::from_utf8_lossy(&data);
    let text = text.trim();
    if text.chars().count() > 300 {
        format!("{}...", text.chars().take(300).collect::<String>())
    } else {
        text.to_string()
    }
}

/// Che cosa fare dopo un tentativo.
pub enum Step<T> {
    Done(T),
    /// Errore temporaneo: si riprova (eventualmente dopo l'attesa indicata dal server).
    Retry(anyhow::Error, Option<Duration>),
    Fail(anyhow::Error),
}

/// Il tempo di attesa indicato dal server, se c'è.
pub fn retry_after(resp: &ureq::Response) -> Option<Duration> {
    resp.header("Retry-After").and_then(|v| v.trim().parse::<u64>().ok()).map(|s| Duration::from_secs(s.min(300)))
}

/// I codici che vale la pena ritentare: troppe richieste e guasti temporanei del server.
pub fn transient(status: u16) -> bool {
    status == 408 || status == 429 || (500..600).contains(&status)
}

/// Esegue `f` fino a `ATTEMPTS` volte, aspettando 1, 2, 4, 8 secondi (o quanto chiede il server).
pub fn retry<T>(what: &str, mut f: impl FnMut() -> Step<T>) -> Result<T> {
    let mut last = anyhow!("nessun tentativo");
    for attempt in 0..ATTEMPTS {
        match f() {
            Step::Done(v) => return Ok(v),
            Step::Fail(e) => return Err(e.context(what.to_string())),
            Step::Retry(e, wait) => {
                last = e;
                if attempt + 1 < ATTEMPTS {
                    let backoff = Duration::from_secs(1 << attempt.min(4));
                    std::thread::sleep(wait.unwrap_or(backoff).max(Duration::from_millis(200)));
                }
            }
        }
    }
    Err(last.context(format!("{what}: ancora in errore dopo {ATTEMPTS} tentativi")))
}

/// Interpreta un errore di trasporto o una risposta con un codice non previsto.
pub fn unexpected<T>(resp: ureq::Response) -> Step<T> {
    let status = resp.status();
    let wait = retry_after(&resp);
    let msg = anyhow!("risposta {status}: {}", body_text(resp));
    if transient(status) {
        Step::Retry(msg, wait)
    } else {
        Step::Fail(msg)
    }
}

/// Codifica per un segmento di percorso: tutto tranne i caratteri non riservati (RFC 3986).
pub fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Codifica un percorso segmento per segmento, lasciando le `/`.
pub fn encode_path(path: &str) -> String {
    path.split('/').map(encode).collect::<Vec<_>>().join("/")
}

/// Decodifica `%XX` (e lascia il resto com'è).
pub fn decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Some(v) = std::str::from_utf8(&bytes[i + 1..i + 3]).ok().and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Disponibilità di un backend di rete, verificata una volta e poi tenuta in memoria finché
/// `reset` (il "Ricontrolla" dell'interfaccia) non la fa verificare di nuovo.
#[derive(Default)]
pub struct Availability(Mutex<Option<bool>>);

impl Availability {
    pub fn get(&self, check: impl FnOnce() -> Result<bool>, name: &str) -> bool {
        let mut cached = self.0.lock().unwrap();
        if let Some(v) = *cached {
            return v;
        }
        let v = match check() {
            Ok(v) => v,
            Err(e) => {
                eprintln!("attenzione: backend {name} non raggiungibile: {e:#}");
                false
            }
        };
        *cached = Some(v);
        v
    }

    pub fn set(&self, v: bool) {
        *self.0.lock().unwrap() = Some(v);
    }

    pub fn reset(&self) {
        *self.0.lock().unwrap() = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codifica_e_decodifica() {
        assert_eq!(encode("a b/c~é"), "a%20b%2Fc~%C3%A9");
        assert_eq!(encode_path("data/ab/x y"), "data/ab/x%20y");
        assert_eq!(decode("/dav/a%20b/%C3%A9%"), "/dav/a b/é%");
        assert_eq!(decode("%4"), "%4");
    }
}
