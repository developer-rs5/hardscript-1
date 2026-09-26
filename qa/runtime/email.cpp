// qa/runtime/email.cpp -- email / notification runtime
//
// The only client is a mock SMTP server on a loopback port, so every test
// names the exact wire dialogue the runtime speaks: the command order, the
// AUTH exchange, dot-stuffing, the headers it built, and what it does with
// each rejection. `MockSmtp` also fails on demand (RCPT, DATA, greeting,
// EHLO, hang, hang up) because the interesting half of an email client is
// what it does when the server says no.

#include "hs_runtime_email.hpp"

#include "support.hpp"

#include <atomic>
#include <cstdlib>
#include <thread>

namespace qa {
namespace {

// ---------------------------------------------------------------------------
// Environment
// ---------------------------------------------------------------------------

void set_env(const char* k, const std::string& v) {
#ifdef _WIN32
    _putenv_s(k, v.c_str());
#else
    if (v.empty())
        unsetenv(k);
    else
        setenv(k, v.c_str(), 1);
#endif
}

/// Point the runtime at a mock server and back to nothing: config is read per
/// send, so each test sets only what it cares about.
struct SmtpEnv {
    std::vector<std::pair<std::string, std::string>> saved;
    explicit SmtpEnv(int port) {
        set("SMTP_HOST", "127.0.0.1");
        set("SMTP_PORT", std::to_string(port));
        set("SMTP_USER", "");
        set("SMTP_PASS", "");
        set("SMTP_FROM", "noreply@test.local");
    }
    void set(const char* k, const std::string& v) {
        if (std::getenv(k)) saved.emplace_back(k, std::getenv(k));
        set_env(k, v);
    }
    ~SmtpEnv() {
        for (auto& kv : saved)
            set_env(kv.first.c_str(), kv.second);
        set_env("SMTP_HOST", "");
        set_env("SMTP_PORT", "");
        set_env("SMTP_USER", "");
        set_env("SMTP_PASS", "");
        set_env("SMTP_FROM", "");
    }
};

// ---------------------------------------------------------------------------
// Mock SMTP server
// ---------------------------------------------------------------------------

enum class Fault {
    None,
    BadGreeting,   // 554 instead of 220
    BadEhlo,       // 500 instead of 250
    NoRecipients,  // 550 on RCPT
    NoData,        // 554 on DATA
    HangUp,        // close the socket after the greeting
    Slow,          // never answer the greeting
};

struct Session {
    std::vector<std::string> commands;
    std::string data;
};

/// A single-connection-at-a-time SMTP server, good enough for the client's
/// dialogue and nothing more: no extensions, no TLS, no pipelining.
class MockSmtp {
  public:
    explicit MockSmtp(Fault fault = Fault::None) : fault_(fault) {
        listen_fd_ = socket(AF_INET, SOCK_STREAM, 0);
        int one = 1;
        setsockopt(listen_fd_, SOL_SOCKET, SO_REUSEADDR, (const char*)&one, sizeof one);
        struct sockaddr_in addr;
        memset(&addr, 0, sizeof addr);
        addr.sin_family = AF_INET;
        addr.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
        addr.sin_port = 0;  // ephemeral
        if (bind(listen_fd_, (struct sockaddr*)&addr, sizeof addr) != 0)
            throw std::runtime_error("mock smtp: bind failed");
        if (listen(listen_fd_, 8) != 0) throw std::runtime_error("mock smtp: listen failed");
        socklen_t len = sizeof addr;
        getsockname(listen_fd_, (struct sockaddr*)&addr, &len);
        port_ = ntohs(addr.sin_port);
        thread_ = std::thread([this] { serve(); });
    }

    ~MockSmtp() {
        stop_ = true;
        if (listen_fd_ >= 0) {
            shutdown(listen_fd_, SHUT_RDWR);
            close(listen_fd_);
        }
        if (thread_.joinable()) thread_.join();
    }

    MockSmtp(const MockSmtp&) = delete;
    MockSmtp& operator=(const MockSmtp&) = delete;

    int port() const { return port_; }

    std::vector<Session> sessions() {
        std::lock_guard<std::mutex> lock(mu_);
        return sessions_;
    }

    int connections() const { return connections_.load(); }

    /// Wait for `n` delivered messages, so async tests do not sleep blindly.
    bool wait_for_messages(size_t n, int timeout_ms = 4000) {
        for (int waited = 0; waited < timeout_ms; waited += 5) {
            {
                std::lock_guard<std::mutex> lock(mu_);
                if (delivered_ >= n) return true;
            }
            std::this_thread::sleep_for(std::chrono::milliseconds(5));
        }
        return false;
    }

