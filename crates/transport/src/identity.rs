use std::{fmt, fs, path::Path, str::FromStr};

use sha2::{Digest, Sha256};

use crate::Error;

/// SHA-256 of a DER certificate. Peers are identified by this, never by name.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Fingerprint(pub [u8; 32]);

impl Fingerprint {
    pub fn of(cert_der: &[u8]) -> Self {
        Self(Sha256::digest(cert_der).into())
    }

    /// Constant-time comparison, so timing does not leak how many bytes matched.
    pub fn ct_eq(&self, other: &Self) -> bool {
        self.0.iter().zip(other.0.iter()).fold(0u8, |acc, (a, b)| acc | (a ^ b)) == 0
    }
}

impl fmt::Display for Fingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&hex::encode(self.0))
    }
}

impl fmt::Debug for Fingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Fingerprint({self})")
    }
}

impl FromStr for Fingerprint {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self, Error> {
        let bytes = hex::decode(s.trim()).map_err(|_| Error::BadFingerprint)?;
        Ok(Self(bytes.try_into().map_err(|_| Error::BadFingerprint)?))
    }
}

/// Self-signed certificate plus private key, both DER.
#[derive(Clone)]
pub struct Identity {
    pub cert_der: Vec<u8>,
    pub key_der: Vec<u8>,
}

impl Identity {
    pub fn generate() -> Result<Self, Error> {
        let ck = rcgen::generate_simple_self_signed(vec![crate::SERVER_NAME.to_owned()])?;
        Ok(Self { cert_der: ck.cert.der().to_vec(), key_der: ck.signing_key.serialize_der() })
    }

    pub fn fingerprint(&self) -> Fingerprint {
        Fingerprint::of(&self.cert_der)
    }

    /// Loads `cert.der` / `key.der` from `dir`, creating them on first use.
    pub fn load_or_create(dir: &Path) -> Result<Self, Error> {
        let (cert_path, key_path) = (dir.join("cert.der"), dir.join("key.der"));
        if cert_path.exists() && key_path.exists() {
            return Ok(Self { cert_der: fs::read(cert_path)?, key_der: fs::read(key_path)? });
        }
        let id = Self::generate()?;
        create_private_dir(dir)?;
        write_private(&key_path, &id.key_der)?;
        fs::write(&cert_path, &id.cert_der)?;
        Ok(id)
    }
}

/// Creates `dir` (and parents) readable only by the current user.
pub fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// Writes a secret file with 0600 permissions, atomically via rename.
pub fn write_private(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    {
        use std::io::Write;
        let mut f = opts.open(&tmp)?;
        f.write_all(data)?;
        f.sync_all()?;
    }
    fs::rename(tmp, path)
}

/// Random URL-safe token for pairing.
pub fn random_token() -> String {
    hex::encode(rand::random::<[u8; 16]>())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_parse_roundtrip() {
        let id = Identity::generate().unwrap();
        let fp = id.fingerprint();
        let parsed: Fingerprint = fp.to_string().parse().unwrap();
        assert!(fp.ct_eq(&parsed));
        assert!("abcd".parse::<Fingerprint>().is_err());
    }

    #[test]
    fn load_or_create_is_stable() {
        let dir = std::env::temp_dir().join(format!("umac-id-test-{}", std::process::id()));
        let a = Identity::load_or_create(&dir).unwrap();
        let b = Identity::load_or_create(&dir).unwrap();
        assert_eq!(a.fingerprint(), b.fingerprint());
        fs::remove_dir_all(dir).unwrap();
    }
}
