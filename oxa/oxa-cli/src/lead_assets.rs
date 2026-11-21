//! Transport for the lead web page body, stylesheet and script: content-hashed asset URLs,
//! gzip when the client accepts it, and the `304` that answers an unchanged repeat read.
//! Encoding, validators and caching are decided here; authorization stays with the caller,
//! which refuses before this module builds a header. Nothing here holds page text, session
//! state or a credential, and nothing is kept across requests except the two assets
//! compiled into the binary.
use crate::lead_page;
use oxa_http::server::Headers;
use sha2::{Digest, Sha256};
use std::io::Write as _;
use std::sync::OnceLock;

/// gzip level for pages and assets: a few milliseconds at this size, close to the smallest
/// output. Level 9 saves about 3% and costs several times the CPU on every render.
const GZIP_LEVEL: u32 = 6;

/// A compiled-in asset, compressed once on first use and served at a URL that names its
/// bytes, so an unchanged page needs no second request for it.
struct Asset {
    /// The file name and media subtype: `style.css` or `live.js`.
    file: &'static str,
    body: &'static str,
    /// First eight hex digits of the SHA-256 of `body`: the URL segment and the validator.
    tag: OnceLock<String>,
    gzip: OnceLock<Vec<u8>>,
    packed_etag: OnceLock<String>,
}

impl Asset {
    const fn new(file: &'static str, body: &'static str) -> Self {
        Self {
            file,
            body,
            tag: OnceLock::new(),
            gzip: OnceLock::new(),
            packed_etag: OnceLock::new(),
        }
    }

    fn tag(&self) -> &str {
        self.tag.get_or_init(|| {
            let digest = format!("{:x}", Sha256::digest(self.body.as_bytes()));
            digest[..8].to_owned()
        })
    }

    /// The served path: one directory below the root, so a page path never meets it.
    fn path(&self) -> String {
        format!("/{}/{}/{}", HASHED, self.tag(), self.file)
    }

    fn etag(&self) -> String {
        format!("\"{}\"", self.tag())
    }

    /// The validator of the compressed representation: the digest of the bytes actually sent.
    fn packed_etag(&self) -> String {
        self.packed_etag
            .get_or_init(|| format!("\"{}\"", &format!("{:x}", Sha256::digest(self.gzip()))[..8]))
            .clone()
    }

    /// The media type of the file: `text/css` or `text/javascript`, without a charset (the
    /// files are UTF-8 and the browser must not guess otherwise).
    fn kind(&self) -> &'static str {
        match self.file.rsplit_once('.').map(|(_, extension)| extension) {
            Some("css") => "text/css",
            _ => "text/javascript",
        }
    }

    fn gzip(&self) -> &[u8] {
        self.gzip.get_or_init(|| {
            // A dozen kilobytes cannot fail to compress into memory; if the encoder ever
            // did, the raw bytes are served and no client sees half a body.
            gzip(self.body.as_bytes()).unwrap_or_default()
        })
    }
}

/// The one directory of content-hashed assets.
const HASHED: &str = "a";

static ASSETS: [Asset; 2] = [
    Asset::new("style.css", lead_page::STYLE),
    Asset::new("live.js", lead_page::SCRIPT),
];

/// The stylesheet URL the document names.
pub(crate) fn style_path() -> String {
    ASSETS[0].path()
}

/// The script URL the document names.
pub(crate) fn script_path() -> String {
    ASSETS[1].path()
}

/// gzip one body. Level [`GZIP_LEVEL`], a fixed header and no name or timestamp, so the same
/// bytes compress to the same bytes on every call and a validator over them stays stable.
fn gzip(body: &[u8]) -> Option<Vec<u8>> {
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::new(GZIP_LEVEL));
    encoder.write_all(body).ok()?;
    encoder.finish().ok()
}

