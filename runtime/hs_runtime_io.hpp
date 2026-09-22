#ifndef HS_RUNTIME_IO_HPP
#define HS_RUNTIME_IO_HPP
#include "hs_runtime_value.hpp"
// ===========================================================================
// fs / env / time / runtime modules
// ===========================================================================
inline std::string env_get(const std::string& k) {
    const char* v = std::getenv(k.c_str());
    return v ? std::string(v) : "";
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

#endif
