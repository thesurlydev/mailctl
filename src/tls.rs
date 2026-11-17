//! TLS for the IMAP connection, on rustls.
//!
//! Trust is the bundled Mozilla roots plus, optionally, one CA file the user
//! names with `ca_cert`. Bridge's own certificate is self-signed, so a setup
//! that runs Bridge in SSL mode exports it from Bridge's settings and points
//! `ca_cert` at it. There is deliberately no switch that turns verification off.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context as _, Result};
use rustls::RootCertStore;
use rustls::pki_types::pem::PemObject as _;
use rustls::pki_types::{CertificateDer, ServerName};
use tokio_rustls::TlsConnector;

pub fn connector(ca_cert: Option<&Path>) -> Result<TlsConnector> {
    let mut roots = RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    };
    if let Some(path) = ca_cert {
        let mut added = 0;
        for cert in CertificateDer::pem_file_iter(path)
            .with_context(|| format!("reading ca_cert {}", path.display()))?
        {
            let cert = cert.with_context(|| format!("parsing ca_cert {}", path.display()))?;
            roots
                .add(cert)
                .with_context(|| format!("trusting a certificate from {}", path.display()))?;
            added += 1;
        }
        if added == 0 {
            anyhow::bail!("ca_cert {} holds no PEM certificate", path.display());
        }
    }

    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .context("choosing TLS protocol versions")?
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(TlsConnector::from(Arc::new(config)))
}

/// The name the server certificate is checked against: a DNS name or an IP.
pub fn server_name(host: &str) -> Result<ServerName<'static>> {
    ServerName::try_from(host.to_string()).with_context(|| format!("{host} is not a valid host"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connector_builds_without_a_custom_ca() {
        connector(None).unwrap();
    }

    #[test]
    fn a_missing_ca_file_is_an_error_naming_the_path() {
        let err = connector(Some(Path::new("/nonexistent/bridge.pem")))
            .err()
            .unwrap();
        assert!(format!("{err:#}").contains("/nonexistent/bridge.pem"));
    }

    #[test]
    fn a_file_without_certificates_is_rejected() {
        let path =
            std::env::temp_dir().join(format!("mailctl-empty-ca-{}.pem", std::process::id()));
        std::fs::write(&path, "not a certificate\n").unwrap();
        let err = connector(Some(&path)).err().unwrap();
        std::fs::remove_file(&path).ok();
        assert!(format!("{err:#}").contains("no PEM certificate"));
    }

    #[test]
    fn server_name_accepts_dns_names_and_ip_addresses() {
        assert!(server_name("imap.example.com").is_ok());
        assert!(server_name("127.0.0.1").is_ok());
        assert!(server_name("").is_err());
    }
}
