//! TLS utilities for QUIC
use anyhow::Result;
use openssl::asn1::Asn1Time;
use openssl::ec::{EcGroup, EcKey};
use openssl::nid::Nid;
use openssl::pkey::{PKey, Private};
use openssl::x509::extension::SubjectAlternativeName;
use openssl::x509::{X509, X509NameBuilder};
use std::borrow::Cow;
use std::collections::BTreeSet;
use std::fs;
use std::net::{Ipv4Addr, Ipv6Addr};
use std::path::Path;
use std::sync::Arc;

use rustls::Error;
use rustls::client::danger::*;
use rustls::pki_types::*;
#[allow(deprecated)]
use s2n_quic::provider::tls::rustls::rustls;

pub const SSLKEYLOGFILE_VAR: &str = "SSLKEYLOGFILE";

/// Custom certificate verifier allowing built-in and user-provided certificates
#[derive(Debug, Default)]
pub(crate) struct CertVerifier {
    pub(crate) user_provided: Option<Arc<dyn ServerCertVerifier>>,
    pub(crate) allow_insecure_connection: bool,
    platform: rustls_platform_verifier::Verifier,
}

impl ServerCertVerifier for CertVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, Error> {
        if self.allow_insecure_connection {
            return Ok(ServerCertVerified::assertion());
        }

        let up_res = self.user_provided.as_ref().map(|v| {
            v.verify_server_cert(end_entity, intermediates, server_name, ocsp_response, now)
        });
        if let Some(Ok(up_res)) = up_res {
            Ok(up_res)
        } else {
            self.platform.verify_server_cert(
                end_entity,
                intermediates,
                server_name,
                ocsp_response,
                now,
            )
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        if self.allow_insecure_connection {
            return Ok(HandshakeSignatureValid::assertion());
        }

        let up_res = self
            .user_provided
            .as_ref()
            .map(|v| v.verify_tls12_signature(message, cert, dss));
        if let Some(Ok(up_res)) = up_res {
            Ok(up_res)
        } else {
            self.platform.verify_tls12_signature(message, cert, dss)
        }
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        if self.allow_insecure_connection {
            return Ok(HandshakeSignatureValid::assertion());
        }

        let up_res = self
            .user_provided
            .as_ref()
            .map(|v| v.verify_tls12_signature(message, cert, dss));
        if let Some(Ok(up_res)) = up_res {
            Ok(up_res)
        } else {
            self.platform.verify_tls12_signature(message, cert, dss)
        }
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        let mut schemes = self.platform.supported_verify_schemes();
        if let Some(user_provided) = self.user_provided.as_ref() {
            schemes.extend(user_provided.supported_verify_schemes());
        }
        schemes.dedup();
        schemes
    }
}

/// TLS material stored in DER form
pub struct DerMaterial {
    pub certificate: Vec<u8>,
    pub pkey: Vec<u8>,
}

/// TLS material stored in PEM form
pub struct PemMaterial {
    pub certificate: String,
    pub pkey: String,
}

/// TLS material, both the certificate and the public/private key pair
#[derive(Debug, Clone)]
pub struct Material {
    pub certificate: X509,
    pub pkey: PKey<Private>,
}

impl TryFrom<Material> for PemMaterial {
    type Error = anyhow::Error;
    fn try_from(m: Material) -> Result<PemMaterial> {
        Ok(PemMaterial {
            certificate: String::from_utf8(m.certificate.to_pem()?)?,
            pkey: String::from_utf8(m.pkey.private_key_to_pem_pkcs8()?)?,
        })
    }
}

impl TryFrom<Material> for DerMaterial {
    type Error = anyhow::Error;
    fn try_from(m: Material) -> Result<DerMaterial> {
        Ok(DerMaterial {
            certificate: m.certificate.to_der()?,
            pkey: m.pkey.private_key_to_der()?,
        })
    }
}

