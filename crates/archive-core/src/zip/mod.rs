//! ZIP archive reader — built on top of [`ReadAt`].
//!
//! ```ignore
//! let reader = LocalReadAt::new("archive.zip");
//! let zip = ZipArchive::open(reader).await?;
//!
//! for entry in zip.entries() {
//!     println!("{} ({} bytes)", entry.name, entry.uncompressed_size);
//! }
//!
//! let data = zip.extract(&zip.entries()[0]).await?;
//! ```

pub mod index;
mod parse;

pub use index::ZipIndex;

use std::io::Read;

use bytes::Bytes;
use flate2::read::DeflateDecoder;

use crate::{ArchiveEntry, ArchiveError, ReadAt};

use self::parse::{METHOD_DEFLATE, METHOD_STORED};

/// A parsed ZIP archive backed by any [`ReadAt`] source.
///
/// Created via [`ZipArchive::open`], which reads the EOCD and Central
/// Directory to build the file listing. Extraction calls issue additional
/// targeted range reads.
pub struct ZipArchive<R: ReadAt> {
    reader: R,
    entries: Vec<ArchiveEntry>,
}

impl<R: ReadAt> ZipArchive<R> {
    /// Open a ZIP archive by reading its EOCD and Central Directory.
    ///
    /// This issues exactly **2 range reads** (or 1 if the file is small).
    pub async fn open(reader: R) -> Result<Self, ArchiveError> {
        let eocd = parse::find_eocd(&reader).await?;
        let entries = parse::parse_central_directory(&reader, &eocd).await?;

        Ok(Self { reader, entries })
    }

    /// Return the list of entries (files and directories) in the archive.
    pub fn entries(&self) -> &[ArchiveEntry] {
        &self.entries
    }

    /// Extract a single entry's contents, decompressing if necessary.
    ///
    /// Issues exactly **1 range read** for the Local File Header + data.
    pub async fn extract(&self, entry: &ArchiveEntry) -> Result<Bytes, ArchiveError> {
        if entry.is_directory {
            return Ok(Bytes::new());
        }

        let compressed = parse::read_local_file_data(&self.reader, entry).await?;

        match entry.compression_method {
            METHOD_STORED => Ok(compressed),
            METHOD_DEFLATE => {
                let mut decoder = DeflateDecoder::new(&compressed[..]);
                let mut decompressed = Vec::with_capacity(entry.uncompressed_size as usize);
                decoder.read_to_end(&mut decompressed).map_err(|e| {
                    ArchiveError::Other(format!("deflate decompression failed: {e}"))
                })?;
                Ok(Bytes::from(decompressed))
            }
            other => Err(ArchiveError::Other(format!(
                "unsupported compression method: {other}"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Cursor, Write};

    use zip::CompressionMethod;
    use zip::write::SimpleFileOptions;

    use crate::local::LocalReadAt;

    use super::*;

    /// Helper: create a real ZIP file in a temp file, return a `LocalReadAt`.
    fn create_test_zip(
        files: &[(&str, &[u8], CompressionMethod)],
    ) -> (tempfile::NamedTempFile, LocalReadAt) {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        {
            let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
            for (name, data, method) in files {
                let options = SimpleFileOptions::default().compression_method(*method);
                writer.start_file(*name, options).unwrap();
                writer.write_all(data).unwrap();
            }
            let buf = writer.finish().unwrap().into_inner();
            std::fs::write(tmp.path(), &buf).unwrap();
        }
        let reader = LocalReadAt::new(tmp.path());
        (tmp, reader)
    }

    #[tokio::test]
    async fn list_files_in_zip() {
        let (_tmp, reader) = create_test_zip(&[
            ("hello.txt", b"Hello, world!", CompressionMethod::Deflated),
            (
                "data/numbers.bin",
                b"\x01\x02\x03\x04",
                CompressionMethod::Stored,
            ),
        ]);

        let zip = ZipArchive::open(reader).await.unwrap();
        let entries = zip.entries();

        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].name, "hello.txt");
        assert_eq!(entries[0].uncompressed_size, 13);
        assert!(!entries[0].is_directory);

        assert_eq!(entries[1].name, "data/numbers.bin");
        assert_eq!(entries[1].uncompressed_size, 4);
    }

    #[tokio::test]
    async fn extract_deflated_file() {
        let content = b"Archive Sniper is awesome! Compressing this text with Deflate.";
        let (_tmp, reader) =
            create_test_zip(&[("compressed.txt", content, CompressionMethod::Deflated)]);

        let zip = ZipArchive::open(reader).await.unwrap();
        let data = zip.extract(&zip.entries()[0]).await.unwrap();

        assert_eq!(data.as_ref(), content);
    }

    #[tokio::test]
    async fn extract_stored_file() {
        let content = b"\x00\x01\x02\x03\x04\x05\x06\x07";
        let (_tmp, reader) = create_test_zip(&[("raw.bin", content, CompressionMethod::Stored)]);

        let zip = ZipArchive::open(reader).await.unwrap();
        let data = zip.extract(&zip.entries()[0]).await.unwrap();

        assert_eq!(data.as_ref(), content);
    }

    #[tokio::test]
    async fn handles_empty_zip() {
        let (_tmp, reader) = create_test_zip(&[]);

        let zip = ZipArchive::open(reader).await.unwrap();
        assert!(zip.entries().is_empty());
    }
}
