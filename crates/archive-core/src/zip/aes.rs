//! WinZip AES decryption (AE-1 / AE-2).
//!
//! Key derivation: PBKDF2-HMAC-SHA1, 1000 iterations.
//! Encryption: AES-CTR (128-bit little-endian counter starting at 1).
//! Integrity: HMAC-SHA1 over ciphertext, first 10 bytes.
//!
//! Reference: WinZip AES encryption specification (2003).

use std::io::{self, Read};

use aes::cipher::{KeyIvInit, StreamCipher};
use aes::{Aes128, Aes192, Aes256};
use ctr::Ctr128LE;
use hmac::{Hmac, Mac};
use sha1::Sha1;

use crate::{CoreError, PasswordDetectionMethod};

// ── AES-CTR cipher dispatch ───────────────────────────────────────────────
// AES-128/192/256 produce different concrete types in RustCrypto, so we wrap
// them in an enum to get a single apply_keystream call site.

enum AesCtrCipher {
    Aes128(Ctr128LE<Aes128>),
    Aes192(Ctr128LE<Aes192>),
    Aes256(Ctr128LE<Aes256>),
}

impl AesCtrCipher {
    fn new(key: &[u8], iv: &[u8; 16], strength: u8) -> Result<Self, CoreError> {
        match strength {
            1 => Ctr128LE::<Aes128>::new_from_slices(key, iv)
                .map(Self::Aes128)
                .map_err(|_| CoreError::InvalidFormat {
                    message: "AES-128 key/iv size mismatch".into(),
                }),
            2 => Ctr128LE::<Aes192>::new_from_slices(key, iv)
                .map(Self::Aes192)
                .map_err(|_| CoreError::InvalidFormat {
                    message: "AES-192 key/iv size mismatch".into(),
                }),
            3 => Ctr128LE::<Aes256>::new_from_slices(key, iv)
                .map(Self::Aes256)
                .map_err(|_| CoreError::InvalidFormat {
                    message: "AES-256 key/iv size mismatch".into(),
                }),
            _ => Err(CoreError::InvalidFormat {
                message: format!("unknown AES strength byte: {strength}"),
            }),
        }
    }

    fn apply_keystream(&mut self, buf: &mut [u8]) {
        match self {
            Self::Aes128(c) => c.apply_keystream(buf),
            Self::Aes192(c) => c.apply_keystream(buf),
            Self::Aes256(c) => c.apply_keystream(buf),
        }
    }
}

// ── Public API ────────────────────────────────────────────────────────────

/// Decrypt a WinZip AES-encrypted entry from `reader`.
///
/// Reads exactly `compressed_size` bytes: salt + 2-byte pw_verify +
/// encrypted_data + 10-byte HMAC auth code.
///
/// Returns the decrypted (but still compressed) bytes on success.
///
/// # Errors
/// - `WrongPassword { EarlyHeaderValidation }` — password verification bytes mismatch.
/// - `WrongPassword { CrcMismatch }` — HMAC authentication failure.
/// - `InvalidFormat` — stream is too short or strength byte is unknown.
pub(crate) fn decrypt_winzip_aes(
    reader: &mut impl Read,
    password: &[u8],
    strength: u8,
    compressed_size: u64,
) -> Result<Vec<u8>, CoreError> {
    let key_len: usize = match strength {
        1 => 16, // AES-128
        2 => 24, // AES-192
        3 => 32, // AES-256
        _ => {
            return Err(CoreError::InvalidFormat {
                message: format!("unknown AES strength byte: {strength}"),
            });
        }
    };
    let salt_len = key_len / 2; // 8 / 12 / 16 bytes

    // Total overhead: salt + 2-byte pw_verify + 10-byte auth_code
    let overhead = (salt_len + 2 + 10) as u64;
    let encrypted_data_len = compressed_size.checked_sub(overhead).ok_or_else(|| {
        CoreError::InvalidFormat {
            message: format!(
                "AES entry compressed_size {compressed_size} is smaller than crypto overhead {overhead}"
            ),
        }
    })?;

    // ── Key derivation ────────────────────────────────────────────────────
    let mut salt = vec![0u8; salt_len];
    read_exact(reader, &mut salt, "AES salt")?;

    let mut pw_verify_stored = [0u8; 2];
    read_exact(reader, &mut pw_verify_stored, "AES password verification")?;

    // PBKDF2-HMAC-SHA1, 1000 iterations → (key_len * 2 + 2) bytes:
    //   [0..key_len]           = AES key
    //   [key_len..2*key_len]   = HMAC-SHA1 key
    //   [2*key_len..2*key_len+2] = password verification value
    let mut derived = vec![0u8; key_len * 2 + 2];
    pbkdf2::pbkdf2_hmac::<Sha1>(password, &salt, 1000, &mut derived);

    let aes_key = &derived[..key_len];
    let hmac_key = &derived[key_len..key_len * 2];
    let pw_verify_derived = [derived[key_len * 2], derived[key_len * 2 + 1]];

    if pw_verify_stored != pw_verify_derived {
        return Err(CoreError::WrongPassword {
            method: PasswordDetectionMethod::EarlyHeaderValidation,
        });
    }

    // ── AES-CTR cipher ────────────────────────────────────────────────────
    // WinZip AES uses CTR mode with a 128-bit little-endian counter starting at 1.
    let iv = [1u8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
    let mut cipher = AesCtrCipher::new(aes_key, &iv, strength)?;

    // ── HMAC-SHA1 over ciphertext ─────────────────────────────────────────
    let mut mac = Hmac::<Sha1>::new_from_slice(hmac_key).expect("HMAC-SHA1 accepts any key size");

    // ── Decrypt in 64 KB chunks ───────────────────────────────────────────
    let mut decrypted = Vec::with_capacity(encrypted_data_len as usize);
    let mut chunk = vec![0u8; 64 * 1024];
    let mut remaining = encrypted_data_len;

    while remaining > 0 {
        let to_read = remaining.min(chunk.len() as u64) as usize;
        let n = reader.read(&mut chunk[..to_read]).map_err(map_io_err)?;
        if n == 0 {
            return Err(CoreError::InvalidFormat {
                message: "unexpected EOF inside AES encrypted data".into(),
            });
        }
        // HMAC is computed over ciphertext (before decryption).
        mac.update(&chunk[..n]);
        cipher.apply_keystream(&mut chunk[..n]);
        decrypted.extend_from_slice(&chunk[..n]);
        remaining -= n as u64;
    }

    // ── Verify 10-byte auth code ──────────────────────────────────────────
    let mut auth_code = [0u8; 10];
    read_exact(reader, &mut auth_code, "AES authentication code")?;

    let tag = mac.finalize().into_bytes();
    if tag[..10] != auth_code {
        return Err(CoreError::WrongPassword {
            method: PasswordDetectionMethod::CrcMismatch,
        });
    }

    Ok(decrypted)
}

// ── Helpers ───────────────────────────────────────────────────────────────

fn read_exact(reader: &mut impl Read, buf: &mut [u8], context: &str) -> Result<(), CoreError> {
    reader
        .read_exact(buf)
        .map_err(|e| CoreError::InvalidFormat {
            message: format!("truncated {context}: {e}"),
        })
}

fn map_io_err(e: io::Error) -> CoreError {
    // Propagate structured ReadErrors from the SyncIoBridge; everything else
    // is an unexpected truncation.
    if let Some(inner) = e.into_inner()
        && let Ok(re) = inner.downcast::<crate::io::ReadError>()
    {
        return CoreError::Io(*re);
    }
    CoreError::InvalidFormat {
        message: "I/O error reading AES encrypted data".into(),
    }
}
