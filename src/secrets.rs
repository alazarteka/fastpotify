//! Durable credentials with an intentionally small security boundary.
//!
//! The current store is a versioned, owner-private file backend. It does not
//! pretend that local encryption with a colocated key protects against code
//! already running as the user. On Unix, every parent Fastpotify owns is mode
//! 0700 and every secret file is mode 0600. Windows inherits the account-only
//! ACL of the user's local application-data directory; it has no Unix mode to
//! validate. A future platform credential store can implement [`SecretStore`]
//! without changing the Web or playback credential lifecycles.

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD_NO_PAD;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use thiserror::Error;

const STORE_VERSION: u32 = 1;
const MAX_SECRET_BYTES: usize = 512 * 1024;
const MAX_ENVELOPE_BYTES: usize = 1024 * 1024;
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

pub type Result<T> = std::result::Result<T, SecretError>;

#[derive(Debug, Error)]
pub enum SecretError {
    #[error("unable to {action} {path}: {source}")]
    Io {
        action: &'static str,
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("refusing unsafe credential path {path}: {reason}")]
    UnsafePath { path: PathBuf, reason: String },
    #[error("stored {kind} credential is corrupt: {reason}")]
    Corrupt {
        kind: &'static str,
        reason: String,
    },
    #[error("unable to encode the {kind} credential: {reason}")]
    Encode {
        kind: &'static str,
        reason: String,
    },
    #[error("the {kind} credential did not pass durable read-back verification")]
    Verification { kind: &'static str },
    #[error("legacy {kind} credentials conflict with the versioned store; neither was deleted")]
    MigrationConflict { kind: &'static str },
    #[error("a partial legacy {kind} credential remains at {path}; refusing to ignore it")]
    StaleLegacy {
        kind: &'static str,
        path: PathBuf,
    },
}

fn io(action: &'static str, path: &Path, source: std::io::Error) -> SecretError {
    SecretError::Io {
        action,
        path: path.to_path_buf(),
        source,
    }
}

/// The two grants are intentionally isolated from one another.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SecretId {
    WebApi,
    Playback,
}

impl SecretId {
    pub const ALL: [Self; 2] = [Self::WebApi, Self::Playback];

    pub fn label(self) -> &'static str {
        match self {
            Self::WebApi => "Web API",
            Self::Playback => "playback",
        }
    }

    fn envelope_name(self) -> &'static str {
        match self {
            Self::WebApi => "web-api",
            Self::Playback => "playback",
        }
    }

    fn file_name(self) -> &'static str {
        match self {
            Self::WebApi => "web-api.secret",
            Self::Playback => "playback.secret",
        }
    }
}

/// Byte-oriented so a future platform store need not know any credential
/// schema. Serialization stays at the call site and both grants remain
/// separate items.
pub trait SecretStore: Send + Sync {
    fn load(&self, id: SecretId) -> Result<Option<Vec<u8>>>;
    fn store(&self, id: SecretId, secret: &[u8]) -> Result<()>;
    fn delete(&self, id: SecretId) -> Result<()>;
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    version: u32,
    kind: String,
    payload: String,
}

/// Version-one private files. The contents are encoded, not encrypted: the
/// directory and file access controls are the security boundary.
#[derive(Clone)]
pub struct PrivateFileStore {
    root: PathBuf,
}

impl PrivateFileStore {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn path(&self, id: SecretId) -> PathBuf {
        self.root.join(id.file_name())
    }

    fn prepare(&self) -> Result<()> {
        ensure_private_dir(&self.root)
    }

    fn temporary_path(&self, id: SecretId) -> PathBuf {
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        self.root.join(format!(
            ".{}.tmp-{}-{sequence}",
            id.envelope_name(),
            std::process::id()
        ))
    }

