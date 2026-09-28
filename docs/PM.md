# The HardScript package manager

`hard` ships a package manager: manifests, a semver resolver, a lockfile, a
content-addressed cache, a registry client, and signature verification. This
document is what v0.9-alpha actually does, and what it deliberately does not
do.

- [The manifest](#the-manifest)
- [Dependencies](#dependencies)
- [Resolution](#resolution)
- [The lockfile](#the-lockfile)
- [The cache](#the-cache)
- [Registries and mirrors](#registries-and-mirrors)
- [Signatures and the trust store](#signatures-and-the-trust-store)
- [Command reference](#command-reference)
- [Where state lives](#where-state-lives)
- [Environment](#environment)
- [What v0.9 does not do](#what-v09-does-not-do)

## The manifest

`hard.toml` describes one package. The top-level fields identify it; the
tables configure it.

```toml
schema = 1
name = "jwt"
version = "1.2.0"
edition = "2027"
description = "JSON web tokens"
authors = ["ada"]
license = "MIT"

[registry]
default = "https://registry.hardscript.org"

[[registry.mirror]]
url = "https://registry.eu.hardscript.org"
priority = 1

[package]
tags = ["auth", "web"]
keywords = ["jwt", "rfc7519"]
homepage = "https://example.org/jwt"

[dependencies]
base64 = "^0.4.0"
jsonwebtoken = "2"

[dev-dependencies]
mock = "*"

[compiler]
opt = 3
warnings = true
```

Rules worth knowing:

- `name` is lowercase letters, digits, `_` and `-`. It is the identity of the
  package on a registry: the first account to publish a name owns it, and
  another account publishing the same name is refused.
- `version` is semver. `[dependencies]` uses caret semantics by default.
- `[[registry.mirror]]` blocks are ordered by `priority`, lower first, and
  ties keep the order they were written in. A mirror with `disabled = true` is
  kept in the file and never contacted.
- A manifest with no mirrors keeps the older one-line `registry = "url"`
  form, so files written before mirrors existed still round-trip unchanged.
- `[dev-dependencies]` belong to a package's own test suite. A consumer
  installs a package's runtime dependencies, never its dev-dependencies.

## Dependencies

Requirement syntax is the familiar semver set:

| requirement | means |
| --- | --- |
| `1.2.3` or `^1.2.3` | `>=1.2.3 <2.0.0` |
| `=1.2.3` | exactly that version |
| `~1.2.3` | `>=1.2.3 <1.3.0` |
| `~1.2` | `>=1.2.0 <1.3.0` |
| `>=1.0.0 <2.0.0` | that range |
| `^2.0.0 \|\| ^3.0.0` | either range |
| `*` | any release |

Two details that surprise people:

- **`0.x` is not `1.x`.** `^0.2.0` means `>=0.2.0 <0.3.0`, matching what
  cargo does. `^0.0.3` matches only `0.0.3`.
- **A pre-release is only chosen when asked for.** With `*`, a published
  `1.0.0` beats `2.0.0-rc.1`; an explicit `=2.0.0-rc.1` selects it.

## Resolution

`hard install` resolves, downloads, verifies, links, and locks:

1. **Read** `hard.toml`, then the lockfile if there is one.
2. **Index** every requested package from the registry (or the cache, when
   offline). Retracted versions are dropped here: a yanked version is never a
   candidate, which is what makes `hard yank` mean something.
3. **Resolve** with bounded backtracking. If two requirements on one package
   cannot both be satisfied, the resolver backs off its most recent choice
   rather than failing at the first contradiction.
4. **Download** what is missing, in parallel, resuming partial files.
5. **Verify** signatures, before anything is linked.
6. **Link** the sources into `.hard/packages/<name>/`.
7. **Lock** the exact result.

Dependency ordering is topological: a package always precedes the packages
that depend on it. Cycles in runtime dependencies are reported, not resolved.

The resolver prefers locked versions, so adding a dependency does not move
what you already had. `hard update` is the one install that ignores the
lockfile.

Useful flags:

| flag | effect |
| --- | --- |
| `--offline` | resolve and install from the cache; never touch the network |
| `--frozen` | trust the lockfile exactly; do not re-resolve |
| `--verify=<mode>` | `strict`, `warn` (default) or `off` |
| `--parallel <n>` | how many downloads may be in flight |

## The lockfile

`hard.lock` is `hard-lock/v1`, TOML, and byte-for-byte deterministic for the
same inputs: packages are emitted sorted by (source, name, version).

```toml
schema = "hard-lock/v1"
compiler = "0.5.0"
platform = "linux-x86_64 7.1.5"

[package.jwt]
version = "1.10.0"
integrity = "sha256:2915136c..."
signature = "1QBs3CumwcyTRW81phwFg791eok8LlGpFYFoNd35/..."
key_id = "k:4c7e8ea17f12b39d"
fingerprint = "sha256:3ec4acdd7e8174d0..."
verified = true
source = "registry"
```

The last four fields are the trust record. They are **optional**: a lockfile
written before signature support parses unchanged and re-renders identically,
and an older `hard` ignores fields it does not know.

## The cache

`$HARD_HOME/cache` is shared by every project, so a package is downloaded once
per machine:

```
cache/packages/<name>/<name>-<version>.hspkg    the archive, as published
cache/packages/<name>/<name>-<version>.json     its recorded digest
cache/packages/<name>/<name>-<version>.src/     the extracted sources
cache/index/<name>.json                         the registry's metadata
cache/signatures/<name>@<version>.json           the signature, for offline checks
```

Every entry is checked against its recorded digest before use. A corrupt entry
is not installed: it is discarded and fetched again.

```bash
hard cache info                  # what is cached, and how big
hard cache verify                # check every entry
hard cache verify --repair       # delete what fails its digest check
hard cache clean                 # empty the cache
```

`--repair` removes rather than patches. A corrupt archive is worse than a
missing one, and the next install fetches the package again and verifies it
from scratch.

## Registries and mirrors

A registry is an HTTP service. The default comes from `[registry] default`,
then `HARD_REGISTRY`, then the built-in registry.

Every read — metadata, search, signature, download — tries the default and
then each mirror in priority order. Three properties make the fallback honest:

- **A 404 is not a reason to try a mirror.** The registry understood the
  question and answered "no such package"; a mirror would answer the same. Only
  transport failures, 5xx, 408 and 429 fall through.
- **A mirror that refused the connection is skipped for a cooldown**, instead
  of being retried on every request. Failures that are definitive — a refused
  connection, a name that does not resolve — skip the retry backoff entirely,
  because nothing is going to start listening in 200 ms.
- **Provenance is reported.** A search served by a mirror says so.

```bash
hard registry list               # the default and the mirrors
hard registry add <url>          # add a mirror to hard.toml
hard registry remove <url>
hard registry health             # reachability, latency, sequence, test key
hard registry sync               # refresh the cached index from the feeds
hard registry sync --full        # re-list everything instead of the delta
hard registry mirror verify <url># does a mirror match the primary?
```

`hard registry sync` writes the same documents `hard search --offline` and
`hard install --offline` read, so a synced cache is an offline registry.

## Signatures and the trust store

Every publish is signed with Ed25519 over a canonical payload:

```text
hs-signature/1
name jwt
version 1.10.0
integrity sha256:<digest of the .hspkg>
fingerprint sha256:<canonical manifest fingerprint>
```

Deliberately absent: timestamps, download counts, transport headers. Two
mirrors serve the same signature, and re-publishing unchanged metadata does not
change it.

A signature proves *who* signed something, not that you should trust them:

```bash
hard keys add <registry-url>     # record the key that registry signs with
hard keys trust <key-id>         # pin it
hard keys list
hard keys export > keys.toml
```

The store lives at `$HARD_HOME/trusted-keys.toml`. A key can be scoped to one
package (`hard keys add --key <hex> --for jwt`), and adding a scope to a key
that was already general does not narrow it by accident.

Verification modes:

| mode | behaviour |
| --- | --- |
| `--verify=off` | signatures are not looked at |
| `--verify=warn` | default: verify, and report what did not check out |
| `--verify=strict` | refuse to install anything unverifiable, including a package the registry never signed |

Under `strict`, an install that fails writes no lockfile and links nothing. A
misspelled mode exits 2 rather than quietly downgrading to `warn`.

```bash
hard verify jwt@1.10.0           # a package on a registry
hard verify ./jwt-1.10.0.hspkg   # a local archive, against its .sig sidecar
```

`hard doctor` reports the registry's signing key, the payload format, how many
keys are pinned, and warns about test keys, unpinned keys and format
mismatches.

## Command reference

| command | what it does |
| --- | --- |
| `hard add <pkg>[@<req>]` | add a dependency and install it |
| `hard remove <pkg>` | remove a dependency |
| `hard install` | resolve, download, verify, link, lock |
| `hard update [pkg]` | move to the newest matching versions |
| `hard list` | what the lockfile pins |
| `hard outdated` | what has something newer |
| `hard search <text>` | search the registry |
| `hard info <pkg>` | one package's registry metadata |
| `hard download <pkg>[@<ver>]` | fetch an archive |
| `hard publish` | build a `.hspkg` and publish it |
| `hard yank <pkg>[@<ver>]` | retract a published version |
| `hard register` / `login` / `logout` / `whoami` | accounts and sessions |
| `hard token create\|list\|revoke` | personal access tokens |
| `hard verify` | check a signature |
| `hard keys` | the trust store |
| `hard registry` | mirrors, health, sync |
| `hard cache` | inspect, verify, repair, clean |

`hard <command> --help` prints that command's own usage.

## Where state lives

| path | contents |
| --- | --- |
| `$HARD_HOME/credentials.toml` | registry sessions and tokens, mode 600 |
| `$HARD_HOME/trusted-keys.toml` | the trust store |
| `$HARD_HOME/cache/` | archives, sources, index, signatures |
| `$HARD_HOME/mirror-sync.toml` | the last applied change sequence per registry |
| `./hard.lock` | the resolved tree, with trust information |
| `./.hard/packages/<name>/` | the installed sources for this project |

`HARD_HOME` defaults to `~/.hard`.

## Environment

| variable | effect |
| --- | --- |
| `HARD_HOME` | where all state lives |
| `HARD_REGISTRY` | the default registry, when the manifest names none |
| `HARD_REGISTRY_TIMEOUT` | request timeout in ms (default 30000) |
| `HARD_REGISTRY_RETRIES` | retries per request (default 2) |
| `HARD_VERIFY` | `strict`, `warn` or `off`, as if `--verify=` |
| `HARD_TOKEN` | a token to use instead of the stored one |
| `HARD_JOBS` | how many downloads may be in flight |
| `HARD_VERBOSE` | per-package download lines |

An empty `HARD_REGISTRY=` means "unset", not "the empty URL".

## What v0.9 does not do

- **No namespaces.** Names are global per registry, and the first publisher
  owns one.
- **No install scripts.** A package is data; a build script is a compiler
  feature, not a package-manager one.
- **No alternative registries in one lockfile.** One lockfile is resolved
  against one set of registries; two different `[registry]` configurations are
  two different projects as far as resolution is concerned.
- **No Yanked-but-installed tracking.** A version already in your lockfile
  keeps working; yanking stops *new* resolutions from choosing it.
- **No publish-from-a-workspace-member aggregation.** Publish each member from
  its own directory.

## See also

- [REGISTRY.md](REGISTRY.md) — the registry service, its API, and hosting it
- [errors.md](errors.md) — every compiler diagnostic
- [../reports/registry-qa-report.md](../reports/registry-qa-report.md) — the
  QA gate, and what it covered
- [../reports/package-download-performance.md](../reports/package-download-performance.md)
  — measured install and cache behaviour
