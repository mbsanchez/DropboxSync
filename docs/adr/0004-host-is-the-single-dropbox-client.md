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
2. **A request queue**: files in the container, under `fp-exchange/requests/`, where the extension records what
   the system asked of it. The host executes the requests. (Amended 2026-11-07, slice 3: this was a table in
   `app.db`. That would have made the extension a second SQLite writer, against the read-only connection the
   rest of this ADR relies on, and files need no schema and wait on no write lock.)
3. **Staging**: file contents move between the two processes through a directory in the container.
4. **A Darwin notification** that wakes the host when a request is queued. The host also scans the queue every
   2 s and at startup, so a lost notification, or one posted while it was starting, delays a request and
   does not lose it.
5. **A lock**: the host holds an exclusive `flock` on `fp-exchange/host.lock` for its whole life. The kernel
   drops it when the process dies, however it dies, so the extension tells a host that is down from one that
   is slow, without a heartbeat and without hanging.

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
  request files already give durable writes. XPC would add a protocol to version and no capability the queue
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
- A delete that races a remote edit is refused (`deletionRejected`), and the system restores the file with
  Dropbox's version.
- No lingering "refused" state is ever left behind: every collision resolves into ordinary state.

### When the host is not running

A request that needs the host fails at once with `serverUnreachable`: the extension finds the host's lock free,
before it queues anything and at every poll while it waits. The system shows an error and backs off until
signalled, so the host calls `signalErrorResolved:` when it starts, when a projection finishes populating, and
30 s after the last answer that Dropbox is unreachable or the session needs the user. To keep that case rare, the host opens at login on macOS through an **`SMAppService` login item**. `SMAppService` and
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
each one changed the design. Fact 2 was corrected by a second capture on 2026-11-06:

1. **A `deleted` entry carries a path and nothing else.** No id, no rev. A delete is therefore resolved
   through the item's *current* path (the projection keeps a `path_lower` index); only upserts are keyed by
   `dropbox_id`.
2. **A folder rename arrives deletes first, and not always in one batch.** The old child path and the old
   folder path are deleted, then the folder is re-added at its new path with the **same id**, then every
   descendant with its same id. Applied entry by entry in that order (see *Contract model* below), this is
   a move: the re-add finds the tombstone of the same id and updates it in place, so the ids survive and the
   rows end live. A two-entry rename came in one invocation. A folder of 204 entries came in two: 204 deletes
   with `has_more=false`, then, 2.9 s later, 204 re-adds. DBSYNC-102 had already measured the same split on a
   share and an un-share (3.45 s and 3.43 s). See *A rename split in two* below.
3. **A move changes the file's `rev` while its content stays the same.** Confirmed on the wire, so the
   content version is derived from `content_hash` only (see *Versions*).
4. **Deleting a folder with content lists the child deletes explicitly**; an empty folder is one entry.
   Whether that holds for a large or deep tree is *not* observed, so a delete also tombstones everything
   beneath its path prefix as a fallback. A renamed folder's descendants **are** all re-listed: the 204-entry
   rename listed every file and subfolder, two levels deep.

### A rename split in two

Between the two invocations the projection correctly says the rows are deleted: `has_more=false` means
nothing announces the second half. Publishing that gap would make Finder drop the folder and bring it back,
and from slice 3 on it could cost downloaded content or a pending change.

So the host holds the **signal**, not the data. A delta that leaves a previously live row non-live delays the
working-set signal by 10 s, moved later by each further such delta and never past 60 s from the first. A
delta that removes nothing does not shorten a hold in progress, and a rename inside one batch holds nothing.
Ten seconds is about three times the longest gap measured; the price is that a real remote delete reaches
Finder ten seconds late.

This narrows the window and does not close it: the data is already gone from the projection during the hold,
so an enumeration the system starts itself, `item(for:)` or `fetchContents` all read the gap. Slices 3 and 4
must assume an item can be reported deleted and return with the same identifier seconds later. The answer in
that gap matters: `noSuchItem` tells the system the item was deleted and to attempt deleting it from disk, so
once items have content (slice 3) a rename in progress must not answer it for a row that may come back.

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

### Download on open (slice 3)

`fetchContents` checks the row and the lock, renames a request into `fp-exchange/requests/`, posts the
notification and polls. The host takes the request, reads the row, downloads `rev:<rev>` into
`fp-exchange/staging/`, checks the bytes against the row's `content_hash` and renames a result into
`fp-exchange/results/`. The extension then compares the result's `content_version` with the row as it stands
and hands the file over; a row that moved on meanwhile answers a transient error, and the system asks again.

- **Answers:** Dropbox unreachable is `serverUnreachable`, a session that needs the user is `notAuthenticated`,
  a file Dropbox will not hand out (`unsupported_file`, which a Paper document gets) is
  `NSFileReadNoPermissionError`, and anything else is a transient error. Dropbox's "not found" is transient
  too: a real delete reaches the system through the change feed.
