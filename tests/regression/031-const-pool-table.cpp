// ms2.4: 031-const-pool-table.cpp — the pooled-value set forms an immutable
// compile-time table (snapshot via table_json()); freeze() seals the entries
// and every pre-freeze pointer stays valid and byte-identical forever.
#include <hs_runtime.hpp>
#include <cstdio>
#include <string>

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

    const Value* keep[10] = {};
    keep[0] = const_pool().intern_str("method");
    keep[1] = const_pool().intern_str("path");
    keep[2] = const_pool().intern_i64(200);
    keep[3] = const_pool().intern_i64(204);
    keep[4] = const_pool().intern_f64(0.5);
    keep[5] = const_pool().intern_bool(true);
    keep[6] = const_pool().intern_bool(false);
    keep[7] = const_pool().intern_empty_array();
    keep[8] = const_pool().intern_empty_object();
    keep[9] = const_pool().intern_i64(-7);

    std::string pre = const_pool().table_json();
    std::printf("pool pre-freeze: items=%zu strings=%zu bytes=%zu table=%s\n",
        const_pool().count_items(), const_pool().size(), const_pool().bytes(), pre.c_str());

    const_pool().freeze();

    // Snapshot unchanged, pointers identical to the pre-freeze reads.
    CHECK(const_pool().table_json() == pre, "table identical after freeze");
    CHECK(const_pool().count_items() == 10 && const_pool().size() == 2, "counts unchanged after freeze");
    CHECK(const_pool().intern_str("method") == keep[0], "string pointer stable after freeze");
    CHECK(const_pool().intern_i64(200) == keep[2], "int pointer stable after freeze");
    CHECK(const_pool().intern_f64(0.5) == keep[4], "float pointer stable after freeze");
    CHECK(const_pool().intern_bool(true) == keep[5], "bool pointer stable after freeze");
    CHECK(const_pool().intern_empty_array() == keep[7], "empty array pointer stable after freeze");
    CHECK(const_pool().intern_empty_object() == keep[8], "empty object pointer stable after freeze");

    // Content is fully intact and valid JSON for every pooled kind.
    CHECK(pre == "[\"method\",\"path\",200,204,0.5,true,false,[],{},-7]", "table snapshot exact");
    std::printf("pool post-freeze: table=%s\n", const_pool().table_json().c_str());

    std::printf("ok\n");
    return fails ? 1 : 0;
}