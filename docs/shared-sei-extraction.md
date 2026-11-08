# Shared SEI extraction: investigation and follow-up plan

Status: proposal only; no implementation. Recorded 2026-11-08.

This branch starts from upstream main at `7cc0cbd`. Implementation should wait
until the RecentClips PR merges, then rebase onto the updated upstream main.
The findings below were investigated on `codex/archive-driving-filter` at
`56a0c93` (logging-only follow-up: `1e61345`). Archive-filter symbols mentioned
here belong to that PR and may not yet exist on this branch's base.

## Objective

Read and decode each unchanged front-camera clip once, then share the result
between RecentClips archive selection and drive-map processing. Reduce repeated
video I/O and parsing on Pis without changing either feature's behavior or
making archiving depend on drive mapping being enabled.

## Evidence in the current implementation

- `run/archiveloop` calls `filter-archive-recent-driving` before transfers, and
  calls `run/post-archive-process.sh` after the clip archive stage.
- The post-archive script runs mapping against `/mutable/TeslaCam/RecentClips`
  (local snapshot-backed storage), through `/api/drives/process`. It can be
  disabled with `DRIVE_MAP_ENABLED=false`. Mapping while away can also mean
  drive data exists before an archive cycle; neither ordering is guaranteed.
- `crates/drives/src/processor.rs::process_one_clip` calls
  `extract_gps_from_file`; directory discovery selects front-camera clips.
  RecentClips scanning excludes event aliases, while a separate gap-fill pass
  may process SavedClips/SentryClips. Discovery is not identical to the archive
  candidate list.
- `crates/drives/src/extract.rs::extract_archive_gears` calls the same underlying
  MP4/SEI scanner as GPS extraction, with GPS validation disabled. It builds the
  scanner's arrays but returns only gears. GPS extraction validates coordinates
  and assembles/deduplicates telemetry.
- `crates/drives/src/archive_filter.rs` stores only gear evidence in
  `archive_driving_evidence` in `/backingfiles/drive-data.db`. It checks source
  identity, commits successful scans individually, retries read failures, and
  prunes entries against current archive candidates.
- `crates/drives/src/db.rs::add_route_obj` marks a path processed but writes a
  route only when GPS points exist. The processor can also mark extraction
  failures processed. `processed_files` is therefore not a successful SEI-scan
  cache, and absence of a route does not establish Park or absence of SEI.
- `crates/drives/src/grouper.rs::telemetry_has_driving_core` can accept speed
  evidence as well as gear evidence. It is not the archive's gear-only policy.
- `crates/api/src/clips.rs::clip_telemetry_blocking` scans SEI again for playback,
  then reads video timing. It needs frame-aligned data, not deduplicated routes.

The duplicated scan is established from the call paths. Physical disk reads,
wall time, and savings have not been benchmarked; the OS page cache may already
avoid some physical reads while parsing work is still duplicated.

## Behavior to preserve

- Ordinary RecentClips archiving with driving-only off performs no SEI selection;
  clips without SEI remain eligible under the existing general archive rules.
- Driving-only uses front-camera non-Park gear (D/R/N), including stops, to keep
  all associated camera angles. Park-only and successful no-SEI scans skip that
  recording. GPS validity and speed must not alter this decision.
- Missing/unreadable sources are retryable, not successful negative evidence.
- Mapping keeps its GPS validation, grouping, and event-gap behavior.
- Mapping disabled must not disable driving-only archive selection.
- Snapshot safety, cancellation, USB availability, and interruption recovery
  must remain intact. No reads of an actively changing live camera filesystem
  should be introduced.

## Proposed design

Introduce a shared extraction/cache layer in the drives crate, backed by the
existing database. Either consumer requests extraction for a front-camera
source. A valid cache hit returns the saved result; a miss performs one scan
and saves it for both consumers. Keep route construction after archiving;
there is no need to move full mapping/grouping into the pre-transfer stage.

