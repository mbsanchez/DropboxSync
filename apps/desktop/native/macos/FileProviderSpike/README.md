# FileProviderSpike — DISPOSABLE

**This directory is a throwaway spike for DBSYNC-79 (GitHub #140). It must be deleted
before that ticket closes. It ships nothing and must never become the production
File Provider extension.**

Its only purpose is to answer questions that documentation cannot:

1. Can the Tauri build bundle a **second** appex, and does it survive signing and notarization?
2. Does a `com.apple.fileprovider-nonui` extension coexist with our `com.apple.FinderSync` one?
3. What does the App Group actually require in practice?

The enumerator returns four hardcoded items. There is no network, no database and no
engine. If this directory still exists when DBSYNC-79 is closed, the ticket was closed
wrong.
