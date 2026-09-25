#ifndef HS_RUNTIME_ARENA_HPP
#define HS_RUNTIME_ARENA_HPP
#include <atomic>
#include <cstdint>
#include <cstdlib>
#include <cstring>
#include <string>
#include <string_view>

// Shared counters are declared in the (single) `hs` namespace below.
// NOTE: this header follows the other runtime parts and contributes to the
// open `namespace hs` span opened by hs_runtime_value.hpp and closed by
// hs_runtime_util.hpp; include it through the umbrella to stay correct.

inline std::atomic<size_t> g_arena_alloc_count{0};
inline std::atomic<size_t> g_arena_bytes{0};

class Arena {
public:
    explicit Arena(size_t first_block = 4096) : cap_(first_block) {}
    Arena(const Arena&) = delete;
    Arena& operator=(const Arena&) = delete;
    ~Arena() { std::free(data_); }

    // Allocate `n` bytes aligned to `align` from the bump region.
    void* allocate(size_t n, size_t align = 16) {
        size_t a = align - 1;
        size_t start = (used_ + a) & ~a;
        if (data_ == nullptr || start + n > cap_) {
            size_t want = cap_ ? cap_ : 4096;
            while (want < start + n) want *= 2;
            data_ = (unsigned char*)std::realloc(data_, want);
            cap_ = want;
        }
        void* p = data_ + start;
        used_ = start + n;
        allocs_++;
        g_arena_alloc_count.fetch_add(1, std::memory_order_relaxed);
        g_arena_bytes.store(reserved(), std::memory_order_relaxed);
        return p;
    }

    // Re-usable char buffer that writes into its OWN heap block (plain
    // realloc), never into the arena: the arena reallocs its block on growth,
    // and a Str pointing into it would read freed memory after a grow
    // (use-after-free on large keep-alive bodies — ms1.9 fix). Steady-state
    // keep-alive reuses the grown block (no allocation per request).
    struct Str {
        char* data = nullptr;
        size_t len = 0;
        size_t cap = 0;
        ~Str() { std::free(data); }
        void reset() { len = 0; }
        void ensure(size_t extra) {
            if (len + extra <= cap) return;
            size_t want = cap ? cap : 256;
            while (want < len + extra) want *= 2;
            char* nd = (char*)std::realloc(data, want);
            if (!nd) std::abort();
            data = nd;
            cap = want;
        }
        void append(const void* p, size_t n) { ensure(n); std::memcpy(data + len, p, n); len += n; }
        void append(const char* s) { append(s, std::strlen(s)); }
        void append(const std::string& s) { append(s.data(), s.size()); }
        void append(std::string_view s) { append(s.data(), s.size()); }
        void append(char c) { ensure(1); data[len++] = c; }
    };

    // Rewind the bump pointer so the next request reuses all reserved bytes.
    void reset() {
        used_ = 0;
        allocs_ = 0;
    }
    size_t reserved() const { return cap_; }
    size_t used() const { return used_; }
    size_t allocs() const { return allocs_; }

private:
    unsigned char* data_ = nullptr;
    size_t cap_ = 0;
    size_t used_ = 0;
    size_t allocs_ = 0;
};

// Aggregate runtime.stats as a plain struct for diagnostics/reports.
struct Stats {
    size_t allocations = 0;
    size_t arena_bytes = 0;
};
inline Stats runtime_stats() {
    Stats s;
    s.allocations = g_arena_alloc_count.load(std::memory_order_relaxed);
    s.arena_bytes = g_arena_bytes.load(std::memory_order_relaxed);
    return s;
}

#endif // HS_RUNTIME_ARENA_HPP