use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use rcgen::{
    BasicConstraints, CertificateParams, DistinguishedName, DnType, IsCa, KeyPair, KeyUsagePurpose,
    SanType,
};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};

const ROOT_CRT_FILE: &str = "root.pem";
const ROOT_KEY_FILE: &str = "root.key.pem";

pub struct ProxyCa {
    dir: PathBuf,
    root_cert_pem: String,
    root_cert: rcgen::Certificate,
    root_key: KeyPair,
    leaf_cache: Mutex<HashMap<String, Arc<LeafCert>>>,
}

pub struct LeafCert {
    pub cert_chain: Vec<CertificateDer<'static>>,
    pub private_key: PrivateKeyDer<'static>,
}

impl ProxyCa {
    pub fn load_or_create(dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(dir).with_context(|| format!("create CA dir {}", dir.display()))?;
        let crt = dir.join(ROOT_CRT_FILE);
        let key = dir.join(ROOT_KEY_FILE);

        if crt.exists() && key.exists() {
            let cert_pem = std::fs::read_to_string(&crt)?;
            let key_pem = std::fs::read_to_string(&key)?;
            let kp = KeyPair::from_pem(&key_pem).context("parse CA key pem")?;
            let params =
                CertificateParams::from_ca_cert_pem(&cert_pem).context("parse CA cert pem")?;
            let cert = params.self_signed(&kp).context("re-sign CA params")?;
            return Ok(Self {
                dir: dir.to_path_buf(),
                root_cert_pem: cert_pem,
                root_cert: cert,
                root_key: kp,
                leaf_cache: Mutex::new(HashMap::new()),
            });
        }

        let mut params = CertificateParams::default();
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params.distinguished_name = DistinguishedName::new();
        params
            .distinguished_name
            .push(DnType::CommonName, "agent-payment-sandbox local CA");
        params
            .distinguished_name
            .push(DnType::OrganizationName, "agent-payment-sandbox");
        params.key_usages = vec![
            KeyUsagePurpose::KeyCertSign,
            KeyUsagePurpose::CrlSign,
            KeyUsagePurpose::DigitalSignature,
        ];
        let kp = KeyPair::generate().context("generate CA keypair")?;
        let cert = params.self_signed(&kp).context("self-sign CA")?;
        let cert_pem = cert.pem();
        let key_pem = kp.serialize_pem();
        std::fs::write(&crt, &cert_pem)?;
        std::fs::write(&key, &key_pem)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).ok();
        }

        Ok(Self {
            dir: dir.to_path_buf(),
            root_cert_pem: cert_pem,
            root_cert: cert,
            root_key: kp,
            leaf_cache: Mutex::new(HashMap::new()),
        })
    }

    pub fn cert_path(&self) -> PathBuf {
        self.dir.join(ROOT_CRT_FILE)
    }

    pub fn root_cert_pem(&self) -> &str {
        &self.root_cert_pem
    }

    pub fn leaf_for(&self, host: &str) -> Result<Arc<LeafCert>> {
        if let Some(c) = self.leaf_cache.lock().unwrap().get(host) {
            return Ok(c.clone());
        }
        let leaf = self.mint_leaf(host)?;
        let arc = Arc::new(leaf);
        self.leaf_cache
            .lock()
            .unwrap()
            .insert(host.to_string(), arc.clone());
        Ok(arc)
    }

    fn mint_leaf(&self, host: &str) -> Result<LeafCert> {
        let mut params = CertificateParams::default();
        params.distinguished_name = DistinguishedName::new();
        params
            .distinguished_name
            .push(DnType::CommonName, host.to_string());
        params.subject_alt_names = vec![SanType::DnsName(
            host.to_string().try_into().context("invalid DNS name")?,
        )];
        params.key_usages = vec![
            KeyUsagePurpose::DigitalSignature,
            KeyUsagePurpose::KeyEncipherment,
        ];
        params.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ServerAuth];
        let leaf_kp = KeyPair::generate().context("generate leaf keypair")?;
        let cert = params
            .signed_by(&leaf_kp, &self.root_cert, &self.root_key)
            .context("sign leaf cert")?;
        let cert_der = CertificateDer::from(cert.der().to_vec());
        let root_der = CertificateDer::from(self.root_cert.der().to_vec());
        let key_der = PrivatePkcs8KeyDer::from(leaf_kp.serialize_der());
        Ok(LeafCert {
            cert_chain: vec![cert_der, root_der],
            private_key: PrivateKeyDer::Pkcs8(key_der),
        })
    }
}
