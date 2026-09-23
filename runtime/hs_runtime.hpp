#pragma once
// hs_runtime.hpp — HardScript runtime (umbrella header, C++17).
//
// The runtime is split into focused headers that all live in the same
// `namespace hs`. This umbrella includes them in dependency order and is the
// only header we guarantee you can `#include` (generated code does exactly
// that). Each part is also self-contained: it starts by including the headers
// it needs, so a part can be studied or reused on its own.
//
//   hs_runtime_value.hpp     Val, JSON, operators, equality
//   hs_runtime_io.hpp        environment, filesystem
//   hs_runtime_crypto.hpp    sha1/sha256/md5/hmac/base64/random, JWT
//   hs_runtime_http.hpp      HTTP/1.1 server, routing, middleware, WebSocket
//   hs_runtime_sched.hpp     test registry/runner
//   hs_runtime_postgres.hpp  PostgreSQL native wire client
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
#include "hs_runtime_util.hpp"

#endif // HS_RUNTIME_HPP