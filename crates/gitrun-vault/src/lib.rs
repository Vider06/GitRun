//! GitVault: encrypted-at-rest secret storage, shared across GitRun runner
//! containers via the same mount point as the package-manager cache
//! (`shared_cache_volume` in `gitrun-core::Config`).
//!
//! Threat model this addresses: today, secrets an operator wants available
//! to CI jobs (API keys, deploy credentials) have no first-class place to
//! live in GitRun — they'd end up as plaintext environment variables baked
//! into the runner container spec, visible via `docker inspect` to anyone
//! with Docker socket access (see `docs/SECURITY_MODEL.md`). GitVault gives
//! them an encrypted-at-rest home instead: secrets are stored as
//! AES-256-GCM ciphertext on disk, and only decrypted in memory when a
//! runner actually requests one by name.
//!
//! Scoping (reworked from an earlier `<repo>__NAME` / `global__NAME` naming
//! convention to an explicit `Scope` field): a secret belongs to exactly one
//! of three scopes —
//! - `Scope::Global`: injected into every repo's runners.
//! - `Scope::Group(name)`: injected into runners for any repo that lists
//!   `name` among its GitVault groups (see `gitrun_core::Config` — groups
//!   are how an operator says "these repos share the same deploy key"
//!   without duplicating the secret per repo or falling back to Global for
//!   everything).
//! - `Scope::Repo(repo)`: injected only into that one repo's runners.
//!
//! This is a first-match-most-specific model when resolving for a repo:
//! repo-scoped secrets, then group-scoped (for every group that repo is a
//! member of), then global — see `resolve_for_repo`. A name collision across
//! scopes (e.g. a repo-scoped `API_KEY` and a global `API_KEY`) resolves to
//! the more specific one; this is deliberate (repo-specific overrides should
//! win) and is exercised by a test.
//!
//! What this does *not* solve on its own: anyone with Docker socket access
//! (see `docs/SECURITY_MODEL.md`) can still start a container that mounts
//! the vault's storage path and, if it also has the master key, decrypt
//! everything. Encryption-at-rest protects against disk theft, backup
//! leakage, and casual inspection of the storage file — not against a
//! fully compromised host, which no software-only secret store can prevent.
//!
//! Master key handling: a 32-byte key generated once and stored at
//! `<vault_dir>/master.key` with 0600 permissions — same treatment
//! `gitrun-setup` already gives `gitrun.env`. Losing this file makes every
//! stored secret permanently unrecoverable by design (there is no
//! backdoor); back it up if the vault holds anything that isn't otherwise
//! recoverable.

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use rand::{rngs::SysRng, TryRng};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use thiserror::Error;

/// Where a secret applies. See the module docs for the resolution order
/// (`resolve_for_repo`) when a repo could match more than one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Hash)]
pub enum Scope {
    Global,
    Group(String),
    Repo(String),
}

impl Scope {
    /// Deterministic sort key used to pick the most specific match when a
    /// repo has secrets available from more than one scope: higher number
    /// wins. Repo-specific overrides group, which overrides global.
    fn specificity(&self) -> u8 {
        match self {
            Scope::Global => 0,
            Scope::Group(_) => 1,
            Scope::Repo(_) => 2,
        }
    }
}

#[derive(Debug, Error)]
pub enum VaultError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("vault storage file is corrupt or tampered with: {0}")]
    Decode(#[from] serde_json::Error),
    #[error("secret '{0}' not found")]
    NotFound(String),
    #[error("encryption failed for secret '{0}'")]
    EncryptionFailed(String),
    #[error(
        "decryption failed for secret '{0}' — wrong master key or corrupted/tampered ciphertext"
    )]
    DecryptionFailed(String),
    #[error("master key file is the wrong size (expected 32 bytes, got {0})")]
    InvalidMasterKey(usize),
    #[error("secret name must not be empty")]
    EmptyName,
    #[error("secret identifier contains a reserved control character")]
    InvalidIdentifier,
    #[error("cryptographic randomness unavailable: {0}")]
    Randomness(#[from] rand::rngs::SysError),
}

pub type Result<T> = std::result::Result<T, VaultError>;

