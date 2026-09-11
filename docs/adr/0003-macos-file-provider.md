# ADR-0003: macOS placeholders — File Provider vs the `.cloudsc` sidecar

## Status

**Accepted** (2026-09-02, by the maintainer). Supersedes ADR-0002 **on the macOS placeholder
path only**; ADR-0002's Windows CfAPI and hydration-on-demand decisions stand.

Written in English, unlike ADR-0001 and ADR-0002. The repository policy is English for
commits and code comments; the two earlier ADRs predate that being applied consistently.
Flagged rather than decided silently — say so if they should be Spanish instead.

## Context

ADR-0002 chose visible `.cloudsc` sidecar files to represent not-yet-downloaded content on
macOS, where Windows uses native CfAPI placeholders. It never recorded *why* macOS could not
have an equivalent, because nobody had asked. The answer is that it can: macOS has
`NSFileProviderReplicatedExtension` at the `com.apple.fileprovider-nonui` extension point.

The premise cost real work. Of 23 tickets opened in the 30 days to 2026-08-30, **11 were badge,
overlay or placeholder defects** — the maintenance rent on the sidecar model. On 2026-08-19 the
maintainer knowingly accepted throwaway `.cloudsc` badge work rather than block on this
decision; every defect fixed in that subsystem since has been fixed without knowing whether the
subsystem has a future.

DBSYNC-79 was opened to answer that. Its findings follow. Each states how it was verified,
because this ticket produced six false claims — none in code, all in assertions about it — and
the recurring cause was a check that passes identically whether the thing works or not.

## Evidence

**File Provider and Finder Sync are complementary, not alternatives.** Both reference clients
on the maintainer's Mac ship one of each — Dropbox (`DropboxFileProvider.appex` +
`garcon.appex`) and OneDrive. *Verified with `PlistBuddy` on the installed bundles.* The Finder
Sync work from DBSYNC-72 and DBSYNC-76 survives either outcome.

**Our distribution model works.** Both reference providers are signed `Developer ID Application`
with hardened runtime, not App Store. Our own spike appex built, signed as
`Developer ID Application: Manuel Sanchez (XCAA3WMJM6)`, bundled into the app, and the release
workflow's verification passed over it. *Verified by building the app from a clean state and
listing `Contents/PlugIns/`.*

**A read-only File Provider cannot exist.** `createItem`, `modifyItem` and `deleteItem` sit
before `@optional` in the SDK header; the build fails with "does not conform" until all three
are implemented. The system owns the filesystem and pushes local mutations at the provider.
*Verified by the compiler.* This is the single most important finding: adoption is a **rewrite
of the macOS path**, not a backend added beside the existing one.

**Identity is the dominant cost.** `NSFileProviderItemIdentifier` must be stable across renames
and moves. **We capture no identifier of any kind** — the deserialization struct `DropboxEntry`
has exactly six fields (`.tag`, `path_display`, `content_hash`, `rev`, `server_modified`, `size`)
and it is the only parse point for Dropbox metadata. *Verified at the struct.*

> **Unverified premise, flagged.** The plan assumes Dropbox supplies a stable `id:` on every
> entry. **That has never been observed here.** What was checked is that `DropboxEntry` carries
> no `deny_unknown_fields`, so serde silently drops fields it does not declare — which establishes
> only that *if* an id arrives, it is discarded. No recorded response, test fixture or JSON literal
> anywhere under `src/` contains an `id` field. Dropbox's HTTP documentation could not be cited
> either: like Apple's, the page is a JavaScript application and renders as navigation and footer
> only. It is probably true. It is not evidence, and everything below that prices it as cheap
> depends on it.

**Five tables carry identity, not three** *(DBSYNC-96, correcting this ADR's original claim of
"all three index tables")*. `local_file_index`, `remote_file_index` and `known_folders` are the
obvious ones; `sync_conflicts` stores three paths per row and `sync_jobs` addresses its target by
path. Rename a file with an open conflict, or with a job queued against it, and the record points
at nothing.

**A rename already costs a full delete plus a full re-upload — today, on both platforms.**
*Confirmed by test in DBSYNC-96, with each test proven able to fail under two independent
mutations.* The watcher keeps only `e.path`, the schema's CHECK constraint admits no `move` or
`rename` job type, and the index row is dropped rather than moved — so sync state is lost as well
as bytes re-sent. A directory rename costs one delete per tracked descendant plus the folder row.

That last finding changes what this decision is worth: **stable identity is not only File Provider
enablement.** See *Sizing and recommendation* below.

