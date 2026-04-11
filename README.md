# DropboxSync

Cross-platform Dropbox sync desktop project (Tauri + Rust), organized as an npm workspace monorepo.

## Main app

- Desktop client: `apps/desktop`
- Full setup and usage guide: `apps/desktop/README.md`

## Quick start

From repository root:

```bash
npm install
npm run dev
```

Other useful commands (use `dev:mac` on macOS and `dev:win` on Windows):

```bash
npm run dev:mac
npm run dev:win
npm run build
npm run test
```

On Windows, `dev:win` compiles the release `.exe` without generating an installer (NSIS/MSI) and starts that executable.

## Workspace layout

- `apps/desktop`: Tauri + React desktop application
- `packages/*`: shared workspace packages

## License

MIT — see `LICENSE`.
