# Invoke Tool Suite

A cross-platform desktop app for working with InvokeAI. It has separate tools for moving images from a v6 backup into v7 and managing CivitAI trigger words and thumbnails.

## CivitAI model details

Choose the current v7 database and outputs folder on the Home screen. Both tools use the database selection, and Image transfer uses the outputs folder. The Home screen also sets how many recent safety backups to retain; the default is three, shared between both tools.

The CivitAI tool scans installed LoRAs, checkpoints, or both. It reads model file paths from the v7 model records, checks file hashes against CivitAI, and fills in missing trigger phrases and cover images in the v7 model database. It leaves existing values alone and shows the lookup cache location and counts. Cached successful and not-found lookups are reused; you can clear the cache and rescan, or cancel an active job (cancellation takes effect after the current lookup finishes). It creates or reuses the session safety backup before writing. InvokeAI must be closed while it updates the database. A CivitAI API key is optional and, when provided, is stored in the operating system credential store (Windows Credential Manager, macOS Keychain, or Linux Secret Service/keyutils), not in app settings or the lookup cache.

## Transfer behavior

- Reads the v6 SQLite database in read-only mode.
- Includes non-deleted, non-intermediate images with `image_origin = 'internal'` and `image_category = 'general'`.
- Copies image files while preserving `image_subfolder` and the original `created_at` value.
- Adds each image to the selected v7 board and preserves its chronological position in `board_images`.
- Skips duplicate names, missing files, and failed records, then lists them in the transfer summary.
- Creates a SQLite backup of the v7 database before making changes.
- Requires InvokeAI to be closed while the v7 database is updated.

The selected output folder can be either the outputs root (with an `images` child) or the actual images folder. Both v6 and v7 use the same default layout; selecting the actual images folder also supports a custom configured location.

## Build

This project uses Tauri 2, React, and TypeScript. Building the Windows desktop app requires Node.js, npm, and the Rust toolchain with the Windows C++ build tools installed.

```powershell
npm install
npm run desktop
```

Create a packaged app with:

```powershell
npm run desktop:build
```