/// Whether the `Accept-Encoding` value allows gzip. `gzip;q=0` and `*;q=0` refuse it; an
/// absent or unreadable value is served raw.
fn accepts_gzip(value: Option<&str>) -> bool {
    /// The coding name (borrowed) and its weight, defaulting to 1.
    fn coding(text: &str) -> (&str, f32) {
        let mut parts = text.split(';');
        let name = parts.next().unwrap_or_default().trim();
        let q = parts.find_map(|parameter| {
            parameter
                .trim()
                .strip_prefix("q=")
                .and_then(|q| q.trim().parse::<f32>().ok())
        });
        (name, q.unwrap_or(1.0))
    }
    let best = |codings: &mut dyn Iterator<Item = f32>| -> Option<f32> {
        codings.fold(None, |best, q| Some(best.map_or(q, |b: f32| b.max(q))))
    };
    value.is_some_and(|value| {
        let codings: Vec<(&str, f32)> = value.split(',').map(coding).collect();
        // A named `gzip` decides on its own; `*` answers only for a coding the request does
        // not name, which is why `gzip, *;q=0` still allows gzip.
        let named = best(
            &mut codings
                .iter()
                .filter(|(name, _)| name.eq_ignore_ascii_case("gzip"))
                .map(|(_, q)| *q),
        );
        let wildcard = best(&mut codings.iter().filter(|(name, _)| *name == "*").map(|(_, q)| *q));
        named.or(wildcard).is_some_and(|q| q > 0.0)
    })
}

/// The header block every answer carries, `200` or `304`, so the page stack is the same on a
/// revalidated read as on the first one.
fn security_headers(form_action: &str, referrer: &str) -> String {
    format!("X-Content-Type-Options: nosniff\r\nContent-Security-Policy: default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action {form_action}\r\nReferrer-Policy: {referrer}\r\n")
}

/// What the request lets this response skip: the validator of the body it would receive, the
/// tags it already holds, and whether it accepts gzip. Built only after the caller authorized
/// the request.
pub(crate) struct Conditional<'a> {
    etag: &'a str,
    tags: Option<&'a str>,
    gzip: bool,
}

impl<'a> Conditional<'a> {
    pub(crate) fn of(headers: &'a Headers<'a>, etag: &'a str) -> Self {
        Self {
            etag,
            tags: headers.get("if-none-match"),
            gzip: accepts_gzip(headers.get("accept-encoding")),
        }
    }

    /// Whether the client holds exactly the bytes it would receive. Only a strong tag
    /// matches: this validator names the bytes and the resolution the server chose, and a
    /// client that may ask for either encoding cannot vouch for the one it has. `*` states
    /// nothing about this body and never answers `304`.
    fn holds(&self) -> bool {
        self.tags
            .is_some_and(|tags| tags.split(',').any(|tag| tag.trim() == self.etag))
    }

    /// The bytes to send: gzip when the client accepts it and it is smaller than the source.
    fn encode(&self, body: &[u8]) -> (Vec<u8>, &'static str) {
        match self.gzip.then(|| gzip(body)).flatten() {
            Some(packed) if packed.len() < body.len() => (packed, "Content-Encoding: gzip\r\n"),
            _ => (body.to_vec(), ""),
        }
    }
}

/// One response of this module: bytes with their head, or the `304` that replaces them.
pub(crate) enum Reply {
    Full {
        status: &'static str,
        kind: &'static str,
        encoding: &'static str,
        body: Vec<u8>,
        /// Headers between `Content-Encoding` and `Connection`.
        extra: String,
    },
    Unchanged(String),
}

impl Reply {
    /// A page or fragment: per response, `no-store`, and `304` to a client that holds it.
    pub(crate) fn page(
        status: &'static str,
        body: String,
        etag: &str,
        headers: &Headers<'_>,
        form_action: &str,
        referrer: &str,
    ) -> Self {
        let security = security_headers(form_action, referrer);
        let wants = Conditional::of(headers, etag);
        if wants.holds() {
            return Self::Unchanged(format!("{security}Cache-Control: no-store\r\nETag: {etag}\r\n"));
        }
        let (body, encoding) = wants.encode(body.as_bytes());
        Self::Full {
            status,
            kind: "text/html",
            encoding,
            body,
            extra: format!("{security}Cache-Control: no-store\r\nETag: {etag}\r\nVary: Accept-Encoding\r\n"),
        }
    }

