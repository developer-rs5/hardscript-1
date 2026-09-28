# The HardScript registry

`hard-registry` is the package registry: a single Rust binary with an HTTP
API, a SQLite store, Ed25519 signing, and a change feed that mirrors can
replicate from. This document covers the API, the semantics behind it, and
what it takes to run one.

- [Running a registry](#running-a-registry)
- [The API](#the-api)
- [Publish](#publish)
- [Download](#download)
- [Search](#search)
- [Authentication](#authentication)
- [Ownership and yanking](#ownership-and-yanking)
- [Signatures](#signatures)
- [Mirrors](#mirrors)
- [Storage](#storage)
- [Limits and error codes](#limits-and-error-codes)
- [Operating notes](#operating-notes)
- [Security notes](#security-notes)

## Running a registry

```bash
hard-registry serve --addr 127.0.0.1:8484 --data ./data
```

| flag | meaning |
| --- | --- |
| `--addr <host:port>` | socket address, without a scheme |
| `--data <dir>` | database, archives and signing key live here |
| `--open` | allow unauthenticated publishes (local development) |
| `--max-archive <bytes>` | reject archives larger than this |

Two more subcommands are for inspection, not serving:

```bash
hard-registry stats --data ./data     # package, version and download counts
hard-registry verify --data ./data    # re-verify stored signatures
```

The signing key is taken from `HARD_REGISTRY_SIGNING_KEY` (a 32-byte seed, hex
or base64) or `data/registry.key`, and is created on first start. **If neither
is set, the built-in test key is used.** `/keys` says so with `"test_key":
true`, and `hard doctor` warns about it. A registry on the test key is
reproducible by anyone, which is fine for a sandbox and wrong for anything
else.

## The API

Everything is JSON unless it is an archive. Errors are
`{"error": {"code": ..., "message": ...}}`.

| method | path | auth | purpose |
| --- | --- | --- | --- |
| GET | `/health` | — | liveness, package count, change sequence, key id |
| GET | `/stats` | — | counts for a mirror or a dashboard |
| GET | `/keys` | — | the signing key and its payload format |
| POST | `/api/publish` | publish | upload a version |
| GET | `/packages/{name}` | read | metadata: versions, tags, downloads, latest |
| GET | `/packages/{name}/{version}/manifest` | read | that version's deps, files, digests |
| GET | `/packages/{name}/{version}/signature` | read | signature, key id, public key, payload |
| GET | `/packages/{name}/{version}/download` | read | the `.hspkg` bytes |
| HEAD | `/packages/{name}/{version}/download` | read | existence and length, no counter |
| GET | `/search` | read | `?q=`, `?tag=`, `?prefix=`, `?limit=`, `?offset=` |
| POST | `/auth/register` | — | create an account |
| POST | `/auth/login` | — | start a session |
| POST | `/auth/logout` | session | end a session |
| GET | `/auth/whoami` | any token | the identity behind a token |
| POST | `/auth/tokens` | token:token | create a personal access token |
| GET | `/auth/tokens` | token:token | list them |
| DELETE | `/auth/tokens/{id}` | token:token | revoke one |
| GET | `/api/mirror/changes` | read | the change feed, `?since=` `?limit=` |
| GET | `/api/mirror/manifest` | read | every version, for a cold mirror |

## Publish

```bash
hard publish                 # build and upload
hard publish --dry-run       # validate and describe, upload nothing
```

The client builds a deterministic `.hspkg`: a tar of the manifest and the
sources, with a recorded file list, the same bytes for the same inputs, and
nothing that does not belong in a package. The lockfile, build output and
source control directories are excluded, as is anything that looks like a
secret.

The server then:

1. **Validates** the name, the semver, the dependency edges and the file list.
2. **Reads the archive.** A malformed archive is a `422` and writes nothing —
   not even a package row.
3. **Checks ownership.** A name belongs to the account that first published it.
4. **Hashes** the bytes, computes the manifest fingerprint, and signs.
5. **Commits** the version and bumps the change sequence.

A dry run stops after step 1 and reports the same digest the real publish will
send, which is what makes it useful in a pre-commit hook.

## Download

`GET /packages/{name}/{version}/download` serves the archive. Range requests
are supported, including open-ended ones (`bytes=1024-`), so an interrupted
download resumes instead of starting over.

`HEAD` on the same path answers with the length and **does not count a
download**. A client asking "does this version exist?" before uploading should
not inflate anybody's statistics, so only a `GET` counts.

`409` means the version exists with different bytes; `404` means it does not
exist at all.

## Search

```
GET /search?q=jwt&tag=web&prefix=hard&limit=20&offset=0
```

Scoring is deliberately simple and explainable, so a result can be reproduced
and a mirror can re-rank identically:

| band | score |
| --- | ---: |
| exact name | 1000 |
| name starts with the term | 500 |
| name contains the term | 250 |
| name matches as a subsequence | 120 |
| tag matches | 60 |
| description contains the term | 30 |
| fuzzy name match (edit distance ≤ 2) | 15 |

Every term must match somewhere, otherwise the package is out. Ties break on
downloads, then name, so the order is total. `total` is the number of matches;
`count` is the size of the page.

A search needs at least one of `q`, `tag` or `prefix`; a request with none is a
`400`, and so is an unparseable or negative `limit`, a negative `offset`, or an
empty `tag`. An empty tag is a filter that cannot mean anything, and silently
dropping it would turn the request into "match everything".

## Authentication

Accounts exist so that a name belongs to somebody.

```bash
hard register --user ada --password <a good one>
hard login --user ada
hard whoami
hard token create --name ci --scope publish
```

Passwords are hashed with PBKDF2-HMAC-SHA256 (100 000 iterations by default) and a random 16-byte
salt per account, and are
never returned by any endpoint. Tokens are opaque strings, stored as hashes, and
shown once. Four scopes exist:

| scope | grants |
| --- | --- |
| `read` | everything a non-authenticated client can already do |
| `publish` | uploading new versions of names you own |
| `yank` | retracting versions of names you own |
| `token` | creating and revoking personal access tokens |

`admin` may publish to any name. A read-only token is refused for a publish
with a `403`, not a `401`: the token was understood, and it is not enough.

`--open` is for local development. It relaxes *publishing* only; token
management is always an authenticated act.

## Ownership and yanking

The first account to publish a name owns it. Later publishes to that name need
either the same account or the `admin` scope; anyone else gets a `409` with an
explanation.

`hard yank <pkg>@<ver>` retracts a version. A yanked version:

- keeps serving for anyone who already pinned it — a lockfile is a promise;
- is never a candidate for a fresh resolution;
- cannot be unpublished and re-published: the version exists.

## Signatures

Every version is signed over a canonical payload:

```text
hs-signature/1
name jwt
version 1.10.0
integrity sha256:<archive digest>
fingerprint sha256:<manifest fingerprint>
```

No timestamps, no counters, no transport metadata. That means two mirrors
serve the same signature, and re-publishing metadata that does not change the
package identity does not change the signature.

`GET /packages/{name}/{version}/signature` returns the signature, the key id,
the public key, the payload, and a server-side `verified` flag.
`GET /keys` returns the registry's current key, so a client can pin it.

The client re-derives the payload from `integrity` and `fingerprint` rather
than trusting the server's copy of it, and checks the signature before it
consults the trust store. A pinned key with a bad signature is a bad
signature, not a pass.

## Mirrors

A mirror replicates with two feeds:

```
GET /api/mirror/changes?since=<seq>&limit=200   # the delta
GET /api/mirror/manifest                        # everything, for a cold start
```

Both report a `seq`, which is the registry's change sequence. A mirror stores
the sequence it has applied and replays from there; the manifest is for
seeding.

A mirror is a full registry process. Point it at a copy of the data directory,
or let a client sync from a primary it can reach — `hard registry sync` writes
the documents `hard search --offline` and `hard install --offline` read, so a
synced cache behaves as an offline registry.

`hard registry mirror verify <url>` compares a mirror with the primary: the
same packages, the same version lists, and every signature it serves verifying
against the local trust store. A mirror serving a different version list is the
failure worth catching, because resolution would then depend on which host
answered.

## Storage

```
data/registry.db      SQLite: packages, versions, accounts, tokens, feeds
data/archives/        the .hspkg bytes
data/registry.key     the signing seed, mode 600
```

SQLite is opened with a connection pool and a busy timeout, because a
concurrent reader must not turn into "database is locked". The schema version
is checked at open; a database from a newer registry is refused rather than
guessed at.

`latest` is computed by semver, in Rust, not by string comparison in SQL: a
package with `1.9.0` and `1.10.0` published must report `1.10.0`.

## Limits and error codes

| status | when |
| --- | --- |
| 400 | a malformed request: bad range, bad count, empty tag, no filter |
| 401 | no token, or an unknown or revoked one |
| 403 | the token is valid but lacks the scope |
| 404 | no such package, version, or route |
| 405 | the path exists but not for that method |
| 409 | a conflict: the name is owned elsewhere, or the version exists |
| 413 | the archive is larger than `--max-archive` |
| 422 | a well-formed request with a malformed archive |
| 429 | rate limited (when a limit is configured) |
| 500 | a store failure; the details are in the log, not the response |

## Operating notes

- **Back up `data/`.** The database and the archives are both required; a
  registry missing its archives has metadata that points at nothing.
- **Keep the signing key.** Without it, existing signatures no longer verify
  against the key you publish, and clients that pinned the old key id will
  refuse everything.
- **Watch `/health`.** It reports the package count and the change sequence,
  which is what a mirror and a load balancer both want.
- **`--open` is not a mode of operation for a public registry.** It exists so
  a local mirror or a test can be brought up in one command.
- **Rotate a key by publishing a new one**, not by editing the file: the key id
  is derived from the public key, and clients pin it.

## Security notes

- Passwords are PBKDF2-HMAC-SHA256, 100 000 iterations by default
  (`security::DEFAULT_ITERATIONS`), with a random 16-byte salt per account.
  Argon2id would be better; the work factor is one constant for when it is.
- Tokens are stored hashed, compared in constant time, and never logged.
- The credentials file the client writes is mode 600.
- Integrity digests are compared in constant time, so a mismatch does not leak
  how many leading bytes were right.
- A signature covers the archive digest *and* the manifest fingerprint, so
  neither a modified tarball nor edited metadata can pass verification.
- The registry has no TLS of its own. Put it behind a reverse proxy that does.

## See also

- [PM.md](PM.md) — the client side
- [../reports/registry-performance.md](../reports/registry-performance.md) —
  measured behaviour
- [../reports/registry-qa-report.md](../reports/registry-qa-report.md) — the
  QA gate
