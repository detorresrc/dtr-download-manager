//! Streaming media capture: HLS (`.m3u8`) and DASH (`.mpd`).
//!
//! Unlike a regular file download, a streaming manifest is an *index* that
//! lists many small media segments. To "download the video" we:
//!   1. Fetch and parse the manifest.
//!   2. If it's a master/multivariant manifest, pick the highest-bandwidth
//!      rendition and fetch its media manifest.
//!   3. Resolve the ordered list of segment URLs (plus an optional init segment).
//!   4. Download the segments (concurrently, bounded) and concatenate them in
//!      order into a single output file.
//!
//! ## Supported
//! - HLS media playlists (`#EXTINF` + URI lines), master playlists
//!   (`#EXT-X-STREAM-INF` → highest `BANDWIDTH`), and `#EXT-X-MAP` init segments.
//! - DASH `SegmentTemplate` with `$Number$` / `$RepresentationID$` substitution,
//!   `initialization`, `startNumber`, `duration`/`timescale`, and `BaseURL`;
//!   highest-`bandwidth` `Representation` is chosen.
//!
//! ## Known limitations (documented, not silently ignored)
//! - HLS AES-128 / SAMPLE-AES decryption is not performed (`#EXT-X-KEY` is
//!   detected and reported as an error rather than producing a broken file).
//! - DASH `SegmentList` and `SegmentTimeline` (`$Time$`) are parsed best-effort;
//!   live/dynamic manifests are treated as their current (VOD-like) snapshot.
//! - The result is the raw concatenation of segments (e.g. an MPEG-TS or fMP4
//!   stream). It is a valid, playable media file but is not remuxed into MP4.

use crate::types::DownloadKind;

/// A resolved plan: the ordered media segment URLs plus an optional init
/// segment that must be written first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaPlan {
    /// Written at the very start of the output, if present (e.g. fMP4 init).
    pub init_segment: Option<String>,
    /// Media segment URLs, in playback order.
    pub segments: Vec<String>,
    /// A hint for the output file extension (e.g. "ts", "mp4").
    pub suggested_ext: &'static str,
}

impl MediaPlan {
    pub fn is_empty(&self) -> bool {
        self.segments.is_empty() && self.init_segment.is_none()
    }

    /// Every URL to fetch, init first.
    pub fn all_urls(&self) -> Vec<&str> {
        let mut v = Vec::with_capacity(self.segments.len() + 1);
        if let Some(i) = &self.init_segment {
            v.push(i.as_str());
        }
        v.extend(self.segments.iter().map(|s| s.as_str()));
        v
    }
}

/// Resolve a relative URL reference against a base (the manifest URL).
/// Minimal RFC-3986-style join that handles the cases manifests actually use:
/// absolute URLs, absolute paths (`/a/b`), and relative paths.
pub fn resolve_url(base: &str, reference: &str) -> String {
    let reference = reference.trim();
    if reference.is_empty() {
        return base.to_string();
    }
    // Already absolute.
    if reference.contains("://") {
        return reference.to_string();
    }
    // Protocol-relative (//host/path): borrow the base's scheme.
    if let Some(rest) = reference.strip_prefix("//") {
        let scheme = base.split("://").next().unwrap_or("https");
        return format!("{scheme}://{rest}");
    }

    let (scheme, after_scheme) = match base.split_once("://") {
        Some((s, rest)) => (s, rest),
        None => return reference.to_string(),
    };
    let authority_end = after_scheme.find('/').unwrap_or(after_scheme.len());
    let authority = &after_scheme[..authority_end];
    let base_path = &after_scheme[authority_end..]; // begins with '/' or empty

    // Strip any query/fragment from the base path before joining.
    let base_path = base_path.split(['?', '#']).next().unwrap_or(base_path);

    if let Some(abs) = reference.strip_prefix('/') {
        return format!("{scheme}://{authority}/{abs}");
    }

    // Relative: drop the base's last path segment (the file), keep the dir.
    let dir = match base_path.rfind('/') {
        Some(i) => &base_path[..=i],
        None => "/",
    };
    // Normalize a few `../` and `./` the lazy but correct way.
    let joined = format!("{dir}{reference}");
    let normalized = normalize_path(&joined);
    format!("{scheme}://{authority}{normalized}")
}

