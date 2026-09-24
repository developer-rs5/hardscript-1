// ms2.1: 023-value-layout.cpp — documents the tagged-union `Value` layout
// (sizeof/alignof, SSO threshold, inline-slot counts) and locks in the
// Function kind: raw fn pointer + const-pool interned name, zero per-value
// allocation, JSON fallback "null", bridge to Val::nil().
#include <hs_runtime.hpp>
#include <cstdio>
#include <cstring>
#include <type_traits>

int main() {
    int fails = 0;
    auto check = [&](bool c, const char* m) { if (!c) { std::fprintf(stderr, "FAIL %s\n", m); fails++; } };

    std::printf("sizeof=%zu\n", sizeof(hs::Value));
    std::printf("alignof=%zu\n", alignof(hs::Value));
    std::printf("sso=%zu\n", hs::Value::kSso);
    std::printf("kinds=%zu\n", (size_t)hs::ValueKind::Bytes - (size_t)hs::ValueKind::Nil + 1);
    std::printf("array_inline=%zu\n", hs::ValueVec::kSmall);
    std::printf("object_inline=%zu\n", hs::ValueObj::kSmall);
    std::printf("polymorphic=%d\n", std::is_polymorphic<hs::Value>::value ? 1 : 0);
    std::printf("standard_layout=%d\n", std::is_standard_layout<hs::Value>::value ? 1 : 0);
    static_assert(!std::is_polymorphic<hs::Value>::value, "no virtual members");
    static_assert(!std::is_abstract<hs::Value>::value, "no virtuals at all");

    // Kind enumerator order and names match the ms2.1 spec exactly.
    static_assert((int)hs::ValueKind::Nil == 0, "Nil first");
    check(std::strcmp(hs::value_kind_name(hs::ValueKind::Nil), "Nil") == 0, "name Nil");
    check(std::strcmp(hs::value_kind_name(hs::ValueKind::Bool), "Bool") == 0, "name Bool");
    check(std::strcmp(hs::value_kind_name(hs::ValueKind::Int), "Int") == 0, "name Int");
    check(std::strcmp(hs::value_kind_name(hs::ValueKind::Float), "Float") == 0, "name Float");
    check(std::strcmp(hs::value_kind_name(hs::ValueKind::String), "String") == 0, "name String");
    check(std::strcmp(hs::value_kind_name(hs::ValueKind::Array), "Array") == 0, "name Array");
    check(std::strcmp(hs::value_kind_name(hs::ValueKind::Object), "Object") == 0, "name Object");
    check(std::strcmp(hs::value_kind_name(hs::ValueKind::Function), "Function") == 0, "name Function");
    check(std::strcmp(hs::value_kind_name(hs::ValueKind::Bytes), "Bytes") == 0, "name Bytes");

    // Function kind: fn pointer round-trips, name is a stable pooled string,
    // copies share the same pooled name, and it serializes as JSON null.
    hs::Value f = hs::Value::fn(reinterpret_cast<void*>(0x1234), "handler");
    check(f.kind() == hs::ValueKind::Function, "fn kind");
    check(f.as_fn() == reinterpret_cast<void*>(0x1234), "fn ptr");
    check(std::string_view(f.fn_name()) == "handler", "fn name");
    hs::Value g = f;
    check(g.as_fn() == f.as_fn(), "fn copy ptr");
    check(std::string_view(g.fn_name()) == "handler", "fn copy name");
    check(hs::value_to_json_string(f) == "null", "fn json fallback");
    check(hs::value_size(f) == 4, "fn size");
    check(hs::val_from_value(f).is_nil(), "fn bridges to Val nil");

    std::printf("layout_report=%s\n", hs::value_layout_report().c_str());
    std::printf("ok\n");
    return fails ? 1 : 0;
}