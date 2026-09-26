// hs_runtime_email.hpp -- email / notification runtime (M6.6)
//
// `email.send(to="a@example.com", subject="Welcome", body="...")` delivers
// one plain-text message through SMTP, or enqueues it when `async=true`.
// Templates render `{{var}}` placeholders from `data`; anything missing is
// an error rather than a silently blank email.
//
// Delivery is synchronous plaintext SMTP (EHLO, optional AUTH LOGIN, MAIL,
// RCPT, DATA, QUIT) against the configured host: `SMTP_HOST` (default
// 127.0.0.1), `SMTP_PORT` (default 25), `SMTP_USER`/`SMTP_PASS` (optional),
// `SMTP_FROM` (default noreply@localhost). No TLS stack lives here, so
// STARTTLS-only servers are a documented boundary, not a silent fallback.
// Async sends become `__email_send` jobs on the queue with the SMTP config
// captured at enqueue time: retries, backoff and the dead-letter queue come
// along, and a failed address is visible in the DLQ instead of a log line.

#ifndef HS_RUNTIME_EMAIL_HPP
#define HS_RUNTIME_EMAIL_HPP

#include "hs_runtime_crypto.hpp"
#include "hs_runtime_io.hpp"
#include "hs_runtime_queue.hpp"
#include "hs_runtime_value.hpp"

#include <atomic>
#include <chrono>
#include <cstring>
#include <cerrno>
#include <map>
#include <mutex>
#include <string>
#include <thread>
#include <vector>

#ifdef _WIN32
#include <winsock2.h>
#else
#include <arpa/inet.h>
#include <netdb.h>
#include <netinet/in.h>
#include <sys/socket.h>
#include <unistd.h>
#endif

namespace hs {

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

struct SmtpConfig {
    std::string host;
    int port = 25;
    std::string user;
    std::string pass;
    std::string from;
};

/// Read SMTP config from the environment at each send: tests re-point it per
/// case without restarting anything, and operators rotate credentials without
/// redeploying.
inline SmtpConfig smtp_config() {
    SmtpConfig c;
    c.host = env_get("SMTP_HOST");
    if (c.host.empty()) c.host = "127.0.0.1";
    std::string port = env_get("SMTP_PORT");
    c.port = port.empty() ? 25 : atoi(port.c_str());
    c.user = env_get("SMTP_USER");
    c.pass = env_get("SMTP_PASS");
    c.from = env_get("SMTP_FROM");
    if (c.from.empty()) c.from = "noreply@localhost";
    return c;
}

/// A light address check: something, one at sign, something with a dot. The
/// server has the final word; this catches typos, and header injection, before
/// a round trip.
inline bool email_looks_valid(const std::string& addr) {
    if (addr.find('@') != addr.rfind('@')) return false;  // exactly one
    size_t at = addr.find('@');
    if (at == std::string::npos || at == 0) return false;
    std::string domain = addr.substr(at + 1);
    if (domain.empty() || domain.find('.') == std::string::npos) return false;
    if (domain.front() == '.' || domain.back() == '.') return false;
    // A space or newline in an address either is a typo or splits a header.
    for (char c : addr)
        if (c == ' ' || c == '\n' || c == '\r' || c == '\t') return false;
    return true;
}

// ---------------------------------------------------------------------------
// Templates: named bodies with {{var}} holes
// ---------------------------------------------------------------------------

inline std::map<std::string, std::string>& email_templates() {
    static std::map<std::string, std::string> t;
    return t;
}

inline std::mutex& email_templates_mu() {
    static std::mutex m;
    return m;
}

inline void email_register_template(const std::string& name, const std::string& content) {
    if (name.empty()) throw std::runtime_error("email: template needs a name");
    std::lock_guard<std::mutex> lock(email_templates_mu());
    email_templates()[name] = content;
}

/// Render `{{var}}` holes from `data`. A hole with no value is an error:
/// blank emails are how typos ship to customers.
inline std::string email_render(const std::string& name, const std::string& content, const Val& data) {
    std::string out;
    size_t i = 0;
    while (i < content.size()) {
        size_t open = content.find("{{", i);
        if (open == std::string::npos) {
            out += content.substr(i);
            break;
        }
        out += content.substr(i, open - i);
        size_t close = content.find("}}", open + 2);
        if (close == std::string::npos)
            throw std::runtime_error("email: template `" + name + "` has an unclosed `{{`");
        std::string key = content.substr(open + 2, close - open - 2);
        // Trim spaces: `{{ name }}` reads better and means the same.
        size_t a = 0, b = key.size();
        while (a < b && isspace((unsigned char)key[a])) a++;
        while (b > a && isspace((unsigned char)key[b - 1])) b--;
        key = key.substr(a, b - a);
        if (key.empty())
            throw std::runtime_error("email: template `" + name + "` has an empty `{{}}`");
        const Val* v = data.is_obj() ? data.find(key) : nullptr;
        if (!v)
            throw std::runtime_error("email: template `" + name + "` needs `" + key +
                                     "`, which `data` does not have");
        out += v->is_str() ? v->sv : to_json(*v);
        i = close + 2;
    }
    return out;
}

// ---------------------------------------------------------------------------
// SMTP delivery
// ---------------------------------------------------------------------------

/// One TCP connection with a read deadline: every protocol step below waits
/// at most this long, so a hung server fails the send instead of the thread.
constexpr int SMTP_TIMEOUT_MS = 10000;

class SmtpConn {
  public:
    SmtpConn(const std::string& host, int port, int timeout_ms = SMTP_TIMEOUT_MS) : timeout_ms_(timeout_ms) {
        fd_ = socket(AF_INET, SOCK_STREAM, 0);
        if (fd_ < 0) throw std::runtime_error("email: cannot create a socket");
        struct sockaddr_in addr;
        memset(&addr, 0, sizeof addr);
        addr.sin_family = AF_INET;
        addr.sin_port = htons((uint16_t)port);
        // A dotted address connects directly; anything else resolves first.
        if (inet_pton(AF_INET, host.c_str(), &addr.sin_addr) != 1) {
            struct hostent* he = gethostbyname(host.c_str());
            if (!he || !he->h_addr_list || !he->h_addr_list[0]) {
                close_fd();
                throw std::runtime_error("email: cannot resolve `" + host + "`");
            }
            memcpy(&addr.sin_addr, he->h_addr_list[0], sizeof addr.sin_addr);
        }
        if (::connect(fd_, (struct sockaddr*)&addr, sizeof addr) != 0) {
            int e = errno;
            close_fd();
            throw std::runtime_error("email: cannot connect to `" + host + ":" + std::to_string(port) +
                                     "` (" + strerror(e) + ")");
        }
    }

