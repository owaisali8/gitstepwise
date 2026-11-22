//! The credentials vault: API keys, passwords, server logins and any other
//! secret, kept in one encrypted file in the config folder. It belongs to no
//! project; every project sees the same vault.
//!
//! The file is encrypted with XChaCha20-Poly1305, under a key made from the
//! master password with Argon2id. Without the password it can't be read, and
//! a wrong password or a changed file is caught rather than misread. The key
//! and the credentials are wiped from memory when the vault locks.

use std::fmt;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};

use argon2::{Algorithm, Argon2, Params, Version};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use chacha20poly1305::aead::rand_core::RngCore as _;
use chacha20poly1305::aead::{Aead, AeadCore, KeyInit, OsRng, Payload};
use chacha20poly1305::{Key, XChaCha20Poly1305, XNonce};
use serde::{Deserialize, Serialize};
use zeroize::{Zeroize, Zeroizing};

use crate::projects;

const FILE_NAME: &str = "credentials.vault";
const FILE_VERSION: u32 = 1;
const ALGORITHM: &str = "argon2id";
/// Bound to the encrypted data, so data taken from another kind of file is
/// refused.
const ASSOCIATED_DATA: &[u8] = b"gitstepwise credentials vault 1";
const SALT_LEN: usize = 16;
const KEY_LEN: usize = 32;
const NONCE_LEN: usize = 24;

/// How much work making the key takes: Argon2id's memory (in KiB), passes
/// and lanes. Kept in the file, so it can be raised later.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cost {
    memory: u32,
    passes: u32,
    lanes: u32,
}

impl Cost {
    /// 64 MiB and 3 passes, the second choice RFC 9106 recommends (the first
    /// needs 2 GiB). It takes a fraction of a second.
    pub const DEFAULT: Cost = Cost {
        memory: 64 * 1024,
        passes: 3,
        lanes: 4,
    };

    /// Whether a cost read from a file is one worth trying: a damaged or
    /// altered file mustn't ask for terabytes of memory.
    fn is_sane(self) -> bool {
        (8..=1024 * 1024).contains(&self.memory)
            && (1..=64).contains(&self.passes)
            && (1..=16).contains(&self.lanes)
            && self.memory >= 8 * self.lanes
    }
}

/// One stored secret, with what's needed to use it.
#[derive(Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Credential {
    /// Tells credentials apart; unique within the vault, and never 0.
    #[serde(default)]
    pub id: u64,
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub username: String,
    /// The password, API key, token or other secret.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub secret: String,
    /// Where it's used: a URL, a host name or an IP address.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub url: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub notes: String,
    /// When it was last changed, in milliseconds since the Unix epoch.
    #[serde(default)]
    pub updated: u64,
}

impl Credential {
    /// A copy with the secret left out, for lists and searching.
    pub fn without_secret(&self) -> Self {
        Self {
            id: self.id,
            name: self.name.clone(),
            username: self.username.clone(),
            secret: String::new(),
            url: self.url.clone(),
            notes: self.notes.clone(),
            updated: self.updated,
        }
    }
}

impl Drop for Credential {
    fn drop(&mut self) {
        self.name.zeroize();
        self.username.zeroize();
        self.secret.zeroize();
        self.url.zeroize();
        self.notes.zeroize();
    }
}

/// Leaves the secret out, so it never lands in a log.
impl fmt::Debug for Credential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Credential")
            .field("id", &self.id)
            .field("name", &self.name)
            .field("username", &self.username)
            .field("url", &self.url)
            .finish_non_exhaustive()
    }
}

#[derive(Debug)]
pub enum VaultError {
    /// The password isn't the master password (or the file was altered:
    /// the two can't be told apart).
    WrongPassword,
    /// The file isn't a vault this version can read.
    Unreadable,
    /// There's already a vault, and making a new one would replace it.
    Exists,
    Io(io::Error),
}

impl fmt::Display for VaultError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WrongPassword => write!(f, "That isn't the master password."),
            Self::Unreadable => write!(
                f,
                "The vault file can't be read. It may be damaged, or from a newer version of Gitstepwise."
            ),
            Self::Exists => write!(f, "There's already a vault."),
            Self::Io(err) => write!(f, "{err}"),
        }
    }
}

impl From<io::Error> for VaultError {
    fn from(err: io::Error) -> Self {
        Self::Io(err)
    }
}

