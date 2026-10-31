//! Page fingerprinting: turn a URL + field set into a stable identity, and
//! measure how similar two field sets are so near-identical pages merge.

use crate::model::FieldSig;
use regex::Regex;
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::sync::LazyLock;

static NUMERIC: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\d+$").unwrap());
static UUID: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$").unwrap()
});
static HEXISH: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[0-9a-fA-F]{16,}$").unwrap());
static TOKENISH: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Za-z0-9_\-]{24,}$").unwrap());

/// Split a URL into (host, pattern). Query string and fragment are dropped and
/// volatile path segments (ids, uuids, hashes, long tokens) become `*`.
pub fn normalize_url(url: &str) -> (String, String) {
    let no_fragment = url.split('#').next().unwrap_or("");
    let no_query = no_fragment.split('?').next().unwrap_or("");
    let after_scheme = match no_query.find("://") {
        Some(i) => &no_query[i + 3..],
        None => no_query,
    };
    let (host, path) = match after_scheme.find('/') {
        Some(i) => (&after_scheme[..i], &after_scheme[i..]),
        None => (after_scheme, "/"),
    };
    let host = host.to_lowercase();
    let host = host.strip_prefix("www.").unwrap_or(&host).to_string();
    let segments: Vec<String> = path
        .split('/')
        .map(|seg| {
            if seg.is_empty() {
                String::new()
            } else if NUMERIC.is_match(seg)
                || UUID.is_match(seg)
                || HEXISH.is_match(seg)
                || (TOKENISH.is_match(seg) && seg.chars().any(|c| c.is_ascii_digit()))
            {
                "*".to_string()
            } else {
                seg.to_string()
            }
        })
        .collect();
    let mut pattern = segments.join("/");
    if pattern.is_empty() {
        pattern = "/".into();
    }
    if pattern.len() > 1 && pattern.ends_with('/') {
        pattern.pop();
    }
    (host, pattern)
}

/// The "site" a context document is scoped to: lowercase hostname without
/// scheme, port, or leading www. Accepts a URL, a host, or a host:port.
pub fn site_of(url_or_host: &str) -> String {
    let (host, _) = normalize_url(url_or_host.trim());
    
    match host.rsplit_once(':') {
        Some((h, port)) if port.chars().all(|c| c.is_ascii_digit()) => h.to_string(),
        _ => host,
    }
}

pub fn field_keys(fields: &[FieldSig]) -> BTreeSet<String> {
    fields.iter().map(FieldSig::key).collect()
}

/// Stable hash of host + url pattern + sorted field identities.
pub fn fingerprint(host: &str, url_pattern: &str, fields: &[FieldSig]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(host.as_bytes());
    hasher.update(b"\n");
    hasher.update(url_pattern.as_bytes());
    hasher.update(b"\n");
    for key in field_keys(fields) {
        hasher.update(key.as_bytes());
        hasher.update(b"\n");
    }
    hex(&hasher.finalize())
}

/// Jaccard similarity over field identities. Two empty sets are identical.
pub fn similarity(a: &[FieldSig], b: &[FieldSig]) -> f64 {
    let ka = field_keys(a);
    let kb = field_keys(b);
    let union = ka.union(&kb).count();
    if union == 0 {
        return 1.0;
    }
    let inter = ka.intersection(&kb).count();
    inter as f64 / union as f64
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(name: &str, label: &str) -> FieldSig {
        FieldSig {
            selector: format!("[name={name}]"),
            tag: "input".into(),
            input_type: "text".into(),
            name: name.into(),
            id: String::new(),
            label: label.into(),
            placeholder: String::new(),
            autocomplete: String::new(),
        }
    }

    #[test]
    fn url_strips_query_fragment_and_ids() {
        let (h, p) = normalize_url("https://www.apply.mit.edu/app/12345/section/profile?x=1#top");
        assert_eq!(h, "apply.mit.edu");
        assert_eq!(p, "/app/*/section/profile");
    }

    #[test]
    fn url_replaces_uuid_and_hex() {
        let (_, p) = normalize_url(
            "https://x.com/a/550e8400-e29b-41d4-a716-446655440000/b/deadbeefdeadbeefdeadbeef/",
        );
        assert_eq!(p, "/a/*/b/*");
    }

    #[test]
    fn url_root_and_no_scheme() {
        assert_eq!(normalize_url("https://x.com"), ("x.com".into(), "/".into()));
        assert_eq!(normalize_url("x.com/path/"), ("x.com".into(), "/path".into()));
    }

    #[test]
    fn site_strips_port_scheme_and_www() {
        assert_eq!(site_of("http://127.0.0.1:7777/test/x.html"), "127.0.0.1");
        assert_eq!(site_of("https://www.Apply.MIT.edu/app"), "apply.mit.edu");
        assert_eq!(site_of("apply.mit.edu"), "apply.mit.edu");
        assert_eq!(site_of("localhost:3000"), "localhost");
    }

    #[test]
    fn fingerprint_ignores_field_order_and_selector_drift() {
        let a = vec![f("first", "First name"), f("last", "Last name")];
        let mut b = vec![f("last", "Last name"), f("first", "First name")];
        b[0].selector = "form > div:nth-of-type(2) > input".into();
        assert_eq!(fingerprint("h", "/p", &a), fingerprint("h", "/p", &b));
        assert_ne!(fingerprint("h", "/p", &a), fingerprint("h", "/q", &a));
    }

    #[test]
    fn similarity_is_jaccard() {
        let a = vec![f("a", ""), f("b", ""), f("c", ""), f("d", "")];
        let b = vec![f("a", ""), f("b", ""), f("c", ""), f("e", "")];
        assert!((similarity(&a, &b) - 0.6).abs() < 1e-9);
        assert_eq!(similarity(&[], &[]), 1.0);
        assert_eq!(similarity(&a, &a), 1.0);
    }

    #[test]
    fn field_key_normalizes_whitespace_and_case() {
        let mut a = f("Email", "  Email   Address ");
        let b = f("email", "email address");
        a.selector = "#other".into();
        assert_eq!(a.key(), b.key());
    }
}
