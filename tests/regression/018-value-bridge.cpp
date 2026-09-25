// ms2.0: hs::Val <-> Value bridge round-trips every kind including nested
// containers, and http_reason lands interned pool strings.
#include <hs_runtime.hpp>
#include <cstdio>
#include <string>

static int fails = 0;
#define CHECK(cond, msg)                                \
    do {                                                \
        if (!(cond)) {                                  \
            std::fprintf(stderr, "FAIL %s\n", msg);     \
            fails++;                                    \
        }                                               \
    } while (0)

int main() {
    using namespace hs;
    Val src = Val::object(std::vector<std::pair<std::string, Val>>{
        {"s", Val::text("hi")},
        {"n", Val::int_(42)},
        {"f", Val::flt(1.5)},
        {"t", Val::boolean(true)},
        {"nil", Val::nil()},
        {"lst", Val::list(std::vector<Val>{Val::int_(1), Val::int_(2), Val::text("x")})},
    });
    Value v = value_from_val(src);
    CHECK(value_to_json_string(v) == "{\"s\":\"hi\",\"n\":42,\"f\":1.5,\"t\":true,\"nil\":null,\"lst\":[1,2,\"x\"]}", "fwd bridge json");

    Val back = val_from_value(v);
    CHECK(to_json(back) == to_json(src), "reverse bridge json");

    // Bridge survives the freeze boundary (pool strings carry over).
    const_pool().freeze();
    Value pv = Value::obj();
    pv.set("reason", Value::str_view(const_pool().intern("OK")));
    CHECK("{\"reason\":\"OK\"}" == std::string(to_json(val_from_value(pv))), "frozen pool bridge");

    // http_reason now returns interned (stable, dedup) strings.
    std::string r200 = http_reason(200);
    std::string r200b = http_reason(200);
    CHECK(r200 == "OK", "reason 200");
    CHECK(r200 == r200b, "reason stable");
    CHECK(http_reason(404) == "Not Found", "reason 404");
    CHECK(http_reason(999) == "Status", "reason fallback");
    std::printf("ok\n");
    return fails ? 1 : 0;
}