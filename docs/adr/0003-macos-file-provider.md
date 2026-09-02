# ADR-0003: macOS placeholders — File Provider vs the `.cloudsc` sidecar

## Status

Proposed. Amends ADR-0002, which chose the `.cloudsc` sidecar model.

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
and moves. All three index tables key on `relative_path`, so a rename destroys identity by
construction. Dropbox supplies a stable `id:` on every entry and **we never capture it** — the
deserialization struct `DropboxEntry` has exactly six fields (`.tag`, `path_display`,
`content_hash`, `rev`, `server_modified`, `size`) and it is the only parse point for Dropbox
metadata. *Verified at the struct.* Closing this means a new column, a migration, and revisiting
every reconciler that joins on path.

**The platform seam already exists.** `cloud_filter.rs` carries `#![cfg(windows)]`, so the module
compiles to nothing on macOS, and all four call sites of its `placeholders_active` switch are
`#[cfg(windows)]`. `cloudsc_ops.rs` carries 22 `cfg(windows)` plus 2 `cfg(not(windows))`. A macOS
arm cannot alter Windows behaviour. *Verified by reading; an earlier claim to the opposite was a
false inference from a grep that matched `target_os` but not `cfg(windows)`.*

**Adoption does not retire `.cloudsc`.** Per `placeholders_active`'s own doc comment the sidecar
serves three consumers: macOS, Windows without package identity, and Windows where sync-root
registration or the hydration connection failed. Adoption removes the first. The other two
remain, so the 121 production references do not go away.

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

**To be made by the maintainer.** The evidence above is what DBSYNC-79 was opened to produce; the
call is a product decision, not a technical one, because it changes where a user's files live.

The recommendation from the evidence is **adopt, but not yet**: the direction is right and the
window is cheap, while the identity gap is large enough that it should be sized as its own ticket
before committing. Rejecting is also defensible if the user-chosen folder is non-negotiable — in
which case that reason belongs here, so the option is not rediscovered a third time.

## Consequences

**If adopted:** ADR-0002 becomes superseded on the macOS path only; its Windows and hydration
decisions stand. Follow-up tickets are needed for stable item identity, the macOS provider arm,
the App Group migration, and the folder relocation. The interim `.cloudsc` badge work is sunset
and should attract no further investment.

**If rejected:** ADR-0002 stands, reaffirmed with the reason it lacked. The `.cloudsc` badge
defects continue to be worth fixing, and this file records why the alternative was declined.

**Either way:** `.cloudsc` is not deleted, Finder Sync stays, and Windows is unaffected.

## Not verified

Stated plainly so no reader assumes otherwise. The spike was never registered as a domain and
never ran: **no File Provider extension of ours has executed.** Specifically untested are the
domain mounting under `~/Library/CloudStorage/`, notarization of a build carrying the App Group
entitlement, and the group container at runtime. Registration requires
`NSFileProviderManager.add(domain:)` from the host app — feasible, since `objc2` is already a
dependency and `finder_extension.rs` uses `msg_send!`, but not written.
