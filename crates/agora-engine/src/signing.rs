//! Signed heads: the server signs each ledger head it shows a person, so the person
//! can later prove what the server committed to.
//!
//! A hash chain alone protects against the *agent*: it can't write the ledger. It
//! doesn't protect against whoever runs the server, who could rewrite the whole
//! chain and recompute every hash. A signed head closes most of that gap: the person
//! keeps the receipt (run, entry count, head, time, signature), and if the server
//! later shows a ledger that doesn't reach that head, the receipt proves it changed,
//! since only the server's key could have signed it. Publishing the public key (or a
//! receipt) somewhere the operator doesn't control anchors it further.
//!
//! Ed25519 (RFC 8032), keys from the OS random source.

use std::fmt::Write as _;
use std::path::Path;

use ed25519_dalek::{Signature, Signer as _, SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};

use crate::journal::JournalError;

/// What a signature covers, as text anyone can rebuild.
const DOMAIN: &str = "agora signed ledger head v1";

/// A ledger head the server signed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedHead {
    pub run: String,
    /// The chain had this many entries…
    pub entries: usize,
    /// …ending in this hash.
    pub head: String,
    pub at_ms: u64,
    /// The server's public key (hex).
    pub key: String,
    /// Ed25519 over [`SignedHead::statement`] (hex).
    pub signature: String,
}

impl SignedHead {
    /// The exact bytes signed.
    #[must_use]
    pub fn statement(&self) -> String {
        statement(&self.run, self.entries, &self.head, self.at_ms)
    }

    /// Whether the signature is the key's, over this head.
    ///
    /// # Errors
    /// Why it doesn't verify.
    pub fn verify(&self) -> Result<(), String> {
        let key: [u8; 32] = from_hex(&self.key).and_then(|k| k.try_into().ok()).ok_or("the key isn't 32 bytes of hex")?;
        let sig: [u8; 64] = from_hex(&self.signature).and_then(|s| s.try_into().ok()).ok_or("the signature isn't 64 bytes of hex")?;
        let key = VerifyingKey::from_bytes(&key).map_err(|_| "the key isn't a valid Ed25519 key")?;
        key.verify_strict(self.statement().as_bytes(), &Signature::from_bytes(&sig)).map_err(|_| "the signature doesn't match this head".to_string())
    }
}

fn statement(run: &str, entries: usize, head: &str, at_ms: u64) -> String {
    format!("{DOMAIN}\nrun {run}\nentries {entries}\nhead {head}\nat_ms {at_ms}\n")
}

#[must_use]
pub fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

fn from_hex(s: &str) -> Option<Vec<u8>> {
    let s = s.trim();
    if !s.len().is_multiple_of(2) || !s.is_ascii() {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok()).collect()
}

/// `n` bytes from the OS random source, as hex: for salts, keys and secrets.
///
/// # Panics
/// If the OS has no random source to give (then nothing here can be safe).
#[must_use]
pub fn random_hex(n: usize) -> String {
    let mut bytes = vec![0u8; n];
    getrandom::fill(&mut bytes).expect("the OS random source is unavailable");
    to_hex(&bytes)
}

/// The server's signing key.
pub struct Signer {
    key: SigningKey,
}

impl std::fmt::Debug for Signer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Signer").field("public", &self.public_hex()).finish_non_exhaustive()
    }
}

impl Signer {
    /// A new key, for this process only.
    ///
    /// # Panics
    /// If the OS has no random source to give.
    #[must_use]
    pub fn ephemeral() -> Self {
        let mut secret = [0u8; 32];
        getrandom::fill(&mut secret).expect("the OS random source is unavailable");
        Self { key: SigningKey::from_bytes(&secret) }
    }

    /// The key in `path` (64 hex characters), or a new one written there, readable
    /// only by this user.
    ///
    /// # Errors
    /// If the file can't be read or written, or doesn't hold a key.
    pub fn load_or_create(path: &Path) -> Result<Self, JournalError> {
        if path.exists() {
            let text = std::fs::read_to_string(path).map_err(|e| JournalError::new(format!("can't read the signing key: {e}")))?;
            let secret: [u8; 32] = from_hex(&text)
                .and_then(|k| k.try_into().ok())
                .ok_or_else(|| JournalError::new(format!("{} doesn't hold a signing key", path.display())))?;
            return Ok(Self { key: SigningKey::from_bytes(&secret) });
        }
        let signer = Self::ephemeral();
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        let mut file = options.open(path).map_err(|e| JournalError::new(format!("can't create the signing key: {e}")))?;
        std::io::Write::write_all(&mut file, to_hex(&signer.key.to_bytes()).as_bytes()).map_err(|e| JournalError::new(format!("can't write the signing key: {e}")))?;
        Ok(signer)
    }

    #[must_use]
    pub fn public_hex(&self) -> String {
        to_hex(&self.key.verifying_key().to_bytes())
    }

    #[must_use]
    pub fn sign(&self, run: &str, entries: usize, head: &str, at_ms: u64) -> SignedHead {
        let signature = self.key.sign(statement(run, entries, head, at_ms).as_bytes());
        SignedHead { run: run.to_string(), entries, head: head.to_string(), at_ms, key: self.public_hex(), signature: to_hex(&signature.to_bytes()) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_signed_head_verifies_and_any_change_breaks_it() {
        let signer = Signer::ephemeral();
        let head = signer.sign("errand-1", 3, &"ab".repeat(32), 1_700_000_000_000);
        assert_eq!(head.verify(), Ok(()));
        for tamper in [
            SignedHead { entries: 2, ..head.clone() },
            SignedHead { head: "cd".repeat(32), ..head.clone() },
            SignedHead { run: "errand-2".into(), ..head.clone() },
            SignedHead { key: Signer::ephemeral().public_hex(), ..head.clone() },
        ] {
            assert!(tamper.verify().is_err(), "{tamper:?}");
        }
    }

    #[test]
    fn the_key_is_kept_and_reloaded() {
        let dir = std::env::temp_dir().join(format!("agora-key-{}", random_hex(6)));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("signing.key");
        let a = Signer::load_or_create(&path).unwrap();
        let b = Signer::load_or_create(&path).unwrap();
        assert_eq!(a.public_hex(), b.public_hex());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        }
        std::fs::remove_dir_all(dir).unwrap();
        assert_ne!(random_hex(16), random_hex(16));
    }
}