**The platform seam already exists.** `cloud_filter.rs` carries `#![cfg(windows)]`, so the module
compiles to nothing on macOS, and all four call sites of its `placeholders_active` switch are
`#[cfg(windows)]`. `cloudsc_ops.rs` carries 22 `cfg(windows)` plus 2 `cfg(not(windows))`. A macOS
arm cannot alter Windows behaviour. *Verified by reading; an earlier claim to the opposite was a
false inference from a grep that matched `target_os` but not `cfg(windows)`.*

**Adoption does not retire `.cloudsc`.** Per `placeholders_active`'s own doc comment the sidecar
serves three consumers: macOS, Windows without package identity, and Windows where sync-root
registration or the hydration connection failed. Adoption removes the first. The other two
remain, so the 121 production references do not go away. *(Re-derived by DBSYNC-96: 121 exactly,
plus 44 in tests, 29 on Windows-specific paths and 89 generic placeholder references.)*

**The App Group is low-risk but unconfirmed.** Dropbox ships a **notarized** Developer ID File
Provider appex declaring an App Group with **no embedded provisioning profile**, and its group
container exists on this machine; OneDrive's embeds profiles. Both ship. *Verified with `find`,
`codesign`, `stapler validate` and `spctl`.* Our own build has not been notarized, so this is a
shipping counter-example rather than a confirmation. Note that a successful `codesign` proves
nothing here — it embeds any entitlement, including invented ones, without validating them.

**Timing.** The only tag, `v0.1.0-rc1`, is an unpublished draft. There are no users to migrate
today, and there will be after the first real release.

## What adoption buys and costs

**Buys:** cloud-only files appear under their real name instead of `Ascensos.cloudsc`; Finder's
own *Make Available Offline* / *Store Online Only* menu, with no UI to write; badges for
cloud-only files become intrinsic instead of the defect they have always been; and the badge /
overlay / placeholder ticket stream on macOS largely stops.

**Costs:** the macOS sync root moves to `~/Library/CloudStorage/<Provider>` and **stops being
user-chosen** — Windows keeps an arbitrary folder, so the product diverges per platform. A stable
item identity must be introduced through the whole index. The macOS materialization path is
rewritten rather than extended. The App Group migration DBSYNC-72 postponed becomes due.

## Decision

**Adopt File Provider as the direction for the macOS placeholder path — but size the identity
work before writing any of it.**

Two parts, and the order matters:

1. **The direction is settled.** macOS moves from `.cloudsc` sidecars to
   `NSFileProviderReplicatedExtension`. The premise that macOS had no CfAPI equivalent is now
   documented as false, and this file is where that is recorded.
2. **Nothing is built until stable item identity is sized.** `NSFileProviderItemIdentifier` must
   survive renames and moves; **five** tables key on `relative_path`; Dropbox's stable `id:` is
   never captured. That gap reaches the core of the index and is the one thing capable of turning
   this from a rewrite into a rewrite plus a migration. It gets its own ticket, and its answer can
   still send this decision back here for amendment.

   **Done — DBSYNC-96.** The sizing and the recommendation are below; this condition is satisfied.

**The accepted cost:** on macOS the sync root moves to `~/Library/CloudStorage/<Provider>` and
stops being user-chosen. Windows keeps an arbitrary folder, so the product deliberately diverges
per platform. This was weighed and accepted rather than discovered later: the window is cheap
precisely because `v0.1.0-rc1` is an unpublished draft and there are no users to migrate.

**What this decision does not license.** No File Provider extension of ours has ever executed
(see *Not verified*). The first implementation ticket carries the burden of proving the domain
mounts and the App Group is authorized at runtime — not this ADR, and not the deleted spike.

## Sizing and recommendation (DBSYNC-96)

### What the identity change costs

| Work | Size |
| --- | --- |
| Capture Dropbox's `id:` — one field on `DropboxEntry` | trivial **if** the id is in the response — see the unverified premise above; a different plan if it is not |
| `dropbox_id` column on five tables + migration | small; the `add_column_if_missing` pattern exists |
| 9 identifier-addressed storage methods beside the 9 path-addressed ones | small; 9 of `db.rs`'s 47 |
| 14 single-parameter engine call sites | medium |
| **6 sites where identity lives in a collection** | **the structural cost** |
| A local identity space for items Dropbox has never seen | medium, and design-shaped |
| Back-fill | small; `seed_remote_delta_cursor` already re-snapshots, and there are no users |

**The collections are the real number.** `remote_by_path: &HashMap<String, RemoteFileMeta>` is the
whole remote index keyed by path; converting it is a key-type change that propagates to every
consumer and every iteration, and it outweighs the fourteen single-parameter sites combined.

**This is addition, not replacement**, for two independent reasons: `remove_remote_subtree` and
the `path_util.rs` shape predicates — ignore globs, editor temp files, include/exclude prefixes,
traversal safety — cannot be expressed by an identifier and stay path-based permanently, on both
platforms. An identifier cannot answer "does this match `*.tmp`".

