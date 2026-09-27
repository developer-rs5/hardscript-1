# Deployment performance, v0.8 (2026-09-28)

Measured with `qa/bench_deploy.sh` at `da3c37d` on 12 CPUs (AMD Ryzen 7 7445HS w/ Radeon 740M
Graphics), 15253 MB RAM, `g++ (Debian 15.3.0-2) 15.3.0`. Median of 5 trials; every time below
is in microseconds, and every cell comes from a run. The script fails rather than printing a
number it did not measure.

The project under test is a small service with a health route, one SQLite migration, one
environment, 40 static files, and a 4 MB server binary -- the shape a real deploy has, and the
size class where the tool's own work is still visible next to everything else.

## What a deploy actually costs

A deploy is a build, a hash, a handful of small commands over one ssh connection, and a health
check that waits. Only one of those is `hard`'s own work. So the table below is mostly the
tool's cost, and the second table is the part that is somebody else's time.

| lane | measured (median) | required | verdict |
|---|---|---|---|
| `startup` -- `hard --version` | 3.71ms | 25ms | met |
| `plan_print` -- build and render a plan | 49.9ms | 100ms | met |
| `plan_print_tls` -- the same, with a unit | 49.1ms | 100ms | met |
| `env_list` / `env_render` / `env_check` | 4.31 / 4.19 / 4.22ms | 25ms | met |
| `config_unit` | 3.86ms | 25ms | met |
| `nginx_render` | 3.78ms | 25ms | met |
| `compose` | 3.81ms | 25ms | met |
| `helper_build` (cached) | 9.62ms | 50ms | met |
| `health_probe` -- one local probe | 9.31ms | 50ms | met |
| `build_warm` -- the binary a deploy uploads | 6.20ms | 50ms | met |

| lane | measured (median) | note |
|---|---|---|
| `build_cold` | 5.09s | the compile `hard` does not do: g++ on the generated translation unit |
| `digest_all` | 280ms / 50 passes = 5.6ms | the release digest, over 42 files and 4 MB |
| `compose_config` (docker) | 86.6ms | `docker compose config` on the generated file |

## Two things the benchmark found

Both were fixed in this milestone, and both were invisible before there was a number for them.

| lane | before | after | what it was |
|---|---|---|---|
| `plan_print` | 3.27s | 22ms | `--print` compiled the migration helper, because the plan asked for the file and the file was built as a side effect of asking. A preview took as long as a deploy. |
| `hard migrate up` | 3.34s | 7ms | the migration helper is a C++ compile that ran on every invocation. It is now keyed on a fingerprint of the helper source and the runtime headers, the same stamp the incremental build uses. |

The first fix needed care rather than a shortcut: the helper's *path* is in the plan whether or
not the binary has been built, because a plan that changes with what happens to be on disk is
not the plan. So the path is always in the plan and the compile happens only for a deploy that
will run it. `qa/run-deploy-tests.sh` checks that a printed plan and an executed plan agree on
the step list, which is what would have caught a shortcut here.

## Where the time goes in a plan

`plan_print` is 49.9ms; startup is 3.7ms and the digest is 5.6ms. The remaining ~40ms is the
front end parsing `main.hard` to decide whether the program has a `/healthz` route to probe --
a health check that requests a route the program does not have reports a healthy service as
broken, so the plan has to know. That is a deliberate trade: 40ms of parse to avoid a deploy
that reports itself as failed, on a step a deploy spends seconds on afterwards.

## What is not measured here, and why

* **Upload, download, and every other network step.** They are the network. A deploy over a
  100 Mbit link moves the 4 MB binary and the assets in well under a second, and a slow link is
  a slow link.
* **`docker pull`, `docker build`, and `docker compose up`.** Measured in
  `reports/runtime-performance.md` and the container notes; they are the same work whether `hard`
  or a shell script asked for it. The one line here is `docker compose config`, which is the
  part `hard` generates input for.
* **systemd's restart and its `Type=simple` bookkeeping.** Sub-millisecond on a warm system, and
  the thing worth measuring is whether the service comes back, which is what
  `qa/run-deploy-tests.sh` and the deploy's own health check are for.

## Reproducing

```sh
cargo build --release
qa/bench_deploy.sh                 # median of 5, prints a TSV
BENCH_TRIALS=9 BENCH_SAMPLES=200 qa/bench_deploy.sh
```

`BENCH_TRIALS` and `BENCH_SAMPLES` change the confidence, not the lanes. The digest lane runs
in-process and repeats `BENCH_SAMPLES` times per trial; every other lane runs the `hard` binary
once per trial, because a process start is part of what is being measured.