    /// The `303` of an accepted submission, with its `Location` (and the token cookie).
    pub(crate) fn redirect(location: &str) -> Self {
        Self::Full {
            status: "303 See Other",
            kind: "text/plain",
            encoding: "",
            body: Vec::new(),
            extra: format!("Location: {location}\r\nCache-Control: no-store\r\n"),
        }
    }

    /// A response with a fixed status and body and nothing to validate (`/events`, a refusal,
    /// `404`): never cached, never compressed.
    pub(crate) fn plain(
        status: &'static str,
        kind: &'static str,
        body: &str,
        form_action: &str,
        referrer: &str,
    ) -> Self {
        Self::Full {
            status,
            kind,
            encoding: "",
            body: body.as_bytes().to_vec(),
            extra: format!(
                "{}Cache-Control: no-store\r\n",
                security_headers(form_action, referrer)
            ),
        }
    }

    /// A compiled-in asset. Its URL names its bytes, so the answer is public and immutable and
    /// shared by every reader of this build; it holds no page data, identity or session.
    pub(crate) fn asset(path: &str, headers: &Headers<'_>) -> Option<Self> {
        let asset = ASSETS.iter().find(|asset| asset.path() == path)?;
        // Each representation has its own validator, so the `304` a client gets always names
        // the bytes it holds. The compressed body is built once and kept.
        let packed = headers
            .get("accept-encoding")
            .filter(|value| accepts_gzip(Some(value)))
            .and_then(|_| Some(asset.gzip()).filter(|gzip| !gzip.is_empty()))
            .map(|gzip| (asset.packed_etag(), gzip, "Content-Encoding: gzip\r\n"));
        let (body, etag, encoding) = match packed {
            Some((etag, gzip, encoding)) => (gzip.to_vec(), etag, encoding),
            None => (asset.body.as_bytes().to_vec(), asset.etag(), ""),
        };
        let immutable = format!(
            "{}Cache-Control: public, max-age=31536000, immutable\r\nVary: Accept-Encoding\r\n",
            security_headers("'none'", "no-referrer")
        );
        let wants = Conditional::of(headers, &etag);
        if wants.holds() {
            return Some(Self::Unchanged(format!("{immutable}ETag: {etag}\r\n")));
        }
        let immutable = format!("{immutable}ETag: {etag}\r\n");
        Some(Self::Full {
            status: "200 OK",
            kind: asset.kind(),
            encoding,
            body,
            extra: immutable,
        })
    }

    /// The status line and headers, and then the body.
    pub(crate) fn head(&self) -> String {
        match self {
            Self::Unchanged(extra) => {
                format!("HTTP/1.1 304 Not Modified\r\nContent-Length: 0\r\n{extra}Connection: close\r\n\r\n")
            }
            Self::Full {
                status,
                kind,
                encoding,
                body,
                extra,
            } => format!(
                "HTTP/1.1 {status}\r\nContent-Type: {kind}; charset=utf-8\r\nContent-Length: {}\r\n{encoding}{extra}Connection: close\r\n\r\n",
                body.len()
            ),
        }
    }

    pub(crate) fn body(&self) -> &[u8] {
        match self {
            Self::Full { body, .. } => body,
            Self::Unchanged(_) => &[],
        }
    }

    #[cfg(test)]
    pub(crate) fn unchanged(&self) -> bool {
        matches!(self, Self::Unchanged(_))
    }
}

