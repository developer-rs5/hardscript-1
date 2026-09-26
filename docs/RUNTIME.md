# Cloud runtime (v0.7)

Everything below is in the runtime the compiler links: no external services are
required to run it, and nothing here is a stub. Each section names the surface,
what it guarantees, and the boundary it does not cross.

Every subsystem is configured from the environment, read per operation or per
process as noted, so credentials and endpoints can rotate without a rebuild.

- [Cache](#cache) · [Queue and jobs](#queue-and-jobs) · [Scheduler](#scheduler)
- [Sessions](#sessions) · [Rate limits](#rate-limits) · [Email](#email)
- [Metrics and health](#metrics-and-health) · [Cluster](#cluster-locks-idempotency)

---

## Cache

```hard
cache users ttl 10m

GET "/users/:id" :: {
    key <- "users/" + http.param("id")
    hit <- cache.get(key)
    when hit != nil {
        <- hit
    }
    fresh <- db.find_user(1)
    cache.set(key, fresh, 600)
    <- fresh
}
```

| call | meaning |
|---|---|
| `cache <name> ttl 10m` | default TTL for a name, in seconds; `-1` persists |
| `cache.get(key)` | the value, or nil when missing or expired |
| `cache.set(key, value, ttl?)` | store; the TTL defaults to the declaration |
| `cache.exists(key)` | whether a live value is there |
| `cache.incr(key, step?)` | add to an integer value, creating it at 0 |

Sixty-four mutex-guarded shards, so writes to different keys do not contend.
Expiration is lazy on read and swept in the background on a one-second timer
that a single sweeper thread owns; an entry is never handed out after its TTL
whether or not the sweeper has reached it.

A program that never uses the cache starts no sweeper and allocates no shards:
the declaration and every operation reach the runtime through one entry point,
and that is what flips the flag the metrics endpoint reads.

## Queue and jobs

```hard
job SendWelcome(Text to)

worker SendWelcome {
    email.send(to = to, subject = "Welcome", body = "Glad you are here.")
}

GET "/signup" :: {
    id <- queue SendWelcome("ada@example.com", delay = 60, priority = 5)
    <- { queued: id }
}
```

| call | meaning |
|---|---|
| `job Name(Type arg, ...)` | declare a job type; parameters name the payload |
| `worker Name { ... }` | handle it; the parameter names are the payload fields |
| `queue Name(args, delay = 60, priority = 0, max_attempts = 5)` | enqueue |

At-least-once delivery: a job that fails is retried with exponential backoff
(1s, 2s, 4s, ... capped at a minute) and buried in a dead-letter queue after
`max_attempts`. `queue.depth("Name")` and `queue.dlq("Name")` read the waiting
and buried sets.

The default backend is memory, which is a process restart away from forgetting
its work. The SQL backend (`SqlJobBackend`, over SQLite or PostgreSQL) is what
survives one, and claims atomically so two workers cannot take the same job.

## Scheduler

```hard
every 1h { cleanup() }
every day at "03:00" { report() }
every monday at "09:00" timezone "Europe/Berlin" { digest() }
every startup { warm() }
```

Intervals run on the monotonic clock; `day` and weekday forms run on a wall
clock in UTC by default, in `local` (with real DST) or in a fixed `+HH:MM`
offset. One scheduler thread owns every timer and runs handlers
synchronously, so a scheduled body must be fast -- slow work belongs on the
queue. Sleeps are 100ms slices, so a clock that jumps (an NTP step, a test
clock) is noticed rather than slept through. A job missed while the process
was down fires once when it comes back, then resumes its rhythm without a
backlog.

## Sessions

```hard
GET "/login" :: {
    session.start({ id: 1, email: "ada@example.com" })
    <- "welcome"
}

GET "/me" :: {
    user <- session.user()
    when user == nil {
        <- { error: "not signed in" }
    }
    <- user
}
```

| call | meaning |
|---|---|
| `session.start(user)` | sign in; the cookie is written on the response |
| `session.user()` | the signed-in record, or nil |
| `session.flash("notice", "saved")` | one message, shown once |
| `session.csrf()` | a token for this session |
| `session.csrf_valid(token)` | check a submitted token |
| `session.destroy()` | sign out |

The cookie is HMAC-SHA256 signed with `SESSION_SECRET`, `HttpOnly`,
`SameSite=Lax`, `Secure` outside plain-HTTP development, and it rolls its
expiry as it is used. The payload is signed, not encrypted: put an id in it,
not a secret.

## Rate limits

```hard
limit 100 requests / minute

GET "/api" :: {
    limit 10 requests / second sliding key "api"
    <- "ok"
}
```

A token bucket by default, so bursts are welcome up to the budget; `sliding`
switches to a sliding window, which refuses a burst outright. A top-level
declaration applies to every route as one shared bucket; a declaration inside a
route or `before` block checks inline. `key expr` buckets by the expression
instead of the client IP, which is how you rate limit per account rather than
per address.

Over the limit, the request ends with 429 and a `Retry-After` in seconds,
before the handler and before any auth work -- shedding load is the point.
Buckets live in sixty-four shards with lazy expiry and overflow sweeps that
only shed provably idle buckets, so an attacker minting keys buys memory up to
the cap rather than without bound.

## Email

```hard
email.template("welcome", "Hi {{name}}, your code is {{code}}.")

GET "/resend" :: {
    sent <- email.send(to = "ada@example.com", subject = "Your code",
                       template = "welcome",
                       data = { name: "Ada", code: 4821 },
                       async = true)
    <- { queued: sent }
}
```

| option | meaning |
|---|---|
| `to` | recipient, required |
| `subject` | required |
| `body` | the message, or use `template` + `data` |
| `template`, `data` | render a registered body; a hole with no value is an error |
| `from` | override the envelope sender for this message |
| `async` | hand the send to the queue instead of doing it inline |

Configuration is `SMTP_HOST` (default 127.0.0.1), `SMTP_PORT` (25),
`SMTP_USER`/`SMTP_PASS` for AUTH LOGIN, and `SMTP_FROM`. Delivery is plaintext
SMTP: EHLO, optional AUTH, MAIL, RCPT, DATA, QUIT, with a read deadline on
every step. There is no TLS stack, so a STARTTLS-only server is a boundary to
know about, not a silent downgrade.

An `async = true` send becomes a queue job with the SMTP configuration
captured at enqueue time, so retries and the dead-letter queue apply, and a
bad address is visible in the DLQ rather than in a log line.

## Metrics and health

```hard
GET "/orders" :: {
    metrics.incr("orders_total")
    metrics.observe("order_value", 42)
    <- { ok: true }
}

GET "/ready" :: {
    health.set("redis", db_ok, "connection refused")
    <- health.ready()
}
```

`GET /metrics` serves Prometheus text: counters, gauges and latency histograms
with `# HELP`/`# TYPE` and cumulative buckets. `GET /healthz` answers while the
process is alive; `GET /readyz` answers 503 once it is draining or a gate set
with `health.set` is failing, and names the check that failed. The three
endpoints exist only if a program records a metric somewhere -- codegen
installs them when it sees a `metrics.*` or `health.*` call.

The built-in series cover requests (counted by method, path and status, with
latency), and the cache, queue, limiter and mailer counters -- each reported
only once the program has touched that subsystem, so a scrape never wakes one
it does not use. Distinct label sets are capped, and the overflow is counted
and exposed: one series per request path is how a public metrics endpoint
becomes a memory leak.

## Cluster, locks, idempotency

```hard
GET "/pay" :: {
    // One node owns the key; the others forward to it.
    when !(cluster.is_owner("user:42")) {
        answer <- cluster.call(cluster.owner("user:42"), "/internal/user/42", nil)
        <- answer
    }

    got <- lock.acquire("invoice:1", 30)
    when !got {
        <- { busy: true }
    }
    first <- idem.claim("pay:1", 3600)
    when !first {
        // A retry: answer with what the first attempt recorded.
        <- idem.lookup("pay:1")
    }
    result <- charge()
    idem.record("pay:1", result, 3600)
    lock.release("invoice:1")
    <- result
}
```

| call | meaning |
|---|---|
| `cluster.node()` | this node's id (`CLUSTER_NODE_ID`, else host and pid) |
| `cluster.peers()` | the peers from `CLUSTER_NODES` (`id@host:port`, comma separated) |
| `cluster.owner(key)` | which node owns a key |
| `cluster.is_owner(key)` | whether that is this node |
| `cluster.call(node, path, body)` | POST to a peer, with `CLUSTER_SECRET` |
| `lock.acquire(name, ttl)`, `lock.renew`, `lock.release`, `lock.held` | a lock |
| `idem.claim(key, ttl)`, `idem.record(key, value, ttl)`, `idem.lookup(key)` | exactly-once |

Placement is rendezvous hashing, so adding or removing a node moves about
1/N of the keys and only onto nodes that stay -- the property a modulo ring
does not have. `cluster.call` retries once, because a peer that was
mid-restart is indistinguishable from a lost connection, and carries a shared
secret so the receiving node can tell a cluster call from a stranger.

Locks and idempotency keys are the same idea over a swappable store: memory in
one process, and a database every node can reach when `CLUSTER_STORE` is
`sqlite` or `postgres` with `CLUSTER_DB`. A lock carries a fencing token, so a
holder whose lock expired cannot renew or release the one that replaced it --
without that, a slow request unlocks a lock it no longer holds.

## Configuration

| variable | used by | default |
|---|---|---|
| `SESSION_SECRET` | sessions | required for cookies |
| `SMTP_HOST`, `SMTP_PORT`, `SMTP_USER`, `SMTP_PASS`, `SMTP_FROM` | email | `127.0.0.1`, `25`, unset, unset, `noreply@localhost` |
| `CLUSTER_NODE_ID`, `CLUSTER_NODES`, `CLUSTER_SECRET` | cluster | host and pid, none, none |
| `CLUSTER_STORE`, `CLUSTER_DB` | locks and idempotency | `memory`, none |

## Measured behaviour

`reports/runtime-performance-v0.7.md` is the roll-up, with
`reports/cache-performance.md`, `queue-performance.md` and
`scheduler-performance.md` carrying the per-lane detail. Every number in those
files comes from `qa/bench_runtime.sh`, which fails rather than print a cell it
did not measure.
