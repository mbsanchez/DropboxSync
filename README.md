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

Other useful commands:

```bash
npm run start:dev
npm run build
npm run test
```

## Workspace layout

- `apps/desktop`: Tauri + React desktop application
- `packages/*`: shared workspace packages

## License

MIT — see `LICENSE`.
