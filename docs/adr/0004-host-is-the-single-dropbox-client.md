# ADR-0004: The host app is the single Dropbox client; the File Provider extension never talks to Dropbox

## Status

**Accepted** (2026-11-01, by the maintainer, in the DBSYNC-95 spec, GitHub issue #231). Builds on
[ADR-0003](0003-macos-file-provider.md), which decided to adopt File Provider on macOS and left the process
model open. This ADR fixes it. Written in English, per the repository policy.

## Context

ADR-0003 settled the direction: macOS moves from `.cloudsc` sidecars to an
`NSFileProviderReplicatedExtension` (extension point `com.apple.fileprovider-nonui`). It did not say where the
Dropbox logic lives. A File Provider extension is a separate, sandboxed process that the system starts and stops
on its own schedule, and the host app already owns everything that talks to Dropbox: the OAuth token, the
longpoll, the remote index, the `sync_jobs` engine, the retry classification, the mass-deletion guard and the
conflict resolver. The question is whether the extension reaches Dropbox itself or goes through the host.

Two facts frame it. The host and the Finder Sync appex already share the App Group
`<TEAM_ID>.com.dropboxsyncdesktop` (DBSYNC-97, proven by #229 / PR #230), so a shared container exists. And a
File Provider extension must implement `createItem`, `modifyItem` and `deleteItem` (ADR-0003), so it cannot be
read-only; every local mutation is pushed at it.

## Decision

**The host app is the single Dropbox client.** It holds the token, runs the longpoll, the remote index and the
`sync_jobs` engine, is the only writer of the index, and signals the working-set enumerator after every index
change.

**The extension has no network entitlement and no keychain access.** It works through four things only:

1. **A read-only projection** of the index, read from `app.db` in the App Group container.
2. **A request queue**: a table in the same database where the extension records what the system asked of it
   (fetch, create, modify, delete). The host executes the requests.
3. **Staging**: file contents move between the two processes through a directory in the container.
4. **A Darwin notification** that wakes the host when a request is queued. The host also polls the queue on
   startup, so a notification posted while it was not running is not lost.

**Rejected alternatives**

- **Linking the Rust core into the extension.** The extension would need the token, so the keychain, so a
  keychain access group shared with the host. Keychain prompts come from unstable code identities, and the
  project has already paid for that. It would also need a network entitlement, a second copy of the sync state
  machine, and two writers on one SQLite file, with the extension's lifetime controlled by the system. Rejected
  for security (a second process holding the token), for correctness (two engines), and for build cost
  (splitting the crate).
- **XPC between the extension and the host.** It gives a typed channel, but it needs a registered service
  name, a connection lifecycle to manage on both sides, and a host that must be running for every call,
  including plain metadata reads. The database already gives the extension cheap, offline reads and the
  request table already gives durable writes. XPC would add a protocol to version and no capability the queue
  lacks. Rejected as unneeded. It can be revisited if the queue's latency proves to be a problem.

### Identity

- Every item, including remote-only files and folders, gets an **opaque local identifier**, used as the
  `NSFileProviderItemIdentifier`.
- `dropbox_id` (DBSYNC-99) is only the lookup key towards Dropbox. It is not the item identifier, because items
  created locally have no Dropbox id until their upload succeeds (ADR-0003, *the local identity space*).
- Identifiers are never derived from paths: Apple's header warns that identifiers may be recorded in system
  logs, so a path-derived one would leak user paths.

### Enumeration

- Folder enumerators list their children from the projection.
- The working set uses **one global, database-owned, monotonic change sequence** as its sync anchor.
- "Anchor expired" is returned **only when the Dropbox cursor is reset**, because expiry makes the system do an
  expensive scan of every item it knows.

### Versions

- The **content version** is derived from `content_hash`.
- The **metadata version** is derived from `rev` plus the name and the parent.
- `rev` is never used for content: it changes on a move while `content_hash` does not (ADR-0003, DBSYNC-99).

### Conflicts

- When a modification's base version is stale, **both versions are kept** as a conflicted copy, through the
  existing resolver and `sync_conflicts`.
- A delete that races a remote edit becomes a re-create.
- No lingering "refused" state is ever left behind: every collision resolves into ordinary state.

### When the host is not running

A request that needs the host fails with a **retryable File Provider error**, and the system retries it. To keep
that case rare, the host opens at login on macOS through an **`SMAppService` login item**. `SMAppService` and
`contentPolicy` (behind *Make Available Offline*) both need macOS 13, which is why the minimum system version
rises to 13.0. The target stays arm64 only.

### Rollout switch

Until the cutover slice, domain registration sits behind a switch, so `main` keeps the sidecar path.

- The switch is the **per-user defaults key `FileProviderEnabled`** in the app's defaults domain
  (`defaults write com.mobsanchez.dropboxsyncdesktop FileProviderEnabled -bool YES`), read once at startup
  through `NSUserDefaults`. It defaults to off, and no script or workflow writes it.
- It is not an environment variable, because a launch from Finder or by a login item never inherits one, so an
  env-var switch would remove the domain on every normal launch. It is not a compile-time feature, because the
  off build would then contain no removal code.
- An `app_config` breadcrumb records that this install registered a domain. The decision is a pure function:

  | Switch | Breadcrumb | Action |
  | --- | --- | --- |
  | off | none | nothing; **no File Provider call at all**, which is today's behaviour |
  | off | present | remove the domain, clear the breadcrumb |
  | on | any | add the domain (idempotent), set the breadcrumb |

- The File Provider appex is bundled in every macOS build and is inert until a domain is added. The decision is
  logged once at startup.

### What stays and what is gated at the cutover

Gated off on macOS, kept for Windows: the filesystem watcher, the local scan and the rename heuristics, the
placeholder-folder logic (DBSYNC-111), sidecar materialization and the `hydrate_cloudsc` producers. Reused: the
remote delta, the longpoll and the job engine, the retry classification and the mass-deletion guard, moves,
deletes and the conflict resolver, and the download and upload paths adapted to staging. On macOS `sync_folder`
stays unset, and the gating is explicit rather than implied by the unset value. Windows does not change.

## Consequences

- One Dropbox client means one set of retry, conflict and mass-delete rules, and one writer of the index. The
  cost is that the extension is only as available as the host: the login item and the retryable error exist
  for that.
- The extension is small and offline: it can be reviewed as "reads a database, writes a request row, copies
  files in the container". It cannot leak a token because it never has one.
- The database schema becomes an interface between two processes, so migrations are additive and the extension
  reads through a read-only connection. WAL and `-shm` under the sandbox need checking when the read path is
  built (slice 2).
- The Finder Sync extension stops monitoring on macOS at the cutover; deleting the appex is a separate ticket
  (ADR-0003, amended).
- `.cloudsc` code is not deleted; Windows still uses it.

## Not verified

**Nothing in this ADR has run yet.** It records decisions, not measurements.

- **Whether a File Provider domain mounts from our host, and whether a bundle with two appexes notarizes.**
  Slice 1 (#233) is the tracer for exactly this, and its live checks are made with the maintainer.
- **Whether the objc2 + `block2` calls to `NSFileProviderManager` work at run time.** A failure there is a
  crash, not a compile error. The fallback, decided only if the first live run shows it, is a small Swift shim.
- **Latency of the queue plus Darwin notification** against what the system tolerates for `fetchContents`.
- **Whether a sandboxed extension can read the host's SQLite file under WAL** through the group container.
- **Whether the system restarts the host's requests correctly** after the retryable error when the host starts
  late.
- **The login item** (`SMAppService`) has not been tried in this app.
