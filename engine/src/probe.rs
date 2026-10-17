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

/// Resolve the best filename: Content-Disposition > URL path > request hint > fallback.
fn resolve_filename(req: &DownloadRequest, final_url: &str, headers: &HeaderMap) -> String {
    if let Some(cd) = header_str(headers, &CONTENT_DISPOSITION) {
        if let Some(name) = filename_from_content_disposition(&cd) {
            return sanitize(&name);
        }
    }
    if let Some(name) = filename_from_url(final_url) {
        if !name.is_empty() {
            return sanitize(&name);
        }
    }
    if let Some(hint) = &req.filename {
        return sanitize(hint);
    }
    "download".to_string()
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
}
