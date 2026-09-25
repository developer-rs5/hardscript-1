# Package-manager regression suite (M4.0)

`tests/run-package-regressions.sh` exercises the `hard` package-manager
commands (`add`, `install`, `remove`, `update`, `list`, `outdated`, `cache`,
`workspace`, `search`, `report`) against a local **mock `.hspkg` registry** —
no network, no external tools.

## Running

```sh
tests/run-package-regressions.sh          # uses target/debug/hard
HARD=target/release/hard tests/run-package-regressions.sh   # or a release build
```

Requires `python3` only. The mock registry binds 127.0.0.1 on an ephemeral
port. Exit status is 0 iff every scenario passes.

## Components

| File | Role |
| --- | --- |
| `support/mock_registry.py` | HTTP/1.1 registry: metadata, `.hspkg` archives, `search`; deterministic sha256 integrity; optional per-version `corrupt` flag; optional `--logfile` for request accounting |
| `fixtures/pm-registry.json` | The published package set (versions, deps, source files, a corrupt archive, search hits) |
| `fixtures/pm-registry-late.json` | Same set + a newly published `hello 1.3.0`, used so `outdated` can observe a lock lagging the registry |

## Scenario coverage (35)

- **Resolve / download / lock**: fresh install, lock determinism (no
  re-download via request log), newest-satisfying wins, caret excludes major,
  exact pins, add writes manifest + lock, transitive deps resolved *and* linked,
  transitive reinstall from cache.
- **Errors**: integrity mismatch rejected, unsatisfiable conflict, unknown
  package, dead registry, empty manifest install.
- **Offline / frozen**: cold-cache offline fails, warm-cache offline succeeds,
  frozen reuses the lock, frozen with cold cache fails.
- **Cache**: `HARD_HOME` respected, `cache info`, `cache verify` catches
  corruption, `cache clean` empties the root.
- **Manifest / workspace / registry**: `init` scaffold, `workspace list`,
  `workspace test`, non-workspace error, `search`, `report` output, lockfile
  schema, `[dev-dependencies]`, `remove` + relock, `update` to newest,
  `outdated` against a registry that published a new patch.