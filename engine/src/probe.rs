//! Probing a URL to determine whether it supports segmented (multi-connection)
//! downloads, and to resolve its size and filename.
//!
//! Strategy:
//!   1. Send a lightweight `GET` with `Range: bytes=0-0`. This is more reliable
//!      than a bare `HEAD`, because:
//!        - Some servers don't implement HEAD, or lie in HEAD responses.
//!        - A `206 Partial Content` reply is *proof* that ranges actually work,
//!          not just an advertised `Accept-Ranges` header.
//!   2. Interpret the response:
//!        - `206 Partial Content` + a parseable `Content-Range` total  => segmentable.
//!        - `200 OK` with `Accept-Ranges: bytes` + `Content-Length`     => segmentable.
//!        - anything else (200 without ranges, unknown length, etc.)    => single stream.

use crate::types::{DownloadRequest, ProbeResult};
use anyhow::{Context, Result};
use reqwest::header::{
    HeaderMap, HeaderName, HeaderValue, ACCEPT_ENCODING, ACCEPT_RANGES, CONTENT_DISPOSITION,
    CONTENT_LENGTH, CONTENT_RANGE, ETAG, LAST_MODIFIED, RANGE,
};
use reqwest::{Client, StatusCode};

/// Probe a download request. Never panics; on network failure returns an error
/// the caller can surface to the user.
pub async fn probe(client: &Client, req: &DownloadRequest) -> Result<ProbeResult> {
    let headers = build_headers(req)?;

    let resp = client
        .get(&req.url)
        .headers(headers)
        // Ask for the raw, uncompressed stream. Byte ranges only make sense on
        // the bytes we actually write to disk; letting the server gzip would
        // make Content-Length (compressed) disagree with the range math.
        .header(ACCEPT_ENCODING, "identity")
        .header(RANGE, "bytes=0-0")
        .send()
        .await
        .with_context(|| format!("probe request failed for {}", req.url))?;

    let status = resp.status();
    let final_url = resp.url().to_string();
    let hdrs = resp.headers().clone();

    let etag = header_str(&hdrs, &ETAG);
    let last_modified = header_str(&hdrs, &LAST_MODIFIED);
    let filename = resolve_filename(req, &final_url, &hdrs);

    let (content_length, supports_ranges) = match status {
        // Server honored the range request: this is definitive proof of support.
        StatusCode::PARTIAL_CONTENT => {
            let total = parse_content_range_total(&hdrs);
            (total, total.is_some())
        }
        // Range ignored; fall back to interpreting a normal 200 response.
        StatusCode::OK => {
            let len = header_str(&hdrs, &CONTENT_LENGTH).and_then(|v| v.parse::<u64>().ok());
            let accepts = header_str(&hdrs, &ACCEPT_RANGES)
                .map(|v| v.to_ascii_lowercase().contains("bytes"))
                .unwrap_or(false);
            // Even if the header claims support, we require a known length to segment.
            (len, accepts && len.is_some())
        }
        // Range explicitly rejected -> single connection only.
        StatusCode::RANGE_NOT_SATISFIABLE => {
            let len = header_str(&hdrs, &CONTENT_LENGTH).and_then(|v| v.parse::<u64>().ok());
            (len, false)
        }
        other if other.is_success() => {
            let len = header_str(&hdrs, &CONTENT_LENGTH).and_then(|v| v.parse::<u64>().ok());
            (len, false)
        }
        other => {
            anyhow::bail!("probe got unexpected status {other} for {}", req.url);
        }
    };

    Ok(ProbeResult {
        final_url,
        content_length,
        supports_ranges,
        filename,
        etag,
        last_modified,
    })
}

/// Build the outgoing header map from the request's forwarded headers.
fn build_headers(req: &DownloadRequest) -> Result<HeaderMap> {
    let mut map = HeaderMap::new();
    for (k, v) in &req.headers {
        let name = HeaderName::from_bytes(k.as_bytes())
            .with_context(|| format!("invalid header name: {k}"))?;
        let value =
            HeaderValue::from_str(v).with_context(|| format!("invalid header value for {k}"))?;
        map.insert(name, value);
    }
    Ok(map)
}

fn header_str(headers: &HeaderMap, name: &HeaderName) -> Option<String> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
}

