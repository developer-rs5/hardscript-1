# Deploying a HardScript service

This is the whole of `hard deploy`: what it puts on a host, in what order, and
what to do when it is not working. It assumes a Linux host with systemd and an
account that can `ssh` in without a password prompt. Containers are a separate
story (`hard build --docker`, `hard deploy compose`), and this is not it.

## The first deploy

```sh
hard deploy ssh deploy@app.example.com
```

That builds the project, uploads it, runs the migrations, restarts the service,
and waits until the new release answers a request. It needs a service on the
other side already, because a deploy does not install a service manager's
configuration unless you ask:

```sh
hard deploy config unit                 # print the unit
hard deploy ssh deploy@app.example.com --unit   # and install it
```

The first deploy is the only one that needs a human to read the output. After
that, the sequence is the same and the tool is the thing to read.

## What ends up on the host

```
/srv/app/
  releases/
    0.3.0-20250927T141530Z-3f9a1c2d/     one immutable directory per deploy
      server                            the binary
      hard.toml                         the manifest it was built from
      migrate-helper                    the same migration code, compiled here
  current -> releases/0.3.0-...         what the unit runs
  previous -> releases/0.2.0-...        the one before it
  shared/                               survives every deploy
    env                                 written by the deploy
    env.secrets                         written by you, read by the deploy
    migrations/                         your migration files
    static/                             your static files
    acme/                               the certificate webroot
    tls/                                the certificate
    <app>.db                            a SQLite file, if you use one
```

Nothing is overwritten in place. That is the whole design: a deploy adds a
directory and moves a symlink, so going back to the previous release is one
`ln`, and a deploy that fails halfway leaves the running release exactly where
it was.

A release is named `<version>-<UTC>-<digest>`, where the digest covers the
binary, the manifest, the migrations, and the assets. Two deploys of the same
build have the same digest, so a name tells you which bytes went out.

## Environments

Where the differences between hosts live:

```toml
# hard.toml
[env.production]
host = "deploy@app.example.com"
dir = "/srv/app"
port = 8080
health_path = "/live"
secrets = ["DATABASE_URL", "SESSION_KEY"]

[env.production.vars]
RUST_LOG = "info"
```

```sh
hard deploy env list                    # what exists
hard deploy env show production         # what it says, secrets by name only
hard deploy env check production        # what a deploy would trip over
hard deploy ssh --env production        # deploy as one, host included
```

A flag still beats the environment, the environment beats the manifest, and the
manifest beats the directory name. A flag is somebody saying "this time, this
host".

### Secrets

A secret's *value* is deliberately not in `hard.toml`. The environment names
its secrets, which is enough to tell you what a host is missing and enough for a
deploy to fail with a sentence naming the variable. The values go in
`shared/env.secrets` on the host, which the deploy reads and never writes:

```sh
ssh deploy@app.example.com 'mkdir -p /srv/app/shared && umask 077 && cat > /srv/app/shared/env.secrets' <<'EOF'
DATABASE_URL=postgresql://app@db/app
SESSION_KEY=...
EOF
```

The deploy writes `shared/env` -- the declared variables, plus
`HS_ENVIRONMENT` and `HS_RELEASE`, so a running service can be asked what it is.
A missing secret fails the deploy *before* the restart, not as a service that
will not boot.

## Everyday commands

```sh
hard deploy status --env production     # what is live, what is running, what is kept
hard deploy releases --env production   # the releases on the host, newest first
hard deploy logs --env production -f    # the journal, following
hard deploy logs --env production -n 500 --since "1 hour ago"
hard deploy restart --env production
hard deploy stop --env production
```

`status` is healthy only when a release is live *and* the unit is active. A
running service with no release behind it is executing something nobody can
name, and a release with a stopped service is a deploy that did not take;
either one reported as fine is how a status command stops being believed.

## When it does not work

`hard deploy config check <host>` asks the six questions that explain almost
every deployment failure, and prints the host's own words for the ones that
fail:

| check | what it means when it fails |
|---|---|
| unit installed | systemd cannot start a unit whose file is not there |
| runs the current release | an `ExecStart` in a release directory serves that release forever |
| enabled at boot | active, and gone after a reboot |
| running | the cheapest true answer about the process |
| a release is live | `current` is not pointing at a release |
| the previous release is kept | there is no rollback |

`hard deploy logs` is the next command, not `journalctl` with a unit name you
remember.

## Rolling back

```sh
hard deploy rollback --env production            # says what it would do
hard deploy rollback --env production --yes      # does it
hard deploy rollback --env production --to 0.2.0 --yes
```

The swap keeps the release it replaced as `previous`, so a rollback is itself
reversible. If the release you rolled back to does not come up, the tool puts
the one that was live back, restarts it, and says so: a rollback that leaves
the service down is worse than the deploy that prompted it.

## Old releases

```sh
hard deploy prune --env production --keep 5         # what it would remove
hard deploy prune --env production --keep 5 --yes   # remove it
```

It keeps the newest `--keep N`, and never removes `current` or `previous`.
Nothing is deleted without `--yes`; the default is a list.

## TLS and a hostname

```sh
hard deploy nginx --env production --tls    # the server block
hard deploy nginx --env production --check  # ask nginx whether it parses
hard deploy nginx --env production --tls --install deploy@app.example.com
hard deploy https --env production          # the certbot command
hard deploy https --env production --run deploy@app.example.com
```

The block proxies to `127.0.0.1:<port>`, serves `shared/static` itself, keeps
WebSocket upgrades alive, and serves the ACME challenge on port 80 *before* it
redirects to HTTPS -- the certificate is issued against that webroot with
`--webroot`, so a renew works on a live server. `--install` runs `nginx -t`
before the reload, because a reload with a bad file takes the working sites
down too.

`hard deploy https` prints the command and only runs it when a host is named:
certbot talks to the outside world, and running it by surprise is how a rate
limit gets spent.

## Before you press enter

```sh
hard deploy ssh --env production --print
```

That is the exact list of remote commands and uploads a deploy would run, in
order, with nothing hidden in the tool. Read it once per project and you will
not be surprised by a deploy.