    void set_fault(Fault f) { fault_ = f; }

  private:
    void serve() {
        while (!stop_) {
            int fd = accept(listen_fd_, nullptr, nullptr);
            if (fd < 0) {
                if (stop_) return;
                continue;
            }
            connections_++;
            session(fd);
            close(fd);
        }
    }

    /// One line, or false at end of stream. An empty line is real content --
    /// every message has a blank one between headers and body -- so only a
    /// closed socket ends the conversation.
    bool read_line(int fd, std::string& line) {
        line.clear();
        char c = 0;
        while (true) {
            ssize_t n = recv(fd, &c, 1, 0);
            if (n == 0) return false;
            if (n < 0) {
                if (errno == EINTR) continue;
                return false;
            }
            if (c == '\n') return true;
            if (c != '\r') line.push_back(c);
        }
    }

    void reply(int fd, const std::string& line) {
        std::string out = line + "\r\n";
        size_t sent = 0;
        while (sent < out.size()) {
            ssize_t n = ::send(fd, out.data() + sent, out.size() - sent, 0);
            if (n <= 0) return;
            sent += (size_t)n;
        }
    }

    void session(int fd) {
        // The session is appended to the log up front and filled in place, so
        // a reader that has just been answered by QUIT sees the whole dialogue
        // including the goodbye.
        size_t idx = 0;
        {
            std::lock_guard<std::mutex> lock(mu_);
            sessions_.push_back(Session());
            idx = sessions_.size() - 1;
        }
        auto note = [&](const std::string& line) {
            std::lock_guard<std::mutex> lock(mu_);
            sessions_[idx].commands.push_back(line);
        };
        if (fault_ == Fault::Slow) {
            // Never answer, but wake often enough that the server can be shut
            // down promptly: a fixture that costs 15s of suite time teaches
            // nobody anything.
            for (int i = 0; i < 3000 && !stop_.load(); i++)
                std::this_thread::sleep_for(std::chrono::milliseconds(5));
            return;
        }
        if (fault_ == Fault::BadGreeting) {
            reply(fd, "554 no service here");
        } else {
            reply(fd, "220 mock.local ESMTP ready");
        }
        if (fault_ == Fault::HangUp) return;

        bool in_data = false;
        int auth_stage = 0;
        std::string line;
        while (read_line(fd, line)) {
            if (in_data) {
                if (line == ".") {
                    in_data = false;
                    {
                        std::lock_guard<std::mutex> lock(mu_);
                        sessions_[idx].data = body_;
                        body_.clear();
                        sessions_[idx].commands.push_back("<message>");
                    }
                    delivered_++;
                    reply(fd, fault_ == Fault::NoData ? "554 message refused" : "250 queued as MOCK1");
                } else {
                    // Undo dot-stuffing the way a real server does.
                    if (line.size() > 1 && line[0] == '.') line = line.substr(1);
                    if (!body_.empty()) body_ += "\r\n";
                    body_ += line;
                }
                continue;
            }
            note(line);
            if (line.rfind("EHLO", 0) == 0) {
                if (fault_ == Fault::BadEhlo) {
                    reply(fd, "500 what is this");
                } else {
                    // Multi-line reply: the client must read to the last line.
                    reply(fd, "250-mock.local greets you");
                    reply(fd, "250-SIZE 10485760");
                    reply(fd, "250 8BITMIME");
                }
            } else if (line.rfind("MAIL FROM", 0) == 0) {
                reply(fd, "250 sender ok");
            } else if (line.rfind("RCPT TO", 0) == 0) {
                reply(fd, fault_ == Fault::NoRecipients ? "550 no such mailbox" : "250 recipient ok");
            } else if (line == "DATA") {
                if (fault_ == Fault::NoData) {
                    reply(fd, "554 no data today");
                } else {
                    in_data = true;
                    reply(fd, "354 send it");
                }
            } else if (line == "QUIT") {
                reply(fd, "221 bye");
                break;
            } else if (line == "AUTH LOGIN") {
                auth_stage = 1;
                reply(fd, "334 VXNlcm5hbWU6");
            } else if (auth_stage == 1) {
                auth_stage = 2;
                reply(fd, "334 UGFzc3dvcmQ6");
            } else if (auth_stage == 2) {
                auth_stage = 0;
                reply(fd, "235 authenticated");
            } else if (line == "RSET" || line == "NOOP") {
                reply(fd, "250 ok");
            } else {
                reply(fd, "500 unknown command");
            }
        }
    }