    fn open_new_temporary(&self, id: SecretId) -> Result<(PathBuf, File)> {
        for _ in 0..32 {
            let path = self.temporary_path(id);
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt as _;
                options.mode(0o600);
            }
            match options.open(&path) {
                Ok(file) => {
                    validate_open_file(&path, &file, true)?;
                    return Ok((path, file));
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(io("create a temporary credential file", &path, error)),
            }
        }
        Err(SecretError::UnsafePath {
            path: self.root.clone(),
            reason: "could not allocate a fresh temporary filename".into(),
        })
    }
}

impl SecretStore for PrivateFileStore {
    fn load(&self, id: SecretId) -> Result<Option<Vec<u8>>> {
        self.prepare()?;
        let path = self.path(id);
        let Some(bytes) = read_private_file(&path, MAX_ENVELOPE_BYTES, true)? else {
            return Ok(None);
        };
        let envelope: Envelope = serde_json::from_slice(&bytes).map_err(|error| {
            SecretError::Corrupt {
                kind: id.label(),
                reason: error.to_string(),
            }
        })?;
        if envelope.version != STORE_VERSION {
            return Err(SecretError::Corrupt {
                kind: id.label(),
                reason: format!("unsupported store version {}", envelope.version),
            });
        }
        if envelope.kind != id.envelope_name() {
            return Err(SecretError::Corrupt {
                kind: id.label(),
                reason: "credential type does not match its filename".into(),
            });
        }
        let payload = STANDARD_NO_PAD
            .decode(envelope.payload.as_bytes())
            .map_err(|error| SecretError::Corrupt {
                kind: id.label(),
                reason: format!("invalid payload encoding: {error}"),
            })?;
        if payload.is_empty() || payload.len() > MAX_SECRET_BYTES {
            return Err(SecretError::Corrupt {
                kind: id.label(),
                reason: "payload is empty or exceeds the credential size limit".into(),
            });
        }
        Ok(Some(payload))
    }

    fn store(&self, id: SecretId, secret: &[u8]) -> Result<()> {
        if secret.is_empty() || secret.len() > MAX_SECRET_BYTES {
            return Err(SecretError::Encode {
                kind: id.label(),
                reason: "payload is empty or exceeds the credential size limit".into(),
            });
        }
        self.prepare()?;
        let destination = self.path(id);
        if destination_exists(&destination)? {
            validate_path_as_private_file(&destination, true)?;
        }
        let envelope = Envelope {
            version: STORE_VERSION,
            kind: id.envelope_name().to_string(),
            payload: STANDARD_NO_PAD.encode(secret),
        };
        let encoded = serde_json::to_vec(&envelope).map_err(|error| SecretError::Encode {
            kind: id.label(),
            reason: error.to_string(),
        })?;
        let (temporary, mut file) = self.open_new_temporary(id)?;
        let result = (|| {
            file.write_all(&encoded)
                .map_err(|error| io("write a temporary credential file", &temporary, error))?;
            file.sync_all()
                .map_err(|error| io("sync a temporary credential file", &temporary, error))?;
            validate_open_file(&temporary, &file, true)?;
            drop(file);
            atomic_replace(&temporary, &destination)?;
            sync_parent(&self.root)?;
            match self.load(id)? {
                Some(read_back) if read_back == secret => Ok(()),
                _ => Err(SecretError::Verification { kind: id.label() }),
            }
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&temporary);
        }
        result
    }

    fn delete(&self, id: SecretId) -> Result<()> {
        self.prepare()?;
        let path = self.path(id);
        delete_private_file(&path, true)?;
        sync_parent(&self.root)
    }
}

pub fn load_json<T: DeserializeOwned>(
    store: &dyn SecretStore,
    id: SecretId,
) -> Result<Option<T>> {
    let Some(bytes) = store.load(id)? else {
        return Ok(None);
    };
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|error| SecretError::Corrupt {
            kind: id.label(),
            reason: error.to_string(),
        })
}

