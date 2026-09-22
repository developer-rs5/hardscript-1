#ifndef HS_RUNTIME_POSTGRES_HPP
#define HS_RUNTIME_POSTGRES_HPP
#include "hs_runtime_http.hpp"
// ===========================================================================
// PostgreSQL (native wire protocol)
// ===========================================================================
struct Pg {
    int fd = -1;
};

static uint32_t pg_read_int32(int fd) {
    uint32_t v = 0;
    ssize_t got = recv(fd, &v, 4, MSG_WAITALL);
    if (got != 4) throw std::runtime_error("postgres: connection lost");
    return (v >> 24) | ((v >> 8) & 0x0000ff00) | ((v << 8) & 0x00ff0000) | (v << 24);
}
static void pg_write_all(int fd, const std::string& data) {
    send_all(fd, data);
}

inline int pg_connect(std::string conninfo) {
    std::map<std::string, std::string> cfg;
    // parse key=value pairs (postgres-like) or URL
    size_t i = 0;
    std::string host = "127.0.0.1", user = getenv("USER") ? getenv("USER") : "postgres", dbname = user;
    int port = 5432;
    std::string password;
    auto set = [&](const std::string& k, const std::string& v) {
        if (k == "host") host = v;
        else if (k == "port") port = atoi(v.c_str());
        else if (k == "user") user = v;
        else if (k == "password") password = v;
        else if (k == "dbname") dbname = v;
    };
    // crude url parse
    if (conninfo.find("://") != std::string::npos) {
        size_t at = conninfo.rfind('@');
        size_t slash = conninfo.find('/', conninfo.find("://") + 3);
        std::string hostport = at == std::string::npos ? conninfo.substr(conninfo.find("://") + 3, slash == std::string::npos ? std::string::npos : slash - (conninfo.find("://") + 3)) : conninfo.substr(at + 1, slash == std::string::npos ? std::string::npos : slash - (at + 1));
        size_t colon = std::string::npos;
        for (size_t k = 0; k < hostport.size(); k++) if (hostport[k] == ':') { colon = k; break; }
        if (colon != std::string::npos) { host = hostport.substr(0, colon); port = atoi(hostport.substr(colon + 1).c_str()); }
        else host = hostport;
        if (at != std::string::npos) {
            std::string up = conninfo.substr(conninfo.find("://") + 3, at - (conninfo.find("://") + 3));
            size_t uc = up.find(':');
            user = uc == std::string::npos ? up : up.substr(0, uc);
            if (uc != std::string::npos) password = up.substr(uc + 1);
        }
        if (slash != std::string::npos) dbname = conninfo.substr(slash + 1);
    } else {
        std::string rest = conninfo;
        while (!rest.empty()) {
            size_t sp = rest.find(' ');
            std::string tok = rest.substr(0, sp == std::string::npos ? rest.size() : sp);
            size_t eq = tok.find('=');
            if (eq != std::string::npos) set(tok.substr(0, eq), tok.substr(eq + 1));
            if (sp == std::string::npos) break;
            rest = rest.substr(sp + 1);
        }
    }
    (void)cfg;
    Pg c;
    c.fd = socket(AF_INET, SOCK_STREAM, 0);
    struct sockaddr_in a;
    memset(&a, 0, sizeof a);
    a.sin_family = AF_INET;
    a.sin_port = htons((uint16_t)port);
    if (inet_pton(AF_INET, host.c_str(), &a.sin_addr) != 1) {
        close(c.fd);
        throw std::runtime_error("postgres: invalid host " + host);
    }
    if (connect(c.fd, (struct sockaddr*)&a, sizeof a) != 0) {
        close(c.fd);
        throw std::runtime_error("postgres: cannot connect to " + host + ":" + std::to_string(port));
    }
    // startup message
    std::string start;
    start += '\0'; start += '\0'; start += (char)0; start += (char)3; // protocol
    auto addkv = [&](const std::string& k, const std::string& v) {
        start += k; start += '\0';
        start += v; start += '\0';
    };
    addkv("user", user);
    addkv("database", dbname);
    if (!host.empty()) addkv("host", host);
    if (!password.empty()) addkv("password", password);
    start += '\0';
    uint32_t len = (uint32_t)start.size() + 4;
    std::string msg;
    msg += (char)(len >> 24); msg += (char)(len >> 16); msg += (char)(len >> 8); msg += (char)len;
    msg += start;
    pg_write_all(c.fd, msg);
    // auth loop
    for (int iter = 0; iter < 4; iter++) {
        char typ;
        if (recv(c.fd, &typ, 1, MSG_WAITALL) != 1) throw std::runtime_error("postgres: no auth response");
        uint32_t l = pg_read_int32(c.fd);
        std::string payload((size_t)(l - 4), '\0');
        size_t got = 0;
        while (got < payload.size()) {
            ssize_t n = recv(c.fd, &payload[got], payload.size() - got, MSG_WAITALL);
            if (n <= 0) break;
            got += (size_t)n;
        }
        if (typ == 'R') {
            uint32_t code = ((uint8_t)payload[0] << 24) | ((uint8_t)payload[1] << 16) | ((uint8_t)payload[2] << 8) | (uint8_t)payload[3];
            if (code == 0) break; // ok
            if (code == 3) { // cleartext password
                std::string pass = password + '\0';
                std::string pmsg;
                pmsg += 'p';
                pmsg += (char)0; pmsg += (char)0; pmsg += (char)0; pmsg += (char)(pass.size() + 4);
                pmsg += pass;
                pg_write_all(c.fd, pmsg);
                continue;
            }
            if (code == 5) { // md5
                std::string salt = payload.substr(4, 4);
                std::string a = md5_hex(password + user);
                std::string b = md5_hex(a + salt);
                std::string pass = std::string("md5") + b + '\0';
                std::string pmsg;
                pmsg += 'p';
                pmsg += (char)0; pmsg += (char)0; pmsg += (char)0; pmsg += (char)(pass.size() + 4);
                pmsg += pass;
                pg_write_all(c.fd, pmsg);
                continue;
            }
            if (code == 10) {
                throw std::runtime_error("postgres: server requires SASL SCRAM auth, which HardScript does not support yet; configure the server to use md5 or trust. Password: not supported");
            }
            throw std::runtime_error("postgres: unsupported auth method " + std::to_string(code));
        } else if (typ == 'E') {
            // error: trailing status
            std::string msgbody;
            for (size_t k = 2; k + 1 < payload.size(); k += 2) {
                if (payload[k] == 'M' || payload[k] == 'S' || payload[k] == 'C' || payload[k] == 'F' ) {
                    msgbody += payload.substr(k + 1, payload.find('\0'));
                    msgbody += " ";
                }
            }
            throw std::runtime_error("postgres: " + msgbody);
        } else if (typ == 'Z') {
            break;
        }
    }
    printf("HardScript postgres connected\n");
    fflush(stdout);
    return c.fd;
}

