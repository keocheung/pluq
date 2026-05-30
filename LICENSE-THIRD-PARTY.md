# Third-Party Licenses

pluq is distributed under `MIT OR Apache-2.0` (see `LICENSE-MIT`, `LICENSE-APACHE`). It links
third-party components with their own terms. The notable non-permissive one:

## UnRAR (RAR decompression)

RAR4/RAR5 extraction is performed through the `unrar` crate, which provides FFI bindings to the
original **UnRAR** C++ source by Alexander Roshal. pluq uses the fork at
`https://github.com/stefandekanski/unrar.rs`; its `unrar_sys` crate vendors the upstream UnRAR
C++ source under `vendor/unrar/` and compiles it via `build.rs` — it is **FFI to the original
source, not a reimplementation** of the algorithm.

The UnRAR source is **freeware** but **not** open source. In short:

- It may be used to **develop software that decompresses** RAR archives, free of charge.
- It may **NOT** be used to **recreate the RAR compression algorithm** or to develop a
  RAR-compatible **archiver**.

pluq complies: it is an **extraction-only** tool — it cannot create RAR archives. This is an
intentional, documented scope limit (see `docs/ROADMAP.md`).

The UnRAR license requires its full text to be reproduced wherever the source is redistributed.
Because pluq's release binaries statically link the compiled UnRAR source, the license is
included verbatim below.

### UnRAR license (verbatim — Alexander L. Roshal)

```
 ******    *****   ******   UnRAR - free utility for RAR archives
 **   **  **   **  **   **  ~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~
 ******   *******  ******    License for use and distribution of
 **   **  **   **  **   **   ~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~
 **   **  **   **  **   **         FREE portable version
                                   ~~~~~~~~~~~~~~~~~~~~~

      The source code of UnRAR utility is freeware. This means:

   1. All copyrights to RAR and the utility UnRAR are exclusively
      owned by the author - Alexander Roshal.

   2. UnRAR source code may be used in any software to handle
      RAR archives without limitations free of charge, but cannot be
      used to develop RAR (WinRAR) compatible archiver and to
      re-create RAR compression algorithm, which is proprietary.
      Distribution of modified UnRAR source code in separate form
      or as a part of other software is permitted, provided that
      full text of this paragraph, starting from "UnRAR source code"
      words, is included in license, or in documentation if license
      is not available, and in source code comments of resulting package.

   3. The UnRAR utility may be freely distributed. It is allowed
      to distribute UnRAR inside of other software packages.

   4. THE RAR ARCHIVER AND THE UnRAR UTILITY ARE DISTRIBUTED "AS IS".
      NO WARRANTY OF ANY KIND IS EXPRESSED OR IMPLIED.  YOU USE AT 
      YOUR OWN RISK. THE AUTHOR WILL NOT BE LIABLE FOR DATA LOSS, 
      DAMAGES, LOSS OF PROFITS OR ANY OTHER KIND OF LOSS WHILE USING
      OR MISUSING THIS SOFTWARE.

   5. Installing and using the UnRAR utility signifies acceptance of
      these terms and conditions of the license.

   6. If you don't agree with terms of the license you must remove
      UnRAR files from your storage devices and cease to use the
      utility.

      Thank you for your interest in RAR and UnRAR.


                                            Alexander L. Roshal
```

## All other dependencies

Every other dependency is permissively licensed (MIT / Apache-2.0 / BSD / Zlib / Unicode), as
enforced by `deny.toml` in CI (`cargo deny check licenses`). For a full machine-generated
inventory run `cargo about generate` or inspect `Cargo.lock`.