impl Material {
    /// Load [`Material`] from PEM-formated files
    pub fn from_pem_files(certificate: &Path, private_key: &Path) -> Result<Self> {
        let certificate = X509::from_pem(&fs::read(certificate)?)?;
        let pkey = PKey::private_key_from_pem(&fs::read(private_key)?)?;

        Ok(Material { certificate, pkey })
    }

    /// Generate a self-signed certificate & corresponding eliptic-curve public/private key pair
    ///
    /// The certificate is valid for localhost, and any provided domains and IP addresses
    pub fn ec_self_signed<I: IntoIterator<Item = S>, S: AsRef<str>>(
        domains: I,
        ips: I,
    ) -> Result<Self> {
        let mut group = EcGroup::from_curve_name(Nid::X9_62_PRIME256V1)?;
        group.set_asn1_flag(openssl::ec::Asn1Flag::NAMED_CURVE);
        let key = EcKey::generate(&group)?;
        let pkey = PKey::from_ec_key(key)?;

        let mut builder = X509::builder()?;
        builder.set_version(2)?;
        builder.set_pubkey(&pkey)?;

        builder.set_not_before(Asn1Time::from_unix(0)?.as_ref())?;
        builder.set_not_after(Asn1Time::days_from_now(65536)?.as_ref())?;

        let mut name_builder = X509NameBuilder::new()?;
        name_builder.append_entry_by_text("C", "AC")?;
        name_builder.append_entry_by_text("O", "ApertureC")?;
        name_builder.append_entry_by_text("CN", "ApertureC")?;
        let name = name_builder.build();
        builder.set_issuer_name(&name)?;
        builder.set_subject_name(&name)?;

        let mut san_builder = SubjectAlternativeName::new();
        for domain in domains {
            san_builder.dns(domain.as_ref());
        }
        for ip in ips {
            san_builder.ip(ip.as_ref());
        }
        san_builder
            .dns("localhost")
            .ip(&Ipv4Addr::LOCALHOST.to_string())
            .ip(&Ipv6Addr::LOCALHOST.to_string());
        let subject_alternative_name = san_builder.build(&builder.x509v3_context(None, None))?;
        builder.append_extension(subject_alternative_name)?;

        builder.sign(&pkey, openssl::hash::MessageDigest::sha256())?;

        let certificate = builder.build();

        Ok(Material { certificate, pkey })
    }

    pub fn is_valid_for_sans<I: AsRef<str>>(&self, sans: impl IntoIterator<Item = I>) -> bool {
        if !self.certificate.verify(&self.pkey).unwrap_or(false) {
            return false;
        }

        let cert_sans = match self.certificate.subject_alt_names() {
            Some(sans_stack) => {
                let dnses = sans_stack
                    .iter()
                    .filter_map(|general_name| general_name.dnsname())
                    .map(str::to_string);
                let ips = sans_stack
                    .iter()
                    .filter_map(|general_name| general_name.ipaddress())
                    .map(String::from_utf8_lossy)
                    .map(Cow::into_owned);
                dnses.chain(ips).collect()
            }
            None => BTreeSet::default(),
        };

        sans.into_iter()
            .all(|provided| cert_sans.contains(provided.as_ref()))
    }
}

#[cfg(test)]
pub mod test_material {
    use super::*;

    use once_cell::sync::Lazy;
    use std::iter;

    impl Material {
        fn ec_self_signed_localhost_only() -> Self {
            Self::ec_self_signed(iter::empty::<&str>(), iter::empty::<&str>())
                .expect("create material")
        }
    }

    pub static RAW: Lazy<Material> = Lazy::new(Material::ec_self_signed_localhost_only);
    pub static DER: Lazy<DerMaterial> = Lazy::new(|| RAW.clone().try_into().expect("DER"));
    pub static PEM: Lazy<PemMaterial> = Lazy::new(|| RAW.clone().try_into().expect("PEM"));
}