    ~SmtpConn() { close_fd(); }
    SmtpConn(const SmtpConn&) = delete;
    SmtpConn& operator=(const SmtpConn&) = delete;

    /// Read one reply line; multi-line replies (250-...) collapse to their
    /// code, which is all the client checks.
    int greeting() { return read_code(); }

    void cmd(const std::string& line, int want) {
        std::string out = line + "\r\n";
        size_t sent = 0;
        while (sent < out.size()) {
            ssize_t n = ::send(fd_, out.data() + sent, out.size() - sent, 0);
            if (n <= 0) throw std::runtime_error("email: connection lost writing `" + line + "`");
            sent += (size_t)n;
        }
        int got = read_code();
        if (got != want) {
            throw std::runtime_error("email: `" + line + "` got reply " + std::to_string(got) +
                                     ", wanted " + std::to_string(want) + " (" + last_line_ + ")");
        }
    }

    void cmd_data(const std::string& message) {
        cmd("DATA", 354);
        // Dot-stuffing: a line starting with `.` would end the message early.
        std::string out;
        size_t i = 0;
        bool at_line_start = true;
        while (i < message.size()) {
            size_t eol = message.find('\n', i);
            std::string line =
                eol == std::string::npos ? message.substr(i) : message.substr(i, eol - i);
            if (!line.empty() && line.back() == '\r') line.pop_back();
            if (at_line_start && !line.empty() && line[0] == '.') out += '.';
            out += line;
            out += "\r\n";
            at_line_start = true;
            if (eol == std::string::npos) break;
            i = eol + 1;
        }
        out += ".\r\n";
        size_t sent = 0;
        while (sent < out.size()) {
            ssize_t n = ::send(fd_, out.data() + sent, out.size() - sent, 0);
            if (n <= 0) throw std::runtime_error("email: connection lost sending the message");
            sent += (size_t)n;
        }
        int got = read_code();
        if (got != 250) {
            throw std::runtime_error("email: message rejected with reply " + std::to_string(got) +
                                     " (" + last_line_ + ")");
        }
    }

  private:
    void close_fd() {
        if (fd_ >= 0) {
#ifdef _WIN32
            ::closesocket(fd_);
#else
            ::close(fd_);
#endif
            fd_ = -1;
        }
    }