    Fault fault_;
    int listen_fd_ = -1;
    int port_ = 0;
    std::atomic<bool> stop_{false};
    std::atomic<int> connections_{0};
    std::atomic<size_t> delivered_{0};
    std::thread thread_;
    std::mutex mu_;
    std::vector<Session> sessions_;
    std::string body_;
};

size_t count_sessions_with(const std::vector<Session>& v, const std::string& cmd) {
    size_t n = 0;
    for (const auto& s : v)
        for (const auto& c : s.commands)
            if (c.rfind(cmd, 0) == 0) n++;
    return n;
}

std::string data_of(const std::vector<Session>& v) {
    return v.empty() ? "" : v.back().data;
}

std::string header_of(const std::string& data, const std::string& name) {
    size_t i = data.find(name + ": ");
    if (i == std::string::npos) return "";
    size_t eol = data.find("\r\n", i);
    return data.substr(i + name.size() + 2, eol - i - name.size() - 2);
}

// ---------------------------------------------------------------------------
// Config and address checks
// ---------------------------------------------------------------------------

void config_reads_the_environment() {
    set_env("SMTP_HOST", "smtp.example.com");
    set_env("SMTP_PORT", "587");
    set_env("SMTP_USER", "mailer");
    set_env("SMTP_PASS", "hunter2");
    set_env("SMTP_FROM", "app@example.com");
    hs::SmtpConfig c = hs::smtp_config();
    CHECK_EQ(c.host, std::string("smtp.example.com"), "host from env");
    CHECK_EQ(c.port, 587, "port from env");
    CHECK_EQ(c.user, std::string("mailer"), "user from env");
    CHECK_EQ(c.pass, std::string("hunter2"), "pass from env");
    CHECK_EQ(c.from, std::string("app@example.com"), "from from env");
    set_env("SMTP_HOST", "");
    set_env("SMTP_PORT", "");
    set_env("SMTP_USER", "");
    set_env("SMTP_PASS", "");
    set_env("SMTP_FROM", "");
    hs::SmtpConfig d = hs::smtp_config();
    CHECK_EQ(d.host, std::string("127.0.0.1"), "host default");
    CHECK_EQ(d.port, 25, "port default");
    CHECK_EQ(d.user, std::string(""), "no user by default");
    CHECK_EQ(d.from, std::string("noreply@localhost"), "from default");
}

void addresses_are_checked_before_a_round_trip() {
    CHECK(hs::email_looks_valid("a@b.co"), "simple address");
    CHECK(hs::email_looks_valid("first.last+tag@sub.example.com"), "tagged address");
    CHECK(!hs::email_looks_valid("plain"), "no at sign");
    CHECK(!hs::email_looks_valid("@example.com"), "no local part");
    CHECK(!hs::email_looks_valid("a@"), "no domain");
    CHECK(!hs::email_looks_valid("a@b"), "no dot in domain");
    CHECK(!hs::email_looks_valid("a b@c.de"), "space is a header split");
    CHECK(!hs::email_looks_valid("a@b.c\nBcc: x@y.z"), "newline injects a header");
    CHECK(!hs::email_looks_valid("a@@b.co"), "two at signs");
}

// ---------------------------------------------------------------------------
// Templates
// ---------------------------------------------------------------------------

void templates_render_from_data() {
    hs::email_register_template("welcome", "Hi {{name}}, your code is {{code}}.");
    hs::Val data = hs::Val::object({{"name", hs::Val::text("Ada")}, {"code", hs::Val::int_(42)}});
    CHECK_EQ(hs::email_render("welcome", hs::email_templates()["welcome"], data),
             std::string("Hi Ada, your code is 42."), "both holes filled");
    CHECK_EQ(hs::email_render("plain", "no holes here", hs::Val::object({})),
             std::string("no holes here"), "no holes is the body");
    CHECK_EQ(hs::email_render("t", "{{ name }}", data), std::string("Ada"), "spaces trimmed");
    CHECK_EQ(hs::email_render("t", "a { b {{name}}", data), std::string("a { b Ada"),
             "a lone brace is just text");
    CHECK_EQ(hs::email_render("t", "{{name}} {{name}}", data), std::string("Ada Ada"), "repeated hole");
}

void templates_never_silently_blank() {
    hs::email_register_template("greet", "Hi {{name}}");
    bool threw = false;
    try {
        hs::email_render("greet", hs::email_templates()["greet"], hs::Val::object({}));
    } catch (const std::exception& e) {
        threw = std::string(e.what()).find("needs `name`") != std::string::npos;
    }
    CHECK(threw, "a missing hole is an error naming the hole");
    bool unclosed = false;
    try {
        hs::email_render("t", "Hi {{name", hs::Val::object({{"name", hs::Val::text("x")}}));
    } catch (const std::exception& e) {
        unclosed = std::string(e.what()).find("unclosed") != std::string::npos;
    }
    CHECK(unclosed, "an unclosed hole is an error");
    bool empty = false;
    try {
        hs::email_render("t", "Hi {{}}", hs::Val::object({}));
    } catch (const std::exception& e) {
        empty = std::string(e.what()).find("empty") != std::string::npos;
    }
    CHECK(empty, "an empty hole is an error");
    bool notobj = false;
    try {
        hs::email_render("t", "{{a}}", hs::Val::text("not an object"));
    } catch (const std::exception& e) {
        notobj = std::string(e.what()).find("`data`") != std::string::npos;
    }
    CHECK(notobj, "data must be an object");
}

// ---------------------------------------------------------------------------
// Delivery over a real socket
// ---------------------------------------------------------------------------

void a_send_speaks_smtp_in_order() {
    MockSmtp smtp;
    SmtpEnv env(smtp.port());
    hs::Val r = hs::email_send(hs::Val::object({{"to", hs::Val::text("ada@example.com")},
                                               {"subject", hs::Val::text("Welcome")},
                                               {"body", hs::Val::text("Hello there")}}));
    CHECK(r.is_bool() && r.truthy(), "sync send returns true");
    auto sessions = smtp.sessions();
    CHECK_EQ(sessions.size(), size_t(1), "one connection, one message");
    const std::vector<std::string>& c = sessions[0].commands;
    CHECK_EQ(c.size(), size_t(6), "greeting plus five commands");
    if (c.size() == 6) {
        CHECK(c[0].rfind("EHLO", 0) == 0, "EHLO first");
        CHECK_EQ(c[1], std::string("MAIL FROM:<noreply@test.local>"), "envelope sender is the config");
        CHECK_EQ(c[2], std::string("RCPT TO:<ada@example.com>"), "envelope recipient");
        CHECK_EQ(c[3], std::string("DATA"), "DATA next");
        CHECK_EQ(c[4], std::string("<message>"), "the body arrived");
        CHECK_EQ(c[5], std::string("QUIT"), "QUIT last");
    }
    const std::string d = data_of(sessions);
    CHECK_EQ(header_of(d, "To"), std::string("ada@example.com"), "To header");
    CHECK_EQ(header_of(d, "From"), std::string("noreply@test.local"), "From header");
    CHECK_EQ(header_of(d, "Subject"), std::string("Welcome"), "Subject header");
    CHECK(d.find("Date: ") != std::string::npos, "Date header present");
    CHECK(d.find("Message-ID: <") != std::string::npos, "Message-ID header present");
    CHECK(d.find("MIME-Version: 1.0") != std::string::npos, "MIME-Version header");
    CHECK(d.find("Content-Type: text/plain") != std::string::npos, "Content-Type header");
    CHECK(d.find("\r\n\r\n") != std::string::npos, "headers end with a blank line");
    CHECK(d.size() > d.find("\r\n\r\n") + 4, "the body follows the headers");
    CHECK(d.find("Hello there") != std::string::npos, "body text delivered");
}

void message_ids_differ_per_message() {
    MockSmtp smtp;
    SmtpEnv env(smtp.port());
    for (int i = 0; i < 3; i++) {
        hs::email_send(hs::Val::object({{"to", hs::Val::text("a@b.co")},
                                        {"subject", hs::Val::text("s")},
                                        {"body", hs::Val::text("b")}}));
    }
    auto sessions = smtp.sessions();
    CHECK_EQ(sessions.size(), size_t(3), "three connections");
    std::string a = header_of(sessions[0].data, "Message-ID");
    std::string b = header_of(sessions[1].data, "Message-ID");
    std::string c = header_of(sessions[2].data, "Message-ID");
    CHECK(!a.empty() && a != b && b != c && a != c, "each message gets its own id");
}

void a_body_of_dots_is_stuffed() {
    MockSmtp smtp;
    SmtpEnv env(smtp.port());
    // A line that is just `.` would end DATA early and silently truncate the
    // mail, so it must arrive doubled and be unstuffed by the server.
    hs::email_send(hs::Val::object({{"to", hs::Val::text("a@b.co")},
                                    {"subject", hs::Val::text("s")},
                                    {"body", hs::Val::text("before\n.\nafter")}}));
    auto sessions = smtp.sessions();
    CHECK_EQ(sessions.size(), size_t(1), "delivered");
    const std::string d = data_of(sessions);
    CHECK(d.find("before") != std::string::npos, "first line kept");
    CHECK(d.find("after") != std::string::npos, "last line kept");
    CHECK_EQ(sessions[0].commands.back(), std::string("QUIT"), "the message really ended");
}

void auth_is_sent_only_when_configured() {
    MockSmtp smtp;
    SmtpEnv env(smtp.port());
    env.set("SMTP_USER", "mailer");
    env.set("SMTP_PASS", "hunter2");
    hs::email_send(hs::Val::object({{"to", hs::Val::text("a@b.co")},
                                    {"subject", hs::Val::text("s")},
                                    {"body", hs::Val::text("b")}}));
    auto sessions = smtp.sessions();
    CHECK_EQ(sessions.size(), size_t(1), "delivered");
    CHECK(count_sessions_with(sessions, "AUTH LOGIN") == 1, "AUTH LOGIN asked");
    auto c = sessions[0].commands;
    size_t at = 0;
    for (size_t i = 0; i < c.size(); i++)
        if (c[i] == "AUTH LOGIN") at = i;
    CHECK(at + 2 < c.size(), "two credential lines follow");
    if (at + 2 < c.size()) {
        CHECK_EQ(hs::base64_decode(c[at + 1]), std::string("mailer"), "username, base64");
        CHECK_EQ(hs::base64_decode(c[at + 2]), std::string("hunter2"), "password, base64");
        CHECK(at > 0 && c[at - 1].rfind("EHLO", 0) == 0, "EHLO precedes AUTH");
    }
}

void every_server_refusal_surfaces() {
    struct Case {
        Fault fault;
        const char* needle;
        const char* what;
    };
    const Case cases[] = {
        {Fault::BadGreeting, "greeting", "a bad greeting is refused"},
        {Fault::BadEhlo, "EHLO", "a refused EHLO is reported"},
        {Fault::NoRecipients, "550", "a refused recipient is reported with the server's code"},
        {Fault::NoData, "554", "a refused DATA is reported with the server's code"},
        {Fault::HangUp, "short reply", "a hang-up is an error, not a silent success"},
    };
    for (const Case& c : cases) {
        MockSmtp smtp(c.fault);
        SmtpEnv env(smtp.port());
        bool threw = false;
        std::string msg;
        try {
            hs::email_send(hs::Val::object({{"to", hs::Val::text("a@b.co")},
                                            {"subject", hs::Val::text("s")},
                                            {"body", hs::Val::text("b")}}));
        } catch (const std::exception& e) {
            threw = true;
            msg = e.what();
        }
        CHECK(threw, c.what);
        CHECK(msg.find(c.needle) != std::string::npos,
              std::string("the message names the server's answer: ") + c.needle);
    }
}

void an_unreachable_server_is_an_error() {
    // Bind and drop a port so nothing is listening: the classic misconfigured
    // SMTP_HOST, and it must fail loudly.
    int port = 0;
    {
        MockSmtp probe;
        port = probe.port();
    }
    SmtpEnv env(port);
    bool threw = false;
    std::string msg;
    try {
        hs::email_send(hs::Val::object({{"to", hs::Val::text("a@b.co")},
                                        {"subject", hs::Val::text("s")},
                                        {"body", hs::Val::text("b")}}));
    } catch (const std::exception& e) {
        threw = true;
        msg = e.what();
    }
    CHECK(threw, "a dead server throws");
    CHECK(msg.find("connect") != std::string::npos, "the error says connect");
}

void a_hung_server_fails_in_bounded_time() {
    MockSmtp smtp(Fault::Slow);
    hs::SmtpConfig c;
    c.host = "127.0.0.1";
    c.port = smtp.port();
    c.from = "noreply@test.local";
    auto t0 = std::chrono::steady_clock::now();
    bool threw = false;
    try {
        // A short deadline stands in for the production 10s: the point is that
        // a silent relay ends the send instead of pinning the thread.
        hs::email_deliver(c, "a@b.co", "s", "b", 250);
    } catch (const std::exception&) {
        threw = true;
    }
    long ms = (long)std::chrono::duration_cast<std::chrono::milliseconds>(
                  std::chrono::steady_clock::now() - t0)
                  .count();
    CHECK(threw, "a server that never answers is an error");
    CHECK(ms < 5000, "and it is bounded by the deadline, not by the server");
}

void an_unresolvable_host_is_an_error() {
    SmtpEnv env(25);
    hs::SmtpConfig c;
    c.host = "no-such-host.invalid";
    bool threw = false;
    try {
        hs::email_deliver(c, "a@b.co", "s", "b");
    } catch (const std::exception&) {
        threw = true;
    }
    CHECK(threw, "an unresolvable host is an error");
}

// ---------------------------------------------------------------------------
// The language surface
// ---------------------------------------------------------------------------

void send_options_are_validated_before_delivery() {
    MockSmtp smtp;
    SmtpEnv env(smtp.port());
    auto want_fail = [&](const hs::Val& opts, const char* needle, const char* what) {
        bool threw = false;
        std::string msg;
        try {
            hs::email_send(opts);
        } catch (const std::exception& e) {
            threw = true;
            msg = e.what();
        }
        CHECK(threw, what);
        CHECK(msg.find(needle) != std::string::npos,
              std::string(what) + ": the message names the problem (" + needle + ")");
    };
    want_fail(hs::Val::object({{"subject", hs::Val::text("s")}, {"body", hs::Val::text("b")}}),
              "needs `to`", "no recipient");
    want_fail(hs::Val::object({{"to", hs::Val::text("a@b.co")}, {"body", hs::Val::text("b")}}),
              "needs `subject`", "no subject");
    want_fail(hs::Val::object({{"to", hs::Val::text("a@b.co")}, {"subject", hs::Val::text("s")}}),
              "needs a `body`", "no body and no template");
    want_fail(hs::Val::object({{"to", hs::Val::text("a@b.co")},
                               {"subject", hs::Val::int_(7)},
                               {"body", hs::Val::text("b")}}),
              "must be text", "a numeric subject");
    want_fail(hs::Val::object({{"to", hs::Val::text("a@b.co")},
                               {"subject", hs::Val::text("s")},
                               {"body", hs::Val::text("b")},
                               {"async", hs::Val::text("yes")}}),
              "`async` must be", "a non-boolean async");
    want_fail(hs::Val::object({{"to", hs::Val::text("a@b.co")},
                               {"subject", hs::Val::text("s")},
                               {"body", hs::Val::text("b")},
                               {"cc", hs::Val::text("c@d.co")}}),
              "unknown option `cc`", "an unknown option");
    want_fail(hs::Val::object({{"to", hs::Val::text("nope")},
                               {"subject", hs::Val::text("s")},
                               {"body", hs::Val::text("b")}}),
              "is not an address", "a malformed address");
    want_fail(hs::Val::object({{"to", hs::Val::text("a@b.co")},
                               {"subject", hs::Val::text("s")},
                               {"template", hs::Val::text("no-such-template")}}),
              "unknown template", "an unregistered template");
    want_fail(hs::Val::text("a@b.co"), "named options", "a positional string");
    want_fail(hs::Val::object({}), "needs `to`", "an empty option set");
    CHECK_EQ(smtp.connections(), 0, "not one of those reached the network");
}

void a_template_can_stand_in_for_a_body() {
    MockSmtp smtp;
    SmtpEnv env(smtp.port());
    hs::email_template(hs::Val::text("receipt"), hs::Val::text("Thanks {{who}}, you paid {{amount}}."));
    hs::Val r = hs::email_send(hs::Val::object(
        {{"to", hs::Val::text("ada@example.com")},
         {"subject", hs::Val::text("Receipt")},
         {"template", hs::Val::text("receipt")},
         {"data", hs::Val::object({{"who", hs::Val::text("Ada")},
                                   {"amount", hs::Val::text("$9")}})}}));
    CHECK(r.truthy(), "templated send delivered");
    auto sessions = smtp.sessions();
    CHECK_EQ(sessions.size(), size_t(1), "one message");
    CHECK(data_of(sessions).find("Thanks Ada, you paid $9.") != std::string::npos,
          "the rendered body arrived");
    bool missing = false;
    try {
        hs::email_send(hs::Val::object({{"to", hs::Val::text("a@b.co")},
                                        {"subject", hs::Val::text("s")},
                                        {"template", hs::Val::text("receipt")}}));
    } catch (const std::exception& e) {
        missing = std::string(e.what()).find("needs `who`") != std::string::npos;
    }
    CHECK(missing, "a template with no data errors on the first hole");
}

void from_can_be_overridden_per_message() {
    MockSmtp smtp;
    SmtpEnv env(smtp.port());
    hs::email_send(hs::Val::object({{"to", hs::Val::text("a@b.co")},
                                    {"subject", hs::Val::text("s")},
                                    {"body", hs::Val::text("b")},
                                    {"from", hs::Val::text("noreply@brand.example")}}));
    auto sessions = smtp.sessions();
    CHECK_EQ(sessions.size(), size_t(1), "delivered");
    if (sessions.size() == 1 && sessions[0].commands.size() > 1) {
        CHECK_EQ(sessions[0].commands[1], std::string("MAIL FROM:<noreply@brand.example>"),
                 "envelope sender is the override");
        CHECK_EQ(header_of(sessions[0].data, "From"), std::string("noreply@brand.example"),
                 "From header is the override too");
    }
}

void a_rewrite_of_the_config_is_picked_up_per_send() {
    MockSmtp first, second;
    hs::SmtpConfig cfg = hs::smtp_config();
    {
        SmtpEnv env(first.port());
        hs::email_send(hs::Val::object({{"to", hs::Val::text("a@b.co")},
                                        {"subject", hs::Val::text("s")},
                                        {"body", hs::Val::text("b")}}));
    }
    {
        SmtpEnv env(second.port());
        hs::email_send(hs::Val::object({{"to", hs::Val::text("a@b.co")},
                                        {"subject", hs::Val::text("s")},
                                        {"body", hs::Val::text("b")}}));
    }
    CHECK_EQ(first.sessions().size(), size_t(1), "first server got the first send");
    CHECK_EQ(second.sessions().size(), size_t(1), "second server got the second send");
    (void)cfg;
}

// ---------------------------------------------------------------------------
// Async: the queue does the waiting
// ---------------------------------------------------------------------------

void an_async_send_is_queued_and_delivered() {
    MockSmtp smtp;
    SmtpEnv env(smtp.port());
    hs::Val id = hs::email_send(hs::Val::object({{"to", hs::Val::text("a@b.co")},
                                                {"subject", hs::Val::text("s")},
                                                {"body", hs::Val::text("b")},
                                                {"async", hs::Val::boolean(true)}}));
    CHECK(id.is_int() && id.iv > 0, "an async send returns a job id");
    CHECK(smtp.wait_for_messages(1), "the worker delivered it");
    auto sessions = smtp.sessions();
    CHECK_EQ(sessions.size(), size_t(1), "exactly one message, no duplicates");
    if (sessions.size() == 1 && sessions[0].commands.size() > 2) {
        CHECK_EQ(sessions[0].commands[2], std::string("RCPT TO:<a@b.co>"), "same recipient");
    }
    CHECK_EQ(hs::queue_runtime().dlq("__email_send").size(), size_t(0), "nothing dead-lettered");
}

void an_async_send_captures_the_config_it_was_queued_with() {
    MockSmtp smtp;
    int port = 0;
    {
        SmtpEnv env(smtp.port());
        port = smtp.port();
        hs::email_send(hs::Val::object({{"to", hs::Val::text("a@b.co")},
                                        {"subject", hs::Val::text("s")},
                                        {"body", hs::Val::text("b")},
                                        {"async", hs::Val::boolean(true)}}));
    }
    // The environment now points nowhere useful; the job must still know where
    // it was going, or a config change would silently redirect queued mail.
    SmtpEnv env2(1);
    CHECK(smtp.wait_for_messages(1), "delivered from the captured config");
    auto sessions = smtp.sessions();
    CHECK_EQ(sessions.size(), size_t(1), "one message to the original server");
    (void)port;
}

void a_failed_async_send_ends_in_the_dead_letter_queue() {
    MockSmtp smtp(Fault::NoRecipients);
    SmtpEnv env(smtp.port());
    hs::email_send(hs::Val::object({{"to", hs::Val::text("a@b.co")},
                                    {"subject", hs::Val::text("s")},
                                    {"body", hs::Val::text("b")},
                                    {"async", hs::Val::boolean(true)}}));
    // Default retries take seconds; force one attempt so the DLQ is reachable
    // in a test, since the retry policy itself is the queue's tested contract.
    hs::queue_runtime().enqueue("__email_send",
                                hs::Val::list({hs::Val::object({{"to", hs::Val::text("x@y.co")},
                                                                 {"subject", hs::Val::text("s")},
                                                                 {"body", hs::Val::text("b")},
                                                                 {"host", hs::Val::text("127.0.0.1")},
                                                                 {"port", hs::Val::int_(smtp.port())},
                                                                 {"from", hs::Val::text("noreply@test.local")},
                                                                 {"user", hs::Val::text("")},
                                                                 {"pass", hs::Val::text("")}})}),
                                0, 0, 1);
    bool dead = false;
    for (int waited = 0; waited < 4000 && !dead; waited += 10) {
        dead = !hs::queue_runtime().dlq("__email_send").empty();
        std::this_thread::sleep_for(std::chrono::milliseconds(10));
    }
    CHECK(dead, "a permanently refused send is dead-lettered, not dropped");
    auto dlq = hs::queue_runtime().dlq("__email_send");
    bool has_error = false;
    for (const auto& j : dlq)
        if (j.last_error.find("550") != std::string::npos) has_error = true;
    CHECK(has_error, "the dead letter keeps the server's complaint");
}

void a_malformed_async_payload_fails_the_job() {
    bool threw = false;
    try {
        hs::email_worker_send(hs::Val::object({{"to", hs::Val::text("a@b.co")}}));
    } catch (const std::exception& e) {
        threw = std::string(e.what()).find("needs to, subject and body") != std::string::npos;
    }
    CHECK(threw, "an incomplete payload is an error naming the missing fields");
}

// ---------------------------------------------------------------------------
// Threads
// ---------------------------------------------------------------------------

void concurrent_sends_do_not_interleave() {
    MockSmtp smtp;
    SmtpEnv env(smtp.port());
    const int kThreads = 8;
    const int kPerThread = 4;
    std::vector<std::thread> ts;
    std::atomic<int> delivered{0};
    for (int t = 0; t < kThreads; t++) {
        ts.emplace_back([t, &delivered] {
            for (int i = 0; i < kPerThread; i++) {
                std::string to = "user" + std::to_string(t) + "-" + std::to_string(i) + "@example.com";
                try {
                    hs::email_send(hs::Val::object({{"to", hs::Val::text(to)},
                                                    {"subject", hs::Val::text("s" + std::to_string(t))},
                                                    {"body", hs::Val::text("b" + std::to_string(i))}}));
                    delivered++;
                } catch (...) {
                }
            }
        });
    }
    for (auto& th : ts)
        th.join();
    CHECK_EQ(delivered.load(), kThreads * kPerThread, "every send returned");
    CHECK(smtp.wait_for_messages(kThreads * kPerThread), "every message arrived");
    auto sessions = smtp.sessions();
    CHECK_EQ(sessions.size(), size_t(kThreads * kPerThread), "one connection per message");
    // Each connection must be one whole message: exactly one RCPT and one
    // body, or two threads shared a socket.
    size_t rcp = 0, msgs = 0;
    for (const auto& s : sessions) {
        for (const auto& c : s.commands) {
            if (c.rfind("RCPT TO", 0) == 0) rcp++;
            if (c == "<message>") msgs++;
        }
    }
    CHECK_EQ(rcp, size_t(kThreads * kPerThread), "one recipient per connection");
    CHECK_EQ(msgs, size_t(kThreads * kPerThread), "one message per connection");
}

}  // namespace
}  // namespace qa

int main() {
    qa::config_reads_the_environment();
    qa::addresses_are_checked_before_a_round_trip();
    qa::templates_render_from_data();
    qa::templates_never_silently_blank();
    qa::a_send_speaks_smtp_in_order();
    qa::message_ids_differ_per_message();
    qa::a_body_of_dots_is_stuffed();
    qa::auth_is_sent_only_when_configured();
    qa::every_server_refusal_surfaces();
    qa::an_unreachable_server_is_an_error();
    qa::a_hung_server_fails_in_bounded_time();
    qa::an_unresolvable_host_is_an_error();
    qa::send_options_are_validated_before_delivery();
    qa::a_template_can_stand_in_for_a_body();
    qa::from_can_be_overridden_per_message();
    qa::a_rewrite_of_the_config_is_picked_up_per_send();
    qa::an_async_send_is_queued_and_delivered();
    qa::an_async_send_captures_the_config_it_was_queued_with();
    qa::a_failed_async_send_ends_in_the_dead_letter_queue();
    qa::a_malformed_async_payload_fails_the_job();
    qa::concurrent_sends_do_not_interleave();
    return qa::report("email");
}
