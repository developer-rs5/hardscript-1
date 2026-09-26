// Shared scaffolding for the M6 runtime fixtures in this directory.
//
// Self-asserting like qa/orm/test_support.hpp: each check passes silently or
// the program exits non-zero with file, line, and both texts.
#ifndef HS_QA_RUNTIME_SUPPORT_HPP
#define HS_QA_RUNTIME_SUPPORT_HPP

#include <cstdio>
#include <string>
#include <vector>

namespace qa {

inline int g_checks = 0;
inline int g_failed = 0;

inline void fail(const char* file, int line, const std::string& what, const std::string& got,
                 const std::string& want) {
    g_failed++;
    std::fprintf(stderr, "%s:%d: %s\n  got:  %s\n  want: %s\n", file, line, what.c_str(),
                 got.c_str(), want.c_str());
}

#define CHECK(qa_cond_, qa_what_)                                          \
    do {                                                                   \
        qa::g_checks++;                                                    \
        if (!(qa_cond_))                                                   \
            qa::fail(__FILE__, __LINE__, (qa_what_), #qa_cond_, "true");    \
    } while (0)

#define CHECK_EQ(qa_got_, qa_want_, qa_what_)                                          \
    do {                                                                              \
        qa::g_checks++;                                                                \
        auto g_ = (qa_got_);                                                           \
        auto w_ = (qa_want_);                                                          \
        if (!(g_ == w_))                                                               \
            qa::fail(__FILE__, __LINE__, (qa_what_), qa::show(g_), qa::show(w_));      \
    } while (0)

inline std::string show(const std::string& s) { return "\"" + s + "\""; }
inline std::string show(const char* s) { return std::string("\"") + s + "\""; }
inline std::string show(bool b) { return b ? "true" : "false"; }
inline std::string show(int64_t v) { return std::to_string(v); }
inline std::string show(uint64_t v) { return std::to_string(v); }
inline std::string show(int v) { return std::to_string(v); }
inline std::string show(double v) {
    char b[40];
    snprintf(b, sizeof b, "%.17g", v);
    return b;
}

inline int report(const char* name) {
    if (g_failed) {
        std::fprintf(stderr, "%s: %d of %d checks failed\n", name, g_failed, g_checks);
        return 1;
    }
    std::printf("%s: %d checks passed\n", name, g_checks);
    return 0;
}

}  // namespace qa

#endif  // HS_QA_RUNTIME_SUPPORT_HPP