Separate decoding from consumer policy. Decode raw SEI once, retaining gear
independently of GPS validity. Derive a compact gear summary for archiving and
GPS-valid telemetry for mapping from that same scan. Do not reuse the current
route-exists or telemetry_has_driving result as an archive verdict.

Suggested record content, subject to sizing experiments:

- Canonical clip/source identity and extractor/schema version.
- Successful scan state, including an explicit no-SEI result.
- Gear summary sufficient to distinguish non-Park, Park-only, and no gear.
- Compact/compressed telemetry sufficient to build existing drive routes.
- Optional detailed frame timing only if later extending reuse to playback.

Prefer storing extracted facts rather than a policy-specific `keep` boolean.
Do not treat legacy processed-file rows as evidence of a successful scan.
Existing route blobs have lost GPS-invalid frames and cannot provide complete
negative gear evidence; any legacy reuse needs explicit coverage guarantees.

## Identity, concurrency, and lifecycle decisions

- Normalize logical paths across archive overlays and the mutable index, while
  also tracking the actual source generation. Timestamp names alone are unsafe:
  snapshot links can retarget and files can be replaced or completed.
- The archive cache currently fingerprints link target, size, mtime and ctime
  with nanoseconds. Design a shared identity that works across both aliases;
  blindly copying the path key could prevent all cross-consumer hits.
- Recheck source identity after reading; never persist a result for a source
  that changed mid-scan. Avoid an extra whole-file hash pass just to identify it.
- Coordinate concurrent requests so archive and daemon workers do not scan the
  same source simultaneously. Existing in-process locks do not automatically
  coordinate the CLI and daemon. Use short database transactions; do not hold a
  SQLite write transaction throughout a video scan.
- Commit completed work incrementally for power-loss recovery. Distinguish
  successful empty results, incomplete scans, and failures. Audit truncated MP4
  handling so a partial scan cannot silently become permanent negative evidence.
- Cache invalidation must cover extractor changes, not only file changes.
- Replace archive-candidate-only pruning with a lifecycle serving both consumers.
  Bound detailed payload storage; keep durable route history independent of
  evictable extraction data. Define cleanup for deleted sources and abandoned
  in-progress work. Avoid exporting bulky transient cache payloads unnecessarily.

## Suggested implementation sequence after the PR merges

1. Measure existing scan counts, CPU time, elapsed time, bytes read, database
   writes, and cache size on a Pi for cold and warm runs.
2. Refactor the scanner to produce one result from which GPS-valid routes and
   GPS-independent gear evidence can be derived; prove behavior equivalence.
3. Add the shared store, versioned identity, failure states, and coordination.
4. Migrate archive selection and drive-map to it. Keep mapping optional and
   preserve the existing archive stage ordering.
5. Benchmark and tune compression/retention. Playback reuse is a separate follow-up
   unless frame-aligned storage is demonstrated to be economical.

## Acceptance tests

- Archive first then mapping, and mapping first then archive: one successful
  extraction per unchanged source; assert scanner invocation counts.
- Driving-only on/off, mapping enabled/disabled, D/R/N at zero speed, Park-only,
  no-SEI, invalid/missing GPS with valid gear, and all associated camera angles.
- Missing front camera, unreadable source, partial/truncated source, and recovery
  on the next cycle without caching a transient error as no-SEI.
- Snapshot alias changes, file replacements, mid-scan changes, and parser-version
  upgrades invalidate correctly without stale positive or negative verdicts.
- Concurrent CLI/daemon requests, cancellation, reboot between scans, and cache
  eviction preserve correctness and do not hold long SQLite write locks.
- Route output remains equivalent for GPS, gear runs, events, and timing.
- Compare total local I/O and DB write volume, not only archive-stage duration;
  the optimization should not replace video reads with excessive cache writes.

## Recommendation

Proceed as a separate refactor after the current PR merges. Share extraction
results rather than coupling archiving to drive-map's final route classification.
The immediate target is eliminating the second SEI scan when both consumers
need the same unchanged front clip; quantify the benefit before expanding the
cache into a general playback telemetry store.