/// Collapse `.` and `..` segments in an absolute path, preserving a trailing
/// slash (which distinguishes a directory base from a file base).
fn normalize_path(path: &str) -> String {
    let trailing_slash = path.ends_with('/');
    let mut out: Vec<&str> = Vec::new();
    for seg in path.split('/') {
        match seg {
            "" | "." => { /* skip empty (from leading '/') and current-dir */ }
            ".." => {
                out.pop();
            }
            s => out.push(s),
        }
    }
    let mut s = String::from("/");
    s.push_str(&out.join("/"));
    if trailing_slash && !s.ends_with('/') {
        s.push('/');
    }
    s
}

// ---- HLS --------------------------------------------------------------------

/// Is this text an HLS master (multivariant) playlist?
fn is_hls_master(text: &str) -> bool {
    text.contains("#EXT-X-STREAM-INF")
}

/// From a master playlist, return the variant URI with the highest BANDWIDTH.
pub fn hls_best_variant(text: &str, base_url: &str) -> Option<String> {
    let lines: Vec<&str> = text.lines().collect();
    let mut best: Option<(u64, String)> = None;
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i].trim();
        if line.starts_with("#EXT-X-STREAM-INF") {
            let bw = attr_u64(line, "BANDWIDTH").unwrap_or(0);
            // The URI is on the next non-comment, non-empty line.
            let mut j = i + 1;
            while j < lines.len() {
                let uri = lines[j].trim();
                if !uri.is_empty() && !uri.starts_with('#') {
                    let full = resolve_url(base_url, uri);
                    if best.as_ref().map(|(b, _)| bw > *b).unwrap_or(true) {
                        best = Some((bw, full));
                    }
                    break;
                }
                j += 1;
            }
            i = j;
        }
        i += 1;
    }
    best.map(|(_, u)| u)
}

/// Parse an HLS *media* playlist into a plan.
pub fn parse_hls_media(text: &str, base_url: &str) -> Result<MediaPlan, MediaError> {
    // Refuse encrypted streams rather than emit a corrupt file.
    for line in text.lines() {
        let l = line.trim();
        if l.starts_with("#EXT-X-KEY") && !l.contains("METHOD=NONE") {
            return Err(MediaError::Encrypted);
        }
    }

    let mut init_segment = None;
    let mut segments = Vec::new();
    let mut expect_uri = false;

    for line in text.lines() {
        let l = line.trim();
        if l.is_empty() {
            continue;
        }
        if let Some(rest) = l.strip_prefix("#EXT-X-MAP:") {
            // #EXT-X-MAP:URI="init.mp4"
            if let Some(uri) = attr_str(rest, "URI") {
                init_segment = Some(resolve_url(base_url, &uri));
            }
            continue;
        }
        if l.starts_with("#EXTINF") {
            expect_uri = true;
            continue;
        }
        if l.starts_with('#') {
            continue; // other tags
        }
        // A URI line.
        if expect_uri || !l.starts_with('#') {
            segments.push(resolve_url(base_url, l));
            expect_uri = false;
        }
    }

    if segments.is_empty() {
        return Err(MediaError::NoSegments);
    }
    // fMP4 (has init map) → .mp4 container; otherwise MPEG-TS.
    let suggested_ext = if init_segment.is_some() { "mp4" } else { "ts" };
    Ok(MediaPlan {
        init_segment,
        segments,
        suggested_ext,
    })
}

/// Top-level HLS entry: given master-or-media text, return either a plan or the
/// URL of the best variant's media playlist that the caller must fetch next.
pub enum HlsResolved {
    Plan(MediaPlan),
    FollowVariant(String),
}

pub fn resolve_hls(text: &str, base_url: &str) -> Result<HlsResolved, MediaError> {
    if is_hls_master(text) {
        let variant = hls_best_variant(text, base_url).ok_or(MediaError::NoSegments)?;
        Ok(HlsResolved::FollowVariant(variant))
    } else {
        Ok(HlsResolved::Plan(parse_hls_media(text, base_url)?))
    }
}

// ---- DASH -------------------------------------------------------------------

