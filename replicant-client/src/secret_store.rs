//! Encrypted-at-rest storage for the per-user sync credential.
//! See plan doc for the at-rest key limitation (co-located key = obfuscation,
//! not strong protection; DPAPI wrapping is a Windows follow-up).

use chacha20poly1305::aead::{Aead, KeyInit, OsRng};
use chacha20poly1305::{ChaCha20Poly1305, Nonce};
use rand::RngCore;
use std::io::{self, Error, ErrorKind};
use std::path::{Path, PathBuf};

const KEY_FILE: &str = "key.bin";
const CRED_FILE: &str = "credentials.enc";

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Credentials {
    pub api_key: String,
    pub secret: String,
    pub user_id: uuid::Uuid,
    /// Signs the join. Credentials stored by 0.6 have none; the engine's configured email is used.
    #[serde(default)]
    pub email: Option<String>,
}

/// Reads the at-rest key if present. Never creates one — callers that must
/// mint a key when absent use [`load_or_create_key`].
fn read_key(dir: &Path) -> io::Result<Option<[u8; 32]>> {
    let path = dir.join(KEY_FILE);
    if !path.exists() {
        return Ok(None);
    }
    let bytes = std::fs::read(&path)?;
    let arr: [u8; 32] = bytes
        .try_into()
        .map_err(|_| Error::new(ErrorKind::InvalidData, "bad key length"))?;
    Ok(Some(arr))
}

fn load_or_create_key(dir: &Path) -> io::Result<[u8; 32]> {
    if let Some(key) = read_key(dir)? {
        return Ok(key);
    }
    let mut key = [0u8; 32];
    OsRng.fill_bytes(&mut key);
    std::fs::create_dir_all(dir)?;
    let key_path = dir.join(KEY_FILE);
    let temp = write_temp(&key_path, &key)?;
    #[cfg(test)]
    if let Some(hook) = tests::BEFORE_KEY_LINK.take() {
        hook();
    }
    // `hard_link` fails if the key exists, like O_EXCL, but never exposes a partial key.
    let linked = std::fs::hard_link(&temp, &key_path);
    let _ = std::fs::remove_file(&temp);
    match linked {
        Ok(()) => {
            sync_dir(dir);
            Ok(key)
        }
        Err(error) if error.kind() == ErrorKind::AlreadyExists => read_key(dir)?
            .ok_or_else(|| Error::new(ErrorKind::NotFound, "key removed while being created")),
        Err(error) => Err(error),
    }
}

/// Writes `bytes` to a new private file next to `path` and returns its path.
fn write_temp(path: &Path, bytes: &[u8]) -> io::Result<PathBuf> {
    use std::io::Write;
    let mut name = path.as_os_str().to_owned();
    name.push(format!(
        ".{}.{:016x}.tmp",
        std::process::id(),
        OsRng.next_u64()
    ));
    let temp = PathBuf::from(name);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    // TODO(hardening): wrap with DPAPI on Windows. For now rely on the
    // user-profile directory ACL.
    let written = options.open(&temp).and_then(|mut file| {
        file.write_all(bytes)?;
        file.sync_all()
    });
    match written {
        Ok(()) => Ok(temp),
        Err(error) => {
            let _ = std::fs::remove_file(&temp);
            Err(error)
        }
    }
}

/// Replaces `path` whole: a reader sees the old file or the new one, never part of one.
fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let temp = write_temp(path, bytes)?;
    std::fs::rename(&temp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&temp);
    })?;
    if let Some(dir) = path.parent() {
        sync_dir(dir);
    }
    Ok(())
}

/// Makes a rename or link durable; best effort, since the file itself is already written.
fn sync_dir(dir: &Path) {
    #[cfg(unix)]
    if let Err(error) = std::fs::File::open(dir).and_then(|dir| dir.sync_all()) {
        tracing::warn!(%error, "could not sync the credentials directory");
    }
    #[cfg(not(unix))]
    let _ = dir;
}