pub fn store_json<T: Serialize>(
    store: &dyn SecretStore,
    id: SecretId,
    value: &T,
) -> Result<()> {
    let bytes = canonical_json(id, value)?;
    store.store(id, &bytes)
}

fn canonical_json<T: Serialize + ?Sized>(id: SecretId, value: &T) -> Result<Vec<u8>> {
    serde_json::to_vec(value).map_err(|error| SecretError::Encode {
        kind: id.label(),
        reason: error.to_string(),
    })
}

/// An old plaintext location. `stale` contains temporary files an interrupted
/// legacy writer may have left; those are never guessed to be complete.
#[derive(Clone)]
pub struct LegacySecret {
    pub primary: PathBuf,
    pub stale: Vec<PathBuf>,
}

impl LegacySecret {
    pub fn new(primary: PathBuf) -> Self {
        Self {
            primary,
            stale: Vec::new(),
        }
    }

    pub fn with_stale(mut self, path: PathBuf) -> Self {
        self.stale.push(path);
        self
    }
}

trait LegacyIo {
    fn load(&self, id: SecretId) -> Result<Option<Vec<u8>>>;
    fn delete(&self, id: SecretId) -> Result<()>;
}

impl LegacyIo for LegacySecret {
    fn load(&self, id: SecretId) -> Result<Option<Vec<u8>>> {
        for path in &self.stale {
            if destination_exists(path)? {
                return Err(SecretError::StaleLegacy {
                    kind: id.label(),
                    path: path.clone(),
                });
            }
        }
        read_private_file(&self.primary, MAX_SECRET_BYTES, true)
    }

    fn delete(&self, _id: SecretId) -> Result<()> {
        delete_private_file(&self.primary, false)
    }
}

/// Moves a legacy JSON credential transactionally. The legacy file survives
/// every write, read-back, comparison, and deletion failure. If deletion alone
/// fails, the next call sees matching old and new values and safely retries it.
pub fn load_json_migrating<T: Serialize + DeserializeOwned>(
    store: &dyn SecretStore,
    id: SecretId,
    legacy: &LegacySecret,
) -> Result<Option<T>> {
    load_json_migrating_with(store, id, legacy)
}

fn load_json_migrating_with<T: Serialize + DeserializeOwned>(
    store: &dyn SecretStore,
    id: SecretId,
    legacy: &dyn LegacyIo,
) -> Result<Option<T>> {
    let current = load_json::<T>(store, id)?;
    let old_bytes = legacy.load(id)?;
    let old = match old_bytes {
        Some(bytes) => Some(serde_json::from_slice::<T>(&bytes).map_err(|error| {
            SecretError::Corrupt {
                kind: id.label(),
                reason: format!("legacy JSON: {error}"),
            }
        })?),
        None => None,
    };

    match (current, old) {
        (None, None) => Ok(None),
        (Some(value), None) => Ok(Some(value)),
        (Some(value), Some(old)) => {
            if canonical_json(id, &value)? != canonical_json(id, &old)? {
                return Err(SecretError::MigrationConflict { kind: id.label() });
            }
            legacy.delete(id)?;
            Ok(Some(value))
        }
        (None, Some(old)) => {
            store_json(store, id, &old)?;
            let verified = load_json::<T>(store, id)?
                .ok_or(SecretError::Verification { kind: id.label() })?;
            if canonical_json(id, &verified)? != canonical_json(id, &old)? {
                return Err(SecretError::Verification { kind: id.label() });
            }
            legacy.delete(id)?;
            Ok(Some(verified))
        }
    }
}

#[derive(Debug)]
pub struct ClearError {
    failures: Vec<String>,
}

impl std::fmt::Display for ClearError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}", self.failures.join("; "))
    }
}

impl std::error::Error for ClearError {}

fn record_clear(result: Result<()>, label: &str, failures: &mut Vec<String>) {
    if let Err(error) = result {
        failures.push(format!("{label}: {error}"));
    }
}

