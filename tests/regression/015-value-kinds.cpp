// ms2.0: every ValueKind serializes to the expected JSON. Exercises the
// tagged union, bools, ints, doubles, SSO/short strings, binary bytes and
// the non-owning view path.
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
    {
        hs::Value v = hs::Value::nil();
        CHECK(hs::value_to_json_string(v) == "null", "null");
        CHECK(hs::value_size(v) == 4, "null size");
    }
    {
        hs::Value v = hs::Value::boolean(true);
        CHECK(hs::value_to_json_string(v) == "true", "true");
        CHECK(hs::Value::boolean(false).as_bool() == false, "false");
        CHECK(hs::value_size(hs::Value::boolean(false)) == 5, "false size");
    }
    {
        hs::Value v = hs::Value::i64(-1234567890123LL);
        CHECK(hs::value_to_json_string(v) == "-1234567890123", "i64");
        CHECK(hs::value_size(v) == 14, "i64 size");
    }
    {
        hs::Value v = hs::Value::f64(3.5);
        CHECK(hs::value_to_json_string(v) == "3.5", "f64");
        CHECK(hs::value_size(v) == 3, "f64 size");
    }
    {
        hs::Value v = hs::Value::str("hi");
        CHECK(hs::value_to_json_string(v) == "\"hi\"", "str short");
        CHECK(hs::Value::str("hi").kind() == hs::ValueKind::String, "str kind");
        CHECK(hs::Value::str("hi").as_str() == "hi", "str as_str");
    }
    {
        char raw[2] = { '\x00', '\x01' };
        hs::Value v = hs::Value::bytes(std::string_view(raw, 2));
        CHECK(hs::value_to_json_string(v) == "\"\\u0000\\u0001\"", "bytes escape");
        CHECK(v.kind() == hs::ValueKind::Bytes, "bytes kind");
    }
    std::printf("ok\n");
    return fails ? 1 : 0;
}