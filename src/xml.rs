//! Un lettore XML minimo, quanto basta per le risposte di WebDAV (PROPFIND) e S3 (ListObjectsV2):
//! trova gli elementi per nome locale, ignorando il prefisso dello spazio dei nomi (`d:`, `D:`,
//! nessuno...), e ne restituisce il contenuto grezzo. Non gestisce elementi con lo stesso nome
//! annidati uno nell'altro, che in queste risposte non capitano.

/// Il contenuto di ogni elemento `<...:name>` (vuoto per `<name/>`), nell'ordine.
pub fn elements<'a>(xml: &'a str, name: &str) -> Vec<&'a str> {
    let mut out = Vec::new();
    let mut pos = 0;
    while let Some(off) = xml[pos..].find('<') {
        let start = pos + off;
        let Some(end_off) = xml[start..].find('>') else { break };
        let end = start + end_off;
        let tag = &xml[start + 1..end];
        pos = end + 1;
        if tag.starts_with('/') || tag.starts_with('?') || tag.starts_with('!') {
            continue;
        }
        let self_closing = tag.ends_with('/');
        let qname = tag.trim_end_matches('/').split_whitespace().next().unwrap_or("");
        let local = qname.rsplit(':').next().unwrap_or(qname);
        if local != name {
            continue;
        }
        if self_closing {
            out.push("");
            continue;
        }
        let close = format!("</{qname}>");
        match xml[pos..].find(&close) {
            Some(c) => {
                out.push(&xml[pos..pos + c]);
                pos += c + close.len();
            }
            None => break,
        }
    }
    out
}

/// Il contenuto del primo elemento con quel nome, come testo (entità decodificate).
pub fn text(xml: &str, name: &str) -> Option<String> {
    elements(xml, name).first().map(|s| unescape(s.trim()))
}

/// C'è almeno un elemento con quel nome?
pub fn has(xml: &str, name: &str) -> bool {
    !elements(xml, name).is_empty()
}

/// Decodifica le entità XML più comuni.
pub fn unescape(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        let Some(semi) = rest.find(';') else { break };
        let entity = &rest[1..semi];
        let decoded = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            e if e.starts_with("#x") => u32::from_str_radix(&e[2..], 16).ok().and_then(char::from_u32),
            e if e.starts_with('#') => e[1..].parse().ok().and_then(char::from_u32),
            _ => None,
        };
        match decoded {
            Some(c) => {
                out.push(c);
                rest = &rest[semi + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn propfind_e_s3() {
        let dav = r#"<?xml version="1.0"?><d:multistatus xmlns:d="DAV:">
            <d:response><d:href>/dav/a%20b/</d:href><d:propstat><d:prop><d:resourcetype><d:collection/></d:resourcetype></d:prop></d:propstat></d:response>
            <d:response><d:href>/dav/a%20b/x.1</d:href><d:propstat><d:prop><d:resourcetype/><d:getcontentlength>42</d:getcontentlength></d:prop></d:propstat></d:response>
            </d:multistatus>"#;
        let resp = elements(dav, "response");
        assert_eq!(resp.len(), 2);
        assert!(has(resp[0], "collection") && !has(resp[1], "collection"));
        assert_eq!(text(resp[1], "href").as_deref(), Some("/dav/a%20b/x.1"));
        assert_eq!(text(resp[1], "getcontentlength").as_deref(), Some("42"));
        let s3 = "<ListBucketResult><IsTruncated>true</IsTruncated><Contents><Key>a&amp;b</Key><Size>3</Size></Contents>\
                  <NextContinuationToken>t/x=</NextContinuationToken></ListBucketResult>";
        assert_eq!(text(s3, "Key").as_deref(), Some("a&b"));
        assert_eq!(text(s3, "NextContinuationToken").as_deref(), Some("t/x="));
        assert_eq!(unescape("&lt;&#65;&#x42;&bogus;"), "<AB&bogus;");
    }
}