/// Parse a DASH MPD (VOD, `SegmentTemplate`-based) into a plan, choosing the
/// highest-bandwidth representation.
pub fn parse_dash(text: &str, base_url: &str) -> Result<MediaPlan, MediaError> {
    // A minimal, dependency-free XML walk. DASH manifests in the wild are
    // regular enough that targeted extraction is reliable for the common
    // SegmentTemplate profile; we avoid a full XML crate to keep the engine lean.

    // An MPD-level <BaseURL> rebases everything.
    let mpd_base = extract_tag_text(text, "BaseURL")
        .map(|b| resolve_url(base_url, &b))
        .unwrap_or_else(|| base_url.to_string());

    // Find the representation with the highest bandwidth and its SegmentTemplate.
    let rep = best_dash_representation(text).ok_or(MediaError::NoSegments)?;

    let template = find_segment_template(&rep.block)
        .or_else(|| find_segment_template(text)) // template may live on AdaptationSet
        .ok_or(MediaError::Unsupported("DASH without SegmentTemplate"))?;

    let media_tpl = attr_str(&template, "media").ok_or(MediaError::NoSegments)?;
    let init_tpl = attr_str(&template, "initialization");
    let start_number = attr_u64(&template, "startNumber").unwrap_or(1);

    // Number of segments: prefer an explicit count, else derive from
    // duration/timescale against the total media presentation duration.
    let count = dash_segment_count(text, &template)?;

    let subst = |tpl: &str, number: u64| -> String {
        let s = tpl
            .replace("$RepresentationID$", &rep.id)
            .replace("$Bandwidth$", &rep.bandwidth.to_string());
        substitute_number(&s, number)
    };

    let init_segment = init_tpl.map(|t| {
        let s = t.replace("$RepresentationID$", &rep.id);
        resolve_url(&mpd_base, &s)
    });

    let mut segments = Vec::with_capacity(count as usize);
    for n in 0..count {
        let number = start_number + n;
        segments.push(resolve_url(&mpd_base, &subst(&media_tpl, number)));
    }

    if segments.is_empty() {
        return Err(MediaError::NoSegments);
    }
    Ok(MediaPlan {
        init_segment,
        segments,
        suggested_ext: "mp4",
    })
}

struct DashRep {
    id: String,
    bandwidth: u64,
    block: String,
}

/// Find the `<Representation …>…</Representation>` with the highest bandwidth.
fn best_dash_representation(text: &str) -> Option<DashRep> {
    let mut best: Option<DashRep> = None;
    let mut search_from = 0;
    while let Some(start) = text[search_from..].find("<Representation") {
        let abs = search_from + start;
        // Extract the opening tag (attributes).
        let tag_end = text[abs..].find('>').map(|e| abs + e)?;
        let open_tag = &text[abs..=tag_end];
        let id = attr_str(open_tag, "id").unwrap_or_default();
        let bandwidth = attr_u64(open_tag, "bandwidth").unwrap_or(0);

        // The block is from the open tag to the matching close (or self-close).
        let block_end = text[abs..]
            .find("</Representation>")
            .map(|e| abs + e + "</Representation>".len())
            .unwrap_or_else(|| text.len());
        let block = text[abs..block_end].to_string();

        if best.as_ref().map(|b| bandwidth > b.bandwidth).unwrap_or(true) {
            best = Some(DashRep { id, bandwidth, block });
        }
        search_from = block_end;
    }
    best
}

fn find_segment_template(block: &str) -> Option<String> {
    let start = block.find("<SegmentTemplate")?;
    let end = block[start..].find('>').map(|e| start + e)? + 1;
    Some(block[start..end].to_string())
}

/// Determine how many media segments the template yields.
fn dash_segment_count(mpd: &str, template: &str) -> Result<u64, MediaError> {
    // 1) Explicit SegmentTimeline (count the <S> entries, honoring repeats).
    if let Some(count) = count_segment_timeline(template).or_else(|| count_segment_timeline(mpd)) {
        return Ok(count);
    }
    // 2) duration/timescale against total presentation duration.
    let duration = attr_u64(template, "duration");
    let timescale = attr_u64(template, "timescale").unwrap_or(1).max(1);
    let total_secs = mediapresentationduration_secs(mpd);
    if let (Some(dur), Some(total)) = (duration, total_secs) {
        let seg_secs = dur as f64 / timescale as f64;
        if seg_secs > 0.0 {
            return Ok((total / seg_secs).ceil() as u64);
        }
    }
    Err(MediaError::Unsupported(
        "cannot determine DASH segment count (no SegmentTimeline or duration)",
    ))
}

