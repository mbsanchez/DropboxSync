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
- "Anchor expired" is returned **only where the list under *Every place anchors expire* says**: a snapshot (the
  Dropbox cursor was empty or reset, or the projection is being reseeded), a clear, and the tombstone cap. A delta
  never expires one. Expiry makes the system do an expensive scan of every item it knows, so each of those places
  is deliberate.

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

Until the cutover slice, domain registration sits behind a switch, so `main` keeps the sidecar path. (See *Merge ordering* below: the switch keeps the domain off, but it does not keep Finder Sync enabled.)

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

- The File Provider appex is bundled in every macOS build and does nothing as a File Provider until a domain is
  added (it still disables Finder Sync on macOS 26; see *Merge ordering*). The decision is logged once at startup.

### Merge ordering: DBSYNC-95 and DBSYNC-98 land together

**Decided by the maintainer on 2026-11-01, after the slice 1 live run (#233).** On macOS 26, merely shipping
the File Provider appex in the bundle keeps the Finder Sync appex disabled (`-` in `pluginkit`), with or
without a domain registered, and the two toggles in System Settings are coupled. The same pattern shows in
the official Dropbox app on that Mac. The rollout switch above therefore does **not** keep `main` unchanged
for Finder Sync: it only keeps the domain off.

Consequently **nothing from DBSYNC-95 reaches `main` before the DBSYNC-98 cutover**, which retires Finder
Sync anyway. The two tickets are merged to `main` together (PR #240 records the same). Until then the work
lives on `feature/DBSYNC-95-file-provider-extension`.

### What Dropbox actually sends (captured live, 2026-11-01, #234)

Slice 2 designs the projection around the raw `list_folder/continue` entries, logged by the app's own
`remote delta: raw entry` line while the maintainer made the changes on dropbox.com. Four facts follow, and
each one changed the design:

1. **A `deleted` entry carries a path and nothing else.** No id, no rev. A delete is therefore resolved
   through the item's *current* path (the projection keeps a `path_lower` index); only upserts are keyed by
   `dropbox_id`.
2. **A folder rename arrives as one batch, deletes first.** The old child path and the old folder path are
   deleted, then the folder is re-added at its new path with the **same id**, then the child with its same
   id. Applied entry by entry in that order (see *Contract model* below), this is a move: the re-add finds
   the tombstone of the same id and updates it in place, so both ids survive and both rows end live.
3. **A move changes the file's `rev` while its content stays the same.** Confirmed on the wire, so the
   content version is derived from `content_hash` only (see *Versions*).
4. **Deleting a folder with content lists the child deletes explicitly**; an empty folder is one entry.
   Whether that holds for a large or deep tree is *not* observed, so a delete also tombstones everything
   beneath its path prefix as a fallback. Whether a renamed folder's descendants are all re-listed is not
   observed either, and the projection does not guess: it follows the contract literally (see below) and the
   question goes to a live check in slice 2b.

### The projection (slice 2a)

The host maintains three additive tables in `app.db`, behind the same switch (macOS only, written only when
the switch is on):

- `fp_identity` holds the opaque `item_id` (minted from the same `item_id_seq` as `local_file_index`, so the
  two cannot collide) and the Dropbox id; rows are never deleted.
- `fp_item` holds the tree: a stored parent id (0 is the domain root), name, `path_lower`, kind, size, mtime,
  `content_hash`, `rev`, both versions, a `downloaded` flag, a state (live, hidden by selective sync,
  deleted, orphan) and a `changed_seq`.
- `fp_state` holds the anchor ingredients: `generation` (minted once), `seq`, `min_valid_seq`, `populated`.

The change sequence is global and monotonic; every observable row change takes the next value through one
`stamp` function. Stamping happens once, at the end of a batch, and only for a row whose final state differs
from its state before the batch, so a replayed delta ends in the same state and bumps nothing.
Selective-sync exclusions (DBSYNC-114) are hidden through the state flag, flipped in one transaction when the
filter is saved (the filter is read inside that transaction, so a concurrent Save cannot leave stale flags).
The versions are 67-byte digests: content from `content_hash`, metadata from name, parent and `rev`.

**Every place anchors expire.** An anchor `v1.<generation>.<seq>` expires when its generation differs, its
`seq` is below `min_valid_seq`, or it is above the database's `seq`. `min_valid_seq` moves in exactly three
places, and `generation` in none (it is minted once, when the database is first migrated, so a database that
is deleted and recreated expires every anchor an earlier one handed out):

1. **A snapshot** (the Dropbox cursor was empty or was reset, or the projection was empty and is being
   reseeded): `min_valid_seq` becomes the new `seq`, and the old tombstones are dropped.
2. **A clear**: logout or a new sync folder (`reset_sync_state`), the switch going off at startup, and the
   projection giving up on a failing delta or a failing snapshot (below). `min_valid_seq` becomes the new
   `seq` and the rows are deleted. Identities are kept.
3. **The tombstone cap.** Tombstones are what lets an anchor older than a deletion learn of it. With one
   global `min_valid_seq` every valid anchor may need every tombstone stamped after it, so none can be
   dropped for free. Past 50 000 tombstones the oldest are dropped and `min_valid_seq` is raised to the
   newest one dropped. An anchor `S` needs a tombstone stamped `t` only if `S < t`; after the raise every
   valid `S` is at least the largest dropped `t`, so no valid anchor needs a deletion that is gone. The cost
   is that anchors older than that expire and the system re-enumerates (as after any expiry); it is paid
   only by an account that deletes more than 50 000 items between two snapshots.

Nothing else moves it: not a delta, not a filter change, not a download flag.

**Contract model: each item changes only through its own entry.** The projection applies a batch the way
Dropbox describes `list_folder`: the entries one by one, in order (pages in order, entries in order within a
page, across the pages that `run_remote_delta` accumulates), inside the one IMMEDIATE transaction of the batch.

*Where this comes from, stated plainly.* The contract text (entries in order; a `deleted` entry names only a
path; a file or folder entry replaces what is at its path) is Dropbox's classic `/delta` rules. **It is not
in the current v2 documentation we could read** (the "detecting changes" page does not state it), so it is an
assumption inherited from `/delta`. It has been checked against the #234 capture, and only for the shapes
captured: a nested create, a folder rename, a file move, an empty folder delete and the delete of a folder
with content. Nothing beyond those shapes is verified.

An earlier design resolved a batch as a whole, and a later one carried a moved folder's descendants and
revived the ones a delete had swept up, as hedges for the unverified case where Dropbox does not re-list a
renamed folder's descendants. Review rounds showed that those hedges create phantoms and lose items that were
listed explicitly, so they were removed: there is no carry, no revival and no inference.

1. **`deleted P`.** The non-deleted row at `P` and every non-deleted row under `P/` become tombstones.
2. **`file|folder P` with id `I`.**
   - *A different non-deleted item `R` at `P`* is replaced. A **file** newcomer tombstones `R`, and its
     subtree when `R` is a folder (a file has no children). A **folder** newcomer over a file tombstones the
     file. A **folder** over a **folder** tombstones only `R` and **keeps its children**: the non-deleted
     children of `R` are re-parented to `I` (a parent change, stamped if observable); the deeper levels keep
     their own parents.
   - *An id the projection knows* is updated in place, in whatever state (live, hidden, orphan, deleted):
     path, name, parent, metadata, versions and state. Nothing else moves; its children keep their parent id
     and their paths until their own entries arrive.
   - *An unknown id* gets an identity from `item_id_seq` and a new row.
3. **After the last entry**, orphans whose parent folder now exists are healed, following selective sync, and
   rows left under a folder that became an orphan are demoted with it. Both repeat until nothing moves, so a
   chain of orphans heals in one batch. A heal never touches a tombstone.
4. **Stamps.** A new row is stamped as it is inserted. A row that existed takes one sequence value after the
   batch if its final state differs from the one it began with, and none otherwise. A row an entry restates
   writes nothing at all (these guards were worth 10 to 100 times in time).

*The two orders of a rename, for children that are not re-listed.* With the **deletes first** (what the
capture showed) `deleted /a` tombstones the subtree, the re-add restores the folder by its id, and a child that
is not re-listed stays deleted. With the **deletes last** the folder and the re-listed children move through
their own upserts, and a child that is not re-listed keeps its parent id and its old path, so the trailing
`deleted /a` sweeps whatever still sits at the old paths; anything re-listed has already moved.

*Orphans, the rule for 2b.* A folder at state ORPHAN is **not** a parent: a child of an orphan folder is itself
an orphan, so a live or hidden row never has an orphan parent. An orphan has `parent_item = -1` and is never
listed.

*The snapshot* is total and infers nothing: every listed entry is placed by its id alone, every id not listed
is swept, and only then are the parent and orphan state of every remaining row resolved again (a parent found
while placing may be a row the sweep removed). The result does not depend on the order of the listing, and
after a snapshot no listed id is a tombstone (checked).

Covered by tests that build their state through `run_remote_delta` and `finish_seed`, with entries shaped like
the capture (deletes without id or rev): the captured orders, replay (a second application changes nothing and
signals nothing), the two rename orders, file-over-folder and folder-over-folder, orphan chains and listing-order
independence of the snapshot. **What the model trusts:** the order, and that a renamed folder's descendants are
re-listed.

**Live check for 2b (open).** Rename a folder of about 200 files on dropbox.com and capture whether every
descendant is re-listed. If not, the unlisted ones are left as the contract leaves them and this section must
be revisited with the capture in hand.

**Paths are folded.** `path_lower` is stored and queried as `path_util::fold` of what Dropbox sent (NFC, then
lowercase), the comparison form for remote-versus-remote matches (DBSYNC-103). An NFD delete finds the NFC row
it names.

**The projection never pins normal sync.** A projection failure fails the delta (the cursor stays, the batch
replays, a replay changes nothing) up to five times in a row, about 75 seconds with the long-poll loop's
error backoff. The fifth gives up: the projection is cleared and marked unpopulated, an ERROR is logged, and
the cursor advances. A cursor beside an unpopulated projection counts as no cursor, so the next iteration
seeds a snapshot, but not before a backoff (1 minute, doubling to 30 minutes) while the failures continue. The
backoff is kept in wall-clock time, because a monotonic clock does not advance while the Mac sleeps; a deadline
further away than the longest backoff means the clock was set back and counts as due. A snapshot that fails is
handled the same way and does not hold the cursor back. So does a reseed whose *listing* fails while a cursor is
stored: the stored cursor is used for that iteration and the next reseed is backed off, so a failing listing
cannot pin normal sync. With no stored cursor the error is returned, as before. With the switch off none of this
applies.

**A projection reseed is a full seed.** The Dropbox cursor is single: the same cursor drives the local remote
index and the projection. A reseed of the projection therefore runs the normal seed, `seed_remote_delta_cursor`:
it lists the whole account, reconciles the local index when a sync folder is set (through the same
mass-deletion breaker as the periodic sweep), writes the projection snapshot, and *replaces the stored cursor*
with the one that listing returned. On macOS with the projection on there is normally no sync folder, so the
reconcile has nothing to do; but if one is set, a projection reseed is also a reseed of its index. There is no
separate cursor for the projection.

The extension (slice 2b) only reads these tables, through a read-only connection, and the host tells the
system to re-enumerate the working set after each commit with a fire-and-forget signal.

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

- **Measured by slice 1 (#233):** a domain mounts from our host, notarization accepts a bundle with two
  appexes, and the `objc2` + `block2` calls to `NSFileProviderManager` (`addDomain`, `removeDomain`,
  `getDomains`) work at run time. The finding that the File Provider appex disables Finder Sync on macOS 26
  is recorded above.
- **The working-set signal** (`getDomains`, then `managerForDomain:`, then
  `signalEnumeratorForContainerItemIdentifier:` with the framework's working-set constant) is written
  against the SDK headers and has not run live; a wrong selector is a crash, not a compile error.
- **Whether a sandboxed extension can read the host's SQLite file under WAL** through the group container.
  Slice 2b checks it, with the host running and with it quit.
- **Latency of the queue plus Darwin notification** against what the system tolerates for `fetchContents`.
- **Whether the system restarts the host's requests correctly** after the retryable error when the host starts
  late.
- **The login item** (`SMAppService`) has not been tried in this app.
- **Deep or large folder deletes and renames** (fact 4 above): the prefix fallback exists because the
  listing of every descendant is unproven. The contract text itself comes from the classic `/delta` rules and
  was only checked against the shapes captured in #234.