**The local identity space was not in the original framing.** `local_file_index` and
`remote_file_index` are separate tables; a file created locally has no remote row until its upload
succeeds, and that window is unbounded if uploads keep failing. File Provider will ask about those
items. So identifiers cannot be a thin mirror of Dropbox's id.

### Recommendation: proceed

Three reasons, in order of weight:

1. **The value case is no longer macOS-only.** The rename defect is live on Windows too, confirmed
   by test. This work stops being a platform port and becomes a defect fix that File Provider
   happens to require.
2. **The storage cost is smaller than this ADR feared** — 9 of 47 methods, and the path-addressed
   API survives intact beside the new one.
3. **The window is cheap and closing.** No published release means no back-fill problem; that ends
   at the first publish.

### The amendment threshold, and an honest note about it

**Amend this ADR if Dropbox's `id:` turns out to be absent or unstable** — for files or folders.
`known_folders` needs identity as much as files do, and folder metadata is a different response
shape. Nothing else found so far is capable of flipping the direction: the collection work is heavy
but bounded, and the local identity space is design work rather than a migration.

**Note the awkwardness:** this threshold and the unverified premise above are the same fact. The
decision therefore rests its escape hatch on the one question it did not answer. That is tolerable
only because answering it is cheap — one API call — and because the first implementation ticket
cannot get far without doing so.

**That threshold was written after the numbers were known, not before.** DBSYNC-96's plan required
the opposite, and the same agent produced both the findings and the threshold, so there was no
moment at which it could be set blind. Recorded rather than dressed up — a reader should discount
it accordingly.

Everything this ADR does not know is listed in one place: **Not verified**, at the end.

## Consequences

**ADR-0002 is superseded on the macOS placeholder path only.** Its Windows CfAPI and
hydration-on-demand decisions stand untouched.

**The interim `.cloudsc` badge work is sunset.** Defects in the macOS sidecar badge path attract
no further investment. Of the 23 tickets opened in the 30 days to 2026-08-30, 11 were in that
subsystem; that spend stops being justified the moment this decision is recorded, which is the
whole reason DBSYNC-79 was worth doing before the next one.

**`.cloudsc` is not deleted, and the code is not retired.** It still serves Windows without
package identity and Windows where sync-root registration or the hydration connection failed.
The 121 production references stay; what changes is that macOS stops being one of its consumers.

**Finder Sync stays**, on both platforms, unchanged. So does everything DBSYNC-72 and DBSYNC-76
built.

**The folder relocation copies nothing.** The macOS sync root moves to the new domain and the
content re-downloads from Dropbox; there is no migration code, no copy step and no data to keep
in two places at once. Decided deliberately rather than by omission — a re-download costs
bandwidth once, while a copy-and-verify path would be permanent code guarding a one-off event.
It also exercises the first-run install path for real, which nothing else currently does.

**Follow-up work**, in dependency order: stable item identity (blocking, and capable of sending
this decision back for amendment); then the macOS provider arm, the App Group migration, and the
folder relocation with its onboarding consequences.

## Not verified

**The single place this ADR records what it does not know.** There were briefly two such sections,
which is how a premise came to be asserted in one part of this document and doubted in another;
merged deliberately, so that finding one list means finding the whole list.

### The identity premise — the one that could amend this decision

**Whether Dropbox returns an `id:` at all**, for files or folders, and whether it survives rename
and move. What was checked is only that `DropboxEntry` carries no `deny_unknown_fields`, so serde
discards fields it does not declare: *if* an id arrives, it is dropped. Nothing under `src/` — no
recorded response, fixture or JSON literal — contains an `id` field. Neither Dropbox's nor Apple's
HTTP documentation could be cited: both render as JavaScript applications and yield navigation and
footer only.

**This is the same fact as the amendment threshold.** The decision rests its escape hatch on its
one unanswered question. Tolerable only because answering it costs one API call, and because the
first implementation ticket cannot get far without doing so. **Answer it first in DBSYNC-95.**

### Apple's identifier stability requirement

Whether identifiers must survive provider restarts and domain re-registration. The SDK header
states no requirement; its only guidance is that identifiers *"should not contain sensitive
information, as it may be recorded in system logs"* — which independently rules out path-derived
identifiers, since they would leak user paths into the unified log. Mitigated but unanswered: ids
must be persisted either way.

### The extension has never run

The spike was never registered as a domain: **no File Provider extension of ours has executed.**
Untested are the domain mounting under `~/Library/CloudStorage/`, notarization of a build carrying
the App Group entitlement, and the group container at runtime. Registration requires
`NSFileProviderManager.add(domain:)` from the host app — feasible, since `objc2` is already a
dependency and `finder_extension.rs` uses `msg_send!`, but not written.
