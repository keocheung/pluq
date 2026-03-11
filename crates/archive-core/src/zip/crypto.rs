//! ZipCrypto (PKWARE Traditional Encryption) decryptor.
//!
//! Traditional ZIP encryption uses a stream cipher based on three 32-bit keys
//! updated per decrypted byte via CRC-32 table lookups. Weak by modern
//! standards but extremely common in the wild — most password-protected ZIPs
//! in the wild use this method.
//!
//! Reference: PKWARE APPNOTE.TXT §6.1

use std::io::{self, Read};

// ── CRC-32 lookup table ───────────────────────────────────────────────────
// Standard CRC-32 table with polynomial 0xEDB88320 (reflected/reversed).
// The ZIP crypto key update requires a per-byte CRC update, which needs direct
// access to the lookup table. We can't use crc32fast for this — it doesn't
// expose a per-byte update primitive.

const fn make_crc32_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut i = 0usize;
    while i < 256 {
        let mut c = i as u32;
        let mut j = 0;
        while j < 8 {
            if c & 1 != 0 {
                c = 0xEDB8_8320 ^ (c >> 1);
            } else {
                c >>= 1;
            }
            j += 1;
        }
        table[i] = c;
        i += 1;
    }
    table
}

static CRC32_TABLE: [u32; 256] = make_crc32_table();

#[inline]
fn crc32_update(crc: u32, byte: u8) -> u32 {
    CRC32_TABLE[((crc ^ u32::from(byte)) & 0xFF) as usize] ^ (crc >> 8)
}

// ── ZipCryptoDecryptor ────────────────────────────────────────────────────

/// Traditional PKWARE ZIP encryption (ZipCrypto).
///
/// Stream cipher using three 32-bit keys updated per byte via CRC-32 table
/// lookups. Weak by modern standards but ubiquitous in the wild.
pub(crate) struct ZipCryptoDecryptor {
    key0: u32,
    key1: u32,
    key2: u32,
}

impl ZipCryptoDecryptor {
    /// Initialize the cipher state from a password.
    pub fn new(password: &[u8]) -> Self {
        let mut d = Self {
            key0: 305_419_896,
            key1: 591_751_049,
            key2: 878_082_192,
        };
        for &b in password {
            d.update_keys(b);
        }
        d
    }

    fn update_keys(&mut self, byte: u8) {
        self.key0 = crc32_update(self.key0, byte);
        self.key1 = self.key1.wrapping_add(self.key0 & 0xFF);
        self.key1 = self.key1.wrapping_mul(134_775_813).wrapping_add(1);
        self.key2 = crc32_update(self.key2, (self.key1 >> 24) as u8);
    }

    /// Decrypt a single byte and update the key state.
    pub fn decrypt_byte(&mut self, cipher_byte: u8) -> u8 {
        let temp = self.key2 | 2;
        let plain = cipher_byte ^ ((temp.wrapping_mul(temp ^ 1) >> 8) as u8);
        self.update_keys(plain);
        plain
    }

    /// Decrypt a buffer in-place.
    pub fn decrypt(&mut self, buf: &mut [u8]) {
        for b in buf.iter_mut() {
            *b = self.decrypt_byte(*b);
        }
    }
}

// ── ZipCryptoReader ───────────────────────────────────────────────────────

/// A `Read` adapter that ZipCrypto-decrypts bytes on the fly.
pub(crate) struct ZipCryptoReader<R: Read> {
    inner: R,
    decryptor: ZipCryptoDecryptor,
}

impl<R: Read> ZipCryptoReader<R> {
    pub fn new(inner: R, decryptor: ZipCryptoDecryptor) -> Self {
        Self { inner, decryptor }
    }
}

impl<R: Read> Read for ZipCryptoReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.decryptor.decrypt(&mut buf[..n]);
        Ok(n)
    }
}