/// Gate for reporting security-relevant vault events to an external
/// observer — designed for GSR (not yet implemented) to plug into once it
/// exists, without GitVault needing to depend on GSR directly. A no-op
/// default means GitVault works standalone today; wiring in a real sink
/// later is additive.
pub trait VaultEventSink: Send + Sync {
    /// Called when a stored secret fails to decrypt for reasons other than
    /// a simple missing entry — i.e. wrong master key or, more seriously,
    /// tampered ciphertext (AES-GCM's authentication tag check failing).
    /// This is exactly the kind of signal GSR's hardening layer should see:
    /// it can mean an attacker modified the vault's storage file on disk.
    fn on_decryption_failure(&self, secret_name: &str) {
        let _ = secret_name;
    }

    /// Called when the OS cryptographic random source fails while generating
    /// material required for vault encryption. This is a security-critical
    /// condition: the operation must fail rather than fall back to weaker
    /// randomness.
    fn on_randomness_failure(&self, operation: &str) {
        let _ = operation;
    }
}

/// Default sink used when no observer is configured: does nothing. Kept
/// explicit (rather than `Option<Box<dyn VaultEventSink>>` everywhere) so
/// `Vault::open` doesn't need an `Option` check on every operation.
pub struct NoopEventSink;
impl VaultEventSink for NoopEventSink {}

const MASTER_KEY_FILE: &str = "master.key";
const SECRETS_FILE: &str = "secrets.json";
const NONCE_LEN: usize = 12;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredSecret {
    /// Base64-encoded nonce, unique per encryption — AES-GCM security
    /// depends entirely on never reusing a nonce with the same key, so this
    /// is generated fresh (via OS RNG) every time a secret is written, even
    /// when overwriting an existing name.
    nonce: String,
    /// Base64-encoded ciphertext (includes the GCM authentication tag).
    ciphertext: String,
    /// Unix seconds when this secret was last written, surfaced to callers
    /// (e.g. the dashboard) without requiring decryption.
    updated_at: u64,
    /// Where this secret applies. Defaults to `Global` when deserializing
    /// records written before scopes existed, so upgrading GitVault doesn't
    /// silently orphan or misfile previously-stored secrets — they keep
    /// behaving exactly as before (available to every repo).
    #[serde(default = "Scope::default_for_migration")]
    scope: Scope,
}