- **A file Dropbox will not hand out** is refused without a message from the system, so when Finder asked
  (`isFileViewerRequest`) the host opens its page on dropbox.com instead, at most once per file per 10 s.
  Spotlight and Quick Look get the refusal and nothing else.
- **`noSuchItem`** answers only an identifier with no row. A row that is not live answers a transient error, in
  `item(for:)`, `fetchContents` and folder enumeration, because it may come back (*A rename split in two*). The
  system should stop asking about a real delete once the feed has told it.
- **`NSFileProviderItem.isDownloaded`** is ignored by a replicated extension, so `fp_item.downloaded` stays
  unset here.
- **Cleanup:** the system clones and unlinks what it is handed. A fetch abandoned after the host took it
  leaves a marker, and the host drops the bytes when the download ends: the download itself is not aborted.
  The host sweeps exchange files older than an hour, which a crash leaves behind, and removes the directory
  at startup with the switch off.

### Writes (slice 4)

`createItem`, `modifyItem` and `deleteItem` go through the same queue; the extension clones new bytes into
`fp-exchange/staging/<id>.up`. The host calls Dropbox, waits for its answer and writes the row from the
metadata Dropbox returns, through the delta writer, so the echo of the change in the feed finds it already
there. Nothing is queued on the host: a failure answers the system, which retries.

- **Paths** are built from the chain of names up to the root, not from `path_lower`, so a write under a folder
  renamed here uses the new name. The rows under a folder renamed or moved here take its new `path_lower` at once,
  so deletes and sweeps that match by path find them before the echo arrives.
- **Edits** upload with `mode: update` on the row's `rev`. A stale base, or Dropbox refusing the `rev`, sends
  the edit up beside Dropbox's version as `name (conflicted copy <UTC stamp>).ext`, records it in
  `sync_conflicts` and makes the system fetch Dropbox's version again. An edit to a file deleted on Dropbox in
  the meantime ended the same way when measured live: the edit came back as a conflicted copy.
- **Deletes** carry `parent_rev`; a file edited on Dropbox since is refused with `deletionRejected`. So is a
  folder holding anything selective sync hides or that waits as an orphan: Dropbox deletes a folder whole,
  and that content was never in Finder (the sync folder refuses the same, DBSYNC-114). A delete the system
  does not mark recursive is refused with `directoryNotEmpty` when the projection holds children.
- **Deleted on Dropbox meanwhile.** An edit or a move of an item that Dropbox deleted answers `noSuchItem`
  for the item (the header's rule: the system deletes it from disk, or re-creates it when it holds an
  edit); a create in, or a move into, a folder that Dropbox deleted answers `noSuchItem` for that folder. While a removal is still held back from the system it answers a transient error instead,
  because the row may be the first half of a split rename and come back with its id.
- **Sent again.** A create whose item is already a live child with the same bytes (or, for a folder, a
  folder), and an edit refused as stale whose bytes are already Dropbox's, answer that item without
  calling Dropbox: the system sends a write again when the extension stopped waiting, and the first one may
  have landed. Before the delta catches up, a refused upload reads Dropbox's metadata to tell its own landed
  edit from someone else's; when that read fails, the edit is retried rather than kept as a copy. A child whose delete is still held or waiting does not count, so a file put back under its
  name is a new item. Before a write is sent, and between the chunks of a session, the host checks that the
  extension still waits for it; one it stopped waiting for is not sent.
- **Not sent.** A create, or a move, that selective sync would hide is refused with `cannotSynchronize`, since it would
  reach Dropbox and never show. Packages (`.rtfd`, `.pages`) are refused with `featureUnsupported`: they are
  directories the host would have to send as one file.
- **One write at a time.** Finder sends a selection as concurrent calls, and Dropbox answered concurrent
  writes with 429 (40 of 50 parallel deletes, 2 of 6 parallel creates). A file larger than one 8 MiB chunk
  goes up in an upload session, and only its `finish` waits for the lane, so a long upload does not hold up
  other writes while its bytes travel. A write that waits for the lane longer than 240 s gives up without
  sending anything, well inside the extension's 600 s, so nothing is written after the extension stopped
  waiting.
