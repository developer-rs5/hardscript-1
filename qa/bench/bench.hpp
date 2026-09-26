// bench.hpp -- shared timing and metrics for the ORM benchmarks
//
// Every bench program prints `RESULT <workload> <backend> <ops> <seconds>`
// lines; qa/bench_orm.sh turns those into the report tables. Nothing here is
// clever: a monotonic clock, getrusage for peak RSS, and mallinfo2 for heap
// bytes held (glibc only, guarded).
#ifndef HS_BENCH_HPP
#define HS_BENCH_HPP

#include <cstdio>
#include <ctime>
#include <string>
#include <sys/resource.h>

#ifdef __GLIBC__
#include <malloc.h>
#endif

namespace bench {

inline double now() {
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return t.tv_sec + t.tv_nsec / 1e9;
}

struct Timer {
    const char* workload;
    const char* backend;
    long long ops = 0;
    double start = now();
    Timer(const char* w, const char* b) : workload(w), backend(b) {}
    void add(long long n) { ops += n; }
    // One result line, printed exactly once per workload.
    void done() {
        double dt = now() - start;
        if (dt <= 0) dt = 1e-9;
        printf("RESULT %s %s %lld %.6f\n", workload, backend, ops, dt);
        fflush(stdout);
    }
};

/// Peak resident set, in kilobytes, for the whole process so far.
inline long long peak_rss_kb() {
    struct rusage u;
    if (getrusage(RUSAGE_SELF, &u) != 0) return -1;
    return (long long)u.ru_maxrss;
}

/// Heap bytes currently held from the OS (glibc). A coarse number: it moves
/// with everything the process allocated, which in a bench program is the
/// workload and nothing else.
inline long long heap_held_bytes() {
#ifdef __GLIBC__
    return (long long)mallinfo2().uordblks;
#else
    return -1;
#endif
}

inline void metrics(const char* backend) {
    printf("RSS %s %lld\n", backend, peak_rss_kb());
    printf("HEAP %s %lld\n", backend, heap_held_bytes());
    fflush(stdout);
}

}  // namespace bench

#endif  // HS_BENCH_HPP