impl Scope {
    fn default_for_migration() -> Self {
        Scope::Global
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct VaultFile {
    #[serde(default)]
    secrets: HashMap<String, StoredSecret>,
}

pub struct Vault {
    dir: PathBuf,
    cipher: Aes256Gcm,
    data: VaultFile,
    events: Box<dyn VaultEventSink>,
}

impl Vault {
    /// Opens (or initializes) a vault at `dir`, generating a new master key
    /// on first use. `dir` should be a path only GitRun's own processes can
    /// read — same trust boundary as `gitrun.env`. Uses a no-op event sink;
    /// use `open_with_sink` to observe decryption failures (e.g. from GSR).
    pub fn open(dir: impl Into<PathBuf>) -> Result<Self> {
        Self::open_with_sink(dir, Box::new(NoopEventSink))
    }

    pub fn open_with_sink(
        dir: impl Into<PathBuf>,
        events: Box<dyn VaultEventSink>,
    ) -> Result<Self> {
        let dir = dir.into();
        fs::create_dir_all(&dir)?;
        let key_bytes = load_or_create_master_key(&dir, events.as_ref())?;
        let cipher = Aes256Gcm::new_from_slice(&key_bytes)
            .map_err(|_| VaultError::InvalidMasterKey(key_bytes.len()))?;

        let secrets_path = dir.join(SECRETS_FILE);
        let data = match fs::metadata(&secrets_path) {
            Ok(_) => {
                set_secret_file_permissions(&secrets_path)?;
                let raw = fs::read_to_string(&secrets_path)?;
                serde_json::from_str(&raw)?
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => VaultFile::default(),
            Err(error) => return Err(error.into()),
        };

        Ok(Self {
            dir,
            cipher,
            data,
            events,
        })
    }

    /// Encrypts and stores `value` under `name` with `Scope::Global`,
    /// overwriting any existing secret with the same name in that scope.
    /// Kept for compatibility with callers written before scopes existed;
    /// prefer `set_scoped` for anything new.
    pub fn set(&mut self, name: &str, value: &str) -> Result<()> {
        self.set_scoped(name, value, Scope::Global)
    }

    /// Encrypts and stores `value` under `name` within `scope`, overwriting
    /// any existing secret with the same (name, scope) pair — the same name
    /// can exist independently in different scopes (e.g. a repo-scoped
    /// `API_KEY` that overrides a global one of the same name for that repo
    /// only; see `resolve_for_repo`). Persists immediately (no separate
    /// "save" step) so a crash right after `set_scoped` can't silently lose
    /// the write — same reasoning as the atomic-write pattern used elsewhere
    /// in GitRun.
    pub fn set_scoped(&mut self, name: &str, value: &str, scope: Scope) -> Result<()> {
        validate_name(name)?;
        validate_scope(&scope)?;
        let mut nonce_bytes = [0u8; NONCE_LEN];
        let mut rng = SysRng;
        if let Err(error) = rng.try_fill_bytes(&mut nonce_bytes) {
            self.events
                .on_randomness_failure("generating an AES-GCM nonce");
            return Err(VaultError::from(error));
        }
        let nonce = Nonce::try_from(nonce_bytes.as_slice())
            .map_err(|_| VaultError::EncryptionFailed(name.to_owned()))?;

        let ciphertext = self
            .cipher
            .encrypt(nonce, value.as_bytes())
            // aes-gcm encryption failures are not caused by operator-provided
            // secret content; still report them with the correct operation.
            .map_err(|_| VaultError::EncryptionFailed(name.to_owned()))?;

        self.data.secrets.insert(
            storage_key(name, &scope),
            StoredSecret {
                nonce: base64_encode(&nonce_bytes),
                ciphertext: base64_encode(&ciphertext),
                updated_at: now(),
                scope,
            },
        );
        self.persist()
    }

    /// Decrypts and returns the secret stored under `name` in `Scope::Global`.
    /// Kept for compatibility with callers written before scopes existed;
    /// prefer `get_scoped` for anything new.
    pub fn get(&self, name: &str) -> Result<String> {
        self.get_scoped(name, &Scope::Global)
    }

    /// Decrypts and returns the secret stored under `name` within `scope`.
    pub fn get_scoped(&self, name: &str, scope: &Scope) -> Result<String> {
        let key = storage_key(name, scope);
        let entry = self
            .data
            .secrets
            .get(&key)
            .ok_or_else(|| VaultError::NotFound(name.to_owned()))?;
        self.decrypt_entry(name, entry)
    }

    fn decrypt_entry(&self, name: &str, entry: &StoredSecret) -> Result<String> {
        let nonce_bytes = base64_decode(&entry.nonce).ok_or_else(|| {
            self.events.on_decryption_failure(name);
            VaultError::DecryptionFailed(name.to_owned())
        })?;
        if nonce_bytes.len() != NONCE_LEN {
            self.events.on_decryption_failure(name);
            return Err(VaultError::DecryptionFailed(name.to_owned()));
        }

        let ciphertext = base64_decode(&entry.ciphertext).ok_or_else(|| {
            self.events.on_decryption_failure(name);
            VaultError::DecryptionFailed(name.to_owned())
        })?;
        let nonce = Nonce::try_from(nonce_bytes.as_slice()).map_err(|_| {
            self.events.on_decryption_failure(name);
            VaultError::DecryptionFailed(name.to_owned())
        })?;
        let plaintext = self
            .cipher
            .decrypt(nonce, ciphertext.as_slice())
            .map_err(|_| {
                self.events.on_decryption_failure(name);
                VaultError::DecryptionFailed(name.to_owned())
            })?;

        String::from_utf8(plaintext).map_err(|_| {
            self.events.on_decryption_failure(name);
            VaultError::DecryptionFailed(name.to_owned())
        })
    }

    pub fn delete(&mut self, name: &str) -> Result<()> {
        self.delete_scoped(name, &Scope::Global)
    }

    pub fn delete_scoped(&mut self, name: &str, scope: &Scope) -> Result<()> {
        let key = storage_key(name, scope);
        if self.data.secrets.remove(&key).is_none() {
            return Err(VaultError::NotFound(name.to_owned()));
        }
        self.persist()
    }

    /// Lists every secret's name, scope, and last-updated time, without
    /// decrypting anything — safe to call for a dashboard listing view.
    pub fn list(&self) -> Vec<(String, Scope, u64)> {
        let mut entries: Vec<_> = self
            .data
            .secrets
            .iter()
            .map(|(key, secret)| {
                (
                    name_part_of_key(key),
                    secret.scope.clone(),
                    secret.updated_at,
                )
            })
            .collect();
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        entries
    }

    /// Lists only secrets within a specific scope — e.g. everything scoped
    /// to one repo, for a per-repo dashboard view.
    pub fn list_in_scope(&self, scope: &Scope) -> Vec<(String, u64)> {
        let mut entries: Vec<_> = self
            .data
            .secrets
            .iter()
            .filter(|(_, secret)| &secret.scope == scope)
            .map(|(key, secret)| (name_part_of_key(key), secret.updated_at))
            .collect();
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        entries
    }

    pub fn contains(&self, name: &str, scope: &Scope) -> bool {
        self.data.secrets.contains_key(&storage_key(name, scope))
    }

    /// Resolves every secret a given repo's runners should receive,
    /// decrypted and ready to inject as environment variables — the
    /// counterpart to the old `main.rs::vault_env_for_repo` naming-prefix
    /// scan, now scope-aware. `groups` is the list of GitVault group names
    /// this repo belongs to (from `gitrun_core::Config`); order doesn't
    /// matter, all matching groups are included.
    ///
    /// Resolution order when the same secret *name* exists in more than one
    /// applicable scope: repo-scoped wins over group-scoped, which wins over
    /// global (see `Scope::specificity`). This means an operator can set a
    /// global default and override it for one specific repo without
    /// deleting or renaming anything.
    pub fn resolve_for_repo(&self, repo: &str, groups: &[String]) -> Vec<(String, String)> {
        // Iterate (key, secret) pairs — not just secret values — because the
        // original secret name is encoded in the storage key, not
        // recoverable from `Scope` alone (multiple scopes can share a name).
        let mut best: HashMap<String, (&Scope, &str)> = HashMap::new();
        for (key, secret) in &self.data.secrets {
            let applies = match &secret.scope {
                Scope::Global => true,
                Scope::Group(g) => groups.iter().any(|owned| owned == g),
                Scope::Repo(r) => r == repo,
            };
            if !applies {
                continue;
            }
            let name = name_part_of_key(key);
            match best.get(&name) {
                Some((existing_scope, _))
                    if existing_scope.specificity() >= secret.scope.specificity() => {}
                _ => {
                    best.insert(name, (&secret.scope, key.as_str()));
                }
            }
        }

        let mut resolved: Vec<_> = best
            .into_iter()
            .filter_map(|(name, (scope, key))| {
                let entry = self.data.secrets.get(key)?;
                match self.decrypt_entry(&name, entry) {
                    Ok(value) => Some((name, value)),
                    Err(_) => {
                        let _ = scope;
                        None
                    }
                }
            })
            .collect();
        resolved.sort_by(|a, b| a.0.cmp(&b.0));
        resolved
    }

    fn persist(&self) -> Result<()> {
        let path = self.dir.join(SECRETS_FILE);
        let tmp = path.with_extension("json.tmp");
        write_secret_file(&tmp, &self.data)?;
        fs::rename(&tmp, &path)?;
        Ok(())
    }
}

fn validate_name(name: &str) -> Result<()> {
    if name.trim().is_empty() {
        return Err(VaultError::EmptyName);
    }
    if name.chars().any(|c| c == '\u{1}') {
        return Err(VaultError::InvalidIdentifier);
    }
    Ok(())
}

fn validate_scope(scope: &Scope) -> Result<()> {
    match scope {
        Scope::Global => Ok(()),
        Scope::Group(group) | Scope::Repo(group) => {
            if group.trim().is_empty() || group.chars().any(|c| c == '\u{1}') {
                Err(VaultError::InvalidIdentifier)
            } else {
                Ok(())
            }
        }
    }
}

#[cfg(unix)]
fn write_secret_file(path: &Path, data: &VaultFile) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    let serialized = serde_json::to_string_pretty(data)?;
    let _ = fs::remove_file(path);
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(serialized.as_bytes())?;
    file.sync_all()?;
    Ok(())
}

#[cfg(not(unix))]
fn write_secret_file(path: &Path, data: &VaultFile) -> Result<()> {
    fs::write(path, serde_json::to_string_pretty(data)?)?;
    Ok(())
}

#[cfg(unix)]
fn set_secret_file_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut permissions = fs::metadata(path)?.permissions();
    permissions.set_mode(0o600);
    fs::set_permissions(path, permissions)?;
    Ok(())
}

#[cfg(not(unix))]
fn set_secret_file_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

fn load_or_create_master_key(dir: &Path, events: &dyn VaultEventSink) -> Result<[u8; 32]> {
    let path = dir.join(MASTER_KEY_FILE);
    match fs::read(&path) {
        Ok(bytes) => {
            if bytes.len() != 32 {
                return Err(VaultError::InvalidMasterKey(bytes.len()));
            }
            let mut key = [0u8; 32];
            key.copy_from_slice(&bytes);
            Ok(key)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let mut key = [0u8; 32];
            let mut rng = SysRng;
            if let Err(error) = rng.try_fill_bytes(&mut key) {
                events.on_randomness_failure("generating the vault master key");
                return Err(VaultError::from(error));
            }
            write_master_key(&path, &key)?;
            Ok(key)
        }
        Err(error) => Err(error.into()),
    }
}

/// Writes the master key with 0600 permissions set atomically at creation —
/// same fix already applied in `gitrun-setup` for `gitrun.env` (avoid the
/// create-then-chmod window where sensitive material sits world-readable).
#[cfg(unix)]
fn write_master_key(path: &Path, key: &[u8; 32]) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(key)?;
    file.sync_all()?;
    Ok(())
}