- **The mass-deletion breaker** (DBSYNC-64) holds a Finder selection that it calls a mass deletion until the
  user confirms or restores it in the app. Each call waits until no delete has arrived for 1.5 s, so every
  call sees the whole burst; refused, each answers `deletionRejected` and the system restores the files. The
  system keeps at most 32 deletes in flight and sends the next only as one returns (measured: a 300-file
  selection arrived as 32, then one per answered call), so the breaker never sees a large selection whole.
  A burst that fills all 32 slots is therefore held whatever the threshold says: judged on the threshold
  alone, the first 32 of a 300-file selection went to Dropbox in an account of 440 files, and with 2,000
  files or more the one-by-one stream that follows (about 1.5 deletes a second through the write lane)
  could never reach 200 within the window, so a whole folder would have gone unasked. The cost: deleting 32
  files or more in Finder always asks, and the banner gives the count so far, since the rest has not arrived.
  An answer covers the operation: the calls waiting, the rest of the selection while it keeps arriving with
  no pause over 5 s (one Dropbox delete), however long, and items the system sends again (a call that gave
  up waiting, a confirmed delete Dropbox did not take), which carry their answer whatever comes between. A
  pause ends it, and any later delete is a new question, including the same files deleted again after a
  restore: the system does not retry a `deletionRejected`. A burst under 32 is judged together with the
  small bursts let through in the last 60 s, so small selections made one after another are asked about
  once they add up; those let through before the question have gone. A confirmation empties the window. A
  burst is judged before another can start, so a delete that arrives as a mass deletion settles cannot let
  it through unasked; an answer nobody was waiting for (every call gave up) settles its own burst before
  the next delete is looked at. The cost of the stream is that a delete of something else made while the
  rest of an answered selection is still arriving takes that answer. The answer is one key, last click
  wins; the click names the question it answers, and counts only if that question is still on screen (the
  sync folder's comes first); it is cleared when a question opens and dropped with the question at
  startup, because the burst it was about lived in memory.
- **Move to Trash** (Cmd+Delete) needs `allowsTrashing`. The domain sets `supportsSyncingTrash` to NO, so
  Finder moves a downloaded file to the user's Trash and sends `deleteItem`, and offers to delete one that is
  not downloaded. The setter exists from macOS 15; before it the items do not offer trashing, and only Delete
  Immediately works.
- **Cached capabilities.** The system keeps an item's capabilities until the item reaches it with another
  version. `ItemFormat` is part of every anchor and every metadata version, so a build that changes what an
  item reports bumps it, every older anchor expires into a rescan, and the rescan replaces each item. Slice
  3's read-only items kept Finder refusing deletes until this existed.
- **Reimport** (`mayAlreadyExist`): a folder, or a file with no content on disk, that matches a live child of
  the same name is answered with that row; an unmatched file with no content is refused, which removes the
  empty placeholder; anything else is an ordinary create.
- **Change order.** `enumerateChanges` reports updates before deletes. The other order would delete a child
  that moves out of a folder deleted in the same batch; this one can make the system rename one of two items
  that swap a path in one batch, which loses nothing.

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
- The extension is small and offline: it can be reviewed as "reads a database, writes a request file, copies
  files in the container". It cannot leak a token because it never has one.
- Requests hold an item id and results a version and a class, so no name or path sits in the exchange. The
  file names are random, and only the staged bytes are user content.
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
- **Measured by slice 3 (#235), signed but not notarized:** the Darwin notification and the `flock` both cross
  the sandbox; a 379 KB file opened through the queue in under 2 s; with the app quit an open failed in 0.56 s,
  and it worked after relaunching.
- **A file whose transfer takes longer than the download's 300 s timeout** fails as a transient error, and
  keeps failing on every retry. The bound is inherited from the existing download path.
- **Measured for Paper documents (#235):** the feed lists one like any file, with a size and a
  `content_hash`; its download answers `409 unsupported_file/`; the system shows nothing for the refusal; and
  `https://www.dropbox.com/preview/<path>` opens the document, with the parent folder in lower case, by
  redirecting to its link. A `/home?preview=` URL does not: it lands on the home page.
- **The login item** (`SMAppService`) registers in this app. Starting at a real login has not been observed.
- **Measured by slice 4 (#236), signed but not notarized:** creates, edits, renames, moves and deletes of
  files and folders; a 200 MB upload through a session, with Dropbox's `content_hash` matching the file; a
  stale edit and an edit racing a remote delete both ending as conflicted copies; a delete racing a remote
  edit restored; 50-file selections restored, then confirmed with a single question; Move to Trash on a
  downloaded and on a dataless file, on macOS 26; how the system sends a 300-file selection (32 at once,
  then one per answered call).
- **Not observed:** a modification arriving after the feed has already removed its row (the host answers
  `noSuchItem` and the header says the system then re-creates an edited item); a reimport
  (`mayAlreadyExist`); the order of updates and deletes when one item replaces another at a path; a
  non-recursive folder delete; a package; Put Back from the Trash.
- **Known gaps, left to the delta's echo:** the subtree path rewrite after a folder moved here applies to
  whatever sits under the old path when it runs, and does not recompute selective-sync state; and paths
  this host lowercases itself (the hide check, a delete's tombstone) use Rust's `to_lowercase`, which
  differs from Dropbox's `path_lower` for a few characters.
- **Deep or large folder deletes and renames** (fact 4 above): the prefix fallback exists because the
  listing of every descendant is unproven. The contract text itself comes from the classic `/delta` rules and
  was only checked against the shapes captured in #234.
