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

## Releasing (macOS)

Pushing a tag matching `v*` runs `.github/workflows/release.yml`: build → sign → notarize →
staple → verify → draft GitHub release. `workflow_dispatch` runs the same job without
minting a tag, and uploads the artifacts instead of publishing them.

It automates the path that DBSYNC-72 Slice 4 proved by hand first, and it verifies its own
output with the checks a user's Mac performs on arrival — `codesign --deep --strict`,
`stapler validate`, and `spctl --assess`. A build that would be rejected on someone else's
machine fails the pipeline instead of shipping.

### Repository secrets it needs

Names only. Never commit a value, and never echo one into a job log.

| Secret | What it is |
|---|---|
| `APPLE_CERTIFICATE` | base64 of a `.p12` export of the Developer ID Application certificate |
| `APPLE_CERTIFICATE_PASSWORD` | the password used when exporting that `.p12` |
| `APPLE_SIGNING_IDENTITY` | `Developer ID Application: NAME (TEAMID)` |
| `APPLE_TEAM_ID` | the 10-character team id — a repository **variable**, not a secret |
| `APPLE_API_KEY_P8` | base64 of `AuthKey_XXXXXXXXXX.p8` from App Store Connect |
| `APPLE_API_KEY_ID` | the key id — the `XXXXXXXXXX` in that filename |
| `APPLE_API_ISSUER` | App Store Connect issuer id (a UUID) |
| `DROPBOX_APP_KEY` | the real app key |
| `DROPBOX_REDIRECT_URI` | the real redirect URI |

Notarization uses an App Store Connect **API key** rather than an Apple ID plus an
app-specific password: an app-specific password is account-wide, while an API key is scoped
and can be revoked on its own.

The last two are easy to overlook and expensive to get wrong. `auth/oauth.rs` reads both
through `option_env!`, so a release built with `ci.yml`'s `dummy` placeholder compiles,
signs and notarizes perfectly — and then cannot authenticate against Dropbox at all.