/// Count segments described by a `<SegmentTimeline>` (`<S t= d= r=>`).
fn count_segment_timeline(block: &str) -> Option<u64> {
    let tl_start = block.find("<SegmentTimeline")?;
    let tl_end = block[tl_start..]
        .find("</SegmentTimeline>")
        .map(|e| tl_start + e)?;
    let tl = &block[tl_start..tl_end];
    let mut total = 0u64;
    let mut from = 0;
    while let Some(s) = tl[from..].find("<S ") {
        let abs = from + s;
        let end = tl[abs..].find('>').map(|e| abs + e)?;
        let tag = &tl[abs..=end];
        let repeat = attr_u64(tag, "r").unwrap_or(0);
        total += 1 + repeat; // r is additional repeats
        from = end;
    }
    if total > 0 {
        Some(total)
    } else {
        None
    }
}

/// Parse `mediaPresentationDuration="PT1H2M3.5S"` into seconds.
fn mediapresentationduration_secs(mpd: &str) -> Option<f64> {
    let raw = attr_str_anywhere(mpd, "mediaPresentationDuration")?;
    parse_iso8601_duration(&raw)
}

/// Parse an ISO-8601 duration like `PT1H2M3.5S` (the subset DASH uses).
pub fn parse_iso8601_duration(s: &str) -> Option<f64> {
    let s = s.strip_prefix('P')?;
    // Only the time part (after 'T') matters for typical VOD MPDs.
    let t = s.split('T').nth(1).unwrap_or(s);
    let mut secs = 0f64;
    let mut num = String::new();
    for c in t.chars() {
        match c {
            '0'..='9' | '.' => num.push(c),
            'H' => {
                secs += num.parse::<f64>().ok()? * 3600.0;
                num.clear();
            }
            'M' => {
                secs += num.parse::<f64>().ok()? * 60.0;
                num.clear();
            }
            'S' => {
                secs += num.parse::<f64>().ok()?;
                num.clear();
            }
            _ => num.clear(),
        }
    }
    Some(secs)
}

// ---- small attribute helpers ------------------------------------------------

/// Substitute `$Number$` and width-formatted `$Number%0Nd$` in a template.
fn substitute_number(tpl: &str, number: u64) -> String {
    let mut out = String::new();
    let mut rest = tpl;
    while let Some(pos) = rest.find("$Number") {
        out.push_str(&rest[..pos]);
        let after = &rest[pos + "$Number".len()..];
        if let Some(end) = after.find('$') {
            let spec = &after[..end]; // "" or "%05d"
            if let Some(width) = parse_pad_width(spec) {
                out.push_str(&format!("{:0width$}", number, width = width));
            } else {
                out.push_str(&number.to_string());
            }
            rest = &after[end + 1..];
        } else {
            out.push_str(&number.to_string());
            rest = after;
        }
    }
    out.push_str(rest);
    out
}

/// Parse a printf-style pad spec like `%05d` → Some(5); "" → None.
fn parse_pad_width(spec: &str) -> Option<usize> {
    let spec = spec.strip_prefix('%')?;
    let spec = spec.strip_suffix('d').unwrap_or(spec);
    let spec = spec.strip_prefix('0').unwrap_or(spec);
    spec.parse::<usize>().ok()
}

/// Extract `KEY=value` or `KEY="value"` from a tag/attr line.
fn attr_str(s: &str, key: &str) -> Option<String> {
    let needle = format!("{key}=");
    let idx = find_attr(s, &needle)?;
    let rest = &s[idx + needle.len()..];
    if let Some(stripped) = rest.strip_prefix('"') {
        stripped.split('"').next().map(|v| v.to_string())
    } else {
        Some(
            rest.split([',', ' ', '>', '/'])
                .next()
                .unwrap_or("")
                .trim_matches('"')
                .to_string(),
        )
    }
}

/// Like `attr_str` but searches the whole document (for root MPD attributes).
fn attr_str_anywhere(s: &str, key: &str) -> Option<String> {
    attr_str(s, key)
}

fn attr_u64(s: &str, key: &str) -> Option<u64> {
    attr_str(s, key)?.parse::<u64>().ok()
}

