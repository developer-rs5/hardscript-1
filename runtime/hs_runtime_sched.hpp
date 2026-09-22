#ifndef HS_RUNTIME_SCHED_HPP
#define HS_RUNTIME_SCHED_HPP
#include "hs_runtime_http.hpp"
// ===========================================================================
// Test runner
// ===========================================================================
inline std::vector<std::pair<std::string, std::function<void()>>>& test_registry() {
    static std::vector<std::pair<std::string, std::function<void()>>> r;
    return r;
}
struct TestState {
    int passed = 0;
    int failed = 0;
};
inline TestState& test_state() {
    static TestState s;
    return s;
}
inline void expect(bool cond, const std::string& what, const std::string& where) {
    if (cond) {
        test_state().passed++;
        printf("  ok   %s\n", what.c_str());
    } else {
        test_state().failed++;
        printf("  FAIL %s   [%s]\n", what.c_str(), where.c_str());
        fflush(stdout);
    }
}

inline void register_test(const std::string& name, std::function<void()> fn) {
    test_registry().push_back({ name, std::move(fn) });
}

inline int Server::test_main() {
    test_state() = TestState();
    printf("\nHardScript test run\n");
    printf("==================\n\n");
    for (size_t i = 0; i < test_registry().size(); i++) {
        printf("[%zu/%zu] %s\n", i + 1, test_registry().size(), test_registry()[i].first.c_str());
        try {
            test_registry()[i].second();
        } catch (const std::exception& e) {
            expect(false, std::string("test raised: ") + e.what(), "runtime");
        }
        printf("\n");
    }
    printf("==================\n");
    printf("%d passed, %d failed\n", test_state().passed, test_state().failed);
    return test_state().failed == 0 ? 0 : 1;
}

#endif
