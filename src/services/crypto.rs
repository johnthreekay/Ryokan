//! AEAD encryption for OAuth tokens + other at-rest secrets.
//!
//! Issue #62 groundwork. AL / MAL OAuth tokens are 1-year-TTL
//! credentials that authorize reading + writing the user's real
//! external account (Ryokan doesn't write back itself, but the token
//! scope still grants write access — if one leaked, an attacker
//! could modify the user's AL list). That's a higher stakes profile
//! than the qBit / Jellyfin API keys Ryokan historically stored in
//! plaintext, so tokens live in `external_accounts.*_token_encrypted`
//! blobs encrypted with ChaCha20-Poly1305 AEAD.
//!
//! Plaintext tokens exist only briefly in memory during outbound API
//! calls. The DB-at-rest copy is always ciphertext. A `ryokan
//! --sanitize-db-for-debug` CLI (planned)
//! blanks these columns before the user posts their DB in a bug
//! report.
//!
//! ## Key provisioning
//!
//! 32 raw bytes, loaded once at startup via [`static@KEY`]
//! (`LazyLock<[u8; 32]>`, matching the existing `TRUST_PROXY_HEADERS`
//! + `COOKIE_SECURE` pattern). Three sources, first match wins:
//!
//!   1. **`RYOKAN_ENCRYPTION_KEY` env var** — base64-encoded 32 bytes
//!      (44 characters padded or 43 unpadded; both accepted). The
//!      Docker / Kubernetes path: secrets belong in the orchestrator,
//!      mounted as an env var at container start.
//!   2. **`<data dir>/.ryokan-key` file** — raw 32 bytes, mode `0600`
//!      (`RYOKAN_KEY_FILE_PATH` overrides the path). The
//!      bare-metal install path: `cargo run` auto-generates one on
//!      first boot and every subsequent boot reads it back. `data/`
//!      is gitignored so the key can't accidentally get committed.
//!   3. **Auto-generated on first run** — via `rand::rng()`. Written
//!      to the key-file path with mode `0600` (Unix only — on other
//!      platforms the file is created with default permissions and a
//!      `warn` is logged).
//!
//! Key rotation is a later-PR concern. For now the key is stable for
//! the process lifetime + across restarts.
//!
//! ## Security model
//!
//! If an attacker has the SQLite database *and* the key, they have
//! the tokens. The encryption protects against three specific vectors:
//!
//!   - **"User pastes their DB in a bug report"** — common enough that
//!     the `--sanitize-db-for-debug` CLI exists, and the encryption
//!     provides defense-in-depth for the cases where a user forgets.
//!   - **"Read-only DB exfiltration"** (stolen backup, misconfigured
//!     share, a reverse-proxy path traversal that reaches `data/`
//!     but not `data/.ryokan-key` by virtue of the `.` prefix) — the
//!     ciphertext is useless without the key.
//!   - **"Passive observer with DB access but no filesystem access"** —
//!     e.g., a DB admin on a managed SQLite-backed service.
//!
//! It does NOT protect against: full filesystem compromise, process
//! memory inspection, a running-process key extraction, or a stolen
//! DB *and* stolen key file together.
//!
//! Passphrase-derived keys (user types a passphrase at startup,
//! derived via Argon2) are deferred to a follow-up issue — the UX
//! cost doesn't fit the "self-hosted single-user PVR" shape at v1.4.0.

use std::fs;
use std::path::Path;
use std::sync::LazyLock;

use base64::Engine;
use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
// `Rng` is rand 0.10's infallible-fill trait (what used to be
// `RngCore` in earlier rand versions). `rand::rng()` returns a
// `ThreadRng` which is `CryptoRng`-qualified, so `fill_bytes` here
// sources from the OS random device.
use rand::Rng;

/// Nonce length for ChaCha20-Poly1305: 96 bits = 12 bytes.
const NONCE_LEN: usize = 12;

/// Key-file name inside the data directory (`services::paths`) when
/// the `RYOKAN_ENCRYPTION_KEY` env var isn't set. `RYOKAN_KEY_FILE_PATH`
/// overrides the whole path. Before #259 the default was CWD-relative
/// `data/.ryokan-key`, which the Docker image (WORKDIR /app) resolved
/// to an unwritable `/app/data/`, so the image had to pin the override;
/// the image now sets `RYOKAN_DATA_DIR=/data` and the key lands next to
/// the database on its own.
const KEY_FILE_NAME: &str = ".ryokan-key";

