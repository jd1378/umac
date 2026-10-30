//! Paired clients and one-time pairing tokens, persisted as 0600 JSON files.
//!
//! `umac-host pair` writes a token; the running agent re-reads the file on each pairing
//! attempt, so no IPC is needed between the two processes.

use std::{
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use umac_transport::{Fingerprint, identity::write_private};

pub const TOKEN_TTL: Duration = Duration::from_secs(10 * 60);

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PairedClient {
    pub fingerprint: String,
    pub name: String,
    pub paired_at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct PendingToken {
    token: String,
    expires_at: u64,
}

pub struct AuthStore {
    clients_path: PathBuf,
    tokens_path: PathBuf,
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn read_json<T: for<'de> Deserialize<'de> + Default>(path: &Path) -> T {
    std::fs::read(path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

fn write_json<T: Serialize>(path: &Path, value: &T) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        umac_transport::identity::create_private_dir(dir)?;
    }
    write_private(path, &serde_json::to_vec_pretty(value).map_err(std::io::Error::other)?)
}

impl AuthStore {
    pub fn new(dir: &Path) -> Self {
        Self { clients_path: dir.join("authorized_clients.json"), tokens_path: dir.join("pairing_tokens.json") }
    }

    pub fn clients(&self) -> Vec<PairedClient> {
        read_json(&self.clients_path)
    }

    pub fn is_authorized(&self, fp: &Fingerprint) -> bool {
        let fp = fp.to_string();
        self.clients().iter().any(|c| ct_eq(c.fingerprint.as_bytes(), fp.as_bytes()))
    }

    /// Creates a token valid for [`TOKEN_TTL`]. Expired tokens are pruned on the way.
    pub fn issue_token(&self) -> std::io::Result<String> {
        let now = now_secs();
        let mut tokens: Vec<PendingToken> = read_json(&self.tokens_path);
        tokens.retain(|t| t.expires_at > now);
        let token = umac_transport::identity::random_token();
        tokens.push(PendingToken { token: token.clone(), expires_at: now + TOKEN_TTL.as_secs() });
        write_json(&self.tokens_path, &tokens)?;
        Ok(token)
    }

    /// Consumes a valid token and trusts `fp` from now on. Returns false if the token is
    /// unknown or expired.
    pub fn redeem(&self, token: &str, fp: &Fingerprint, name: &str) -> std::io::Result<bool> {
        let now = now_secs();
        let mut tokens: Vec<PendingToken> = read_json(&self.tokens_path);
        tokens.retain(|t| t.expires_at > now);
        // Compare against every token so timing does not reveal which (if any) matched.
        let hit = tokens.iter().fold(None, |acc, t| if ct_eq(t.token.as_bytes(), token.as_bytes()) { Some(t.token.clone()) } else { acc });
        let Some(hit) = hit else {
            write_json(&self.tokens_path, &tokens)?;
            return Ok(false);
        };
        tokens.retain(|t| t.token != hit);
        write_json(&self.tokens_path, &tokens)?;

        let mut clients = self.clients();
        let fp = fp.to_string();
        if !clients.iter().any(|c| c.fingerprint == fp) {
            let name: String = name.chars().filter(|c| !c.is_control()).take(64).collect();
            clients.push(PairedClient { fingerprint: fp, name, paired_at: now });
            write_json(&self.clients_path, &clients)?;
        }
        Ok(true)
    }

    pub fn remove_client(&self, fingerprint_prefix: &str) -> std::io::Result<usize> {
        let mut clients = self.clients();
        let before = clients.len();
        clients.retain(|c| !c.fingerprint.starts_with(fingerprint_prefix));
        write_json(&self.clients_path, &clients)?;
        Ok(before - clients.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (AuthStore, PathBuf) {
        let dir = std::env::temp_dir().join(format!("umac-auth-{}-{}", std::process::id(), rand_suffix()));
        (AuthStore::new(&dir), dir)
    }

    fn rand_suffix() -> String {
        umac_transport::identity::random_token()[..8].to_owned()
    }

    #[test]
    fn token_is_single_use_and_authorizes() {
        let (s, dir) = store();
        let fp = Fingerprint([7; 32]);
        assert!(!s.is_authorized(&fp));
        let t = s.issue_token().unwrap();
        assert!(!s.redeem("wrong", &fp, "pc").unwrap());
        assert!(s.redeem(&t, &fp, "pc").unwrap());
        assert!(s.is_authorized(&fp));
        assert!(!s.redeem(&t, &Fingerprint([8; 32]), "other").unwrap());
        assert_eq!(s.remove_client(&fp.to_string()[..10]).unwrap(), 1);
        assert!(!s.is_authorized(&fp));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn files_are_private() {
        use std::os::unix::fs::PermissionsExt;
        let (s, dir) = store();
        s.issue_token().unwrap();
        let mode = std::fs::metadata(dir.join("pairing_tokens.json")).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn expired_token_rejected() {
        let (s, dir) = store();
        let expired = vec![PendingToken { token: "aa".into(), expires_at: 1 }];
        write_json(&s.tokens_path, &expired).unwrap();
        assert!(!s.redeem("aa", &Fingerprint([1; 32]), "pc").unwrap());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