// run a query, return rows as list of objects, or affected count
inline Val pg_query(int pfd, const std::string& sql) {
    std::string body = sql + '\0';
    std::string msg;
    msg += 'Q';
    msg += (char)0; msg += (char)0; msg += (char)0; msg += (char)(body.size() + 4);
    msg += body;
    pg_write_all(pfd, msg);
    std::vector<std::string> cols;
    bool cols_done = false;
    std::vector<Val> rows;
    int64_t affected = 0;
    bool have_error_tag = false;
    std::string err;
    for (;;) {
        char typ;
        if (recv(pfd, &typ, 1, MSG_WAITALL) != 1) throw std::runtime_error("postgres: connection lost during query");
        uint32_t l = pg_read_int32(pfd);
        std::string payload((size_t)(l - 4), '\0');
        size_t got = 0;
        while (got < payload.size()) {
            ssize_t n = recv(pfd, &payload[got], payload.size() - got, MSG_WAITALL);
            if (n <= 0) break;
            got += (size_t)n;
        }
        if (typ == 'T') { // row description
            uint16_t ncols = ((uint16_t)payload[0] << 8) | (uint8_t)payload[1];
            cols.clear();
            size_t off = 2;
            for (int k = 0; k < ncols; k++) {
                size_t n = 0;
                while (off + n + 1 <= payload.size() && payload[off + n] != '\0') n++;
                cols.push_back(payload.substr(off, n));
                off += n + 1;
                off += 18; // typid, typlen, typmod, format
            }
            (void)cols_done;
        } else if (typ == 'D') { // data row
            uint16_t nf = ((uint16_t)payload[0] << 8) | (uint8_t)payload[1];
            size_t off = 2;
            std::vector<std::pair<std::string, Val>> row;
            for (int k = 0; k < nf; k++) {
                int32_t flen = (int32_t)(((uint32_t)payload[off] << 24) | ((uint32_t)payload[off + 1] << 16) | ((uint32_t)payload[off + 2] << 8) | (uint8_t)payload[off + 3]);
                off += 4;
                std::string cname = k < (int)cols.size() ? cols[(size_t)k] : ("c" + std::to_string(k));
                if (flen < 0) {
                    row.emplace_back(cname, Val::nil());
                } else {
                    std::string v = payload.substr(off, (size_t)flen);
                    off += (size_t)flen;
                    Val boxed;
                    // try numeric detection: if it parses as int -> int
                    std::string d = v;
                    if (!d.empty()) {
                        bool allnum = true;
                        bool dot = false;
                        for (char cc : d) { if (!(cc >= '0' && cc <= '9')) { if (cc == '.' && !dot) { dot = true; } else { allnum = false; break; } } }
                        if (allnum && !d.empty()) {
                            if (dot) boxed = Val::flt(strtod(d.c_str(), nullptr));
                            else {
                                if (d.size() > 0 && d[0] == '0' && d.size() > 1) boxed = Val::text(d);
                                else boxed = Val::int_(strtoll(d.c_str(), nullptr, 10));
                            }
                        } else {
                            boxed = Val::text(d);
                        }
                    } else {
                        boxed = Val::text("");
                    }
                    row.emplace_back(cname, boxed);
                }
            }
            rows.push_back(Val::object(row));
        } else if (typ == 'C') { // command complete
            std::string tag;
            size_t n = 0;
            while (n < payload.size() && payload[n] != '\0') n++;
            tag = payload.substr(0, n);
            size_t sp = tag.rfind(' ');
            if (sp != std::string::npos) affected = strtoll(tag.substr(sp + 1).c_str(), nullptr, 10);
        } else if (typ == 'E') {
            std::string mb;
            for (size_t k = 1; k + 1 < payload.size(); k += 2) {
                if (payload[k] == 'M') {
                    size_t n = 0;
                    while (k + 1 + n < payload.size() && payload[k + 1 + n] != '\0') n++;
                    mb = payload.substr(k + 1, n);
                    break;
                }
            }
            throw std::runtime_error("postgres error: " + mb);
        } else if (typ == 'Z') {
            break;
        }
    }
    if (!cols.empty() || !rows.empty()) return Val::list(rows);
    return Val::int_(affected);
}

#endif
