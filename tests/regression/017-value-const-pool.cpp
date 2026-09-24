// ms2.0: ConstPool — interning dedups to a single stable address, strings
// stay valid across freeze(), ids are stable, and pooled views serialize.
#include <hs_runtime.hpp>
#include <cstdio>

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
    const char* a = const_pool().intern("HTTP/1.1");
    const char* b = const_pool().intern("HTTP/1.1");
    const char* c = const_pool().intern("Content-Type");
    CHECK(a == b, "intern dedup returns same address");
    CHECK(a != c, "intern distinct strings");
    CHECK(std::string_view(a) == "HTTP/1.1", "intern content");
    CHECK(const_pool().get(0) == std::string_view("HTTP/1.1"), "get(0)");
    CHECK(std::string_view(const_pool().cstr(1)) == "Content-Type", "cstr(1)");
    CHECK(const_pool().size() == 2, "pool size");

    const_pool().freeze();
    CHECK(std::string_view(a) == "HTTP/1.1", "stable after freeze");
    CHECK(std::string_view(const_pool().cstr(0)) == "HTTP/1.1", "cstr after freeze");
    CHECK(std::string_view(const_pool().cstr(0)) == std::string_view(a), "ids map to interned addr");

    Value pv = const_pool().v_str("Origin");
    CHECK(pv.kind() == Value::Str, "v_str kind");
    CHECK(value_to_json_string(pv) == "\"Origin\"", "v_str json");
    const char* orig = const_pool().cstr(2);
    CHECK(std::string_view(orig) == "Origin", "last interned id");
    std::printf("ok\n");
    return fails ? 1 : 0;
}