/// Checks that `store` can write in `dir`: creates it and the key, then writes and removes a
/// probe file.
pub fn prepare(dir: &Path) -> io::Result<()> {
    std::fs::create_dir_all(dir)?;
    load_or_create_key(dir)?;
    std::fs::remove_file(write_temp(&dir.join(CRED_FILE), &[])?)
}

pub fn store(dir: &Path, creds: &Credentials) -> io::Result<()> {
    let key = load_or_create_key(dir)?;
    let cipher = ChaCha20Poly1305::new((&key).into());
    let mut nonce_bytes = [0u8; 12];
    OsRng.fill_bytes(&mut nonce_bytes);
    let nonce = Nonce::from_slice(&nonce_bytes);

    let plaintext = serde_json::to_vec(creds).map_err(|e| Error::new(ErrorKind::InvalidData, e))?;
    let ciphertext = cipher
        .encrypt(nonce, plaintext.as_ref())
        .map_err(|_| Error::new(ErrorKind::Other, "encrypt failed"))?;

    let mut out = nonce_bytes.to_vec();
    out.extend_from_slice(&ciphertext);
    write_private(&dir.join(CRED_FILE), &out)
}

pub fn load(dir: &Path) -> io::Result<Option<Credentials>> {
    let path = dir.join(CRED_FILE);
    if !path.exists() {
        return Ok(None);
    }
    // Never mint a key on the read path: an existing credentials file
    // without a key is a broken/tampered state, not a "first run".
    let key = read_key(dir)?.ok_or_else(|| Error::new(ErrorKind::NotFound, "key missing"))?;
    let cipher = ChaCha20Poly1305::new((&key).into());

    let bytes = std::fs::read(&path)?;
    if bytes.len() < 12 {
        return Err(Error::new(ErrorKind::InvalidData, "truncated"));
    }
    let (nonce_bytes, ciphertext) = bytes.split_at(12);
    let plaintext = cipher
        .decrypt(Nonce::from_slice(nonce_bytes), ciphertext)
        .map_err(|_| Error::new(ErrorKind::InvalidData, "decrypt failed"))?;

    let creds = serde_json::from_slice(&plaintext)
        .map_err(|_| Error::new(ErrorKind::InvalidData, "credentials unparseable"))?;
    Ok(Some(creds))
}

/// True for a [`load`] error no retry can fix: the key is missing or the wrong size, or the
/// credentials are truncated, do not decrypt or do not parse. Other errors may pass.
pub fn is_damaged(error: &io::Error) -> bool {
    matches!(error.kind(), ErrorKind::NotFound | ErrorKind::InvalidData)
}