/// The vault as it's written to disk.
#[derive(Serialize, Deserialize)]
struct VaultFile {
    version: u32,
    /// How to make the key again from the master password.
    kdf: Kdf,
    nonce: String,
    /// The credentials, encrypted.
    data: String,
}

#[derive(Serialize, Deserialize)]
struct Kdf {
    algorithm: String,
    #[serde(flatten)]
    cost: Cost,
    salt: String,
}

/// What's encrypted.
#[derive(Serialize)]
struct Contents<'a> {
    credentials: &'a [Credential],
}

#[derive(Deserialize)]
struct OwnedContents {
    #[serde(default)]
    credentials: Vec<Credential>,
}

/// The unlocked vault: the key, and the credentials it holds.
pub struct Vault {
    file: PathBuf,
    cost: Cost,
    salt: [u8; SALT_LEN],
    key: Zeroizing<[u8; KEY_LEN]>,
    credentials: Vec<Credential>,
}

impl Vault {
    /// Where the vault is kept: next to the list of projects.
    pub fn default_file() -> Option<PathBuf> {
        dirs::config_dir().map(|dir| dir.join(projects::CONFIG_FOLDER).join(FILE_NAME))
    }

    /// Make a new, empty vault at `file`, locked with `password`, and save
    /// it. Takes a moment: run it in the background.
    pub fn create(file: PathBuf, password: &str) -> Result<Self, VaultError> {
        Self::create_with(file, password, Cost::DEFAULT)
    }

    fn create_with(file: PathBuf, password: &str, cost: Cost) -> Result<Self, VaultError> {
        if file.exists() {
            return Err(VaultError::Exists);
        }
        let NewKey { salt, key } = NewKey::make(password, cost)?;
        let vault = Self {
            file,
            cost,
            salt,
            key,
            credentials: Vec::new(),
        };
        vault.save()?;
        Ok(vault)
    }

    /// Open the vault at `file` with `password`. Takes a moment: run it in
    /// the background.
    pub fn unlock(file: PathBuf, password: &str) -> Result<Self, VaultError> {
        let bytes = std::fs::read(&file)?;
        Self::open(file, &bytes, password)
    }

    fn open(file: PathBuf, bytes: &[u8], password: &str) -> Result<Self, VaultError> {
        let stored: VaultFile = serde_json::from_slice(bytes).map_err(|_| VaultError::Unreadable)?;
        let cost = stored.kdf.cost;
        if stored.version != FILE_VERSION || stored.kdf.algorithm != ALGORITHM || !cost.is_sane() {
            return Err(VaultError::Unreadable);
        }
        let decode = |text: &str| BASE64.decode(text).map_err(|_| VaultError::Unreadable);
        let salt: [u8; SALT_LEN] = decode(&stored.kdf.salt)?
            .try_into()
            .map_err(|_| VaultError::Unreadable)?;
        let nonce = decode(&stored.nonce)?;
        if nonce.len() != NONCE_LEN {
            return Err(VaultError::Unreadable);
        }
        let data = decode(&stored.data)?;

        let key = make_key(password, &salt, cost)?;
        let payload = Payload {
            msg: &data,
            aad: ASSOCIATED_DATA,
        };
        let plain = XChaCha20Poly1305::new(Key::from_slice(key.as_slice()))
            .decrypt(XNonce::from_slice(&nonce), payload)
            .map(Zeroizing::new)
            .map_err(|_| VaultError::WrongPassword)?;
        let contents: OwnedContents = serde_json::from_slice(&plain).map_err(|_| VaultError::Unreadable)?;
        let mut vault = Self {
            file,
            cost,
            salt,
            key,
            credentials: contents.credentials,
        };
        vault.number_unnumbered();
        Ok(vault)
    }

    /// Encrypt the credentials and write them to the vault's file, in
    /// place of what was there.
    pub fn save(&self) -> Result<(), VaultError> {
        write_private(&self.file, &self.encrypt()?)?;
        Ok(())
    }

