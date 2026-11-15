//! Optional shared-listener TLS; parsing failures never fall back to plaintext.
use serde_json::Value;
use std::{fs::File, io::BufReader, path::Path, sync::Arc};
use tokio_rustls::TlsAcceptor;

pub fn settings(config: &Value) -> Option<(String, String)> {
    let options = &config["lan_share_options"];
    let cert = options["tls_cert"]
        .as_str()
        .filter(|s| Path::new(s).is_absolute())?;
    let key = options["tls_key"]
        .as_str()
        .filter(|s| Path::new(s).is_absolute())?;
    Some((cert.into(), key.into()))
}
pub fn load(cert: &str, key: &str) -> Result<TlsAcceptor, ()> {
    let mut certs = BufReader::new(File::open(cert).map_err(|_| ())?);
    let certs = rustls_pemfile::certs(&mut certs)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| ())?;
    let mut key = BufReader::new(File::open(key).map_err(|_| ())?);
    let key = rustls_pemfile::private_key(&mut key)
        .map_err(|_| ())?
        .ok_or(())?;
    let server = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_protocol_versions(&[&rustls::version::TLS13, &rustls::version::TLS12])
    .map_err(|_| ())?
    .with_no_client_auth()
    .with_single_cert(certs, key)
    .map_err(|_| ())?;
    Ok(TlsAcceptor::from(Arc::new(server)))
}
