# Package-manager benchmarks (M4.0)

- host: `Linux 7.1.5+kali-amd64 x86_64` (linux)
- binary: `/home/rishabh/Projects/hardscript/target/debug/hard`
- registry: mock `.hspkg` on `127.0.0.1:46749` (21 packages, sha256 integrity)
- machine clock: `date +%s%N` (wall, best of 5)

## Results (ms, lower is better)

| Phase | ms |
| --- | ---: |
| resolve + download (cold cache, 21 packages) | 66 |
| reinstall with unchanged lock (no re-download) | 46 |
| offline reinstall from cache | 12 |
| cache verify (all archives) | 10 |
| search (single metadata round-trip) | 6 |
| hard report (5 markdown reports) | 11 |

Notes: cold resolve exercises resolve + sharded fetch loop over the
mock registry; the metadata search and report numbers include one
server round-trip each. Times are wall-clock best-of-5 in ms.