/// `Content-Range: bytes 0-0/12345` -> Some(12345). A `*` total means unknown.
fn parse_content_range_total(headers: &HeaderMap) -> Option<u64> {
    let raw = header_str(headers, &CONTENT_RANGE)?;
    let total = raw.rsplit('/').next()?.trim();
    if total == "*" {
        return None;
    }
    total.parse::<u64>().ok()
}

/// Resolve the best filename. Priority:
///   1. `Content-Disposition` (the server's explicit name — most authoritative)
///   2. The caller's hint IF it looks like a real filename (has an extension) —
///      e.g. the browser's suggested name for a download-page/redirect URL whose
///      path carries no usable name.
///   3. The URL path's last segment, if usable.
///   4. The caller's hint even without an extension.
///   5. `"download"`.
fn resolve_filename(req: &DownloadRequest, final_url: &str, headers: &HeaderMap) -> String {
    if let Some(cd) = header_str(headers, &CONTENT_DISPOSITION) {
        if let Some(name) = filename_from_content_disposition(&cd) {
            return sanitize(&name);
        }
    }
    // A hint with an extension (e.g. "PPSA21783.exfat" from the browser) beats a
    // URL path, since download-page URLs often have no real filename in them.
    if let Some(hint) = &req.filename {
        if looks_like_filename(hint) {
            return sanitize(hint);
        }
    }
    if let Some(name) = filename_from_url(final_url) {
        if !name.is_empty() && looks_like_filename(&name) {
            return sanitize(&name);
        }
    }
    if let Some(hint) = &req.filename {
        if !hint.trim().is_empty() {
            return sanitize(hint);
        }
    }
    // Last resort: a bare URL path segment (no extension) — but ONLY if it looks
    // like a plausible short name, not an opaque token (e.g. a 100-char proxy
    // path like `t1_J8pu5_TELX1B...`). A junk token is worse than a generic name.
    if let Some(name) = filename_from_url(final_url) {
        if plausible_bare_name(&name) {
            return sanitize(&name);
        }
    }
    "download".to_string()
}

/// A bare (extensionless) path segment is only acceptable as a filename if it's
/// short and doesn't look like an opaque download token.
fn plausible_bare_name(name: &str) -> bool {
    let name = name.trim();
    // Too long → almost certainly a token/hash, not a human filename.
    if name.is_empty() || name.len() > 40 {
        return false;
    }
    // Reject if it's dominated by token-ish separators / looks random.
    let underscores = name.matches('_').count();
    if underscores >= 2 {
        return false;
    }
    true
}

/// A heuristic: does this look like an actual filename with a sensible
/// extension (1–8 chars, alphanumeric), not a path fragment or query junk?
fn looks_like_filename(name: &str) -> bool {
    let name = name.trim();
    match name.rsplit_once('.') {
        Some((stem, ext)) => {
            !stem.is_empty()
                && (1..=8).contains(&ext.len())
                && ext.chars().all(|c| c.is_ascii_alphanumeric())
        }
        None => false,
    }
}

/// Parse `filename*=UTF-8''name` (RFC 5987) or `filename="name"`.
fn filename_from_content_disposition(cd: &str) -> Option<String> {
    // Prefer the extended form first.
    for part in cd.split(';') {
        let part = part.trim();
        if let Some(rest) = part.strip_prefix("filename*=") {
            // e.g. UTF-8''my%20file.zip
            if let Some(idx) = rest.find("''") {
                let encoded = &rest[idx + 2..];
                return Some(percent_decode(encoded.trim_matches('"')));
            }
            return Some(rest.trim_matches('"').to_string());
        }
    }
    for part in cd.split(';') {
        let part = part.trim();
        if let Some(rest) = part.strip_prefix("filename=") {
            return Some(rest.trim_matches('"').to_string());
        }
    }
    None
}

fn filename_from_url(url: &str) -> Option<String> {
    let without_query = url.split(['?', '#']).next().unwrap_or(url);
    let last = without_query.rsplit('/').next()?;
    if last.is_empty() {
        None
    } else {
        Some(percent_decode(last))
    }
}

/// Minimal percent-decoder (avoids pulling an extra crate just for this).
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(h), Some(l)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
                out.push((h << 4) | l);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Strip path separators and characters illegal on Windows filesystems.