pub fn clear(dir: &Path) -> io::Result<()> {
    let cred_path = dir.join(CRED_FILE);
    if cred_path.exists() {
        std::fs::remove_file(cred_path)?;
    }
    let key_path = dir.join(KEY_FILE);
    if key_path.exists() {
        std::fs::remove_file(key_path)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_credentials() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load(dir.path()).unwrap().is_none());

        let user_id = uuid::Uuid::new_v4();
        let creds = Credentials {
            api_key: "rpa_x".into(),
            secret: "rps_y".into(),
            user_id,
            email: None,
        };
        store(dir.path(), &creds).unwrap();

        let loaded = load(dir.path()).unwrap().unwrap();
        assert_eq!(loaded.api_key, "rpa_x");
        assert_eq!(loaded.secret, "rps_y");
        assert_eq!(loaded.user_id, user_id);

        clear(dir.path()).unwrap();
        assert!(load(dir.path()).unwrap().is_none());
    }

    #[test]
    fn clear_removes_both_key_and_credentials_files() {
        let dir = tempfile::tempdir().unwrap();
        store(
            dir.path(),
            &Credentials {
                api_key: "rpa_x".into(),
                secret: "rps_y".into(),
                user_id: uuid::Uuid::new_v4(),
                email: None,
            },
        )
        .unwrap();
        assert!(dir.path().join(KEY_FILE).exists());
        assert!(dir.path().join(CRED_FILE).exists());

        clear(dir.path()).unwrap();

        assert!(!dir.path().join(KEY_FILE).exists());
        assert!(!dir.path().join(CRED_FILE).exists());
    }

    #[test]
    fn load_on_empty_dir_returns_none_and_mints_no_key() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load(dir.path()).unwrap().is_none());
        assert!(
            !dir.path().join(KEY_FILE).exists(),
            "load() must never mint a key"
        );
        assert!(!dir.path().join(CRED_FILE).exists());
    }

    #[test]
    fn credentials_without_their_key_are_unreadable_not_absent() {
        let dir = tempfile::tempdir().unwrap();
        store(
            dir.path(),
            &Credentials {
                api_key: "a".into(),
                secret: "b".into(),
                user_id: uuid::Uuid::new_v4(),
                email: None,
            },
        )
        .unwrap();
        std::fs::remove_file(dir.path().join(KEY_FILE)).unwrap();
        assert!(is_damaged(&load(dir.path()).unwrap_err()));
        assert!(
            !dir.path().join(KEY_FILE).exists(),
            "load() must never mint a key"
        );
    }

    #[test]
    fn tampered_ciphertext_fails_to_decrypt() {
        let dir = tempfile::tempdir().unwrap();
        store(
            dir.path(),
            &Credentials {
                api_key: "a".into(),
                secret: "b".into(),
                user_id: uuid::Uuid::new_v4(),
                email: None,
            },
        )
        .unwrap();
        let cred_path = dir.path().join("credentials.enc");
        let mut bytes = std::fs::read(&cred_path).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 0xFF;
        std::fs::write(&cred_path, bytes).unwrap();
        assert!(is_damaged(&load(dir.path()).unwrap_err()));
    }

    #[test]
    fn credentials_stored_without_an_email_still_load() {
        let stored = serde_json::json!({
            "api_key": "rpa_k",
            "secret": "rps_s",
            "user_id": uuid::Uuid::from_u128(1)
        });
        let credentials: Credentials = serde_json::from_value(stored).unwrap();
        assert_eq!(credentials.email, None);
    }

    fn creds(api_key: &str) -> Credentials {
        Credentials {
            api_key: api_key.into(),
            secret: "rps_s".into(),
            user_id: uuid::Uuid::from_u128(1),
            email: Some("a@x.io".into()),
        }
    }

    #[test]
    fn a_store_racing_a_load_is_never_read_as_signed_out_or_unreadable() {
        let dir = tempfile::tempdir().unwrap();
        store(dir.path(), &creds("k0")).unwrap();
        let path = dir.path().to_path_buf();
        let writer = std::thread::spawn(move || {
            for n in 0..300 {
                store(&path, &creds(&format!("k{n}"))).unwrap();
            }
        });
        while !writer.is_finished() {
            let loaded = load(dir.path());
            assert!(matches!(loaded, Ok(Some(_))), "a reader saw {loaded:?}");
        }
        writer.join().unwrap();
    }

    thread_local! {
        pub(super) static BEFORE_KEY_LINK: std::cell::Cell<Option<Box<dyn FnOnce()>>> =
            const { std::cell::Cell::new(None) };
    }

    #[test]
    fn two_racing_first_stores_leave_readable_credentials() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_path_buf();
        // The other store runs whole after this one found no key and before it installs its own.
        BEFORE_KEY_LINK.set(Some(Box::new(move || {
            std::thread::spawn(move || store(&path, &creds("b")).unwrap())
                .join()
                .unwrap();
        })));
        let key = load_or_create_key(dir.path()).unwrap();
        assert_eq!(
            load(dir.path()).unwrap().unwrap().api_key,
            "b",
            "the winner's key was not replaced"
        );
        assert_eq!(read_key(dir.path()).unwrap(), Some(key));
        store(dir.path(), &creds("a")).unwrap();
        assert_eq!(load(dir.path()).unwrap().unwrap().api_key, "a");
    }
}
