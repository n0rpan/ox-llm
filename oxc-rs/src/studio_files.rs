//! Owner-facing Studio upload and file routes. Upload names never use user paths.
use super::{data, error, reply, Studio};
use axum::{body::Body, response::Response};
use nix::{
    fcntl::{open, openat, OFlag},
    sys::stat::Mode,
};
use serde_json::json;
use std::{
    fs::{self, File},
    io::{self, Read},
    path::{Component, Path, PathBuf},
};

fn find(value: &[u8], needle: &[u8]) -> Option<usize> {
    value.windows(needle.len()).position(|w| w == needle)
}
fn anchored_read(path: &Path) -> io::Result<Vec<u8>> {
    let flags = OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC;
    let mut parent = open(Path::new("/"), flags, Mode::empty())?;
    for component in path.parent().ok_or(io::ErrorKind::NotFound)?.components() {
        match component {
            Component::Normal(name) => parent = openat(&parent, name, flags, Mode::empty())?,
            Component::RootDir => {}
            _ => return Err(io::ErrorKind::InvalidInput.into()),
        }
    }
    let leaf = path.file_name().ok_or(io::ErrorKind::NotFound)?;
    let mut file = File::from(openat(
        &parent,
        leaf,
        OFlag::O_RDONLY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
        Mode::empty(),
    )?);
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(bytes)
}
pub(super) fn silent_close() -> Response {
    let mut response = Response::new(Body::empty());
    response.extensions_mut().insert(crate::transport::NoReply);
    response
}
impl Studio {
    pub(super) fn file(&self, query: &str) -> Response {
        let Ok(url) = reqwest::Url::parse(&format!("http://localhost/?{query}")) else {
            return reply(404, b"not found".to_vec(), "application/json");
        };
        let path = PathBuf::from(
            url.query_pairs()
                .find(|(k, v)| k == "path" && !v.is_empty())
                .map(|(_, v)| v.into_owned())
                .unwrap_or_default(),
        );
        let Ok(resolved) = fs::canonicalize(&path) else {
            return reply(404, b"not found".to_vec(), "application/json");
        };
        if ![self.output.clone(), self.output.join("_uploads")]
            .iter()
            .any(|root| fs::canonicalize(root).is_ok_and(|root| resolved.starts_with(root)))
        {
            return reply(404, b"not found".to_vec(), "application/json");
        }
        match anchored_read(&resolved) {
            Ok(bytes) => reply(
                200,
                bytes,
                if path
                    .extension()
                    .is_some_and(|e| e.to_string_lossy().to_lowercase() == "png")
                {
                    "image/png"
                } else {
                    "image/jpeg"
                },
            ),
            Err(_) => silent_close(),
        }
    }
    pub(super) fn upload(&self, content_type: &[u8], body: &[u8]) -> Response {
        static BOUNDARY: std::sync::OnceLock<regex::bytes::Regex> = std::sync::OnceLock::new();
        static FILENAME: std::sync::OnceLock<regex::bytes::Regex> = std::sync::OnceLock::new();
        let boundary = BOUNDARY.get_or_init(|| {
            regex::bytes::Regex::new(r#"(?-u)boundary=(?:"([^"]+)"|([^;]+))"#)
                .expect("multipart boundary")
        });
        let Some(capture) = boundary.captures(content_type) else {
            return error(400, "bad multipart");
        };
        let boundary = [
            b"--".as_slice(),
            capture
                .get(1)
                .or_else(|| capture.get(2))
                .expect("boundary value")
                .as_bytes(),
        ]
        .concat();
        let filename = FILENAME.get_or_init(|| {
            regex::bytes::Regex::new(r#"(?-u)filename="([^"]*)""#).expect("multipart filename")
        });
        let mut start = 0;
        loop {
            let end = find(&body[start..], &boundary).map_or(body.len(), |i| start + i);
            let part = &body[start..end];
            if find(part, b"filename=\"").is_some() {
                let (head, content) = find(part, b"\r\n\r\n")
                    .map_or((part, b"".as_slice()), |i| (&part[..i], &part[i + 4..]));
                let content = content.strip_suffix(b"\r\n").unwrap_or(content);
                let Some(capture) = filename.captures(head) else {
                    return silent_close();
                };
                let raw = String::from_utf8_lossy(capture.get(1).expect("filename").as_bytes());
                let suffix = Path::new(raw.as_ref())
                    .extension()
                    .map(|s| s.to_string_lossy().to_lowercase())
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| "png".into());
                if !["png", "jpg", "jpeg", "webp"].contains(&suffix.as_str()) {
                    return error(400, "unsupported image type");
                }
                let mut bytes = [0u8; 16];
                if getrandom::fill(&mut bytes).is_err() {
                    return silent_close();
                }
                bytes[6] = (bytes[6] & 0x0f) | 0x40;
                bytes[8] = (bytes[8] & 0x3f) | 0x80;
                let name = bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
                let destination = self
                    .output
                    .join("_uploads")
                    .join(format!("{name}.{suffix}"));
                if fs::write(&destination, content).is_err() {
                    return silent_close();
                }
                return data(200, json!({"path":destination}));
            }
            if end == body.len() {
                break;
            }
            start = end + boundary.len();
        }
        error(400, "no file")
    }
}
