#pragma once
// hs_runtime.hpp — HardScript runtime (umbrella header, C++17).
//
// The runtime is split into focused headers, each of which opens and closes its
// own `namespace hs` and includes the parts it depends on. This umbrella
// includes them in dependency order and is what generated code includes; any
// single part can also be included on its own.
//
//   hs_runtime_value.hpp     Val, JSON, operators, equality
//   hs_runtime_io.hpp        environment, filesystem
//   hs_runtime_crypto.hpp    sha1/sha256/md5/hmac/base64/random, JWT
//   hs_runtime_http.hpp      HTTP/1.1 server, routing, middleware, WebSocket
//   hs_runtime_sched.hpp     test registry/runner
//   hs_runtime_postgres.hpp  PostgreSQL native wire client
//   hs_runtime_validation.hpp  validation engine (M5.1)
//   hs_runtime_auth.hpp        JWT auth, route guard, password hashing (M5.2)
//   hs_runtime_orm.hpp         model metadata, query builder, backend seam (M5.3)
//   hs_runtime_sqlite.hpp      SQLite backend, libsqlite3 loaded at run time
//   hs_runtime_pgsql.hpp       PostgreSQL backend, extended query protocol
//   hs_runtime_util.hpp      CLI/args helpers + codegen glue (hs_respond, tests)
//
// The generated code calls into these helpers (see compiler/src/codegen.rs).

#ifndef HS_RUNTIME_HPP
#define HS_RUNTIME_HPP

#include "hs_runtime_value.hpp"
#include "hs_runtime_io.hpp"
#include "hs_runtime_crypto.hpp"
#include "hs_runtime_arena.hpp"
#include "hs_runtime_http.hpp"
#include "hs_runtime_sched.hpp"
#include "hs_runtime_postgres.hpp"
#include "hs_runtime_validation.hpp"
#include "hs_runtime_auth.hpp"
#include "hs_runtime_orm.hpp"
#include "hs_runtime_sqlite.hpp"
#include "hs_runtime_pgsql.hpp"
#include "hs_runtime_util.hpp"

#endif // HS_RUNTIME_HPP