/// Process-lifetime AEAD key. Initialized lazily on first call to
/// [`encrypt`] or [`decrypt`]. Panicking inside the initializer is
/// deliberate — a key-load failure at startup is unrecoverable and
/// should halt boot loudly rather than silently running with an
/// empty token store or random per-process key.
static KEY: LazyLock<[u8; 32]> = LazyLock::new(|| match load_or_generate_key() {
    Ok(bytes) => bytes,
    Err(e) => panic!("ryokan encryption-key init failed: {e}"),
});

fn load_or_generate_key() -> Result<[u8; 32], String> {
    if let Ok(env_val) = std::env::var("RYOKAN_ENCRYPTION_KEY") {
        let trimmed = env_val.trim();
        if !trimmed.is_empty() {
            return decode_key_from_base64(trimmed)
                .map_err(|e| format!("RYOKAN_ENCRYPTION_KEY: {e}"));
        }
    }
    load_or_generate_key_file(&key_file_path())
}

/// The effective key-file path: `RYOKAN_KEY_FILE_PATH` when set, else
/// `<data dir>/.ryokan-key`. Read on every key-init attempt (LazyLock
/// fires once per process) rather than cached. Backup (#126) includes
/// this file in the archive and restore writes it back. Not the source
/// of truth when `RYOKAN_ENCRYPTION_KEY` is set; that key wins at load
/// time regardless of the file.
pub fn key_file_path() -> std::path::PathBuf {
    crate::services::paths::override_or_data_path("RYOKAN_KEY_FILE_PATH", KEY_FILE_NAME)
}

fn decode_key_from_base64(s: &str) -> Result<[u8; 32], String> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(s)
        .or_else(|_| base64::engine::general_purpose::STANDARD_NO_PAD.decode(s))
        .map_err(|e| format!("base64 decode failed: {e}"))?;
    if bytes.len() != 32 {
        return Err(format!(
            "expected 32 bytes after base64 decode, got {}",
            bytes.len()
        ));
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(&bytes);
    Ok(out)
}

fn load_or_generate_key_file(path: &Path) -> Result<[u8; 32], String> {
    match fs::read(path) {
        Ok(bytes) if bytes.len() == 32 => {
            let mut out = [0u8; 32];
            out.copy_from_slice(&bytes);
            Ok(out)
        }
        Ok(bytes) => Err(format!(
            "key file {} is {} bytes, expected 32 — delete it to regenerate",
            path.display(),
            bytes.len()
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let key = generate_random_key()?;
            write_key_file(path, &key)?;
            Ok(key)
        }
        Err(e) => Err(format!("could not read key file {}: {}", path.display(), e)),
    }
}

fn generate_random_key() -> Result<[u8; 32], String> {
    // `ThreadRng` is `CryptoRng`-qualified and sources from the OS
    // random device; `fill_bytes` is infallible in practice, but we
    // keep the Result return type so callers can treat key-gen as
    // fallible alongside the other key-load paths.
    let mut buf = [0u8; 32];
    rand::rng().fill_bytes(&mut buf);
    Ok(buf)
}

fn write_key_file(path: &Path, key: &[u8; 32]) -> Result<(), String> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)
            .map_err(|e| format!("could not create {}: {}", parent.display(), e))?;
    }

    // On Unix, create with mode 0600 atomically — `fs::write` followed
    // by `set_permissions` opens a TOCTOU window where the file briefly
    // exists with umask-default perms (typically 0644). A local user
    // with read access to `data/` could see the key during that window
    // on first boot. `OpenOptions::create_new(true).mode(0o600)` makes
    // the kernel apply 0600 at inode creation time; a concurrent reader
    // in the gap before the write completes sees an empty file at
    // worst. On non-Unix platforms (Windows) we fall through to plain
    // `fs::write` and rely on the parent dir's ACL — already user-scoped
    // on a typical install.
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
            .map_err(|e| format!("could not create {}: {}", path.display(), e))?;
        f.write_all(key)
            .map_err(|e| format!("could not write {}: {}", path.display(), e))?;
    }
    #[cfg(not(unix))]
    {
        fs::write(path, key).map_err(|e| format!("could not write {}: {}", path.display(), e))?;
    }
    Ok(())
}