    fn encrypt(&self) -> Result<Vec<u8>, VaultError> {
        let contents = Contents {
            credentials: &self.credentials,
        };
        let plain = Zeroizing::new(serde_json::to_vec(&contents).map_err(io::Error::other)?);
        let nonce = XChaCha20Poly1305::generate_nonce(&mut OsRng);
        let payload = Payload {
            msg: plain.as_slice(),
            aad: ASSOCIATED_DATA,
        };
        let data = XChaCha20Poly1305::new(Key::from_slice(self.key.as_slice()))
            .encrypt(&nonce, payload)
            .map_err(|_| io::Error::other("couldn't encrypt the vault"))?;
        let stored = VaultFile {
            version: FILE_VERSION,
            kdf: Kdf {
                algorithm: ALGORITHM.into(),
                cost: self.cost,
                salt: BASE64.encode(self.salt),
            },
            nonce: BASE64.encode(nonce),
            data: BASE64.encode(data),
        };
        serde_json::to_vec_pretty(&stored).map_err(|err| VaultError::Io(io::Error::other(err)))
    }

    /// Lock the vault with a new master password from now on. Takes a
    /// moment: run it in the background, or make the key with `NewKey`
    /// there and pass it to `change_key`.
    #[cfg(test)]
    pub fn change_password(&mut self, password: &str) -> Result<(), VaultError> {
        let key = NewKey::make(password, self.cost)?;
        self.change_key(key)
    }

    /// Lock the vault with `key`, made from a new master password, and save
    /// it. If saving fails, the old password stays.
    pub fn change_key(&mut self, key: NewKey) -> Result<(), VaultError> {
        let old = (self.salt, std::mem::replace(&mut self.key, key.key));
        self.salt = key.salt;
        if let Err(err) = self.save() {
            // The file still has the old password, so keep using it.
            (self.salt, self.key) = old;
            return Err(err);
        }
        Ok(())
    }

    /// How much work making the key takes, for `NewKey::make`.
    pub fn cost(&self) -> Cost {
        self.cost
    }

    #[cfg(test)]
    pub fn file(&self) -> &Path {
        &self.file
    }

    pub fn credentials(&self) -> &[Credential] {
        &self.credentials
    }

    pub fn get(&self, id: u64) -> Option<&Credential> {
        self.credentials.iter().find(|credential| credential.id == id)
    }

    /// Add `credential`, or, if its id is taken, replace the one with that
    /// id. One without an id (0) gets a new one. Returns its id. Not saved
    /// yet.
    pub fn put(&mut self, mut credential: Credential, now: u64) -> u64 {
        credential.updated = now;
        if let Some(slot) = self
            .credentials
            .iter_mut()
            .find(|existing| credential.id != 0 && existing.id == credential.id)
        {
            *slot = credential;
            return slot.id;
        }
        if credential.id == 0 {
            credential.id = self.next_id();
        }
        let id = credential.id;
        self.credentials.push(credential);
        id
    }

    /// Take a credential out. Not saved yet.
    pub fn remove(&mut self, id: u64) -> Option<Credential> {
        let ix = self.credentials.iter().position(|credential| credential.id == id)?;
        Some(self.credentials.remove(ix))
    }

    fn next_id(&self) -> u64 {
        self.credentials.iter().map(|credential| credential.id).max().unwrap_or(0) + 1
    }

    /// Give an id to credentials without one (written by hand, say), and a
    /// new one to any that share an id.
    fn number_unnumbered(&mut self) {
        let mut seen = std::collections::HashSet::new();
        for ix in 0..self.credentials.len() {
            let id = self.credentials[ix].id;
            if id == 0 || !seen.insert(id) {
                let id = self.next_id();
                self.credentials[ix].id = id;
                seen.insert(id);
            }
        }
    }
}

/// A key made from a new master password, with its fresh salt.
pub struct NewKey {
    salt: [u8; SALT_LEN],
    key: Zeroizing<[u8; KEY_LEN]>,
}

impl NewKey {
    /// Takes a moment: run it in the background.
    pub fn make(password: &str, cost: Cost) -> Result<Self, VaultError> {
        let mut salt = [0; SALT_LEN];
        OsRng.fill_bytes(&mut salt);
        let key = make_key(password, &salt, cost)?;
        Ok(Self { salt, key })
    }
}

fn make_key(password: &str, salt: &[u8], cost: Cost) -> Result<Zeroizing<[u8; KEY_LEN]>, VaultError> {
    let params = Params::new(cost.memory, cost.passes, cost.lanes, Some(KEY_LEN))
        .map_err(|_| VaultError::Unreadable)?;
    let mut key = Zeroizing::new([0; KEY_LEN]);
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
        .hash_password_into(password.as_bytes(), salt, key.as_mut())
        .map_err(|_| VaultError::Unreadable)?;
    Ok(key)
}

