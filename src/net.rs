//! Outbound TLS trust.
//!
//! Every HTTPS client trusts the OS store and the bundled Mozilla roots (both reqwest
//! features are on), plus any extra CA bundles named by environment variables, so a
//! TLS-inspecting corporate proxy works once its root CA is supplied. Nothing here
//! writes to stdout: `sharepoint mcp` speaks JSON-RPC there.

use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::{Arc, OnceLock},
};

use anyhow::{anyhow, Result};
use rustls::pki_types::{pem::PemObject, CertificateDer};

/// Variables read for extra CA bundles, in load order. Ours are required: a bad path
/// is an error. The ambient ones are set for other tools, so a bad path only warns.
const EXTRA_CA_VARS: [(&str, bool); 4] = [
    ("SHAREPOINT_EXTRA_CA_PEM", true),
    ("ONEDRIVE_EXTRA_CA_PEM", true),
    ("SSL_CERT_FILE", false),
    ("NODE_EXTRA_CA_CERTS", false),
];

/// Appended to a transport error whose certificate chain ends at an unknown authority.
pub const UNKNOWN_ISSUER_HINT: &str = "TLS certificate was issued by an unknown authority. Behind a corporate proxy, install the proxy root CA in the OS trust store, or set SHAREPOINT_EXTRA_CA_PEM=<path-to-ca.pem> (SSL_CERT_FILE and NODE_EXTRA_CA_CERTS are also read)";

/// One CA bundle named by an environment variable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtraCaPath {
    pub var: &'static str,
    pub path: PathBuf,
    /// A failure to load it is an error rather than a warning.
    pub required: bool,
}

/// Certificates from every bundle that loaded, and one warning per ambient bundle that
/// did not.
#[derive(Debug, Default)]
pub struct ExtraCas {
    pub certs: Vec<CertificateDer<'static>>,
    pub warnings: Vec<String>,
}

/// Every set variable, in `EXTRA_CA_VARS` order, without repeating a path.
pub fn extra_ca_paths(lookup: impl Fn(&str) -> Option<String>) -> Vec<ExtraCaPath> {
    let mut seen = HashSet::new();
    let mut paths = Vec::new();
    for (var, required) in EXTRA_CA_VARS {
        let Some(value) = lookup(var) else { continue };
        let value = value.trim();
        if value.is_empty() {
            continue;
        }
        let path = PathBuf::from(value);
        if seen.insert(path.clone()) {
            paths.push(ExtraCaPath { var, path, required });
        }
    }
    paths
}

/// Reads each bundle. A required bundle that cannot be used fails the whole load, naming
/// the variable and the path (never the file's contents).
pub fn load_extra_ca_certs(paths: &[ExtraCaPath]) -> Result<ExtraCas> {
    let mut loaded = ExtraCas::default();
    for entry in paths {
        match read_pem_bundle(&entry.path) {
            Ok(certs) => loaded.certs.extend(certs),
            Err(reason) if entry.required => {
                return Err(anyhow!(
                    "{}={}: cannot use this CA bundle: {reason}",
                    entry.var,
                    entry.path.display()
                ));
            }
            Err(reason) => loaded.warnings.push(format!(
                "warning: ignoring {}={}: {reason}",
                entry.var,
                entry.path.display()
            )),
        }
    }
    Ok(loaded)
}

fn read_pem_bundle(path: &Path) -> std::result::Result<Vec<CertificateDer<'static>>, String> {
    let bytes = std::fs::read(path).map_err(|err| err.to_string())?;
    let certs = CertificateDer::pem_slice_iter(&bytes)
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|err| format!("not a PEM certificate bundle ({err:?})"))?;
    if certs.is_empty() {
        return Err("no PEM certificates found".into());
    }
    // Parse each as a trust anchor now, so a bad bundle is reported here, once, with its
    // variable name rather than as an opaque failure on the first request.
    let mut probe = rustls::RootCertStore::empty();
    for cert in &certs {
        probe
            .add(cert.clone())
            .map_err(|err| format!("certificate is not usable as a root ({err})"))?;
    }
    Ok(certs)
}