/// AEAD-encrypt `plaintext`. Output layout: `nonce || ciphertext || tag`
/// concatenated as a single `Vec<u8>`, nonce randomized per-call.
///
/// Suitable for blob columns (no base64 wrapping — the DB driver
/// handles binary transparently). If the caller needs a string
/// representation (logs, debug dumps), base64-encode the output
/// separately.
pub fn encrypt(plaintext: &[u8]) -> Result<Vec<u8>, String> {
    let cipher = ChaCha20Poly1305::new(&Key::from(*KEY));
    // 96-bit random nonce, generated through the same rand 0.10 path
    // `generate_random_key` uses (rather than chacha20poly1305's own
    // re-export of rand_core 0.6's OsRng, which doesn't match rand
    // 0.10's trait surface).
    let mut nonce_bytes = [0u8; NONCE_LEN];
    rand::rng().fill_bytes(&mut nonce_bytes);
    let nonce = Nonce::from(nonce_bytes);
    let ciphertext = cipher
        .encrypt(&nonce, plaintext)
        .map_err(|e| format!("AEAD encrypt failed: {e}"))?;

    let mut out = Vec::with_capacity(NONCE_LEN + ciphertext.len());
    out.extend_from_slice(&nonce_bytes);
    out.extend_from_slice(&ciphertext);
    Ok(out)
}

/// AEAD-decrypt the output of [`encrypt`]. Returns an error on any
/// tampering (wrong key, modified nonce, modified ciphertext, or
/// truncated input); the Poly1305 tag check rejects altered inputs
/// before the ChaCha20 stream keying step, so a tampered blob never
/// decrypts into anything — never silently corrupted plaintext.
pub fn decrypt(ciphertext_with_nonce: &[u8]) -> Result<Vec<u8>, String> {
    if ciphertext_with_nonce.len() < NONCE_LEN {
        return Err("ciphertext too short — missing nonce prefix".into());
    }
    let (nonce_bytes, body) = ciphertext_with_nonce.split_at(NONCE_LEN);
    let cipher = ChaCha20Poly1305::new(&Key::from(*KEY));
    let nonce =
        Nonce::try_from(nonce_bytes).map_err(|_| "nonce prefix has wrong length".to_string())?;
    cipher
        .decrypt(&nonce, body)
        .map_err(|e| format!("AEAD decrypt failed: {e}"))
}

/// Blank an encrypted blob for `--sanitize-db-for-debug`. Callers
/// overwrite the DB cell with this sentinel value so a sanitized
/// dump is distinguishable from a genuinely-empty field. Deliberately
/// not valid ciphertext (too short for any real encrypted payload)
/// so a regression that tries to decrypt a blanked cell fails loudly.
pub const SANITIZED_SENTINEL: &[u8] = b"[REDACTED]";

