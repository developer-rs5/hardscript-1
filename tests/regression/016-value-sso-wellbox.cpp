// ms2.0: SSO boundary (13/14/15-char strings) and deep-WellBox arrays —
// the inline small_ slots absorb up to 4 elements with no heap growth.
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
    std::string s13(13, 'a');
    std::string s14(14, 'b');
    std::string s15(15, 'c');
    CHECK(Value::str(s13).as_str().size() == 13, "sso13");
    CHECK(Value::str(s14).as_str().size() == 14, "sso14");
    CHECK(Value::str(s15).as_str().size() == 15, "long15");
    CHECK(value_to_json_string(Value::str(s13)) == "\"" + s13 + "\"", "sso13 json");
    CHECK(value_to_json_string(Value::str(s14)) == "\"" + s14 + "\"", "sso14 json");
    CHECK(value_to_json_string(Value::str(s15)) == "\"" + s15 + "\"", "long15 json");

    // 4 elements stay in the inline slots; 5 spills to the fallback vector.
    Value a4 = Value::arr();
    for (int i = 0; i < 4; i++) a4.push(Value::i64(i));
    CHECK(value_to_json_string(a4) == "[0,1,2,3]", "arr4");
    Value a5 = Value::arr();
    for (int i = 0; i < 5; i++) a5.push(Value::i64(i));
    CHECK(value_to_json_string(a5) == "[0,1,2,3,4]", "arr5 spill");

    // Copy + move semantics at depth (no double free, no alias).
    Value inner = Value::obj();
    inner.set("k", Value::str("v"));
    Value outer = Value::arr();
    outer.push(std::move(inner));
    outer.push(Value::boolean(true));
    Value copied = outer;
    Value moved = std::move(outer);
    CHECK(value_to_json_string(copied) == "[{\"k\":\"v\"},true]", "copy preserves content");
    CHECK(copied.size() == 2, "copy arr size");
    CHECK(value_to_json_string(moved) == "[{\"k\":\"v\"},true]", "moved json");
    // set() on an array value must not crash (self-heal to object).
    Value wraponly = Value::arr();
    wraponly.set("x", Value::i64(9));
    CHECK(value_to_json_string(wraponly) == "{\"x\":9}", "set heals arr to obj");
    std::printf("ok\n");
    return fails ? 1 : 0;
}