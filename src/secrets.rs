//! Durable credentials with an intentionally small security boundary.
//!
//! The current store is a versioned, owner-private file backend. It does not
//! pretend that local encryption with a colocated key protects against code
//! already running as the user. On Unix, every parent Fastpotify owns is mode
//! 0700 and every secret file is mode 0600. Windows inherits the account-only
//! ACL of the user's local application-data directory and rejects reparse or
//! multiply-linked files using opened-handle metadata. A future platform
//! credential store can implement [`SecretStore`] without changing the Web or
//! playback credential lifecycles.

use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
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
            platform_file::configure_temporary(&mut options);
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
        let _root_guard = platform_file::lock_directory(&self.root)?;
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
            atomic_replace(&file, &temporary, &destination)?;
            drop(file);
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

trait PreparedLegacy {
    fn bytes(&self) -> &[u8];
    fn delete(self: Box<Self>, id: SecretId) -> Result<()>;
}

trait LegacyIo {
    fn prepare(&self, id: SecretId) -> Result<Option<Box<dyn PreparedLegacy + '_>>>;
}

impl LegacyIo for LegacySecret {
    fn prepare(&self, id: SecretId) -> Result<Option<Box<dyn PreparedLegacy + '_>>> {
        for path in &self.stale {
            if destination_exists(path)? {
                return Err(SecretError::StaleLegacy {
                    kind: id.label(),
                    path: path.clone(),
                });
            }
        }
        let migration = legacy_migration_path(&self.primary);
        if destination_exists(&migration)? {
            return Err(SecretError::StaleLegacy {
                kind: id.label(),
                path: migration,
            });
        }
        prepare_legacy_private_file(&self.primary, migration, MAX_SECRET_BYTES).map(|prepared| {
            prepared.map(|prepared| Box::new(prepared) as Box<dyn PreparedLegacy + '_>)
        })
    }
}

fn legacy_migration_path(path: &Path) -> PathBuf {
    let mut name = OsString::from(".");
    name.push(path.file_name().unwrap_or_default());
    name.push(".migrating");
    path.with_file_name(name)
}

/// Moves a legacy JSON credential transactionally. The validated legacy handle
/// remains live through write and read-back; a deletion failure leaves either
/// the original name or a captured copy at the reported migration path.
pub fn load_json_migrating<T: Serialize + DeserializeOwned>(
    store: &dyn SecretStore,
    id: SecretId,
    legacy: &LegacySecret,
) -> Result<Option<T>> {
    load_json_migrating_with(store, id, legacy, &|_| Ok(()))
}

/// As [`load_json_migrating`], with semantic validation before any legacy
/// copy can be deleted.
pub fn load_json_migrating_validated<T: Serialize + DeserializeOwned>(
    store: &dyn SecretStore,
    id: SecretId,
    legacy: &LegacySecret,
    validate: impl Fn(&T) -> Result<()>,
) -> Result<Option<T>> {
    load_json_migrating_with(store, id, legacy, &validate)
}