fn clear_legacy_files(legacy: &LegacySecret, failures: &mut Vec<String>) {
    record_clear(
        delete_private_file(&legacy.primary, false),
        "legacy credential",
        failures,
    );
    for stale in &legacy.stale {
        record_clear(
            delete_private_file(stale, false),
            "legacy temporary credential",
            failures,
        );
    }
}

/// Clears one grant after its in-memory provider has already been dropped.
pub fn clear_secret_copies(
    store: &dyn SecretStore,
    id: SecretId,
    legacy: &LegacySecret,
) -> std::result::Result<(), ClearError> {
    let mut failures = Vec::new();
    record_clear(store.delete(id), id.label(), &mut failures);
    clear_legacy_files(legacy, &mut failures);
    if failures.is_empty() {
        Ok(())
    } else {
        Err(ClearError { failures })
    }
}

/// Sign-out ordering is part of the contract: memory and the live engine are
/// cleared first, then every new and legacy location is attempted even if an
/// earlier deletion fails.
pub fn clear_all_secrets(
    store: &dyn SecretStore,
    web_legacy: &LegacySecret,
    playback_legacy: &LegacySecret,
    clear_memory: impl FnOnce(),
) -> std::result::Result<(), ClearError> {
    clear_memory();
    let mut failures = Vec::new();
    for id in SecretId::ALL {
        record_clear(store.delete(id), id.label(), &mut failures);
    }
    clear_legacy_files(web_legacy, &mut failures);
    clear_legacy_files(playback_legacy, &mut failures);
    if failures.is_empty() {
        Ok(())
    } else {
        Err(ClearError { failures })
    }
}

/// Creates or hardens an application-owned directory. Symlinks and foreign
/// ownership are rejected. Existing owner directories are tightened to 0700.
pub fn ensure_private_dir(path: &Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => validate_or_harden_directory(path, &metadata),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            std::fs::create_dir_all(path)
                .map_err(|error| io("create a private directory", path, error))?;
            let metadata = std::fs::symlink_metadata(path)
                .map_err(|error| io("inspect a private directory", path, error))?;
            validate_or_harden_directory(path, &metadata)
        }
        Err(error) => Err(io("inspect a private directory", path, error)),
    }
}

fn validate_or_harden_directory(path: &Path, metadata: &std::fs::Metadata) -> Result<()> {
    if is_link_or_reparse(metadata) || !metadata.is_dir() {
        return Err(SecretError::UnsafePath {
            path: path.to_path_buf(),
            reason: "expected a real directory, not a symlink or other file".into(),
        });
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
        if metadata.uid() != effective_uid() {
            return Err(SecretError::UnsafePath {
                path: path.to_path_buf(),
                reason: "directory is not owned by the current user".into(),
            });
        }
        if metadata.mode() & 0o777 != 0o700 {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
                .map_err(|error| io("set owner-only directory permissions on", path, error))?;
            let hardened = std::fs::symlink_metadata(path)
                .map_err(|error| io("verify a private directory", path, error))?;
            if is_link_or_reparse(&hardened)
                || !hardened.is_dir()
                || hardened.uid() != effective_uid()
                || hardened.mode() & 0o777 != 0o700
            {
                return Err(SecretError::UnsafePath {
                    path: path.to_path_buf(),
                    reason: "could not enforce mode 0700".into(),
                });
            }
        }
    }
    Ok(())
}

fn destination_exists(path: &Path) -> Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(io("inspect a credential path", path, error)),
    }
}

fn validate_path_as_private_file(path: &Path, strict_mode: bool) -> Result<()> {
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| io("inspect a credential file", path, error))?;
    validate_file_metadata(path, &metadata, strict_mode)
}