fn sanitize(name: &str) -> String {
    let name = name.replace(['/', '\\'], "_");
    let cleaned: String = name
        .chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '|' | '?' | '*' => '_',
            c if (c as u32) < 0x20 => '_',
            c => c,
        })
        .collect();
    let trimmed = cleaned.trim().trim_matches('.').to_string();
    if trimmed.is_empty() {
        "download".to_string()
    } else {
        trimmed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_content_range_total() {
        let mut h = HeaderMap::new();
        h.insert(CONTENT_RANGE, HeaderValue::from_static("bytes 0-0/987654"));
        assert_eq!(parse_content_range_total(&h), Some(987654));
    }

    #[test]
    fn unknown_total_in_content_range() {
        let mut h = HeaderMap::new();
        h.insert(CONTENT_RANGE, HeaderValue::from_static("bytes 0-0/*"));
        assert_eq!(parse_content_range_total(&h), None);
    }

    #[test]
    fn content_disposition_quoted() {
        let got = filename_from_content_disposition(r#"attachment; filename="report final.pdf""#);
        assert_eq!(got.as_deref(), Some("report final.pdf"));
    }

    #[test]
    fn content_disposition_extended() {
        let got =
            filename_from_content_disposition("attachment; filename*=UTF-8''my%20file%20%281%29.zip");
        assert_eq!(got.as_deref(), Some("my file (1).zip"));
    }

    #[test]
    fn filename_from_url_path() {
        assert_eq!(
            filename_from_url("https://ex.com/a/b/archive.tar.gz?token=x"),
            Some("archive.tar.gz".to_string())
        );
    }

    #[test]
    fn sanitize_strips_illegal() {
        assert_eq!(sanitize("a/b:c*?.txt"), "a_b_c__.txt");
    }

    #[test]
    fn looks_like_filename_heuristic() {
        assert!(looks_like_filename("PPSA21783.exfat"));
        assert!(looks_like_filename("movie.mkv"));
        assert!(!looks_like_filename("download")); // no extension
        assert!(!looks_like_filename("a.verylongextension")); // ext too long
        assert!(!looks_like_filename(".bashrc")); // empty stem
    }

    fn req_with(filename: Option<&str>) -> DownloadRequest {
        DownloadRequest {
            url: String::new(),
            filename: filename.map(|s| s.to_string()),
            headers: Default::default(),
            save_dir: None,
            kind: None,
            resolved_target: None,
            queue: None,
        }
    }

    #[test]
    fn hint_with_extension_beats_nameless_url() {
        // Download-page URL with no real filename; browser forwarded a good name.
        let req = req_with(Some("PPSA21783.exfat"));
        let name = resolve_filename(&req, "https://host.net/download?id=abc123", &HeaderMap::new());
        assert_eq!(name, "PPSA21783.exfat");
    }

    #[test]
    fn content_disposition_still_wins_over_hint() {
        let req = req_with(Some("hint.bin"));
        let mut h = HeaderMap::new();
        h.insert(CONTENT_DISPOSITION, HeaderValue::from_static("attachment; filename=\"real.zip\""));
        assert_eq!(resolve_filename(&req, "https://x/y", &h), "real.zip");
    }

    #[test]
    fn url_path_wins_when_it_has_a_real_name_and_no_hint() {
        let req = req_with(None);
        let name = resolve_filename(&req, "https://cdn.com/files/archive.tar.gz", &HeaderMap::new());
        assert_eq!(name, "archive.tar.gz");
    }

    #[test]
    fn content_disposition_resolves_tokenized_proxy_url() {
        // Real-world case: dlproxy tokenized URL with no filename in the path;
        // the server provides the name via Content-Disposition (seen on the probe).
        let req = req_with(None);
        let url = "https://tunnel5.dlproxy.uk/download/t1_J8pu5_TELX1B?sig=AL8kzhG";
        let mut h = HeaderMap::new();
        h.insert(
            CONTENT_DISPOSITION,
            HeaderValue::from_static("attachment; filename=\"PPSA21783.exfat\""),
        );
        assert_eq!(resolve_filename(&req, url, &h), "PPSA21783.exfat");
    }

    #[test]
    fn tokenized_url_without_cd_does_not_use_junk_path_segment() {
        // Same URL but NO Content-Disposition: the token path segment has no
        // real extension, so we must NOT name the file after it.
        let req = req_with(None);
        let url = "https://tunnel5.dlproxy.uk/download/t1_J8pu5_TELX1B?sig=AL8kzhG";
        let name = resolve_filename(&req, url, &HeaderMap::new());
        // Falls through to the generic name rather than "t1_J8pu5_TELX1B".
        assert_eq!(name, "download");
    }
}