fn load_json_migrating_with<T: Serialize + DeserializeOwned>(
    store: &dyn SecretStore,
    id: SecretId,
    legacy: &dyn LegacyIo,
    validate: &dyn Fn(&T) -> Result<()>,
) -> Result<Option<T>> {
    let current = load_json::<T>(store, id)?;
    if let Some(value) = &current {
        validate(value)?;
    }
    let old = match legacy.prepare(id)? {
        Some(prepared) => {
            let value = serde_json::from_slice::<T>(prepared.bytes()).map_err(|error| {
                SecretError::Corrupt {
                    kind: id.label(),
                    reason: format!("legacy JSON: {error}"),
                }
            })?;
            validate(&value)?;
            Some((value, prepared))
        }
        None => None,
    };

    match (current, old) {
        (None, None) => Ok(None),
        (Some(value), None) => Ok(Some(value)),
        (Some(value), Some((old, prepared))) => {
            if canonical_json(id, &value)? != canonical_json(id, &old)? {
                return Err(SecretError::MigrationConflict { kind: id.label() });
            }
            prepared.delete(id)?;
            Ok(Some(value))
        }
        (None, Some((old, prepared))) => {
            store_json(store, id, &old)?;
            let verified = load_json::<T>(store, id)?
                .ok_or(SecretError::Verification { kind: id.label() })?;
            if canonical_json(id, &verified)? != canonical_json(id, &old)? {
                return Err(SecretError::Verification { kind: id.label() });
            }
            prepared.delete(id)?;
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
    record_clear(
        delete_private_file(&legacy_migration_path(&legacy.primary), false),
        "interrupted legacy migration",
        failures,
    );
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

/// Atomically replaces any application-private file with owner-only mode.
/// This is also used for privacy-sensitive settings, caches, and logs.
pub fn write_private_atomic(path: &Path, contents: &[u8]) -> Result<()> {
    let parent = path.parent().ok_or_else(|| SecretError::UnsafePath {
        path: path.to_path_buf(),
        reason: "private file has no parent directory".into(),
    })?;
    ensure_private_dir(parent)?;
    let _parent_guard = platform_file::lock_directory(parent)?;
    if destination_exists(path)? {
        // Older non-secret application files may predate the private writer
        // and have the process umask rather than 0600. They still have to be
        // regular, singly linked, and ours; the atomic replacement below is
        // always 0600.
        validate_path_as_private_file(path, false)?;
    }
    let stem = path
        .file_name()
        .map(|name| name.to_string_lossy())
        .unwrap_or_default();
    let mut created = None;
    for _ in 0..32 {
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let temporary = parent.join(format!(
            ".{stem}.tmp-{}-{sequence}",
            std::process::id()
        ));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        platform_file::configure_temporary(&mut options);
        match options.open(&temporary) {
            Ok(file) => {
                created = Some((temporary, file));
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(io("create a temporary private file", &temporary, error)),
        }
    }
    let (temporary, mut file) = created.ok_or_else(|| SecretError::UnsafePath {
        path: parent.to_path_buf(),
        reason: "could not allocate a fresh temporary filename".into(),
    })?;
    let result = (|| {
        validate_open_file(&temporary, &file, true)?;
        file.write_all(contents)
            .map_err(|error| io("write a temporary private file", &temporary, error))?;
        file.sync_all()
            .map_err(|error| io("sync a temporary private file", &temporary, error))?;
        atomic_replace(&file, &temporary, path)?;
        drop(file);
        sync_parent(parent)?;
        match read_private_file(path, contents.len(), true)? {
            Some(read_back) if read_back == contents => Ok(()),
            _ => Err(SecretError::Verification {
                kind: "private file",
            }),
        }
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

/// Opens a private file for a fresh log. Existing unsafe paths are rejected
/// instead of followed.
pub fn open_private_truncate(path: &Path) -> Result<File> {
    let parent = path.parent().ok_or_else(|| SecretError::UnsafePath {
        path: path.to_path_buf(),
        reason: "private file has no parent directory".into(),
    })?;
    ensure_private_dir(parent)?;
    let _parent_guard = platform_file::lock_directory(parent)?;
    if destination_exists(path)? {
        validate_path_as_private_file(path, false)?;
    }
    let mut options = OpenOptions::new();
    // Do not truncate until the opened handle and the filename have both
    // passed validation. This keeps a last-moment symlink swap from
    // truncating its target before it is rejected.
    options.write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    platform_file::configure_regular(&mut options);
    let file = options
        .open(path)
        .map_err(|error| io("open a private file", path, error))?;
    validate_open_file(path, &file, false)?;
    platform_file::harden(&file, path)?;
    validate_open_file(path, &file, true)?;
    file.set_len(0)
        .map_err(|error| io("truncate a private file", path, error))?;
    Ok(file)
}

/// Reads an application-owned file under a size cap. Legacy process-umask
/// modes are accepted because the parent is 0700; symlinks, foreign owners,
/// and hard links are still rejected. The next atomic write replaces it with
/// a 0600 file.
pub fn read_private_bounded(path: &Path, limit: usize) -> Result<Option<Vec<u8>>> {
    let parent = path.parent().ok_or_else(|| SecretError::UnsafePath {
        path: path.to_path_buf(),
        reason: "private file has no parent directory".into(),
    })?;
    ensure_private_dir(parent)?;
    read_private_file(path, limit, false)
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
        let directory = File::open(path)
            .map_err(|error| io("open a private directory for validation", path, error))?;
        let opened = directory
            .metadata()
            .map_err(|error| io("inspect an opened private directory", path, error))?;
        if !opened.is_dir()
            || opened.dev() != metadata.dev()
            || opened.ino() != metadata.ino()
        {
            return Err(SecretError::UnsafePath {
                path: path.to_path_buf(),
                reason: "directory changed while it was being opened".into(),
            });
        }
        if opened.uid() != effective_uid() {
            return Err(SecretError::UnsafePath {
                path: path.to_path_buf(),
                reason: "directory is not owned by the current user".into(),
            });
        }
        if opened.mode() & 0o777 != 0o700 {
            directory
                .set_permissions(std::fs::Permissions::from_mode(0o700))
                .map_err(|error| io("set owner-only directory permissions on", path, error))?;
        }
        let hardened = directory
            .metadata()
            .map_err(|error| io("verify an opened private directory", path, error))?;
        let named = std::fs::symlink_metadata(path)
            .map_err(|error| io("reinspect a private directory", path, error))?;
        if is_link_or_reparse(&named)
            || !named.is_dir()
            || hardened.uid() != effective_uid()
            || hardened.mode() & 0o777 != 0o700
            || hardened.dev() != named.dev()
            || hardened.ino() != named.ino()
        {
            return Err(SecretError::UnsafePath {
                path: path.to_path_buf(),
                reason: "could not enforce mode 0700 on the same directory".into(),
            });
        }
    }
    let _validated = platform_file::lock_directory(path)?;
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
    validate_file_metadata(path, &metadata, strict_mode)?;
    platform_file::validate_path(path)
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
    platform_file::validate_named_identity(path, file, &opened, &named)
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
    let parent = path.parent().expect("validated credential parent");
    let _parent_guard = platform_file::lock_directory(parent)?;
    let mut options = OpenOptions::new();
    options.read(true);
    platform_file::configure_regular(&mut options);
    let mut file = options
        .open(path)
        .map_err(|error| io("open a credential file", path, error))?;
    validate_open_file(path, &file, strict_mode)?;
    read_open_bounded(path, &mut file, limit).map(Some)
}

struct PreparedLegacyFile {
    path: PathBuf,
    migration: PathBuf,
    file: File,
    bytes: Vec<u8>,
    limit: usize,
    _parent_guard: Option<File>,
}

impl PreparedLegacy for PreparedLegacyFile {
    fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    fn delete(mut self: Box<Self>, _id: SecretId) -> Result<()> {
        platform_file::delete_prepared_legacy(
            &mut self.file,
            &self.path,
            &self.migration,
            &self.bytes,
            self.limit,
        )
    }
}

/// Historical librespot files were created with the process umask. Secure the
/// directory and file, then retain their validated handles until the migration
/// either fails or consumes that exact file.
fn prepare_legacy_private_file(
    path: &Path,
    migration: PathBuf,
    limit: usize,
) -> Result<Option<PreparedLegacyFile>> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(io("inspect a legacy credential file", path, error)),
    };
    let parent = path.parent().ok_or_else(|| SecretError::UnsafePath {
        path: path.to_path_buf(),
        reason: "legacy credential has no parent directory".into(),
    })?;
    ensure_private_dir(parent)?;
    let parent_guard = platform_file::lock_directory(parent)?;
    validate_file_metadata(path, &metadata, false)?;

    let mut options = OpenOptions::new();
    options.read(true);
    platform_file::configure_legacy(&mut options);
    let mut file = options
        .open(path)
        .map_err(|error| io("open a legacy credential file", path, error))?;
    validate_open_file(path, &file, false)?;
    platform_file::harden(&file, path)?;
    validate_open_file(path, &file, true)?;
    let bytes = read_open_bounded(path, &mut file, limit)?;
    Ok(Some(PreparedLegacyFile {
        path: path.to_path_buf(),
        migration,
        file,
        bytes,
        limit,
        _parent_guard: parent_guard,
    }))
}

fn read_open_bounded(path: &Path, file: &mut File, limit: usize) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    Read::by_ref(file)
        .take((limit + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| io("read a credential file", path, error))?;
    if bytes.len() > limit {
        return Err(SecretError::UnsafePath {
            path: path.to_path_buf(),
            reason: format!("file exceeds the {limit}-byte limit"),
        });
    }
    Ok(bytes)
}

fn verify_open_contents(
    path: &Path,
    file: &mut File,
    expected: &[u8],
    limit: usize,
) -> Result<()> {
    file.seek(SeekFrom::Start(0))
        .map_err(|error| io("rewind a legacy credential file", path, error))?;
    let actual = read_open_bounded(path, file, limit)?;
    if actual != expected {
        return Err(SecretError::UnsafePath {
            path: path.to_path_buf(),
            reason: "legacy credential changed during migration".into(),
        });
    }
    Ok(())
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
    let parent = path.parent().expect("validated credential parent");
    let _parent_guard = platform_file::lock_directory(parent)?;
    validate_path_as_private_file(path, strict_mode)?;
    std::fs::remove_file(path).map_err(|error| io("delete a credential file", path, error))
}

#[cfg(unix)]
mod platform_file {
    use super::*;

    pub fn configure_regular(_options: &mut OpenOptions) {}

    pub fn configure_legacy(_options: &mut OpenOptions) {}

    pub fn configure_temporary(_options: &mut OpenOptions) {}

    pub fn lock_directory(_path: &Path) -> Result<Option<File>> {
        Ok(None)
    }

    pub fn validate_path(_path: &Path) -> Result<()> {
        Ok(())
    }

    pub fn harden(file: &File, path: &Path) -> Result<()> {
        use std::os::unix::fs::PermissionsExt as _;

        file.set_permissions(std::fs::Permissions::from_mode(0o600))
            .map_err(|error| io("set owner-only file permissions on", path, error))
    }

    pub fn validate_named_identity(
        path: &Path,
        _file: &File,
        opened: &std::fs::Metadata,
        named: &std::fs::Metadata,
    ) -> Result<()> {
        use std::os::unix::fs::MetadataExt as _;

        if opened.dev() == named.dev() && opened.ino() == named.ino() {
            Ok(())
        } else {
            Err(SecretError::UnsafePath {
                path: path.to_path_buf(),
                reason: "filename changed while it was being opened".into(),
            })
        }
    }

    pub fn atomic_replace(_file: &File, source: &Path, destination: &Path) -> Result<()> {
        std::fs::rename(source, destination)
            .map_err(|error| io("atomically replace a credential file at", destination, error))
    }

    pub fn delete_prepared_legacy(
        file: &mut File,
        path: &Path,
        migration: &Path,
        expected: &[u8],
        limit: usize,
    ) -> Result<()> {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
        let marker = options.open(migration).map_err(|error| {
            io(
                "reserve a legacy credential migration path",
                migration,
                error,
            )
        })?;
        validate_open_file(migration, &marker, true)?;
        if let Err(error) = std::fs::rename(path, migration) {
            drop(marker);
            return Err(io(
                "capture a legacy credential before deletion",
                path,
                error,
            ));
        }
        drop(marker);

        // The rename captures one pathname entry without deleting it. If a
        // concurrent writer substituted that entry, leave it quarantined and
        // fail instead of consuming data that was not migrated.
        validate_open_file(migration, file, true)?;
        verify_open_contents(migration, file, expected, limit)?;
        std::fs::remove_file(migration)
            .map_err(|error| io("delete a migrated legacy credential", migration, error))
    }
}

#[cfg(windows)]
mod platform_file {
    use super::*;
    use std::os::windows::ffi::OsStrExt as _;
    use std::os::windows::fs::OpenOptionsExt as _;
    use std::os::windows::io::AsRawHandle as _;
    use windows_sys::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE};
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, DELETE, FILE_ATTRIBUTE_DIRECTORY,
        FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
        FILE_DISPOSITION_INFO, FILE_RENAME_INFO, FILE_SHARE_DELETE, FILE_SHARE_READ,
        FILE_SHARE_WRITE, FileDispositionInfo, FileRenameInfo, GetFileInformationByHandle,
        SetFileInformationByHandle,
    };

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct FileIdentity {
        volume: u32,
        index: u64,
    }

    struct HandleFacts {
        identity: FileIdentity,
        attributes: u32,
        links: u32,
    }

    pub fn configure_regular(options: &mut OpenOptions) {
        options
            .share_mode(FILE_SHARE_READ)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }

    pub fn configure_legacy(options: &mut OpenOptions) {
        options
            .access_mode(GENERIC_READ | DELETE)
            .share_mode(FILE_SHARE_READ)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }

    pub fn configure_temporary(options: &mut OpenOptions) {
        options
            .access_mode(GENERIC_WRITE | DELETE)
            .share_mode(FILE_SHARE_READ)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }

    pub fn lock_directory(path: &Path) -> Result<Option<File>> {
        let mut options = OpenOptions::new();
        options
            .access_mode(0)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT);
        let directory = options
            .open(path)
            .map_err(|error| io("open a private directory without traversal", path, error))?;
        let opened = handle_facts(&directory, path)?;
        if opened.attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
            || opened.attributes & FILE_ATTRIBUTE_DIRECTORY == 0
        {
            return Err(SecretError::UnsafePath {
                path: path.to_path_buf(),
                reason: "Windows parent handle is a reparse point or not a directory".into(),
            });
        }

        let named_directory = options
            .open(path)
            .map_err(|error| io("reopen a private directory without traversal", path, error))?;
        let named = handle_facts(&named_directory, path)?;
        if named.attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
            || named.attributes & FILE_ATTRIBUTE_DIRECTORY == 0
            || opened.identity != named.identity
        {
            return Err(SecretError::UnsafePath {
                path: path.to_path_buf(),
                reason: "private directory changed while it was being opened".into(),
            });
        }
        Ok(Some(directory))
    }

    pub fn validate_path(path: &Path) -> Result<()> {
        let mut options = OpenOptions::new();
        options
            .access_mode(0)
            .share_mode(FILE_SHARE_READ)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
        let file = options
            .open(path)
            .map_err(|error| io("open a private filename without traversal", path, error))?;
        let opened = file
            .metadata()
            .map_err(|error| io("inspect an opened private file", path, error))?;
        let named = std::fs::symlink_metadata(path)
            .map_err(|error| io("reinspect a private filename", path, error))?;
        validate_named_identity(path, &file, &opened, &named)
    }

    pub fn harden(_file: &File, _path: &Path) -> Result<()> {
        Ok(())
    }

    fn handle_facts(file: &File, path: &Path) -> Result<HandleFacts> {
        let mut info = BY_HANDLE_FILE_INFORMATION::default();
        // SAFETY: `file` owns a live OS handle and `info` is valid writable
        // storage for the duration of the call.
        let succeeded = unsafe {
            GetFileInformationByHandle(file.as_raw_handle().cast(), &mut info)
        };
        if succeeded == 0 {
            return Err(io(
                "inspect a private Windows file handle for",
                path,
                std::io::Error::last_os_error(),
            ));
        }
        Ok(HandleFacts {
            identity: FileIdentity {
                volume: info.dwVolumeSerialNumber,
                index: (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
            },
            attributes: info.dwFileAttributes,
            links: info.nNumberOfLinks,
        })
    }

    fn validate_facts(path: &Path, facts: &HandleFacts) -> Result<()> {
        if facts.attributes & (FILE_ATTRIBUTE_REPARSE_POINT | FILE_ATTRIBUTE_DIRECTORY) != 0 {
            return Err(SecretError::UnsafePath {
                path: path.to_path_buf(),
                reason: "Windows handle is a reparse point or directory".into(),
            });
        }
        if facts.links != 1 {
            return Err(SecretError::UnsafePath {
                path: path.to_path_buf(),
                reason: format!("Windows handle has {} hard links, expected one", facts.links),
            });
        }
        Ok(())
    }

    pub fn validate_named_identity(
        path: &Path,
        file: &File,
        _opened: &std::fs::Metadata,
        _named: &std::fs::Metadata,
    ) -> Result<()> {
        let opened = handle_facts(file, path)?;
        validate_facts(path, &opened)?;

        let mut options = OpenOptions::new();
        options
            .access_mode(0)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
        let named_file = options
            .open(path)
            .map_err(|error| io("reopen a private filename without traversal", path, error))?;
        let named = handle_facts(&named_file, path)?;
        validate_facts(path, &named)?;
        if opened.identity != named.identity {
            return Err(SecretError::UnsafePath {
                path: path.to_path_buf(),
                reason: "filename changed while it was being opened".into(),
            });
        }
        Ok(())
    }

    pub fn atomic_replace(file: &File, _source: &Path, destination: &Path) -> Result<()> {
        if !destination.is_absolute() {
            return Err(SecretError::UnsafePath {
                path: destination.to_path_buf(),
                reason: "Windows handle-based replacement requires an absolute path".into(),
            });
        }
        let destination_wide: Vec<u16> = destination.as_os_str().encode_wide().collect();
        let name_bytes = destination_wide.len().checked_mul(2).ok_or_else(|| {
            SecretError::UnsafePath {
                path: destination.to_path_buf(),
                reason: "destination path is too long".into(),
            }
        })?;
        let header = std::mem::offset_of!(FILE_RENAME_INFO, FileName);
        let buffer_len = header.checked_add(name_bytes).ok_or_else(|| {
            SecretError::UnsafePath {
                path: destination.to_path_buf(),
                reason: "destination path is too long".into(),
            }
        })?;
        let word = std::mem::size_of::<usize>();
        let mut buffer = vec![0usize; buffer_len.div_ceil(word)];
        let info = buffer.as_mut_ptr().cast::<FILE_RENAME_INFO>();
        // SAFETY: `buffer` has pointer alignment, contains the complete fixed
        // header plus `name_bytes`, and every write stays inside that buffer.
        unsafe {
            (*info).Anonymous.ReplaceIfExists = true;
            (*info).RootDirectory = std::ptr::null_mut();
            (*info).FileNameLength = u32::try_from(name_bytes).map_err(|_| {
                SecretError::UnsafePath {
                    path: destination.to_path_buf(),
                    reason: "destination path is too long".into(),
                }
            })?;
            std::ptr::copy_nonoverlapping(
                destination_wide.as_ptr(),
                std::ptr::addr_of_mut!((*info).FileName).cast::<u16>(),
                destination_wide.len(),
            );
        }
        let buffer_len = u32::try_from(buffer_len).map_err(|_| SecretError::UnsafePath {
            path: destination.to_path_buf(),
            reason: "destination path is too long".into(),
        })?;
        // SAFETY: the temporary file was opened with DELETE access, and `info`
        // points to the initialized aligned buffer described above.
        let succeeded = unsafe {
            SetFileInformationByHandle(
                file.as_raw_handle().cast(),
                FileRenameInfo,
                info.cast(),
                buffer_len,
            )
        };
        if succeeded == 0 {
            Err(io(
                "atomically replace a private file at",
                destination,
                std::io::Error::last_os_error(),
            ))
        } else {
            Ok(())
        }
    }

    pub fn delete_prepared_legacy(
        file: &mut File,
        path: &Path,
        _migration: &Path,
        expected: &[u8],
        limit: usize,
    ) -> Result<()> {
        validate_open_file(path, file, true)?;
        verify_open_contents(path, file, expected, limit)?;
        let disposition = FILE_DISPOSITION_INFO { DeleteFile: true };
        // SAFETY: the prepared handle has DELETE access and denies concurrent
        // writes or renames; `disposition` is a complete input structure.
        let succeeded = unsafe {
            SetFileInformationByHandle(
                file.as_raw_handle().cast(),
                FileDispositionInfo,
                (&disposition as *const FILE_DISPOSITION_INFO).cast(),
                u32::try_from(std::mem::size_of::<FILE_DISPOSITION_INFO>())
                    .expect("FILE_DISPOSITION_INFO size fits in u32"),
            )
        };
        if succeeded == 0 {
            Err(io(
                "delete a migrated legacy credential by handle at",
                path,
                std::io::Error::last_os_error(),
            ))
        } else {
            Ok(())
        }
    }
}

#[cfg(not(any(unix, windows)))]
mod platform_file {
    use super::*;

    pub fn configure_regular(_options: &mut OpenOptions) {}
    pub fn configure_legacy(_options: &mut OpenOptions) {}
    pub fn configure_temporary(_options: &mut OpenOptions) {}
    pub fn lock_directory(_path: &Path) -> Result<Option<File>> {
        Ok(None)
    }
    pub fn validate_path(_path: &Path) -> Result<()> {
        Ok(())
    }
    pub fn harden(_file: &File, _path: &Path) -> Result<()> {
        Ok(())
    }
    pub fn validate_named_identity(
        _path: &Path,
        _file: &File,
        _opened: &std::fs::Metadata,
        _named: &std::fs::Metadata,
    ) -> Result<()> {
        Ok(())
    }
    pub fn atomic_replace(_file: &File, source: &Path, destination: &Path) -> Result<()> {
        std::fs::rename(source, destination)
            .map_err(|error| io("atomically replace a credential file at", destination, error))
    }
    pub fn delete_prepared_legacy(
        file: &mut File,
        path: &Path,
        _migration: &Path,
        expected: &[u8],
        limit: usize,
    ) -> Result<()> {
        validate_open_file(path, file, true)?;
        verify_open_contents(path, file, expected, limit)?;
        std::fs::remove_file(path)
            .map_err(|error| io("delete a migrated legacy credential", path, error))
    }
}

fn atomic_replace(file: &File, source: &Path, destination: &Path) -> Result<()> {
    platform_file::atomic_replace(file, source, destination)
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

    #[cfg(unix)]
    struct LegacySwappingStore {
        inner: PrivateFileStore,
        legacy: PathBuf,
        backup: PathBuf,
        replacement: Vec<u8>,
        swapped: std::sync::atomic::AtomicBool,
    }

    #[cfg(unix)]
    impl SecretStore for LegacySwappingStore {
        fn load(&self, id: SecretId) -> Result<Option<Vec<u8>>> {
            self.inner.load(id)
        }

        fn store(&self, id: SecretId, secret: &[u8]) -> Result<()> {
            use std::os::unix::fs::PermissionsExt as _;

            if !self.swapped.swap(true, Ordering::SeqCst) {
                std::fs::rename(&self.legacy, &self.backup).map_err(|error| {
                    io("substitute the test legacy credential", &self.legacy, error)
                })?;
                std::fs::write(&self.legacy, &self.replacement).map_err(|error| {
                    io("write the test replacement credential", &self.legacy, error)
                })?;
                std::fs::set_permissions(
                    &self.legacy,
                    std::fs::Permissions::from_mode(0o600),
                )
                .map_err(|error| {
                    io(
                        "secure the test replacement credential",
                        &self.legacy,
                        error,
                    )
                })?;
            }
            self.inner.store(id, secret)
        }

        fn delete(&self, id: SecretId) -> Result<()> {
            self.inner.delete(id)
        }
    }

    #[derive(Default)]
    struct MockLegacy {
        value: Mutex<Option<Vec<u8>>>,
        fail_delete: bool,
    }

    struct MockPreparedLegacy<'a> {
        legacy: &'a MockLegacy,
        bytes: Vec<u8>,
    }

    impl PreparedLegacy for MockPreparedLegacy<'_> {
        fn bytes(&self) -> &[u8] {
            &self.bytes
        }

        fn delete(self: Box<Self>, id: SecretId) -> Result<()> {
            if self.legacy.fail_delete {
                return Err(SecretError::Verification { kind: id.label() });
            }
            let mut current = self.legacy.value.lock().unwrap();
            if current.as_deref() != Some(self.bytes.as_slice()) {
                return Err(SecretError::UnsafePath {
                    path: PathBuf::from("mock legacy credential"),
                    reason: "legacy credential changed during migration".into(),
                });
            }
            *current = None;
            Ok(())
        }
    }

    impl LegacyIo for MockLegacy {
        fn prepare(&self, _id: SecretId) -> Result<Option<Box<dyn PreparedLegacy + '_>>> {
            Ok(self.value.lock().unwrap().clone().map(|bytes| {
                Box::new(MockPreparedLegacy {
                    legacy: self,
                    bytes,
                }) as Box<dyn PreparedLegacy + '_>
            }))
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
        assert!(
            load_json_migrating_with::<ExampleSecret>(
                &store,
                SecretId::WebApi,
                &legacy,
                &|_| Ok(())
            )
            .is_err()
        );
        assert!(legacy.value.lock().unwrap().is_some());
    }

    #[test]
    fn migration_preserves_legacy_on_semantic_validation_failure() {
        let store = MockStore::default();
        let legacy = MockLegacy {
            value: Mutex::new(Some(encoded_example())),
            fail_delete: false,
        };
        let result = load_json_migrating_with::<ExampleSecret>(
            &store,
            SecretId::WebApi,
            &legacy,
            &|_| Err(SecretError::Corrupt {
                kind: SecretId::WebApi.label(),
                reason: "injected validation failure".into(),
            }),
        );
        assert!(result.is_err());
        assert_eq!(store.0.lock().unwrap().stores, 0);
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
                load_json_migrating_with::<ExampleSecret>(
                    &store,
                    SecretId::WebApi,
                    &legacy,
                    &|_| Ok(())
                )
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
        assert!(
            load_json_migrating_with::<ExampleSecret>(
                &store,
                SecretId::Playback,
                &legacy,
                &|_| Ok(())
            )
            .is_err()
        );
        assert!(legacy.value.lock().unwrap().is_some());
        legacy.fail_delete = false;
        let migrated = load_json_migrating_with::<ExampleSecret>(
            &store,
            SecretId::Playback,
            &legacy,
            &|_| Ok(()),
        )
        .unwrap()
        .unwrap();
        assert_eq!(migrated.value, "test-only");
        assert!(legacy.value.lock().unwrap().is_none());
    }

    #[cfg(unix)]
    #[test]
    fn migration_hardens_historical_librespot_file_before_reading_it() {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

        let root = std::env::temp_dir().join(format!(
            "fastpotify-legacy-mode-test-{}-{}",
            std::process::id(),
            TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        let legacy_dir = root.join("credentials");
        std::fs::create_dir_all(&legacy_dir).unwrap();
        std::fs::set_permissions(&legacy_dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        let legacy_path = legacy_dir.join("credentials.json");
        let encoded = encoded_example();
        std::fs::write(&legacy_path, &encoded).unwrap();
        std::fs::set_permissions(&legacy_path, std::fs::Permissions::from_mode(0o644)).unwrap();

        let store = PrivateFileStore::new(root.join("secrets-v1"));
        let migrated = load_json_migrating::<ExampleSecret>(
            &store,
            SecretId::Playback,
            &LegacySecret::new(legacy_path.clone()),
        )
        .unwrap()
        .unwrap();

        assert_eq!(migrated.value, "test-only");
        assert!(!legacy_path.exists());
        assert_eq!(std::fs::metadata(&legacy_dir).unwrap().mode() & 0o777, 0o700);
        let stored = store.path(SecretId::Playback);
        assert_eq!(std::fs::metadata(store.root()).unwrap().mode() & 0o777, 0o700);
        assert_eq!(std::fs::metadata(stored).unwrap().mode() & 0o777, 0o600);

        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn failed_legacy_decode_preserves_hardened_file_contents() {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

        let root = std::env::temp_dir().join(format!(
            "fastpotify-legacy-failure-test-{}-{}",
            std::process::id(),
            TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        let legacy_dir = root.join("credentials");
        std::fs::create_dir_all(&legacy_dir).unwrap();
        let legacy_path = legacy_dir.join("credentials.json");
        let invalid = b"not valid JSON";
        std::fs::write(&legacy_path, invalid).unwrap();
        std::fs::set_permissions(&legacy_path, std::fs::Permissions::from_mode(0o644)).unwrap();

        let store = PrivateFileStore::new(root.join("secrets-v1"));
        let result = load_json_migrating::<ExampleSecret>(
            &store,
            SecretId::Playback,
            &LegacySecret::new(legacy_path.clone()),
        );

        assert!(matches!(
            result,
            Err(SecretError::UnsafePath { reason, .. })
                if reason == "filename changed while it was being opened"
        ));
        assert_eq!(std::fs::read(&legacy_path).unwrap(), invalid);
        assert_eq!(std::fs::metadata(&legacy_path).unwrap().mode() & 0o777, 0o600);

        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn migration_never_deletes_a_substituted_legacy_file() {
        use std::os::unix::fs::PermissionsExt as _;

        let root = std::env::temp_dir().join(format!(
            "fastpotify-legacy-swap-test-{}-{}",
            std::process::id(),
            TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        let legacy_dir = root.join("credentials");
        std::fs::create_dir_all(&legacy_dir).unwrap();
        let legacy_path = legacy_dir.join("credentials.json");
        let original = encoded_example();
        std::fs::write(&legacy_path, &original).unwrap();
        std::fs::set_permissions(&legacy_path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let replacement = serde_json::to_vec(&ExampleSecret {
            value: "newer".into(),
        })
        .unwrap();
        let backup = legacy_dir.join("original.backup");
        let store = LegacySwappingStore {
            inner: PrivateFileStore::new(root.join("secrets-v1")),
            legacy: legacy_path.clone(),
            backup: backup.clone(),
            replacement: replacement.clone(),
            swapped: std::sync::atomic::AtomicBool::new(false),
        };

        let result = load_json_migrating::<ExampleSecret>(
            &store,
            SecretId::Playback,
            &LegacySecret::new(legacy_path.clone()),
        );

        assert!(result.is_err());
        assert_eq!(std::fs::read(&backup).unwrap(), original);
        assert_eq!(
            std::fs::read(legacy_migration_path(&legacy_path)).unwrap(),
            replacement
        );
        assert_eq!(
            store.load(SecretId::Playback).unwrap(),
            Some(encoded_example())
        );

        let _ = std::fs::remove_dir_all(root);
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

    #[cfg(unix)]
    #[test]
    fn general_private_writers_harden_old_files_without_following_links() {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

        let root = std::env::temp_dir().join(format!(
            "fastpotify-private-file-test-{}-{}",
            std::process::id(),
            TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        ensure_private_dir(&root).unwrap();
        let settings = root.join("settings.json");
        std::fs::write(&settings, b"old").unwrap();
        std::fs::set_permissions(&settings, std::fs::Permissions::from_mode(0o644)).unwrap();
        write_private_atomic(&settings, b"new").unwrap();
        assert_eq!(std::fs::read(&settings).unwrap(), b"new");
        assert_eq!(std::fs::metadata(&settings).unwrap().mode() & 0o777, 0o600);

        let victim = root.join("victim");
        std::fs::write(&victim, b"do not truncate").unwrap();
        let log = root.join("fastpotify.log");
        std::os::unix::fs::symlink(&victim, &log).unwrap();
        assert!(open_private_truncate(&log).is_err());
        assert_eq!(std::fs::read(&victim).unwrap(), b"do not truncate");

        let _ = std::fs::remove_file(log);
        let _ = std::fs::remove_file(victim);
        let _ = std::fs::remove_file(settings);
        let _ = std::fs::remove_dir(root);
    }

    #[cfg(windows)]
    #[test]
    fn windows_private_files_reject_links_lock_names_and_replace_by_handle() {
        use std::io::Write as _;

        let root = std::env::temp_dir().join(format!(
            "fastpotify-windows-private-file-test-{}-{}",
            std::process::id(),
            TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        ensure_private_dir(&root).unwrap();

        let victim = root.join("victim");
        std::fs::write(&victim, b"do not truncate").unwrap();
        let linked_log = root.join("linked.log");
        std::fs::hard_link(&victim, &linked_log).unwrap();
        assert!(open_private_truncate(&linked_log).is_err());
        assert_eq!(std::fs::read(&victim).unwrap(), b"do not truncate");
        std::fs::remove_file(&linked_log).unwrap();

        let log = root.join("fastpotify.log");
        let moved = root.join("swapped.log");
        let mut held = open_private_truncate(&log).unwrap();
        held.write_all(b"held").unwrap();
        assert!(std::fs::rename(&log, &moved).is_err());
        drop(held);

        let settings = root.join("settings.json");
        write_private_atomic(&settings, b"first").unwrap();
        write_private_atomic(&settings, b"second").unwrap();
        assert_eq!(std::fs::read(&settings).unwrap(), b"second");

        let _ = std::fs::remove_dir_all(root);
    }
}