/// The extra CAs from the process environment, loaded on first use. Warnings go to
/// stderr once.
pub fn extra_ca_certs() -> Result<&'static [CertificateDer<'static>]> {
    static LOADED: OnceLock<std::result::Result<Vec<CertificateDer<'static>>, String>> =
        OnceLock::new();
    let loaded = LOADED.get_or_init(|| {
        let paths = extra_ca_paths(|var| std::env::var(var).ok());
        match load_extra_ca_certs(&paths) {
            Ok(cas) => {
                for warning in &cas.warnings {
                    eprintln!("{warning}");
                }
                Ok(cas.certs)
            }
            Err(err) => Err(format!("{err:#}")),
        }
    });
    match loaded {
        Ok(certs) => Ok(certs),
        Err(msg) => Err(anyhow!("{msg}")),
    }
}

/// Adds the extra CAs to a reqwest client. The OS and bundled roots come from features.
pub fn apply_tls_roots(builder: reqwest::ClientBuilder) -> Result<reqwest::ClientBuilder> {
    with_extra_roots(builder, extra_ca_certs()?)
}

fn with_extra_roots(
    mut builder: reqwest::ClientBuilder,
    certs: &[CertificateDer<'static>],
) -> Result<reqwest::ClientBuilder> {
    for cert in certs {
        builder = builder.add_root_certificate(reqwest::Certificate::from_der(cert.as_ref())?);
    }
    Ok(builder)
}

/// A rustls config for the realtime WebSocket when extra CAs are configured; `None`
/// keeps tokio-tungstenite's own OS + bundled roots.
pub fn websocket_tls_config() -> Result<Option<Arc<rustls::ClientConfig>>> {
    let extra = extra_ca_certs()?;
    if extra.is_empty() {
        return Ok(None);
    }
    let mut roots = rustls::RootCertStore::empty();
    roots.add_parsable_certificates(rustls_native_certs::load_native_certs().certs);
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    for cert in extra {
        roots.add(cert.clone())?;
    }
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()?
    .with_root_certificates(roots)
    .with_no_client_auth();
    Ok(Some(Arc::new(config)))
}

/// Puts the recovery hint in front of an error whose cause is an unknown certificate
/// issuer. The original error stays underneath, so its chain and downcasts still work.
pub fn annotate_tls_error(err: anyhow::Error) -> anyhow::Error {
    if is_unknown_issuer(&err) {
        err.context(UNKNOWN_ISSUER_HINT)
    } else {
        err
    }
}

fn is_unknown_issuer(err: &anyhow::Error) -> bool {
    fn matches_rustls(e: &(dyn std::error::Error + 'static)) -> bool {
        matches!(
            e.downcast_ref::<rustls::Error>(),
            Some(rustls::Error::InvalidCertificate(rustls::CertificateError::UnknownIssuer))
        )
    }
    err.chain().any(|cause| {
        if matches_rustls(cause) {
            return true;
        }
        // hyper-rustls hands the TLS error over inside an io::Error, whose `source()`
        // skips the wrapped error itself.
        if let Some(inner) = cause
            .downcast_ref::<std::io::Error>()
            .and_then(|io| io.get_ref())
        {
            if matches_rustls(inner) {
                return true;
            }
        }
        format!("{cause:?}").contains("UnknownIssuer")
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/test-ca.pem");

    fn lookup(vars: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> =
            vars.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        move |key| map.get(key).cloned()
    }

    fn entry(var: &'static str, path: &str, required: bool) -> ExtraCaPath {
        ExtraCaPath { var, path: PathBuf::from(path), required }
    }

    #[test]
    fn extra_ca_paths_collects_all_set_variables_in_order() {
        let paths = extra_ca_paths(lookup(&[
            ("NODE_EXTRA_CA_CERTS", "/etc/node.pem"),
            ("SSL_CERT_FILE", "/etc/ssl.pem"),
            ("ONEDRIVE_EXTRA_CA_PEM", "/etc/corp.pem"),
            ("SHAREPOINT_EXTRA_CA_PEM", "/etc/corp.pem"),
        ]));
        assert_eq!(
            paths,
            vec![
                entry("SHAREPOINT_EXTRA_CA_PEM", "/etc/corp.pem", true),
                entry("SSL_CERT_FILE", "/etc/ssl.pem", false),
                entry("NODE_EXTRA_CA_CERTS", "/etc/node.pem", false),
            ],
            "every set variable is read, a repeated path once"
        );
        assert!(extra_ca_paths(lookup(&[("SSL_CERT_FILE", "  ")])).is_empty());
        assert!(extra_ca_paths(lookup(&[])).is_empty());
    }

    #[test]
    fn load_extra_ca_certs_accepts_bundle() {
        let cas = load_extra_ca_certs(&[entry("SHAREPOINT_EXTRA_CA_PEM", FIXTURE, true)]).unwrap();
        assert_eq!(cas.certs.len(), 2);
        assert!(cas.warnings.is_empty());
        let builder = with_extra_roots(reqwest::Client::builder(), &cas.certs).unwrap();
        builder.build().expect("a client trusting the bundle builds");
    }

    #[test]
    fn load_extra_ca_certs_rejects_garbage() {
        let dir = tempfile::tempdir().unwrap();
        let garbage = dir.path().join("garbage.pem");
        std::fs::write(&garbage, "not a certificate\n").unwrap();
        let bad_body = dir.path().join("bad-body.pem");
        std::fs::write(
            &bad_body,
            "-----BEGIN CERTIFICATE-----\nAAAA\n-----END CERTIFICATE-----\n",
        )
        .unwrap();
        for path in [&garbage, &bad_body] {
            let err = load_extra_ca_certs(&[entry(
                "SHAREPOINT_EXTRA_CA_PEM",
                path.to_str().unwrap(),
                true,
            )])
            .unwrap_err()
            .to_string();
            assert!(err.starts_with("SHAREPOINT_EXTRA_CA_PEM="), "{err}");
            assert!(!err.contains("AAAA"), "file contents never appear: {err}");
        }
    }

    #[test]
    fn missing_file_is_hard_error_for_sharepoint_var() {
        for var in ["SHAREPOINT_EXTRA_CA_PEM", "ONEDRIVE_EXTRA_CA_PEM"] {
            let paths = extra_ca_paths(lookup(&[(var, "/no/such/ca.pem")]));
            let err = load_extra_ca_certs(&paths).unwrap_err().to_string();
            assert!(err.contains(var) && err.contains("/no/such/ca.pem"), "{err}");
        }
    }

    #[test]
    fn missing_file_is_warning_for_ssl_cert_file() {
        let paths = extra_ca_paths(lookup(&[
            ("SSL_CERT_FILE", "/no/such/ssl.pem"),
            ("NODE_EXTRA_CA_CERTS", FIXTURE),
        ]));
        let cas = load_extra_ca_certs(&paths).unwrap();
        assert_eq!(cas.certs.len(), 2, "the readable bundle still loads");
        assert_eq!(cas.warnings.len(), 1);
        assert!(cas.warnings[0].contains("SSL_CERT_FILE=/no/such/ssl.pem"), "{:?}", cas.warnings);
    }

    #[test]
    fn unknown_issuer_is_detected_and_hinted() {
        let tls = rustls::Error::InvalidCertificate(rustls::CertificateError::UnknownIssuer);
        let err = anyhow::Error::new(tls)
            .context("error sending request")
            .context("Graph HTTP request failed (GET https://graph.microsoft.com/v1.0/me)");
        let annotated = annotate_tls_error(err);
        let text = format!("{annotated:#}");
        assert!(text.contains("SHAREPOINT_EXTRA_CA_PEM"), "{text}");
        assert!(text.contains("Graph HTTP request failed"), "the original message stays: {text}");
        assert!(annotated.downcast_ref::<rustls::Error>().is_some(), "the chain is kept");

        // The shape hyper-rustls produces: the TLS error inside an io::Error.
        let io = std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            rustls::Error::InvalidCertificate(rustls::CertificateError::UnknownIssuer),
        );
        let wrapped = annotate_tls_error(anyhow::Error::new(io).context("connect"));
        assert!(format!("{wrapped:#}").contains(UNKNOWN_ISSUER_HINT));

        let other = anyhow!("connection refused").context("Graph HTTP request failed");
        let before = format!("{other:#}");
        assert_eq!(format!("{:#}", annotate_tls_error(other)), before);
        let expired = anyhow::Error::new(rustls::Error::InvalidCertificate(
            rustls::CertificateError::Expired,
        ));
        assert!(!format!("{:#}", annotate_tls_error(expired)).contains(UNKNOWN_ISSUER_HINT));
    }
}
