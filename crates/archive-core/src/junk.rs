//! Filters for OS-generated junk files that commonly appear in archives.
//!
//! These files are metadata artefacts created by operating systems and
//! desktop environments. They have no value to the end user when browsing
//! or extracting an archive across platforms.

use crate::ArchiveEntry;

/// Entry names / path prefixes that are always considered junk.
///
/// | Pattern                  | OS      | Description                          |
/// |--------------------------|---------|--------------------------------------|
/// | `__MACOSX/`              | macOS   | AppleDouble container folder         |
/// | `._*` (any path segment) | macOS   | AppleDouble metadata sidecar file    |
/// | `.DS_Store`              | macOS   | Finder folder-view settings          |
/// | `Thumbs.db`              | Windows | Explorer thumbnail cache             |
/// | `desktop.ini`            | Windows | Explorer folder customisation        |
/// | `ehthumbs.db`            | Windows | Windows Media Center thumbnail cache |
/// | `.directory`             | Linux   | KDE folder-view settings             |

/// Returns `true` if this entry is an OS-generated junk file that should
/// be hidden from the user by default.
pub fn is_junk(entry: &ArchiveEntry) -> bool {
    let name = &entry.name;

    // macOS: entire __MACOSX folder
    if name.starts_with("__MACOSX/") || name == "__MACOSX" {
        return true;
    }

    // macOS: AppleDouble sidecar — any path component starting with ._
    // e.g. "folder/._file.pdf" or "._icon"
    if name.split('/').any(|seg| seg.starts_with("._")) {
        return true;
    }

    // Match against the filename component (last segment)
    let filename = name.split('/').next_back().unwrap_or(name);
    matches!(
        filename,
        ".DS_Store"
            | "Thumbs.db"
            | "thumbs.db"
            | "desktop.ini"
            | "Desktop.ini"
            | "ehthumbs.db"
            | ".directory"
    )
}