fn validate_file_metadata(
    path: &Path,
    metadata: &std::fs::Metadata,
    strict_mode: bool,
) -> Result<()> {
    if is_link_or_reparse(metadata) || !metadata.is_file() {
        return Err(SecretError::UnsafePath {
            path: path.to_path_buf(),
            reason: "expected a real regular file, not a symlink or other object".into(),
        });
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        if metadata.uid() != effective_uid() {
            return Err(SecretError::UnsafePath {
                path: path.to_path_buf(),
                reason: "file is not owned by the current user".into(),
            });
        }
        if strict_mode && metadata.mode() & 0o777 != 0o600 {
            return Err(SecretError::UnsafePath {
                path: path.to_path_buf(),
                reason: format!(
                    "expected mode 0600, found {:04o}",
                    metadata.mode() & 0o777
                ),
            });
        }
        if metadata.nlink() != 1 {
            return Err(SecretError::UnsafePath {
                path: path.to_path_buf(),
                reason: "credential file has more than one hard link".into(),
            });
        }
    }
    Ok(())
}

fn validate_file_parent(path: &Path) -> Result<()> {
    let Some(parent) = path.parent() else {
        return Err(SecretError::UnsafePath {
            path: path.to_path_buf(),
            reason: "credential file has no parent directory".into(),
        });
    };
    let metadata = std::fs::symlink_metadata(parent)
        .map_err(|error| io("inspect a credential parent directory", parent, error))?;
    if is_link_or_reparse(&metadata) || !metadata.is_dir() {
        return Err(SecretError::UnsafePath {
            path: parent.to_path_buf(),
            reason: "credential parent is not a real directory".into(),
        });
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        if metadata.uid() != effective_uid() {
            return Err(SecretError::UnsafePath {
                path: parent.to_path_buf(),
                reason: "credential parent is not owned by the current user".into(),
            });
        }
    }
    Ok(())
}

fn is_link_or_reparse(metadata: &std::fs::Metadata) -> bool {
    if metadata.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt as _;
        // Junctions and other reparse points are not all reported as
        // `is_symlink`, but none belongs inside the credential boundary.
        return metadata.file_attributes() & 0x400 != 0;
    }
    #[cfg(not(windows))]
    false
}

fn validate_open_file(path: &Path, file: &File, strict_mode: bool) -> Result<()> {
    let opened = file
        .metadata()
        .map_err(|error| io("inspect an opened credential file", path, error))?;
    validate_file_metadata(path, &opened, strict_mode)?;
    let named = std::fs::symlink_metadata(path)
        .map_err(|error| io("reinspect a credential filename", path, error))?;
    validate_file_metadata(path, &named, strict_mode)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        if opened.dev() != named.dev() || opened.ino() != named.ino() {
            return Err(SecretError::UnsafePath {
                path: path.to_path_buf(),
                reason: "filename changed while it was being opened".into(),
            });
        }
    }
    Ok(())
}

fn read_private_file(path: &Path, limit: usize, strict_mode: bool) -> Result<Option<Vec<u8>>> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            validate_file_parent(path)?;
            validate_file_metadata(path, &metadata, strict_mode)?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(io("inspect a credential file", path, error)),
    }
    let mut file = OpenOptions::new()
        .read(true)
        .open(path)
        .map_err(|error| io("open a credential file", path, error))?;
    validate_open_file(path, &file, strict_mode)?;
    let mut bytes = Vec::new();
    Read::by_ref(&mut file)
        .take((limit + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| io("read a credential file", path, error))?;
    if bytes.len() > limit {
        return Err(SecretError::UnsafePath {
            path: path.to_path_buf(),
            reason: format!("file exceeds the {limit}-byte limit"),
        });
    }
    Ok(Some(bytes))
}

fn delete_private_file(path: &Path, strict_mode: bool) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            validate_file_parent(path)?;
            validate_file_metadata(path, &metadata, strict_mode)?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(io("inspect a credential before deletion", path, error)),
    }
    std::fs::remove_file(path).map_err(|error| io("delete a credential file", path, error))
}