/// Force the [`static@KEY`] LazyLock to initialize, paralleling the
/// `warm_timing_equalizer` pattern in `models::user`. Called at
/// startup so the first OAuth `/submit` doesn't pay the cold key-
/// load cost (env-var parse + file read or first-run generation +
/// 0600 chmod). Cheap to call repeatedly — `LazyLock::force` short-
/// circuits after the first invocation.
pub fn warm_key() {
    LazyLock::force(&KEY);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;

    /// Lock so tests that manipulate `RYOKAN_ENCRYPTION_KEY` or the
    /// key file don't step on each other's state. The `KEY` static is
    /// a process-wide `LazyLock` and only initializes once per test
    /// binary, so round-trip tests that depend on a particular key
    /// source must be serialized.
    static TEST_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn example_key() -> [u8; 32] {
        let mut k = [0u8; 32];
        for (i, slot) in k.iter_mut().enumerate() {
            *slot = i as u8;
        }
        k
    }

    #[test]
    fn decode_key_accepts_standard_base64() {
        let b64 = base64::engine::general_purpose::STANDARD.encode(example_key());
        assert_eq!(decode_key_from_base64(&b64).unwrap(), example_key());
    }

    #[test]
    fn decode_key_accepts_unpadded_base64() {
        // Unpadded base64 for a 32-byte input drops at most 2 `=`.
        // Accepting both padded and unpadded means a user pasting
        // their key from an arbitrary tool doesn't hit format errors.
        let b64_padded = base64::engine::general_purpose::STANDARD.encode(example_key());
        let b64_unpadded = b64_padded.trim_end_matches('=').to_string();
        assert_eq!(
            decode_key_from_base64(&b64_unpadded).unwrap(),
            example_key()
        );
    }

    #[test]
    fn decode_key_rejects_wrong_length() {
        let too_short = base64::engine::general_purpose::STANDARD.encode([0u8; 16]);
        assert!(decode_key_from_base64(&too_short).is_err());
        let too_long = base64::engine::general_purpose::STANDARD.encode([0u8; 64]);
        assert!(decode_key_from_base64(&too_long).is_err());
    }

    #[test]
    fn decode_key_rejects_non_base64() {
        assert!(decode_key_from_base64("not base64 at all!@#").is_err());
    }

    #[test]
    fn key_file_roundtrips_through_disk() {
        let tmp = tempdir();
        let path = tmp.path().join(".ryokan-key");
        let k1 = load_or_generate_key_file(&path).expect("first generate");
        let k2 = load_or_generate_key_file(&path).expect("second load reuses");
        assert_eq!(k1, k2, "second load must return the stored key");
    }

    #[test]
    fn key_file_wrong_size_errors_loudly() {
        let tmp = tempdir();
        let path = tmp.path().join(".ryokan-key");
        fs::write(&path, b"short").unwrap();
        let err = load_or_generate_key_file(&path).unwrap_err();
        assert!(
            err.contains("32") && err.to_lowercase().contains("byte"),
            "stale / malformed key file should name the size: {err}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn key_file_written_with_0600_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempdir();
        let path = tmp.path().join(".ryokan-key");
        load_or_generate_key_file(&path).expect("generate");
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "key file must not be world- or group-readable");
    }

    #[test]
    fn encrypt_roundtrip() {
        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        // Force a known key via env var so the test doesn't depend
        // on whatever the LazyLock got on first call from some other
        // test in the binary.
        let b64 = base64::engine::general_purpose::STANDARD.encode(example_key());
        unsafe {
            env::set_var("RYOKAN_ENCRYPTION_KEY", b64);
        }
        // First call initializes KEY. Subsequent tests in the same
        // binary won't re-init it because LazyLock is one-shot.
        LazyLock::force(&KEY);

        let plaintext = b"hunter2 oauth token payload with special chars \0\x7f";
        let ct = encrypt(plaintext).unwrap();
        assert_ne!(ct, plaintext, "ciphertext must differ from plaintext");
        assert!(
            ct.len() > plaintext.len() + NONCE_LEN,
            "output must include nonce + Poly1305 tag"
        );
        let pt = decrypt(&ct).unwrap();
        assert_eq!(pt, plaintext);
    }

    #[test]
    fn encrypt_produces_unique_ciphertexts_for_identical_plaintext() {
        // Random nonce per-call → identical plaintext encrypts to
        // distinct ciphertexts. Verifies we're not accidentally using
        // a fixed or monotonic nonce.
        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        LazyLock::force(&KEY);
        let plaintext = b"same payload";
        let a = encrypt(plaintext).unwrap();
        let b = encrypt(plaintext).unwrap();
        assert_ne!(a, b, "nonce randomization must make outputs distinct");
    }

    #[test]
    fn decrypt_rejects_tampered_ciphertext() {
        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        LazyLock::force(&KEY);
        let mut ct = encrypt(b"secret").unwrap();
        // Flip one bit in the body (past the nonce prefix).
        let idx = NONCE_LEN + 1;
        ct[idx] ^= 0x01;
        assert!(
            decrypt(&ct).is_err(),
            "AEAD tag check must reject tampering"
        );
    }

    #[test]
    fn decrypt_rejects_tampered_nonce() {
        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        LazyLock::force(&KEY);
        let mut ct = encrypt(b"secret").unwrap();
        ct[0] ^= 0x01;
        assert!(
            decrypt(&ct).is_err(),
            "modified nonce must not decrypt successfully"
        );
    }

    #[test]
    fn decrypt_rejects_truncated_input() {
        // Too short even to hold the nonce prefix.
        assert!(decrypt(&[0u8; 4]).is_err());
        // Nonce-only, no body — Poly1305 tag absent.
        assert!(decrypt(&[0u8; NONCE_LEN]).is_err());
    }

    #[test]
    fn decrypt_rejects_sanitized_sentinel() {
        // The `--sanitize-db-for-debug` CLI overwrites encrypted-token
        // blobs with `SANITIZED_SENTINEL` (b"[REDACTED]"). The sentinel
        // is shorter than `NONCE_LEN`, so `decrypt` must fail loudly.
        // A regression that lengthens the sentinel without checking
        // this could let a sanitized blob silently decrypt to garbage
        // plaintext — the sync task would then try to use that as an
        // OAuth token and confuse the failure mode.
        assert!(
            decrypt(SANITIZED_SENTINEL).is_err(),
            "SANITIZED_SENTINEL must not be a valid AEAD ciphertext"
        );
    }

    /// Minimal stand-in for the `tempfile` crate so we don't add a
    /// dev-dep just for two tests. Returns a handle that removes the
    /// directory on drop.
    fn tempdir() -> TempDir {
        let path = std::env::temp_dir().join(format!(
            "ryokan-crypto-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&path).unwrap();
        TempDir { path }
    }
    struct TempDir {
        path: std::path::PathBuf,
    }
    impl TempDir {
        fn path(&self) -> &Path {
            &self.path
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}