/// Write `bytes` to a file next to `file`, readable only by this user on
/// Unix, then move it into place, so a crash never leaves half a vault.
fn write_private(file: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut temporary = file.as_os_str().to_owned();
    temporary.push(".tmp");
    let temporary = PathBuf::from(temporary);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut out = options.open(&temporary)?;
    out.write_all(bytes)?;
    out.sync_all()?;
    drop(out);
    std::fs::rename(&temporary, file)
}

/// Whether every word of `query` is somewhere in `text`, ignoring case.
fn has_words(text: &str, query: &str) -> bool {
    let text = text.to_lowercase();
    query
        .to_lowercase()
        .split_whitespace()
        .all(|word| text.contains(word))
}

/// The credentials that match `query`, best first, as indices into
/// `credentials`. The name counts most, then the username and where it's
/// used, then the notes. The secret itself is never searched. With nothing
/// typed, they're all there, by name.
pub fn search(credentials: &[Credential], query: &str) -> Vec<usize> {
    let by_name = |a: &usize, b: &usize| {
        let name = |ix: &usize| credentials[*ix].name.to_lowercase();
        name(a).cmp(&name(b))
    };
    let mut all: Vec<usize> = (0..credentials.len()).collect();
    if query.trim().is_empty() {
        all.sort_by(by_name);
        return all;
    }
    let fuzzy = projects::fuzzy_score;
    let mut scored: Vec<(usize, i32)> = all
        .into_iter()
        .filter_map(|ix| {
            let credential = &credentials[ix];
            let everywhere = format!("{} {} {}", credential.name, credential.username, credential.url);
            let score = [
                fuzzy(query, &credential.name).map(|score| score + 300),
                fuzzy(query, &credential.username).map(|score| score + 200),
                fuzzy(query, &credential.url).map(|score| score + 200),
                fuzzy(query, &everywhere).map(|score| score + 100),
                has_words(&credential.notes, query).then_some(0),
            ]
            .into_iter()
            .flatten()
            .max()?;
            Some((ix, score))
        })
        .collect();
    scored.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| by_name(&a.0, &b.0)));
    scored.into_iter().map(|(ix, _)| ix).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Cheap enough for tests; real vaults use `Cost::DEFAULT`.
    const TEST_COST: Cost = Cost {
        memory: 64,
        passes: 1,
        lanes: 1,
    };

    fn credential(name: &str, username: &str, url: &str, notes: &str) -> Credential {
        Credential {
            id: 0,
            name: name.into(),
            username: username.into(),
            secret: format!("secret of {name}"),
            url: url.into(),
            notes: notes.into(),
            updated: 0,
        }
    }

    fn temporary_file(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("gitstepwise-vault-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir.join(FILE_NAME)
    }

    #[test]
    fn a_saved_vault_opens_with_its_password_only() {
        let file = temporary_file("roundtrip");
        let mut vault = Vault::create_with(file.clone(), "correct horse", TEST_COST).unwrap();
        let id = vault.put(credential("Prod database", "admin", "10.0.0.5", "Port 5432."), 7);
        vault.save().unwrap();

        let bytes = std::fs::read(&file).unwrap();
        let text = String::from_utf8_lossy(&bytes);
        assert!(!text.contains("secret of"), "the secret is stored in the clear");
        assert!(!text.contains("admin"), "the username is stored in the clear");

        let opened = Vault::unlock(file.clone(), "correct horse").unwrap();
        let stored = opened.get(id).unwrap();
        assert_eq!(stored.secret, "secret of Prod database");
        assert_eq!(stored.username, "admin");
        assert_eq!(stored.updated, 7);
        assert!(matches!(
            Vault::unlock(file.clone(), "Correct horse"),
            Err(VaultError::WrongPassword)
        ));
        // A new vault never replaces one that's there.
        assert!(matches!(
            Vault::create_with(file.clone(), "other", TEST_COST),
            Err(VaultError::Exists)
        ));
        let _ = std::fs::remove_dir_all(file.parent().unwrap());
    }

    #[test]
    fn an_altered_or_foreign_file_is_refused() {
        let vault = Vault::create_with(temporary_file("altered"), "pw", TEST_COST).unwrap();
        let bytes = vault.encrypt().unwrap();
        let mut stored: serde_json::Value = serde_json::from_slice(&bytes).unwrap();

        // One changed byte of the data.
        let mut data = BASE64.decode(stored["data"].as_str().unwrap()).unwrap();
        data[0] ^= 1;
        let mut altered = stored.clone();
        altered["data"] = BASE64.encode(data).into();
        let altered = serde_json::to_vec(&altered).unwrap();
        assert!(matches!(
            Vault::open(PathBuf::new(), &altered, "pw"),
            Err(VaultError::WrongPassword)
        ));

        // A cost no sane vault has.
        stored["kdf"]["memory"] = (u32::MAX).into();
        let greedy = serde_json::to_vec(&stored).unwrap();
        assert!(matches!(Vault::open(PathBuf::new(), &greedy, "pw"), Err(VaultError::Unreadable)));
        assert!(matches!(Vault::open(PathBuf::new(), b"hunter2", "pw"), Err(VaultError::Unreadable)));
        let _ = std::fs::remove_dir_all(vault.file().parent().unwrap());
    }

    #[test]
    fn a_new_master_password_replaces_the_old_one() {
        let file = temporary_file("rekey");
        let mut vault = Vault::create_with(file.clone(), "old", TEST_COST).unwrap();
        vault.put(credential("API key", "", "", ""), 1);
        vault.save().unwrap();
        vault.change_password("new").unwrap();
        assert!(matches!(Vault::unlock(file.clone(), "old"), Err(VaultError::WrongPassword)));
        let opened = Vault::unlock(file.clone(), "new").unwrap();
        assert_eq!(opened.credentials()[0].name, "API key");
        let _ = std::fs::remove_dir_all(file.parent().unwrap());
    }

    #[test]
    fn putting_a_credential_adds_or_replaces_it() {
        let mut vault = Vault::create_with(temporary_file("put"), "pw", TEST_COST).unwrap();
        let a = vault.put(credential("A", "", "", ""), 1);
        let b = vault.put(credential("B", "", "", ""), 2);
        assert_ne!(a, b);
        let mut changed = vault.get(a).unwrap().clone();
        changed.secret = "new secret".into();
        assert_eq!(vault.put(changed, 3), a);
        assert_eq!(vault.credentials().len(), 2);
        assert_eq!(vault.get(a).unwrap().secret, "new secret");
        assert_eq!(vault.get(a).unwrap().updated, 3);
        assert!(vault.remove(a).is_some());
        assert!(vault.get(a).is_none());
        // Ids aren't reused while a higher one is in use.
        assert_eq!(vault.put(credential("C", "", "", ""), 4), b + 1);
        let _ = std::fs::remove_dir_all(vault.file().parent().unwrap());
    }

    #[test]
    fn the_secret_stays_out_of_debug_output() {
        let shown = format!("{:?}", credential("Mail", "me", "", ""));
        assert!(shown.contains("Mail"));
        assert!(!shown.contains("secret of"));
    }

    #[test]
    fn search_ranks_names_first_and_never_looks_at_secrets() {
        let credentials = [
            credential("OpenAI API key", "", "https://api.openai.com", ""),
            credential("Prod database", "admin", "10.0.0.5", "Ask ops before rotating."),
            credential("Staging VM", "deploy", "staging.example.com", "Same key as the prod database."),
        ];
        // In the name of one, and only the notes of another.
        assert_eq!(search(&credentials, "prod database"), [1, 2]);
        assert_eq!(search(&credentials, "pdb")[0], 1);
        assert_eq!(search(&credentials, "deploy"), [2]);
        assert_eq!(search(&credentials, "openai.com"), [0]);
        assert_eq!(search(&credentials, "rotating"), [1]);
        // "secret of" is in every secret, and nowhere else.
        assert!(search(&credentials, "secret of").is_empty());
        // Nothing typed: all of them, by name.
        assert_eq!(search(&credentials, " "), [0, 1, 2]);
    }

    #[test]
    fn credentials_without_an_id_get_one() {
        let file = temporary_file("ids");
        let mut vault = Vault::create_with(file.clone(), "pw", TEST_COST).unwrap();
        vault.credentials = vec![credential("A", "", "", ""), credential("B", "", "", "")];
        vault.save().unwrap();
        let opened = Vault::unlock(file.clone(), "pw").unwrap();
        let ids: Vec<u64> = opened.credentials().iter().map(|credential| credential.id).collect();
        assert!(!ids.contains(&0));
        assert_ne!(ids[0], ids[1]);
        let _ = std::fs::remove_dir_all(file.parent().unwrap());
    }
}
