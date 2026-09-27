# Deployment QA report, v0.8 (2026-09-28)

What `hard deploy` is tested by, how much of it is tested, and what is not --
which is the part that usually goes unwritten. Every number here is a count from
a run at `d97e2a7`; nothing is estimated.

## The suites

| suite | what it covers | size |
|---|---|---|
| `cargo test --workspace` | 9 binaries, all of it | 555 tests |
| `qa/run-deploy-tests.sh` | the deploy tool, end to end without a network | 50 checks |
| `qa/cli_matrix` | the CLI surface, exit code per case | 127 lanes |
| `tests/run-regressions.sh` | 24 repros, 10 incremental scenarios, and the deploy suite | 58 cases |
| `tests/integration.sh` | the built server over real HTTP | 21 cases |
| `qa/run-runtime-tests.sh` | the C++ runtime, 33 fixture suites | 33 suites |
| `qa/run-framework-tests.sh` | front end, codegen, formatter, ORM, ASan/UBSan/LSan | 40 gates |
| `qa/bench_deploy.sh` | the tool's own share of a deploy | 15 lanes |

`qa/run-deploy-tests.sh` is now part of `tests/run-regressions.sh`, so a
deployment regression fails the standard gate rather than living in a suite
somebody has to remember to run.

## Where the deployment tests actually are

**The plan, not the network.** Every remote command `hard` sends is a string
produced by a pure function, so the tests assert on the string and the executor
is tested once against a `MockSsh` that records every call in order. That covers
the ordering contract -- upload, configure, migrate, activate, restart, verify
-- without a host.

**The shell, for real.** A mock cannot tell you that `{ ...; }` needs a `;`
before the next command, or that `ln -sfn` on a symlink to a directory creates
the new link *inside* the old one. So the suite takes the commands the tool
prints and runs them with `sh` against a scratch root: the prepare step, the
activation, the env-file heredoc, the secret check, the unit install. It then
asserts on the filesystem: `current` points at the new release, `previous` at the
one before it, no temporary symlink is left, and no `releases/<old>/releases/`
was created by a link that followed its target.

**nginx, for real.** `nginx` is installed on the QA machine, so
`hard deploy nginx --check` builds a wrapper configuration and runs `nginx -t`
on it. That found two template bugs that no string assertion would have: a
`proxy_pass` with a doubled brace and a `server_name` with no semicolon. Where
nginx is not installed, a structural check runs instead and says so.

**systemd, where it is.** `systemd-analyze verify` parses the generated unit
when the tool is available; the same install command is then run against a
scratch `/etc/systemd/system` and the result is compared byte for byte with what
the generator produced.

**The failure paths, which are the point.** A rollback whose health check fails
puts the release that was live back and restarts it. A deploy whose upload
fails never touches the service. A missing secret fails before the restart and
names the variable. A host that cannot be reached is reported once, at the
bottom, instead of in every field of a status report. A prune never removes
`current` or `previous`, which is asserted on a real directory after the
generated `rm` script has run.

**The documentation.** Every subcommand and flag in `docs/DEPLOY.md` is checked
against the tool's own help and source, so the guide cannot drift into naming
something that does not exist.

## What is not covered, and why

* **A real host.** No suite in this repository has ever run a deploy against a
  real server over a real SSH connection. `SystemSsh` shells out to `ssh` and
  `scp` with `BatchMode=yes` and a connect timeout, and every command it builds
  has been run by a shell -- but "the command works" and "the deploy works" are
  different claims, and only the first is automated here. The first deploy at a
  new host should be `--print`.
* **`certbot`.** Not installed on the QA machine. The generated command is
  asserted on as a string -- the webroot, the address, the names, the
  non-interactive flags -- and `hard deploy https` only runs it when a host is
  named, so no test can spend a Let's Encrypt rate limit by accident.
* **Multi-host deploys.** One deploy goes to one host. A second host on the
  command line is a usage error, because a deploy that silently picks one of two
  targets is worse than a refusal.
* **Upload throughput.** It is the network, and `reports/deployment-performance.md`
  says so rather than measuring it and calling it a result.
* **The extension and the LSP.** They do not touch `hard deploy`; their suites
  (`qa/run-extension-tests.sh`, `qa/run-lsp-tests.sh`) are unchanged in v0.8.

## Findings this milestone fixed

Every one of these was found by a test, not by a use:

| found by | what it was |
|---|---|
| `nginx -t` | a `proxy_pass` with a doubled brace and a `server_name` with no semicolon |
| `sh -e` in the deploy suite | a `{ ...; }` with no `;` before the next command in the secret check |
| `sh -e` in the deploy suite | a `grep` pattern quoted inside a quoted pattern, matching a literal apostrophe |
| `systemd-analyze verify` | a unit with a directive that is not allowed where it was written |
| the benchmark | `--print` compiling the migration helper: 3.27s for a preview |
| the benchmark | the migration helper being compiled on every `hard migrate up`: 3.34s against 7ms |
| the doc check | a guide naming subcommands the tool did not have |

## Reproducing

```sh
cargo build --release
cargo test --workspace          # 555
qa/run-deploy-tests.sh          # 50
qa/cli_matrix/run_matrix.py     # 127
tests/run-regressions.sh        # 58, including the deploy suite
tests/integration.sh            # 21
qa/run-runtime-tests.sh         # 33
qa/run-framework-tests.sh       # 40
```