#[cfg(not(unix))]
fn write_master_key(path: &Path, key: &[u8; 32]) -> Result<()> {
    fs::write(path, key)?;
    Ok(())
}

/// Builds the internal storage key combining scope and name, so the same
/// secret name can exist independently in different scopes (a repo-scoped
/// `API_KEY` and a global `API_KEY` are different storage entries). The
/// separator `\u{1}` (a control character) is used rather than something
/// printable like `:` or `/` so it can never collide with a scope or secret
/// name an operator might plausibly choose.
fn storage_key(name: &str, scope: &Scope) -> String {
    let scope_tag = match scope {
        Scope::Global => "global".to_owned(),
        Scope::Group(g) => format!("group\u{1}{g}"),
        Scope::Repo(r) => format!("repo\u{1}{r}"),
    };
    format!("{scope_tag}\u{1}{name}")
}

/// Extracts the original secret name back out of a storage key built by
/// `storage_key`. The name is always the last `\u{1}`-delimited segment.
fn name_part_of_key(key: &str) -> String {
    key.rsplit('\u{1}').next().unwrap_or(key).to_owned()
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Thin wrappers around the `base64` crate (standard alphabet, with
/// padding), kept as functions with this exact signature so every call
/// site in this file is unchanged. Previously a hand-rolled implementation
/// (correct per its own round-trip test, but unaudited and non-constant-
/// time); replaced with the vetted, widely-used crate on request.
fn base64_encode(data: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(data)
}

fn base64_decode(input: &str) -> Option<Vec<u8>> {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.decode(input).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(label: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("gitrun-vault-test-{label}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn base64_round_trips() {
        for input in [
            b"".as_slice(),
            b"a",
            b"ab",
            b"abc",
            b"hello world",
            &[0u8, 255, 128, 1, 2, 3],
        ] {
            let encoded = base64_encode(input);
            let decoded = base64_decode(&encoded).unwrap();
            assert_eq!(decoded, input);
        }
    }

    #[test]
    fn set_and_get_round_trips() {
        let dir = temp_dir("roundtrip");
        let mut vault = Vault::open(&dir).unwrap();
        vault.set("deploy-key", "super-secret-value").unwrap();
        assert_eq!(vault.get("deploy-key").unwrap(), "super-secret-value");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn malformed_nonce_is_rejected_without_panic() {
        let dir = temp_dir("malformed-nonce");
        let mut vault = Vault::open(&dir).unwrap();
        vault.set("key", "value").unwrap();

        let secrets_path = dir.join(SECRETS_FILE);
        let raw = fs::read_to_string(&secrets_path).unwrap();
        let mut file: VaultFile = serde_json::from_str(&raw).unwrap();
        let secret = file.secrets.get_mut("global\u{1}key").unwrap();
        secret.nonce = base64_encode(&[1, 2, 3]);
        fs::write(&secrets_path, serde_json::to_string_pretty(&file).unwrap()).unwrap();

        let reloaded = Vault::open(&dir).unwrap();
        assert!(matches!(
            reloaded.get("key"),
            Err(VaultError::DecryptionFailed(_))
        ));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn reserved_separator_is_rejected_from_names_and_scopes() {
        let dir = temp_dir("identifier-validation");
        let mut vault = Vault::open(&dir).unwrap();

        assert!(matches!(
            vault.set("bad\u{1}name", "value"),
            Err(VaultError::InvalidIdentifier)
        ));
        assert!(matches!(
            vault.set_scoped("key", "value", Scope::Group("bad\u{1}group".into())),
            Err(VaultError::InvalidIdentifier)
        ));
        assert!(matches!(
            vault.set_scoped("key", "value", Scope::Repo("bad\u{1}repo".into())),
            Err(VaultError::InvalidIdentifier)
        ));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn secret_file_has_owner_only_permissions() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let dir = temp_dir("secret-perms");
            let mut vault = Vault::open(&dir).unwrap();
            vault.set("key", "value").unwrap();
            let meta = fs::metadata(dir.join(SECRETS_FILE)).unwrap();
            assert_eq!(meta.permissions().mode() & 0o777, 0o600);
            let _ = fs::remove_dir_all(&dir);
        }
    }

    #[test]
    fn missing_secret_is_not_found() {
        let dir = temp_dir("missing");
        let vault = Vault::open(&dir).unwrap();
        assert!(matches!(vault.get("nope"), Err(VaultError::NotFound(_))));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn empty_name_is_rejected() {
        let dir = temp_dir("emptyname");
        let mut vault = Vault::open(&dir).unwrap();
        assert!(matches!(vault.set("", "value"), Err(VaultError::EmptyName)));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn overwrite_replaces_value() {
        let dir = temp_dir("overwrite");
        let mut vault = Vault::open(&dir).unwrap();
        vault.set("key", "first").unwrap();
        vault.set("key", "second").unwrap();
        assert_eq!(vault.get("key").unwrap(), "second");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn delete_removes_secret() {
        let dir = temp_dir("delete");
        let mut vault = Vault::open(&dir).unwrap();
        vault.set("key", "value").unwrap();
        vault.delete("key").unwrap();
        assert!(matches!(vault.get("key"), Err(VaultError::NotFound(_))));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn delete_missing_secret_errors() {
        let dir = temp_dir("delete-missing");
        let mut vault = Vault::open(&dir).unwrap();
        assert!(matches!(vault.delete("nope"), Err(VaultError::NotFound(_))));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn list_does_not_require_decryption_and_hides_values() {
        let dir = temp_dir("list");
        let mut vault = Vault::open(&dir).unwrap();
        vault.set("a", "secret-a").unwrap();
        vault.set("b", "secret-b").unwrap();
        let listed = vault.list();
        assert_eq!(listed.len(), 2);
        let names: Vec<_> = listed.iter().map(|(n, _, _)| n.clone()).collect();
        assert!(names.contains(&"a".to_owned()));
        assert!(names.contains(&"b".to_owned()));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn persists_across_reopen() {
        let dir = temp_dir("persist");
        {
            let mut vault = Vault::open(&dir).unwrap();
            vault.set("key", "value").unwrap();
        }
        let reopened = Vault::open(&dir).unwrap();
        assert_eq!(reopened.get("key").unwrap(), "value");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn wrong_master_key_fails_decryption_rather_than_returning_garbage() {
        let dir = temp_dir("wrongkey");
        {
            let mut vault = Vault::open(&dir).unwrap();
            vault.set("key", "value").unwrap();
        }
        // Corrupt the master key in place, simulating a mismatched/rotated
        // key being used against old ciphertext.
        let key_path = dir.join(MASTER_KEY_FILE);
        let mut corrupted = fs::read(&key_path).unwrap();
        corrupted[0] ^= 0xFF;
        fs::write(&key_path, &corrupted).unwrap();

        let vault = Vault::open(&dir).unwrap();
        assert!(matches!(
            vault.get("key"),
            Err(VaultError::DecryptionFailed(_))
        ));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn tampered_ciphertext_is_detected_not_silently_decrypted() {
        // This is the core guarantee of an *authenticated* cipher (AES-GCM):
        // flipping a ciphertext bit must fail decryption outright, not
        // produce different-but-plausible plaintext.
        let dir = temp_dir("tampered");
        let mut vault = Vault::open(&dir).unwrap();
        vault.set("key", "value").unwrap();

        let secrets_path = dir.join(SECRETS_FILE);
        let raw = fs::read_to_string(&secrets_path).unwrap();
        let mut file: VaultFile = serde_json::from_str(&raw).unwrap();
        let secret = file.secrets.get_mut("global\u{1}key").unwrap();
        let mut bytes = base64_decode(&secret.ciphertext).unwrap();
        bytes[0] ^= 0xFF;
        secret.ciphertext = base64_encode(&bytes);
        fs::write(&secrets_path, serde_json::to_string_pretty(&file).unwrap()).unwrap();

        let reloaded = Vault::open(&dir).unwrap();
        assert!(matches!(
            reloaded.get("key"),
            Err(VaultError::DecryptionFailed(_))
        ));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn same_name_in_different_scopes_are_independent_entries() {
        let dir = temp_dir("scope-independence");
        let mut vault = Vault::open(&dir).unwrap();
        vault
            .set_scoped("API_KEY", "global-value", Scope::Global)
            .unwrap();
        vault
            .set_scoped("API_KEY", "repo-value", Scope::Repo("owner/repo".into()))
            .unwrap();
        assert_eq!(
            vault.get_scoped("API_KEY", &Scope::Global).unwrap(),
            "global-value"
        );
        assert_eq!(
            vault
                .get_scoped("API_KEY", &Scope::Repo("owner/repo".into()))
                .unwrap(),
            "repo-value"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_for_repo_includes_global_secrets() {
        let dir = temp_dir("resolve-global");
        let mut vault = Vault::open(&dir).unwrap();
        vault
            .set_scoped("SHARED_TOKEN", "shared-value", Scope::Global)
            .unwrap();
        let resolved = vault.resolve_for_repo("owner/repo", &[]);
        assert_eq!(
            resolved,
            vec![("SHARED_TOKEN".to_owned(), "shared-value".to_owned())]
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_for_repo_includes_matching_group_secrets_only() {
        let dir = temp_dir("resolve-group");
        let mut vault = Vault::open(&dir).unwrap();
        vault
            .set_scoped(
                "DEPLOY_KEY",
                "prod-value",
                Scope::Group("production".into()),
            )
            .unwrap();

        // A repo in the "production" group gets it...
        let in_group = vault.resolve_for_repo("owner/repo-a", &["production".to_owned()]);
        assert_eq!(
            in_group,
            vec![("DEPLOY_KEY".to_owned(), "prod-value".to_owned())]
        );

        // ...a repo not in that group does not.
        let not_in_group = vault.resolve_for_repo("owner/repo-b", &["staging".to_owned()]);
        assert!(not_in_group.is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_for_repo_excludes_other_repos_secrets() {
        let dir = temp_dir("resolve-repo-isolation");
        let mut vault = Vault::open(&dir).unwrap();
        vault
            .set_scoped("SECRET", "repo-a-value", Scope::Repo("owner/repo-a".into()))
            .unwrap();
        let resolved = vault.resolve_for_repo("owner/repo-b", &[]);
        assert!(resolved.is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_for_repo_prefers_most_specific_scope_on_name_collision() {
        let dir = temp_dir("resolve-precedence");
        let mut vault = Vault::open(&dir).unwrap();
        vault
            .set_scoped("API_KEY", "global-value", Scope::Global)
            .unwrap();
        vault
            .set_scoped("API_KEY", "group-value", Scope::Group("prod".into()))
            .unwrap();
        vault
            .set_scoped("API_KEY", "repo-value", Scope::Repo("owner/repo".into()))
            .unwrap();

        // Repo scope should win over both group and global for this repo.
        let resolved = vault.resolve_for_repo("owner/repo", &["prod".to_owned()]);
        assert_eq!(
            resolved,
            vec![("API_KEY".to_owned(), "repo-value".to_owned())]
        );

        // A different repo in the same group falls back to the group value.
        let other_repo = vault.resolve_for_repo("owner/other-repo", &["prod".to_owned()]);
        assert_eq!(
            other_repo,
            vec![("API_KEY".to_owned(), "group-value".to_owned())]
        );

        // A repo in no matching group falls back to global.
        let no_group = vault.resolve_for_repo("owner/unrelated", &[]);
        assert_eq!(
            no_group,
            vec![("API_KEY".to_owned(), "global-value".to_owned())]
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn old_unscoped_records_are_read_as_global_for_compatibility() {
        // Simulates a secrets.json written before Scope existed: no "scope"
        // field at all. This must deserialize successfully with Global
        // assumed, not fail to load or silently drop the secret — losing a
        // previously-working secret on a GitRun upgrade would be a real
        // regression for anyone already using GitVault.
        let dir = temp_dir("legacy-migration");
        std::fs::create_dir_all(&dir).unwrap();
        // Bootstrap a real master key via a normal open, then hand-write a
        // pre-scope-shaped secrets.json against it.
        let mut vault = Vault::open(&dir).unwrap();
        vault.set("LEGACY_TOKEN", "legacy-value").unwrap(); // written as Global via today's code
        drop(vault);

        // Rewrite the persisted JSON to strip the "scope" field, simulating
        // what an old version of this file looked like before the field
        // existed, while keeping the same nonce/ciphertext (still valid,
        // since Global's storage key format is unchanged: "global\u{1}NAME").
        let secrets_path = dir.join(SECRETS_FILE);
        let raw = std::fs::read_to_string(&secrets_path).unwrap();
        let mut value: serde_json::Value = serde_json::from_str(&raw).unwrap();
        if let Some(secrets) = value.get_mut("secrets").and_then(|s| s.as_object_mut()) {
            for (_, secret) in secrets.iter_mut() {
                if let Some(obj) = secret.as_object_mut() {
                    obj.remove("scope");
                }
            }
        }
        std::fs::write(&secrets_path, serde_json::to_string_pretty(&value).unwrap()).unwrap();

        let reloaded = Vault::open(&dir).unwrap();
        assert_eq!(reloaded.get("LEGACY_TOKEN").unwrap(), "legacy-value");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn master_key_file_has_owner_only_permissions() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let dir = temp_dir("perms");
            let _vault = Vault::open(&dir).unwrap();
            let meta = fs::metadata(dir.join(MASTER_KEY_FILE)).unwrap();
            assert_eq!(meta.permissions().mode() & 0o777, 0o600);
            let _ = fs::remove_dir_all(&dir);
        }
    }

    #[test]
    fn decryption_failure_notifies_the_event_sink() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        struct CountingSink(Arc<AtomicUsize>);
        impl VaultEventSink for CountingSink {
            fn on_decryption_failure(&self, _secret_name: &str) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }

        let dir = temp_dir("sink");
        let counter = Arc::new(AtomicUsize::new(0));
        {
            let mut vault = Vault::open(&dir).unwrap();
            vault.set("key", "value").unwrap();
        }
        // Corrupt ciphertext to force a decryption failure on next open.
        let secrets_path = dir.join(SECRETS_FILE);
        let raw = fs::read_to_string(&secrets_path).unwrap();
        let mut file: VaultFile = serde_json::from_str(&raw).unwrap();
        let secret = file.secrets.get_mut("global\u{1}key").unwrap();
        let mut bytes = base64_decode(&secret.ciphertext).unwrap();
        bytes[0] ^= 0xFF;
        secret.ciphertext = base64_encode(&bytes);
        fs::write(&secrets_path, serde_json::to_string_pretty(&file).unwrap()).unwrap();

        let vault = Vault::open_with_sink(&dir, Box::new(CountingSink(counter.clone()))).unwrap();
        let _ = vault.get("key");
        assert_eq!(counter.load(Ordering::SeqCst), 1);
        let _ = fs::remove_dir_all(&dir);
    }
}