    std::string read_line() {
        std::string line;
        char c = 0;
        auto deadline =
            std::chrono::steady_clock::now() + std::chrono::milliseconds(timeout_ms_);
        while (std::chrono::steady_clock::now() < deadline) {
            ssize_t n = ::recv(fd_, &c, 1, MSG_DONTWAIT);
            if (n == 1) {
                if (c == '\n') break;
                if (c != '\r') line += c;
            } else if (n == 0) {
                break;  // orderly shutdown
            } else {
                if (errno != EAGAIN
#ifdef EWOULDBLOCK
                    && errno != EWOULDBLOCK
#endif
                )
                    break;
                std::this_thread::sleep_for(std::chrono::milliseconds(1));
            }
        }
        return line;
    }

    int read_code() {
        // Multi-line replies repeat the code with `-` until the last line's
        // space; the last line is the message worth quoting on failure.
        int code = 0;
        for (;;) {
            last_line_ = read_line();
            if (last_line_.size() < 3) throw std::runtime_error("email: short reply from server");
            code = (last_line_[0] - '0') * 100 + (last_line_[1] - '0') * 10 + (last_line_[2] - '0');
            if (last_line_.size() < 4 || last_line_[3] != '-') break;
        }
        return code;
    }

    int fd_ = -1;
    int timeout_ms_ = SMTP_TIMEOUT_MS;
    std::string last_line_;
};

/// Assemble an RFC 5322 message: headers, a blank line, the body. The Date
/// comes from the clock and the id is random; everything else is the caller's.
inline std::string email_message(const std::string& from, const std::string& to,
                                 const std::string& subject, const std::string& body) {
    char date[64] = {};
    std::time_t now = std::time(nullptr);
    std::tm tmv{};
#if defined(_WIN32)
    gmtime_s(&tmv, &now);
#else
    gmtime_r(&now, &tmv);
#endif
    strftime(date, sizeof date, "%a, %d %b %Y %H:%M:%S +0000", &tmv);
    std::string out = "From: " + from + "\r\n";
    out += "To: " + to + "\r\n";
    out += "Subject: " + subject + "\r\n";
    out += "Date: " + std::string(date) + "\r\n";
    out += "Message-ID: <" + random_hex(16) + "@hardscript>\r\n";
    out += "MIME-Version: 1.0\r\n";
    out += "Content-Type: text/plain; charset=utf-8\r\n";
    out += "\r\n";
    out += body;
    if (out.empty() || out.back() != '\n') out += "\n";
    return out;
}

/// Deliver one message, synchronously: EHLO, optional AUTH LOGIN, MAIL, RCPT,
/// DATA, QUIT. Any step can throw with the server's own reply attached.
inline void email_deliver(const SmtpConfig& cfg, const std::string& to, const std::string& subject,
                          const std::string& body, int timeout_ms = SMTP_TIMEOUT_MS) {
    SmtpConn conn(cfg.host, cfg.port, timeout_ms);
    int greet = conn.greeting();
    if (greet != 220) throw std::runtime_error("email: bad greeting " + std::to_string(greet));
    conn.cmd("EHLO hardscript", 250);
    if (!cfg.user.empty()) {
        conn.cmd("AUTH LOGIN", 334);
        conn.cmd(base64_encode(cfg.user), 334);
        conn.cmd(base64_encode(cfg.pass), 235);
    }
    conn.cmd("MAIL FROM:<" + cfg.from + ">", 250);
    conn.cmd("RCPT TO:<" + to + ">", 250);
    conn.cmd_data(email_message(cfg.from, to, subject, body));
    try {
        conn.cmd("QUIT", 221);
    } catch (...) {
        // The mail is sent; a goodbye failure is not a delivery failure.
    }
}

// ---------------------------------------------------------------------------
// Language surface
// ---------------------------------------------------------------------------

/// Resolve the send options object into its parts, with required presence
/// and types checked up front so a bad call fails before any delivery.
struct EmailSend {
    std::string to;
    std::string from;
    std::string subject;
    std::string body;
    bool async = false;
};

inline EmailSend email_parse_options(const Val& options) {
    if (!options.is_obj()) throw std::runtime_error("email: send takes named options (to=..., ...)");
    EmailSend out;
    SmtpConfig cfg = smtp_config();
    out.from = cfg.from;
    auto need_text = [&](const char* key, bool required, std::string& slot) {
        const Val* v = options.find(key);
        if (!v || v->is_nil()) {
            if (required)
                throw std::runtime_error(std::string("email: send needs `") + key + "`");
            return;
        }
        if (!v->is_str())
            throw std::runtime_error(std::string("email: send `") + key + "` must be text");
        slot = v->sv;
    };
    need_text("to", true, out.to);
    need_text("subject", true, out.subject);
    need_text("body", false, out.body);
    std::string from_opt;
    need_text("from", false, from_opt);
    if (!from_opt.empty()) out.from = from_opt;
    const Val* t = options.find("template");
    const Val* d = options.find("data");
    if ((t && !t->is_nil()) || (d && !d->is_nil())) {
        if (!t || t->is_nil() || !t->is_str() || t->sv.empty())
            throw std::runtime_error("email: `template` needs a name");
        std::string content;
        {
            std::lock_guard<std::mutex> lock(email_templates_mu());
            auto it = email_templates().find(t->sv);
            if (it == email_templates().end())
                throw std::runtime_error("email: unknown template `" + t->sv + "`");
            content = it->second;
        }
        Val data = (d && !d->is_nil()) ? *d : Val::object({});
        if (!data.is_obj()) throw std::runtime_error("email: template `data` must be an object");
        out.body = email_render(t->sv, content, data);
    }
    if (out.body.empty()) throw std::runtime_error("email: send needs a `body` (or `template` + `data`)");
    const Val* a = options.find("async");
    if (a && !a->is_nil()) {
        if (!a->is_bool()) throw std::runtime_error("email: `async` must be true or false");
        out.async = a->truthy();
    }
    for (const auto& kv : options.obj) {
        if (kv.first != "to" && kv.first != "from" && kv.first != "subject" && kv.first != "body" &&
            kv.first != "template" && kv.first != "data" && kv.first != "async") {
            throw std::runtime_error("email: unknown option `" + kv.first +
                                     "` (to, from, subject, body, template, data, async)");
        }
    }
    if (!email_looks_valid(out.to))
        throw std::runtime_error("email: `" + out.to + "` is not an address");
    return out;
}

/// The queue worker for async sends: same delivery, driven by the job
/// system, so retries and the dead-letter queue come along.
inline Val email_worker_send(const Val& payload) {
    const Val* to = payload.find("to");
    const Val* subject = payload.find("subject");
    const Val* body = payload.find("body");
    if (!to || !subject || !body)
        throw std::runtime_error("email: async payload needs to, subject and body");
    SmtpConfig cfg;
    cfg.host = payload.find("host") && payload.find("host")->is_str() ? payload.find("host")->sv : "";
    if (cfg.host.empty()) cfg = smtp_config();
    if (const Val* u = payload.find("user")) {
        if (u->is_str()) cfg.user = u->sv;
    }
    if (const Val* p = payload.find("pass")) {
        if (p->is_str()) cfg.pass = p->sv;
    }
    if (const Val* f = payload.find("from")) {
        if (f->is_str() && !f->sv.empty()) cfg.from = f->sv;
    }
    if (const Val* pt = payload.find("port")) {
        if (pt->is_int()) cfg.port = (int)pt->iv;
    }
    email_deliver(cfg, to->sv, subject->sv, body->sv);
    return Val::boolean(true);
}

inline void email_ensure_async_declared() {
    static std::atomic<bool> done{false};
    if (done.exchange(true)) return;
    queue_runtime().declare_job("__email_send", {"payload"}, 5);
    queue_runtime().register_worker(
        "__email_send", 2,
        [](const Val& payload) -> Val {
            const Val* inner = payload.find("payload");
            return email_worker_send(inner ? *inner : payload);
        });
}

/// `email.send(to=..., subject=..., body=...)`: deliver now, or enqueue when
/// `async=true`. Returns true on delivery, the job id when queued.
inline Val email_send(const Val& options) {
    EmailSend s = email_parse_options(options);
    if (!s.async) {
        // `from` is per message; everything else comes from the environment.
        SmtpConfig cfg = smtp_config();
        cfg.from = s.from;
        email_deliver(cfg, s.to, s.subject, s.body);
        return Val::boolean(true);
    }
    email_ensure_async_declared();
    SmtpConfig cfg = smtp_config();
    Val payload = Val::object({{"to", Val::text(s.to)},
                               {"from", Val::text(s.from)},
                               {"subject", Val::text(s.subject)},
                               {"body", Val::text(s.body)},
                               {"host", Val::text(cfg.host)},
                               {"port", Val::int_(cfg.port)},
                               {"user", Val::text(cfg.user)},
                               {"pass", Val::text(cfg.pass)}});
    Val args = Val::list({payload});
    return Val::int_(queue_runtime().enqueue("__email_send", args, 0, 0, 0));
}

/// `email.template(name, content)`: register a named body with `{{var}}`
/// holes, rendered at send time from `data`.
inline Val email_template(const Val& name, const Val& content) {
    if (!name.is_str() || name.sv.empty())
        throw std::runtime_error("email: template needs a name as text");
    if (!content.is_str()) throw std::runtime_error("email: template needs content as text");
    email_register_template(name.sv, content.sv);
    return Val::boolean(true);
}

}  // namespace hs

#endif  // HS_RUNTIME_EMAIL_HPP
