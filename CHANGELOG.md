# Changelog

`ncc-registry` — a self-hosted registry node: one binary plus an embeddable Go library.

Formatted after [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), versioned with
[Semantic Versioning](https://semver.org/). "How to use it" is in [`README.md`](README.md);
**this file only answers what changed between releases**.

> This repository used to live inside [`ncc`](https://github.com/fusedmodel/ncc), in the
> `ncc-registry/` directory, sharing one changelog with the CLI. After becoming its own repository it
> **runs its own version line starting at `0.1.0`** — so `0.1.0` records *everything that already
> existed at the moment of the split*, not new features.

---

## [Unreleased]

### Added · Index: hold the platform's copies and match in place

The platform is **authoritative** for the index; a node holds a **copy** — so an isolated intranet
can still find a person from one sentence of need.

- **`POST /api/index`** receives an index pushed by the platform (`id` = platform index id, used as
  `SourceID` for idempotency): re-pushing the same one **updates** rather than duplicating; nested
  `provider` / `source` shapes are accepted and missing fields are inherited from the source.
- **`GET /api/index`** (channel prefix match, keyword, kind), **`GET /api/index/channels`** (channels
  and sizes) and **`GET /api/match`** (local scoring: channel / category / tags / body tokens / region /
  kind — **channel or region alone can never recall** anything).
- **Scores never leave the platform**: a node's ranking has no reputation weight, and the response
  says so plainly rather than dressing up data it does not have.
- Capability `index` (`GET /api/meta`) plus scopes `index:read` / `index:write`.

---

## [0.1.1] — 2026-09-28

### Added · Hosted state: knowledge bases, memory and checkpoints

Three resources that belong to an agent rather than to a package. **They are deliberately not artifact
kinds** — a package is a *capability* (content-addressed, signed, "install and run"), while state is
*data*: rewritten, growing, private by default, with a lifecycle of its own. A package **declares** what
it needs (`state{}`, harness-use rule R11) and this node stores the bytes.

- **`kb` — knowledge base** (`model.KbDoc` + `KbRevision`): namespace-scoped documents
  (`@ns/slug`) with kind / format / summary / tags / `source` (where the knowledge came from) and
  **one revision per write** (history is never rewritten). `checksum` is the `sha256` of the content.
  Search is **keyword-weighted** (title 3 / summary 2 / content 1) — stated plainly in the API, because
  calling it "search" without saying what kind it is would be misleading. Limit: 1 MB per document
  (a knowledge base is a corpus, not a file dump — large binaries belong in artifacts).
- **`mem` — memory** (`model.MemEntry`): key/value with `subject` (whose memory: `self`, a pipeline, …),
  `kind`, `tags`, `source` (trace id / checkpoint ref / manual), `confidence` (per-mille, so no floats),
  `pinned`, `revision` and `expiresAt`. Unique on `(namespace, subject, key)`: rewriting a key **updates**
  it and bumps `revision`. TTL is enforced **at read time** (an expired entry simply does not exist),
  and `gc` removes it for real. **Memory has no public tier by design** — publishing "memory" makes no
  sense, so that tier is absent rather than unimplemented. Limit: 64 KB per value.
- **`ckpt` — checkpoints** (`model.Checkpoint`): immutable snapshots — bytes in blob storage, metadata in
  the database — with `label` (episode / step / run / release / handoff / manual), `step`, `subjectRef`
  + `subjectVersion` (which package version this was taken against), `parent` (lineage, walked with a
  cycle guard) and free-form `meta`. Creation can declare `digest` + `size` up-front or leave both
  empty; the server **recomputes `sha256` on upload and rejects a mismatch**, and a checkpoint that
  already has bytes cannot be overwritten. `prune` keeps the newest N per subject by marking the rest
  `pruned` and deleting only their bytes — metadata stays, so history has no unexplained holes.
- **Visibility**: private by default; `kb` and `ckpt` have an explicit public tier, `mem` does not;
  anonymous callers see public documents only, credentials see *public ∪ mine ∪ granted*; `all=1`
  (admin only) widens to the whole node. Writes always require namespace membership — a grantee can
  read, never write. The store layer is **fail-closed** (`WHERE 1 = 0` without a scope) and handlers
  re-check per row as defence in depth.
- **New grant kind `state`** covering all three (one granularity on purpose: semantically these are
  "my agent's state"; split it only when someone actually needs "knowledge but not memory").
- **New capabilities `kb` / `mem` / `ckpt`**, new scopes `kb:read|write`, `mem:read|write`,
  `ckpt:read|write`, and `/api/meta` reports `kbDocs` / `memEntries` / `checkpoints`.
- **New endpoints**: `/api/kb` (`kinds`, `bundle`, list, `POST`, `GET|PATCH|DELETE <ref>`,
  `<ref>/revisions`), `/api/mem` (`kinds`, `lookup`, list, `PUT`, `DELETE :id`, `gc`),
  `/api/ckpt` (`kinds`, list, `POST`, `PUT :id/blob`, `GET :id`, `:id/bytes`, `:id/lineage`,
  `:id/prune`, `DELETE :id`). Checkpoint bytes are served through a short-lived signed URL (domain-prefixed
  `ckpt:`, so a signature for one resource can never be replayed for another) **or** a credential that can
  read the row.
- **`scripts/state-smoke.sh`** drives the whole thing end to end against an isolated node with an
  isolated `NCC_HOME` (80 assertions): digest verification on download, rejection of a mismatched blob,
  refusal to overwrite uploaded bytes, TTL expiry at read time, `kb pull` driven by a package
  declaration, cross-account 403s, "a grantee can read but never write", and a self-check that a real
  `~/.ncc` was never touched.

### Added · Run traces: capability evaluation and post-training datasets

A trace is what actually ran — an agent session or a HUR execution. The same data answers two questions:
*is this package version any good* (success rate, latency, tokens, cost, human verdicts) and *can I train on
it* (JSONL export with grades, rewards and splits).

- **New document spec `ncc-trace/v1`** (`model/trace.go`): `kind` (`agent` / `hur-run`), `subject`
  (`ref` + `version` — the grouping key for “did the new version get better?”), `steps`,
  `model` / `usage`, `labels`, `tags`, `payload`, `redaction`, `digest`.
- **`payload` is declared by the collector**: `digest` (default: hashes and structure only) / `preview`
  (truncated) / `full` (verbatim). The server records it as-is — it never fills in or downgrades content.
  Validation enforces coherence: a trace that says `digest` while carrying payload text is rejected.
- **Private by default, no “public trace” tier.** Visibility is *my namespaces ∪ people I granted
  `trace` to*; `mine=1` narrows, and even an admin needs `all=1` to widen to the whole node.
- **Immutable document + append-only labels**: the collector computes `digest`, the server recomputes it
  and rejects a mismatch (`trace_invalid`); evaluation labels go to a separate `trace_labels` table so
  labelling never rewrites the fact being judged.
- **Cross-language digest** (`model.TraceDigestCore`) is a length-prefixed concatenation rather than JSON
  serialisation (floats, HTML escaping and key order differ between Go and Rust), so the same trace hashes
  identically on both sides. Tests pin the same `sha256:` vector as the Rust CLI.
- **New endpoints**: `GET /api/traces/kinds`, `POST /api/traces` (idempotent by `(namespace, traceId)`;
  same id + same digest → `duplicates`, same id + different digest → `trace_conflict`),
  `GET /api/traces`, `GET /api/traces/:id`, `POST|GET /api/traces/:id/labels`,
  `GET /api/traces/stats` (success rate, latency percentiles, tokens/cost, per-version breakdown, label
  coverage, grade distribution, failure taxonomy), `GET /api/traces/export` (JSONL + dataset digest,
  `X-NCC-Truncated` when `limit` cut it short), `DELETE /api/traces/:id`.
- **New scopes** `trace:read` / `trace:write` / `trace:label` (`label` deliberately separate: collecting is
  routine for an agent, judging is an evaluation action); **new grant kind** `trace`; the node now declares
  the **`trace` capability** in `/api/meta`, and the capability vocabulary gains a `trace` offer
  (“this node accepts run traces”, aliases `traces` / `telemetry`).
- **Bounded by design**: 2 MB per trace, 2000 steps, 500 per batch, 20000 per export; a truncated export
  says so instead of silently dropping rows.
- Tests: `model/trace_test.go` (digest vector, validation, statistics) and `store/trace_test.go`
  (idempotency, conflict, fail-closed visibility, filters, export truncation, label projection).

### Changed · the `hur` kind label now follows the pinned HUR definition

`HUR` = **Harness-Use Runtime** — that is the **runtime** (defined as the runtime that supports a
harness in LLM calls, tool orchestration, context management, multi-provider access and run
evaluation); an entry with `kind=hur` is the **package it consumes**. The label therefore no longer
calls HUR itself a "package spec", and both sides now return exactly the same string — the literal
value is:

`Harness-Use Runtime 官方包（kind=agent 的包就是一个 Agent）`

(The definition is pinned in the NCC project's design docs.)

### Added · HUR artifacts: upload validation and signature attachment

- **`PUT /api/registry/<ref>/signature`** (requires `registry:publish`): attach or replace the
  signature of an **already published** `kind=hur` artifact. It accepts only the `signature` object,
  never the whole manifest — the artifact bytes have not changed, and accepting a manifest would mean
  letting a caller quietly rewrite the permission surface and the artifact digest, which is
  "swapping the package", not "signing it".
- **Ingest validation (`httpapi/hursign.go`)**: `kind=hur` must now be **self-describing**
  (`manifest.hur` carrying `spec` / `id` / `artifact.sha256`), with two cross-checks:
  - uploaded bytes' `sha256` ≠ `manifest.hur.artifact.sha256` → `digest_mismatch`;
  - `signature.sha256` ≠ the artifact digest → `signature_mismatch`;
  - a missing `keynum`, something that is not Minisign, or a `url` without `sigSha256` → `bad_signature`.
- **Deliberately not done**: no cryptographic verification (that needs the full Minisign machinery and
  a trusted key list, and belongs to the downloader), and **no signing with this node's own key** —
  "who signed it" must be decided by the publisher's own device.
- **Replicas are read-only**: a replica replicated to this node cannot be signed locally
  (`replica_readonly`); signing goes through the origin node.
- The console marks `kind=hur` entries that carry a signature with a "signed (keynum…)" badge (only
  local entries have a manifest; remote entries reported by workers do not, so the badge is absent
  rather than meaning "unsigned").

## [0.1.0] — 2026-09-24

First standalone release: the repository was split out of `ncc`
(`module github.com/fusedmodel/ncc-registry`), publishing both a binary and a `go get`-able library.

### Changed · split into an independent repository and library

- **Module path**: `github.com/fusedmodel/ncc/ncc-registry` → `github.com/fusedmodel/ncc-registry`.
  Under the old path every package lived under `internal/`, so **not a single package could be
  imported from outside the module** — as a library it had never been usable.
- **Five packages were promoted to the top level** as public API: `config` · `model` · `storage` ·
  `store` · `httpapi`; `p2p` and `secretbox` stay in `internal/` (`httpapi` still imports them, which
  is legal inside the module and keeps them out of reach from outside).
- **Added `httpapi.NewServer` and `(*Server).Close`**: `NewRouter` returned only the routes, so the
  background loops it started (worker heartbeat / master sweeping expired workers / the
  hole-punchable entry point) had no way to stop. That is fine for a process that is exiting, but
  creating it repeatedly leaks goroutines. `NewRouter` keeps its signature and delegates to
  `NewServer` internally.
- The repository brought its own CI (gofmt / vet / build / smoke) and Release (binaries for six
  platforms + `checksums.txt` + a GHCR image).

### Fixed · the binary entry point `cmd/ncc-registry` never existed

`README.md`, `deploy/Dockerfile` and `scripts/smoke.sh` all said `go build ./cmd/ncc-registry`, but
**that directory had never been committed** (the initial commit was 22 files, all under `internal/`).
The documented build command had therefore always been broken and the binary could not be built at
all. `cmd/ncc-registry/main.go` now exists (`config.Load` → `store.Open` → `storage.NewLocal` →
`httpapi.NewServer`, with graceful shutdown).

### Fixed · artifact URLs contained backslashes on Windows (`storage.Local`)

`safeName` cleaned object names with `filepath.Clean`, but object names are **slash-separated**
identifiers (they go into URLs and travel verbatim between master and worker). On Windows
`filepath.Clean("/a")` yields `\a`, and `TrimPrefix(clean, "/")` could not strip that backslash, so
`PublicURL` produced an invalid address such as `…/blobs/\a`; putting it in a JSON request body
turned into a 400. Cleaning now uses `path` (slash semantics) and only converts with
`filepath.FromSlash` when writing to disk. Object names containing `\` or `:` are rejected as well.
**Behaviour on macOS / Linux is unchanged.**

### Fixed · three path assertions in the smoke script on Windows

`scripts/smoke.sh` took Git-Bash paths (`/tmp/xxx`) from `NCCR_*` and compared them against the
Windows absolute paths (`C:\Users\…`) echoed by the API, which can never match. That is a
path-display difference in the script, not in the behaviour under test; CI runs on ubuntu and is
unaffected. The suite currently has **167 checks**.

### Capability · artifact hosting and multi-node

- Accounts / namespaces / publish / search / download / replication; `sha256` verification and a
  stable `@namespace/slug` reference.
- **master / worker**: the master is authoritative (accounts, artifacts, node directory); a worker is
  an edge hosting point that hosts artifacts and nodes itself and periodically reports its directory.
  Clients only need one address — the directory they read is aggregated, and on download the master
  proxies the bytes back from whoever holds them.
- **Cluster writes**: **replicate** entries to workers and **revoke** the replicas when removing them.
- Three storage locations can be pointed at separately: `NCCR_DATA_DIR` / `NCCR_BLOB_DIR` /
  `NCCR_DB_PATH` (the common private-deployment request: bytes on NAS, database on local SSD).

### Capability · hosted nodes and agent discovery

- Users **register + heartbeat** the agents and services on their network, declaring *who I am, where
  I am, what I can do*.
- Nodes inside one trust domain discover each other, keep a connection list and aggregate by region;
  `/api/nodes/route` answers "which node should serve this capability".

### Capability · onboarding and authorization (connected ≠ authorized)

- One short link (or key + secret) adds an agent, and what it redeems is a **least-privilege node
  token**.
- Private artifacts / private nodes / non-public config require an explicit `grant`, and revocation
  takes effect immediately (optionally namespace-scoped).

### Capability · configuration hosting

- Team network / infrastructure / agent configuration as a first-class resource: revision history,
  rollback, and per-environment bundle fetch.
- 10 kinds, 7 formats, a 128 KB limit per entry; configs with `secret=true` are **encrypted at rest**
  (AES-256-GCM, key derived from this node's `jwt-secret`, ciphertext prefixed `enc:v1:`) — backing up
  only the database is safe; conversely, moving machines or losing the data directory means those
  values can no longer be decrypted (intentional, not a defect).

### Capability · share links and node governance

- **Sharing**: turn an artifact into a temporary download address, with no account and no CLI needed
  on the receiving end; limited in uses and time, revocable.
- **Governance (admin)**: the first account registered on this node automatically becomes an admin,
  and a machine credential `AK-…` is issued at the same time; it manages users / nodes / services
  (disable, enable, reset passwords, remove, archive), and every action is audited.

### Capability · node-side P2P (hole-punching decision surface)

- `GET /api/p2p/self`: produce a NAT profile and a conclusion on **this machine**;
  `POST /api/p2p/check`: perform a **real probe** (0 bytes, no business data) against a known mapped
  address; `GET|POST /api/p2p/serve`: open an entry point that answers STUN Binding requests only
  (**off** by default; `NCCR_P2P_SERVE=1` starts it with the service).
- **Measured conclusion**: a purely passive responder receives nothing on an
  `address_and_port_dependent` NAT — the hole must be opened by sending first, so the entry point
  performs **reverse punching** by default (one Binding request to the peer every 300 ms).
- The byte layer (the actual transfer) is not connected yet.