#[cfg(not(windows))]
fn atomic_replace(source: &Path, destination: &Path) -> Result<()> {
    std::fs::rename(source, destination)
        .map_err(|error| io("atomically replace a credential file at", destination, error))
}

#[cfg(windows)]
fn atomic_replace(source: &Path, destination: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt as _;
    use windows_sys::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };

    let source_wide: Vec<u16> = source.as_os_str().encode_wide().chain(Some(0)).collect();
    let destination_wide: Vec<u16> = destination.as_os_str().encode_wide().chain(Some(0)).collect();
    let result = unsafe {
        MoveFileExW(
            source_wide.as_ptr(),
            destination_wide.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if result == 0 {
        Err(io(
            "atomically replace a credential file at",
            destination,
            std::io::Error::last_os_error(),
        ))
    } else {
        Ok(())
    }
}

#[cfg(unix)]
fn sync_parent(path: &Path) -> Result<()> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| io("sync a credential directory", path, error))
}

#[cfg(not(unix))]
fn sync_parent(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn effective_uid() -> u32 {
    unsafe extern "C" {
        fn geteuid() -> u32;
    }
    // SAFETY: `geteuid` has no preconditions and returns a value.
    unsafe { geteuid() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    #[derive(Serialize, Deserialize, PartialEq)]
    struct ExampleSecret {
        value: String,
    }

    #[derive(Default)]
    struct MockState {
        values: HashMap<&'static str, Vec<u8>>,
        stores: usize,
        loads_after_store: usize,
        deletes: Vec<SecretId>,
        fail_store: bool,
        fail_read_back: bool,
        mismatch_read_back: bool,
        fail_delete: Option<SecretId>,
        memory_was_cleared: Option<Arc<std::sync::atomic::AtomicBool>>,
    }

    #[derive(Default)]
    struct MockStore(Mutex<MockState>);

    impl SecretStore for MockStore {
        fn load(&self, id: SecretId) -> Result<Option<Vec<u8>>> {
            let mut state = self.0.lock().unwrap();
            if state.stores > 0 {
                state.loads_after_store += 1;
                if state.fail_read_back {
                    return Err(SecretError::Verification { kind: id.label() });
                }
                if state.mismatch_read_back {
                    return Ok(Some(br#"{"value":"different"}"#.to_vec()));
                }
            }
            Ok(state.values.get(id.envelope_name()).cloned())
        }

        fn store(&self, id: SecretId, secret: &[u8]) -> Result<()> {
            let mut state = self.0.lock().unwrap();
            state.stores += 1;
            if state.fail_store {
                return Err(SecretError::Verification { kind: id.label() });
            }
            state.values.insert(id.envelope_name(), secret.to_vec());
            Ok(())
        }

        fn delete(&self, id: SecretId) -> Result<()> {
            let mut state = self.0.lock().unwrap();
            if let Some(cleared) = &state.memory_was_cleared {
                assert!(cleared.load(Ordering::SeqCst));
            }
            state.deletes.push(id);
            if state.fail_delete == Some(id) {
                return Err(SecretError::Verification { kind: id.label() });
            }
            state.values.remove(id.envelope_name());
            Ok(())
        }
    }

    #[derive(Default)]
    struct MockLegacy {
        value: Mutex<Option<Vec<u8>>>,
        fail_delete: bool,
    }

    impl LegacyIo for MockLegacy {
        fn load(&self, _id: SecretId) -> Result<Option<Vec<u8>>> {
            Ok(self.value.lock().unwrap().clone())
        }

        fn delete(&self, id: SecretId) -> Result<()> {
            if self.fail_delete {
                return Err(SecretError::Verification { kind: id.label() });
            }
            *self.value.lock().unwrap() = None;
            Ok(())
        }
    }

    fn encoded_example() -> Vec<u8> {
        serde_json::to_vec(&ExampleSecret {
            value: "test-only".into(),
        })
        .unwrap()
    }

    #[test]
    fn migration_preserves_legacy_on_store_failure() {
        let store = MockStore::default();
        store.0.lock().unwrap().fail_store = true;
        let legacy = MockLegacy {
            value: Mutex::new(Some(encoded_example())),
            fail_delete: false,
        };
        assert!(load_json_migrating_with::<ExampleSecret>(&store, SecretId::WebApi, &legacy).is_err());
        assert!(legacy.value.lock().unwrap().is_some());
    }

    #[test]
    fn migration_preserves_legacy_on_read_back_failure_or_mismatch() {
        for mismatch in [false, true] {
            let store = MockStore::default();
            {
                let mut state = store.0.lock().unwrap();
                state.fail_read_back = !mismatch;
                state.mismatch_read_back = mismatch;
            }
            let legacy = MockLegacy {
                value: Mutex::new(Some(encoded_example())),
                fail_delete: false,
            };
            assert!(
                load_json_migrating_with::<ExampleSecret>(&store, SecretId::WebApi, &legacy)
                    .is_err()
            );
            assert!(legacy.value.lock().unwrap().is_some());
        }
    }

    #[test]
    fn migration_is_idempotent_when_legacy_deletion_initially_fails() {
        let store = MockStore::default();
        let mut legacy = MockLegacy {
            value: Mutex::new(Some(encoded_example())),
            fail_delete: true,
        };
        assert!(load_json_migrating_with::<ExampleSecret>(&store, SecretId::Playback, &legacy).is_err());
        assert!(legacy.value.lock().unwrap().is_some());
        legacy.fail_delete = false;
        let migrated = load_json_migrating_with::<ExampleSecret>(
            &store,
            SecretId::Playback,
            &legacy,
        )
        .unwrap()
        .unwrap();
        assert_eq!(migrated.value, "test-only");
        assert!(legacy.value.lock().unwrap().is_none());
    }

    #[test]
    fn sign_out_clears_memory_first_and_attempts_both_new_items() {
        let store = MockStore::default();
        let cleared = Arc::new(std::sync::atomic::AtomicBool::new(false));
        {
            let mut state = store.0.lock().unwrap();
            state.fail_delete = Some(SecretId::WebApi);
            state.memory_was_cleared = Some(Arc::clone(&cleared));
        }
        let nowhere = LegacySecret::new(
            std::env::temp_dir().join(format!("fastpotify-absent-{}", std::process::id())),
        );
        let result = clear_all_secrets(&store, &nowhere, &nowhere, || {
            cleared.store(true, Ordering::SeqCst);
        });
        assert!(result.is_err());
        let state = store.0.lock().unwrap();
        assert_eq!(state.deletes.len(), 2);
        assert!(state.deletes.contains(&SecretId::WebApi));
        assert!(state.deletes.contains(&SecretId::Playback));
    }

    #[cfg(unix)]
    #[test]
    fn private_store_uses_owner_only_modes_and_rejects_symlinks() {
        use std::os::unix::fs::MetadataExt as _;

        let root = std::env::temp_dir().join(format!(
            "fastpotify-secret-test-{}-{}",
            std::process::id(),
            TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        let store = PrivateFileStore::new(root.clone());
        store.store(SecretId::WebApi, b"test-only").unwrap();
        assert_eq!(std::fs::metadata(&root).unwrap().mode() & 0o777, 0o700);
        let file = root.join(SecretId::WebApi.file_name());
        assert_eq!(std::fs::metadata(&file).unwrap().mode() & 0o777, 0o600);
        store.delete(SecretId::WebApi).unwrap();
        std::os::unix::fs::symlink("elsewhere", &file).unwrap();
        assert!(store.load(SecretId::WebApi).is_err());
        assert!(std::fs::symlink_metadata(&file).unwrap().file_type().is_symlink());
        let _ = std::fs::remove_file(file);
        let _ = std::fs::remove_dir(root);
    }
}
