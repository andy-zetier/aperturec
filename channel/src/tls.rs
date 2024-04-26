use anyhow::Result;
use openssl::asn1::Asn1Time;
use openssl::ec::{EcGroup, EcKey};
use openssl::nid::Nid;
use openssl::pkey::{PKey, Private};
use openssl::x509::extension::SubjectAlternativeName;
use openssl::x509::{X509NameBuilder, X509};
use std::fs;
use std::net::{Ipv4Addr, Ipv6Addr};
use std::path::Path;

pub struct DerMaterial {
    pub certificate: Vec<u8>,
    pub pkey: Vec<u8>,
}

pub struct PemMaterial {
    pub certificate: String,
    pub pkey: String,
}

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
    pub fn from_pem_files(certificate: &Path, private_key: &Path) -> Result<Self> {
        let certificate = X509::from_pem(&fs::read(certificate)?)?;
        let pkey = PKey::private_key_from_pem(&fs::read(private_key)?)?;

        Ok(Material { certificate, pkey })
    }

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

        builder.set_not_before(&Asn1Time::from_unix(0)?.as_ref())?;
        builder.set_not_after(&Asn1Time::days_from_now(65536)?.as_ref())?;

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
        // Always allow localhost
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
}

#[cfg(feature = "test-tls-material")]
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

    pub static RAW: Lazy<Material> = Lazy::new(|| Material::ec_self_signed_localhost_only());
    pub static DER: Lazy<DerMaterial> = Lazy::new(|| RAW.clone().try_into().expect("DER"));
    pub static PEM: Lazy<PemMaterial> = Lazy::new(|| RAW.clone().try_into().expect("PEM"));
}