/// The validator of one page response: the page's identity and the digest of the exact bytes,
/// so a revalidation never substitutes a fragment for a document or another page's body.
pub(crate) fn etag(page: &str, fragment: bool, body: &str) -> String {
    let digest = format!("{:x}", Sha256::digest(body.as_bytes()));
    // The path is request text: only its characters are kept, so a header never carries more
    // than one line or a byte a client would read as another header.
    let page: String = page
        .chars()
        .take(64)
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || "-_/?=&".contains(ch) {
                ch
            } else {
                '_'
            }
        })
        .collect();
    format!(
        "\"{page}:{}:{}\"",
        if fragment { "f" } else { "d" },
        &digest[..32]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(value: &str) -> Headers<'_> {
        // The parser takes the block between the request line and the empty line.
        oxa_http::server::parse_headers(value.trim_end_matches("\r\n")).unwrap()
    }

    fn gunzip(bytes: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        std::io::Read::read_to_end(&mut flate2::read::GzDecoder::new(bytes), &mut out).unwrap();
        out
    }

    #[test]
    fn an_unchanged_page_costs_one_revalidated_request() {
        let body = "<!doctype html><title>x</title>".repeat(40);
        let tag = etag("home", false, &body);
        let first = Reply::page(
            "200 OK",
            body.clone(),
            &tag,
            &headers(""),
            "'none'",
            "no-referrer",
        );
        assert!(!first.unchanged());
        assert!(first.body().len() > 100);
        let repeat = Reply::page(
            "200 OK",
            body.clone(),
            &tag,
            &headers(&format!(
                "If-None-Match: \"other\", {tag}\r\nAccept-Encoding: gzip\r\n"
            )),
            "'none'",
            "no-referrer",
        );
        assert!(repeat.unchanged());
        // The `304` keeps the page's security and caching headers and carries no body.
        let head = repeat.head();
        assert!(head.starts_with("HTTP/1.1 304 Not Modified\r\n"), "{head}");
        assert!(head.ends_with("\r\n\r\n"), "{head}");
        assert!(
            head.contains("Content-Security-Policy: default-src 'none'"),
            "{head}"
        );
        assert!(head.contains("Cache-Control: no-store"), "{head}");
        assert!(head.contains(&format!("ETag: {tag}")), "{head}");
        assert!(repeat.body().is_empty());
        // Only the bytes the client would receive answer `304`: a weak tag may vouch for a
        // different encoding, and `*` states nothing about this body.
        for other in ["*", "\"other\"", "W/\"{body_digest}\""] {
            let other = other.replace("{body_digest}", tag.trim_matches('"').rsplit(':').next().unwrap());
            let reply = Reply::page(
                "200 OK",
                body.clone(),
                &tag,
                &headers(&format!("If-None-Match: {other}\r\n")),
                "'none'",
                "no-referrer",
            );
            assert!(!reply.unchanged(), "{other}");
        }
    }

    #[test]
    fn a_fragment_never_validates_a_document() {
        let page = "<main><h1>Status</h1></main>";
        let document = etag("home", false, page);
        let fragment = etag("home", true, page);
        assert_ne!(document, fragment);
        let head = format!("If-None-Match: {fragment}\r\n");
        let request = headers(&head);
        let reply = Reply::page(
            "200 OK",
            page.into(),
            &document,
            &request,
            "'none'",
            "no-referrer",
        );
        assert!(!reply.unchanged());
        // Another page with the same bytes is another document, and its own validator.
        assert_ne!(document, etag("board", false, page));
    }

    #[test]
    fn gzip_is_offered_only_when_it_helps_and_the_client_asks() {
        for (accept, compressed) in [
            (None, false),
            (Some(""), false),
            (Some("gzip;q=0"), false),
            (Some("br"), false),
            (Some("deflate"), false),
            (Some("gzip"), true),
            (Some("br;q=1, GZIP ; q=0.5"), true),
            // `*` answers for a coding the request does not name, gzip included.
            (Some("*"), true),
            (Some("*;q=0"), false),
            (Some("gzip, *;q=0"), true),
            // A named coding decides its own weight, even when `*` would allow it.
            (Some("gzip;q=0, *"), false),
            (Some("gzip;q=0.5, gzip;q=0"), true),
        ] {
            assert_eq!(accepts_gzip(accept), compressed, "{accept:?}");
        }
        let page = "<p class=muted>one row</p>".repeat(200);
        let tag = etag("board", false, &page);
        let packed = Reply::page(
            "200 OK",
            page.clone(),
            &tag,
            &headers("Accept-Encoding: gzip"),
            "'none'",
            "no-referrer",
        );
        let head = packed.head();
        assert!(head.contains("Content-Encoding: gzip"), "{head}");
        assert!(head.contains(&format!("Content-Length: {}", packed.body().len())));
        assert_eq!(gunzip(packed.body()), page.as_bytes());
        assert!(
            (packed.body().len() as f64) < (page.len() as f64) * 0.4,
            "{} of {}",
            packed.body().len(),
            page.len()
        );
        // The same bytes every time, so the validator over them never drifts.
        let again = Reply::page(
            "200 OK",
            page.clone(),
            &tag,
            &headers("Accept-Encoding: gzip"),
            "'none'",
            "no-referrer",
        );
        assert_eq!(packed.body(), again.body());
        // Either representation answers to the same validator; the body decides the encoding.
        let plain = Reply::page("200 OK", page, &tag, &headers(""), "'none'", "no-referrer");
        assert!(!plain.head().contains("Content-Encoding"), "{}", plain.head());
        let revalidated = Reply::page(
            "200 OK",
            String::new(),
            &tag,
            &headers(&format!("If-None-Match: {tag}")),
            "'none'",
            "no-referrer",
        );
        assert!(revalidated.unchanged());
    }

    #[test]
    fn an_asset_is_immutable_revalidatable_and_identical_for_every_reader() {
        let style = style_path();
        let digest = format!("{:x}", Sha256::digest(lead_page::STYLE.as_bytes()));
        assert_eq!(style, format!("/a/{}/style.css", &digest[..8]));
        assert_eq!(
            script_path(),
            format!("/a/{}/live.js", {
                let digest = format!("{:x}", Sha256::digest(lead_page::SCRIPT.as_bytes()));
                digest[..8].to_owned()
            })
        );
        let first = Reply::asset(&style, &headers("")).unwrap();
        let head = first.head();
        assert!(
            head.contains("Cache-Control: public, max-age=31536000, immutable"),
            "{head}"
        );
        assert!(head.contains("Content-Type: text/css;"), "{head}");
        assert!(
            head.contains("Content-Security-Policy: default-src 'none'"),
            "{head}"
        );
        assert!(first.body() == lead_page::STYLE.as_bytes());
        // A client that accepts gzip gets the compressed bytes, under their own validator.
        let packed = Reply::asset(&style, &headers("Accept-Encoding: gzip")).unwrap();
        assert!(packed.head().contains("Content-Encoding: gzip"));
        assert!(packed.body().len() < lead_page::STYLE.len());
        assert_eq!(gunzip(packed.body()), lead_page::STYLE.as_bytes());
        assert_ne!(
            packed.head().lines().find(|line| line.starts_with("ETag:")),
            head.lines().find(|line| line.starts_with("ETag:"))
        );
        // Each representation revalidates itself, and only its own validator holds.
        for (reply, encoding) in [(&first, false), (&packed, true)] {
            let tag = reply
                .head()
                .lines()
                .find_map(|line| line.strip_prefix("ETag: "))
                .unwrap()
                .to_owned();
            for (accept, holds) in [("", !encoding), ("Accept-Encoding: gzip", encoding)] {
                let head = format!("If-None-Match: {tag}\r\n{accept}");
                let again = Reply::asset(&style, &headers(&head)).unwrap();
                assert_eq!(again.unchanged(), holds, "{head}");
                if holds {
                    assert!(again.head().contains("immutable"));
                    assert!(!again.head().contains("Content-Encoding"));
                }
            }
        }
        // The script is its own asset, reachable only at its own hashed path.
        let script = Reply::asset(&script_path(), &headers("Accept-Encoding: gzip;q=0")).unwrap();
        assert!(script.head().contains("text/javascript"));
        assert!(!script.head().contains("Content-Encoding"));
        for path in ["/style.css", "/live.js", "/a/00000000/style.css", "/a/", ""] {
            assert!(Reply::asset(path, &headers("")).is_none(), "{path}");
        }
    }
}
