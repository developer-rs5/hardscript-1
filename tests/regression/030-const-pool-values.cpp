// ms2.4: 030-const-pool-values.cpp — the constant pool dedups every poolable
// kind (strings, ints, doubles, booleans, empty array, empty object), and
// `intern_value` routes by value kind into those single instances. Non-empty
// containers are deep-copied without dedup and never share a slot.
#include <hs_runtime.hpp>
#include <cstdio>

static int fails = 0;
#define CHECK(cond, msg)                            \
    do {                                            \
        if (!(cond)) {                              \
            std::fprintf(stderr, "FAIL %s\n", msg); \
            fails++;                                \
        }                                           \
    } while (0)

int main() {
    using namespace hs;

    // Strings: dedup to one stable address.
    const Value* s1 = const_pool().intern_str("hello");
    const Value* s2 = const_pool().intern_str("hello");
    const Value* s3 = const_pool().intern_str("world");
    CHECK(s1 == s2, "string dedup");
    CHECK(s1 != s3, "distinct strings distinct slots");
    CHECK(s1->as_str() == "hello", "string content");

    // Legacy char* path stays NUL-terminated and stable.
    const char* c1 = const_pool().intern("hello");
    const char* c2 = const_pool().intern("world");
    CHECK(std::string_view(c1) == "hello", "legacy intern content");
    CHECK(c1 == s1->as_str().data(), "legacy char* == pooled buffer");

    // Numbers: dedup by value; int and float never collide despite same printout.
    const Value* i1 = const_pool().intern_i64(42);
    const Value* i2 = const_pool().intern_i64(42);
    const Value* i3 = const_pool().intern_i64(43);
    CHECK(i1 == i2, "int dedup");
    CHECK(i1 != i3, "distinct ints");
    CHECK(i1->as_i64() == 42, "int content");
    const Value* f1 = const_pool().intern_f64(2.5);
    const Value* f2 = const_pool().intern_f64(2.5);
    CHECK(f1 == f2, "float dedup");
    CHECK(f1 != i1, "int vs float are distinct slots");
    CHECK(f1->as_f64() == 2.5, "float content");

    // Booleans: two slots total, stable.
    const Value* t1 = const_pool().intern_bool(true);
    const Value* t2 = const_pool().intern_bool(true);
    const Value* fb = const_pool().intern_bool(false);
    CHECK(t1 == t2, "bool dedup");
    CHECK(t1 != fb, "true vs false distinct");
    CHECK(t1->as_bool() && !fb->as_bool(), "bool contents");

    // Empty containers are singletons.
    const Value* e1 = const_pool().intern_empty_array();
    const Value* e2 = const_pool().intern_empty_array();
    const Value* o1 = const_pool().intern_empty_object();
    const Value* o2 = const_pool().intern_empty_object();
    CHECK(e1 == e2, "empty array singleton");
    CHECK(o1 == o2, "empty object singleton");
    CHECK(e1->kind() == ValueKind::Array && e1->size() == 0, "empty array shape");
    CHECK(o1->kind() == ValueKind::Object && o1->size() == 0, "empty object shape");

    // intern_value dispatches onto the same pooled instances.
    CHECK(const_pool().intern_value(Value::i64(42)) == i1, "intern_value int -> pooled int");
    CHECK(const_pool().intern_value(Value::boolean(true)) == t1, "intern_value bool -> pooled bool");
    CHECK(const_pool().intern_value(Value::str("hello")) == s1, "intern_value string -> pooled string");
    CHECK(const_pool().intern_value(Value::f64(2.5)) == f1, "intern_value double -> pooled double");
    CHECK(const_pool().intern_value(Value::arr()) == e1, "intern_value empty array -> singleton");
    CHECK(const_pool().intern_value(Value::obj()) == o1, "intern_value empty object -> singleton");

    // Non-empty containers deep-copy: a fresh slot each time, content preserved.
    Value a = Value::arr(); a.push(Value::i64(1)); a.push(Value::i64(2));
    const Value* na1 = const_pool().intern_value(a);
    const Value* na2 = const_pool().intern_value(a);
    CHECK(na1 != na2, "non-empty containers not deduped");
    CHECK(na1->size() == 2 && na1->values()[1].as_i64() == 2, "non-empty copy content");

    // Accounting: count_items covers all kinds; size() is the legacy string count.
    CHECK(const_pool().size() == 2, "legacy size = 2 strings");
    CHECK(const_pool().count_items() == 11, "11 pooled values");
    CHECK(const_pool().bytes() == 37 + 37 + 160 + 288 + 160 + 160, "pooled payload bytes");
    CHECK(const_pool().table_json() == "[\"hello\",\"world\",42,43,2.5,true,false,[],{},[1,2],[1,2]]",
        "table snapshot JSON");

    std::printf("ok\n");
    return fails ? 1 : 0;
}