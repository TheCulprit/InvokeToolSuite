# Invoke Tool Suite

A desktop app with two tools for InvokeAI:

- **Image Transfer** copies images from a v6 database and outputs folder into a v7 project and board.
- **CivitAI** finds missing trigger words and thumbnails for installed LoRAs and checkpoints.

Close InvokeAI before running either tool. Select the v7 database and outputs folder on the Home tab.

## Warning

This tool is vibecoded and may contain bugs. It creates its own database backups, but you may want to make your own backup before using it. Use it at your own risk.

## Run

Install [Node.js](https://nodejs.org/) and the [Rust toolchain](https://www.rust-lang.org/tools/install), then run:

```powershell
npm install
npm run desktop
```

To build a packaged desktop app:

```powershell
npm run desktop:build
```