/// Find an attribute key ensuring it's not a suffix of a longer key
/// (e.g. don't match `BANDWIDTH` inside `AVERAGE-BANDWIDTH`).
fn find_attr(s: &str, needle: &str) -> Option<usize> {
    let mut from = 0;
    while let Some(rel) = s[from..].find(needle) {
        let idx = from + rel;
        let prev_ok = idx == 0
            || !s.as_bytes()[idx - 1].is_ascii_alphanumeric()
                && s.as_bytes()[idx - 1] != b'-'
                && s.as_bytes()[idx - 1] != b'_';
        if prev_ok {
            return Some(idx);
        }
        from = idx + needle.len();
    }
    None
}

/// Extract the text content of the first `<Tag>…</Tag>`.
fn extract_tag_text(s: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = s.find(&open)? + open.len();
    let end = s[start..].find(&close)? + start;
    Some(s[start..end].trim().to_string())
}

/// Errors from manifest parsing / media planning.
#[derive(Debug, thiserror::Error)]
pub enum MediaError {
    #[error("the stream is encrypted (HLS AES-128/SAMPLE-AES is not supported)")]
    Encrypted,
    #[error("no media segments found in the manifest")]
    NoSegments,
    #[error("unsupported manifest feature: {0}")]
    Unsupported(&'static str),
}

/// Classify a manifest body when the URL extension is ambiguous.
pub fn sniff_kind(body: &str) -> Option<DownloadKind> {
    let head = body.trim_start();
    if head.starts_with("#EXTM3U") {
        Some(DownloadKind::Hls)
    } else if head.starts_with("<?xml") && body.contains("<MPD") {
        Some(DownloadKind::Dash)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_relative_urls() {
        let base = "https://cdn.example.com/video/playlist.m3u8";
        assert_eq!(
            resolve_url(base, "seg1.ts"),
            "https://cdn.example.com/video/seg1.ts"
        );
        assert_eq!(
            resolve_url(base, "/abs/seg1.ts"),
            "https://cdn.example.com/abs/seg1.ts"
        );
        assert_eq!(
            resolve_url(base, "https://other.com/x.ts"),
            "https://other.com/x.ts"
        );
        assert_eq!(
            resolve_url(base, "//cdn2.example.com/x.ts"),
            "https://cdn2.example.com/x.ts"
        );
        assert_eq!(
            resolve_url("https://e.com/a/b/c.mpd", "../x/seg.m4s"),
            "https://e.com/a/x/seg.m4s"
        );
    }

    #[test]
    fn parses_hls_media_playlist() {
        let m3u8 = "#EXTM3U\n\
#EXT-X-VERSION:3\n\
#EXT-X-TARGETDURATION:10\n\
#EXTINF:9.009,\n\
seg0.ts\n\
#EXTINF:9.009,\n\
seg1.ts\n\
#EXTINF:3.003,\n\
seg2.ts\n\
#EXT-X-ENDLIST\n";
        let plan = parse_hls_media(m3u8, "https://cdn.com/v/index.m3u8").unwrap();
        assert_eq!(plan.init_segment, None);
        assert_eq!(plan.segments.len(), 3);
        assert_eq!(plan.segments[0], "https://cdn.com/v/seg0.ts");
        assert_eq!(plan.segments[2], "https://cdn.com/v/seg2.ts");
        assert_eq!(plan.suggested_ext, "ts");
    }

    #[test]
    fn hls_with_init_map_is_fmp4() {
        let m3u8 = "#EXTM3U\n\
#EXT-X-MAP:URI=\"init.mp4\"\n\
#EXTINF:6.0,\n\
seg0.m4s\n\
#EXT-X-ENDLIST\n";
        let plan = parse_hls_media(m3u8, "https://cdn.com/v/index.m3u8").unwrap();
        assert_eq!(
            plan.init_segment.as_deref(),
            Some("https://cdn.com/v/init.mp4")
        );
        assert_eq!(plan.suggested_ext, "mp4");
        assert_eq!(plan.all_urls()[0], "https://cdn.com/v/init.mp4");
    }

    #[test]
    fn hls_encrypted_is_rejected() {
        let m3u8 = "#EXTM3U\n\
#EXT-X-KEY:METHOD=AES-128,URI=\"key.bin\"\n\
#EXTINF:6.0,\n\
seg0.ts\n";
        assert!(matches!(
            parse_hls_media(m3u8, "https://cdn.com/v/index.m3u8"),
            Err(MediaError::Encrypted)
        ));
    }

    #[test]
    fn hls_master_picks_highest_bandwidth() {
        let master = "#EXTM3U\n\
#EXT-X-STREAM-INF:BANDWIDTH=800000,RESOLUTION=640x360\n\
low/index.m3u8\n\
#EXT-X-STREAM-INF:BANDWIDTH=2400000,RESOLUTION=1280x720\n\
high/index.m3u8\n";
        let best = hls_best_variant(master, "https://cdn.com/v/master.m3u8").unwrap();
        assert_eq!(best, "https://cdn.com/v/high/index.m3u8");
        assert!(is_hls_master(master));
    }

    #[test]
    fn number_substitution_plain_and_padded() {
        assert_eq!(substitute_number("seg_$Number$.m4s", 5), "seg_5.m4s");
        assert_eq!(substitute_number("seg_$Number%05d$.m4s", 5), "seg_00005.m4s");
        assert_eq!(substitute_number("a$Number$b$Number$c", 7), "a7b7c");
    }

    #[test]
    fn iso_duration_parsing() {
        assert_eq!(parse_iso8601_duration("PT1H2M3S"), Some(3723.0));
        assert_eq!(parse_iso8601_duration("PT30.5S"), Some(30.5));
        assert_eq!(parse_iso8601_duration("PT600S"), Some(600.0));
    }

    #[test]
    fn parses_dash_segment_template_with_duration() {
        let mpd = r#"<?xml version="1.0"?>
<MPD mediaPresentationDuration="PT30S">
  <Period>
    <AdaptationSet mimeType="video/mp4">
      <Representation id="720" bandwidth="2400000">
        <SegmentTemplate timescale="1000" duration="10000" media="seg_$Number$.m4s" initialization="init_$RepresentationID$.m4s" startNumber="1"/>
      </Representation>
      <Representation id="360" bandwidth="800000">
        <SegmentTemplate timescale="1000" duration="10000" media="low_$Number$.m4s" startNumber="1"/>
      </Representation>
    </AdaptationSet>
  </Period>
</MPD>"#;
        let plan = parse_dash(mpd, "https://cdn.com/v/manifest.mpd").unwrap();
        // 30s total / 10s per segment = 3 segments, highest bandwidth (720).
        assert_eq!(plan.segments.len(), 3);
        assert_eq!(
            plan.init_segment.as_deref(),
            Some("https://cdn.com/v/init_720.m4s")
        );
        assert_eq!(plan.segments[0], "https://cdn.com/v/seg_1.m4s");
        assert_eq!(plan.segments[2], "https://cdn.com/v/seg_3.m4s");
    }

    #[test]
    fn parses_dash_with_base_url() {
        let mpd = r#"<?xml version="1.0"?>
<MPD mediaPresentationDuration="PT20S">
  <BaseURL>video/720/</BaseURL>
  <Period><AdaptationSet>
    <Representation id="1" bandwidth="1000000">
      <SegmentTemplate timescale="1" duration="10" media="seg$Number$.m4s" startNumber="1"/>
    </Representation>
  </AdaptationSet></Period>
</MPD>"#;
        let plan = parse_dash(mpd, "https://cdn.com/manifest.mpd").unwrap();
        assert_eq!(plan.segments.len(), 2);
        assert_eq!(plan.segments[0], "https://cdn.com/video/720/seg1.m4s");
    }

    #[test]
    fn sniffs_kinds() {
        assert_eq!(sniff_kind("#EXTM3U\n..."), Some(DownloadKind::Hls));
        assert_eq!(
            sniff_kind("<?xml version=\"1.0\"?><MPD></MPD>"),
            Some(DownloadKind::Dash)
        );
        assert_eq!(sniff_kind("just a file"), None);
    }

    #[test]
    fn find_attr_does_not_match_suffix() {
        // BANDWIDTH must not match inside AVERAGE-BANDWIDTH.
        let line = "#EXT-X-STREAM-INF:AVERAGE-BANDWIDTH=111,BANDWIDTH=999";
        assert_eq!(attr_u64(line, "BANDWIDTH"), Some(999));
    }
}
