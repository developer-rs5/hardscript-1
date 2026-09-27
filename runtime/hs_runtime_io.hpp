#ifndef HS_RUNTIME_IO_HPP
#define HS_RUNTIME_IO_HPP
#include "hs_runtime_value.hpp"
#include <cstdio>
#include <cstdlib>
namespace hs {

// ===========================================================================
// fs / env / time / runtime modules
// ===========================================================================
inline std::string env_get(const std::string& k) {
    const char* v = std::getenv(k.c_str());
    return v ? std::string(v) : "";
}
/// `MSG_NOSIGNAL` where it exists, zero elsewhere. A server that hangs up
/// mid-message must fail the send, not the process: without this a client of
/// ours dies of SIGPIPE when a peer disconnects early, which is exactly what
/// a restarting peer does.
inline int hs_send_flags() {
#ifdef MSG_NOSIGNAL
    return MSG_NOSIGNAL;
#else
    return 0;
#endif
}

/// Log a warning when `cond` holds. Generated code needs a way to say
/// something without failing, and a helper keeps that out of every `main`.
inline void log_warn_if(bool cond, const char* message) {
    if (cond) fprintf(stderr, "hard: warning: %s\n", message);
}

/// The port the server should listen on: the one in the source unless `PORT`
/// says otherwise.
///
/// The env override is what makes a compiled binary deployable -- a container
/// image, a systemd unit and a load balancer all need to set the port
/// without a rebuild. A value that is not a port is a warning rather than a
/// failure: a typo in an environment variable should not stop a server from
/// answering on the port it was built with.
inline int port_from_env(int fallback) {
    std::string v = env_get("PORT");
    if (v.empty()) return fallback;
    char* end = nullptr;
    long parsed = strtol(v.c_str(), &end, 10);
    bool clean = end != v.c_str() && end != nullptr && *end == '\0' && parsed > 0 && parsed <= 65535;
    if (!clean) {
        fprintf(stderr, "hard: warning: PORT=\"%s\" is not a port, using %d\n", v.c_str(), fallback);
        return fallback;
    }
    return (int)parsed;
}

inline int64_t unix_ms() {
    return (int64_t)std::chrono::duration_cast<std::chrono::milliseconds>(
        std::chrono::system_clock::now().time_since_epoch()).count();
}
inline std::string time_iso() {
    time_t t = time(nullptr);
    struct tm g;
    gmtime_r(&t, &g);
    char b[40];
    std::strftime(b, sizeof b, "%Y-%m-%dT%H:%M:%SZ", &g);
    return b;
}
inline void sleep_sec(double s) {
    std::this_thread::sleep_for(std::chrono::duration<double>(s));
}

inline std::string fs_read(const std::string& path) {
    std::ifstream f(path, std::ios::binary);
    if (!f) throw std::runtime_error("cannot read file: " + path);
    std::ostringstream ss;
    ss << f.rdbuf();
    return ss.str();
}
inline void fs_write(const std::string& path, const std::string& data) {
    std::ofstream f(path, std::ios::binary | std::ios::trunc);
    if (!f) throw std::runtime_error("cannot write file: " + path);
    f.write(data.data(), (std::streamsize)data.size());
}
inline void fs_append(const std::string& path, const std::string& data) {
    std::ofstream f(path, std::ios::binary | std::ios::app);
    if (!f) throw std::runtime_error("cannot append to file: " + path);
    f.write(data.data(), (std::streamsize)data.size());
}
inline bool fs_exists(const std::string& path) {
    struct stat st;
    return stat(path.c_str(), &st) == 0;
}
inline bool fs_is_dir(const std::string& path) {
    struct stat st;
    return stat(path.c_str(), &st) == 0 && S_ISDIR(st.st_mode);
}
inline std::vector<std::string> fs_list(const std::string& path) {
    std::vector<std::string> out;
    DIR* d = opendir(path.c_str());
    if (!d) throw std::runtime_error("cannot list directory: " + path);
    struct dirent* e;
    while ((e = readdir(d))) {
        std::string n = e->d_name;
        if (n != "." && n != "..") out.push_back(n);
    }
    closedir(d);
    std::sort(out.begin(), out.end());
    return out;
}
inline void fs_remove(const std::string& path) {
    if (::remove(path.c_str()) != 0) throw std::runtime_error("cannot remove: " + path);
}
inline int64_t fs_size(const std::string& path) {
    struct stat st;
    if (stat(path.c_str(), &st) != 0) throw std::runtime_error("cannot stat: " + path);
    return (int64_t)st.st_size;
}

} // namespace hs

#endif
