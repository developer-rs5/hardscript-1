#ifndef HS_RUNTIME_CRYPTO_HPP
#define HS_RUNTIME_CRYPTO_HPP
#include "hs_runtime_value.hpp"
// ===========================================================================
// Crypto
// ===========================================================================
inline uint32_t rol32(uint32_t x, int n) { return (x << n) | (x >> (32 - n)); }

inline void sha256_blocks(uint32_t* h, const uint8_t* p) {
    static const uint32_t K[64] = {
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
        0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
        0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
        0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
        0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
        0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
        0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
    };
    uint32_t w[64];
    for (int i = 0; i < 16; i++)
        w[i] = ((uint32_t)p[i * 4] << 24) | ((uint32_t)p[i * 4 + 1] << 16) | ((uint32_t)p[i * 4 + 2] << 8) | p[i * 4 + 3];
    for (int i = 16; i < 64; i++) {
        uint32_t s0 = rol32(w[i - 15], 7) ^ rol32(w[i - 15], 18) ^ (w[i - 15] >> 3);
        uint32_t s1 = rol32(w[i - 2], 17) ^ rol32(w[i - 2], 19) ^ (w[i - 2] >> 10);
        w[i] = w[i - 16] + s0 + w[i - 7] + s1;
    }
    uint32_t a = h[0], b = h[1], c = h[2], d = h[3];
    uint32_t e = h[4], f = h[5], g = h[6], hh = h[7];
    for (int i = 0; i < 64; i++) {
        uint32_t S1 = rol32(e, 6) ^ rol32(e, 11) ^ rol32(e, 25);
        uint32_t ch = (e & f) ^ (~e & g);
        uint32_t t1 = hh + S1 + ch + K[i] + w[i];
        uint32_t S0 = rol32(a, 2) ^ rol32(a, 13) ^ rol32(a, 22);
        uint32_t maj = (a & b) ^ (a & c) ^ (b & c);
        uint32_t t2 = S0 + maj;
        hh = g; g = f; f = e; e = d + t1;
        d = c; c = b; b = a; a = t1 + t2;
    }
    h[0] += a; h[1] += b; h[2] += c; h[3] += d;
    h[4] += e; h[5] += f; h[6] += g; h[7] += hh;
}
inline std::string sha256_hex(const std::string& data) {
    uint32_t h[8] = { 0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19 };
    std::vector<uint8_t> m(data.begin(), data.end());
    m.push_back(0x80);
    while (m.size() % 64 != 56) m.push_back(0);
    uint64_t bitlen = (uint64_t)data.size() * 8;
    for (int i = 7; i >= 0; i--) m.push_back((uint8_t)(bitlen >> (i * 8)));
    for (size_t o = 0; o < m.size(); o += 64) sha256_blocks(h, m.data() + o);
    const char* hex = "0123456789abcdef";
    std::string out;
    for (int i = 0; i < 8; i++)
        for (int b = 28; b >= 0; b -= 4) out += hex[(h[i] >> b) & 15];
    return out;
}
inline std::string sha256_bin(const std::string& d) {
    const std::string hex = sha256_hex(d);
    auto hv = [](char c) { if (c >= '0' && c <= '9') return c - '0'; if (c >= 'a' && c <= 'f') return c - 'a' + 10; return 0; };
    std::string out;
    for (size_t i = 0; i < hex.size(); i += 2) out += (char)((hv(hex[i]) << 4) | hv(hex[i + 1]));
    return out;
}
inline std::string hmac_sha256_bin(const std::string& key, const std::string& data) {
    std::string k = key;
    if (k.size() > 64) k = sha256_bin(k);
    while (k.size() < 64) k.push_back('\0');
    std::string ipad(64, 0x36), opad(64, 0x5c);
    for (int i = 0; i < 64; i++) { ipad[i] ^= k[i]; opad[i] ^= k[i]; }
    return sha256_bin(opad + sha256_bin(ipad + data));
}
inline std::string base64_encode(const std::string& d) {
    static const char* tbl = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    std::string out;
    size_t i = 0;
    while (i + 3 <= d.size()) {
        uint32_t v = ((uint8_t)d[i] << 16) | ((uint8_t)d[i + 1] << 8) | (uint8_t)d[i + 2];
        out += tbl[(v >> 18) & 63]; out += tbl[(v >> 12) & 63]; out += tbl[(v >> 6) & 63]; out += tbl[v & 63];
        i += 3;
    }
    size_t r = d.size() - i;
    if (r == 1) {
        uint32_t v = (uint8_t)d[i] << 16;
        out += tbl[(v >> 18) & 63]; out += tbl[(v >> 12) & 63]; out += "==";
    } else if (r == 2) {
        uint32_t v = ((uint8_t)d[i] << 16) | ((uint8_t)d[i + 1] << 8);
        out += tbl[(v >> 18) & 63]; out += tbl[(v >> 12) & 63]; out += tbl[(v >> 6) & 63]; out += "=";
    }
    return out;
}
inline std::string base64_encode_url(const std::string& d) {
    std::string out = base64_encode(d);
    std::replace(out.begin(), out.end(), '+', '-');
    std::replace(out.begin(), out.end(), '/', '_');
    while (!out.empty() && out.back() == '=') out.pop_back();
    return out;
}
inline std::string base64_decode(const std::string& s) {
    auto hv = [](char c) -> int {
        if (c >= 'A' && c <= 'Z') return c - 'A';
        if (c >= 'a' && c <= 'z') return c - 'a' + 26;
        if (c >= '0' && c <= '9') return c - '0' + 52;
        if (c == '+' || c == '-') return 62;
        if (c == '/' || c == '_') return 63;
        return -1;
    };
    std::string out;
    int v = 0, bits = 0;
    for (char c : s) {
        int d = hv(c);
        if (d < 0) continue;
        v = (v << 6) | d;
        bits += 6;
        if (bits >= 8) { bits -= 8; out += (char)((v >> bits) & 0xff); }
    }
    return out;
}
inline std::string random_hex(int nbytes) {
    static std::atomic<uint64_t> c{0};
    uint64_t seed = std::chrono::high_resolution_clock::now().time_since_epoch().count() ^ (c.fetch_add(1) * 0x9e3779b97f4a7c15ULL);
    std::mt19937_64 g(seed);
    const char* hex = "0123456789abcdef";
    std::string out;
    for (int i = 0; i < nbytes; i++) {
        uint64_t x = g();
        out += hex[(x >> 8) & 15];
        out += hex[x & 15];
    }
    return out;
}
inline std::string random_urlsafe(int nbytes) {
    static const char* tbl = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    static std::atomic<uint64_t> c{0};
    uint64_t seed = std::chrono::high_resolution_clock::now().time_since_epoch().count() ^ (c.fetch_add(1) * 0x9e3779b97f4a7c15ULL);
    std::mt19937_64 g(seed);
    std::string out;
    for (int i = 0; i < nbytes; i++) out += tbl[g() % 64];
    return out;
}
inline std::string sha1_bin(const std::string& data) {
    uint32_t h[5] = { 0x67452301, 0xEFCDAB89, 0x98BADCFE, 0x10325476, 0xC3D2E1F0 };
    std::vector<uint8_t> m(data.begin(), data.end());
    m.push_back(0x80);
    while (m.size() % 64 != 56) m.push_back(0);
    uint64_t bitlen = (uint64_t)data.size() * 8;
    for (int i = 7; i >= 0; i--) m.push_back((uint8_t)(bitlen >> (i * 8)));
    for (size_t o = 0; o < m.size(); o += 64) {
        const uint8_t* p = m.data() + o;
        uint32_t w[80];
        for (int i = 0; i < 16; i++)
            w[i] = ((uint32_t)p[i * 4] << 24) | ((uint32_t)p[i * 4 + 1] << 16) | ((uint32_t)p[i * 4 + 2] << 8) | p[i * 4 + 3];
        for (int i = 16; i < 80; i++) { uint32_t x = w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]; w[i] = rol32(x, 1); }
        uint32_t a = h[0], b = h[1], c = h[2], d = h[3], e = h[4];
        for (int i = 0; i < 80; i++) {
            uint32_t f, k;
            if (i < 20) { f = (b & c) | (~b & d); k = 0x5A827999; }
            else if (i < 40) { f = b ^ c ^ d; k = 0x6ED9EBA1; }
            else if (i < 60) { f = (b & c) | (b & d) | (c & d); k = 0x8F1BBCDC; }
            else { f = b ^ c ^ d; k = 0xCA62C1D6; }
            uint32_t tmp = rol32(a, 5) + f + e + k + w[i];
            e = d; d = c; c = rol32(b, 30); b = a; a = tmp;
        }
        h[0] += a; h[1] += b; h[2] += c; h[3] += d; h[4] += e;
    }
    std::string out;
    for (int i = 0; i < 5; i++)
        for (int b = 24; b >= 0; b -= 8) out += (char)((h[i] >> b) & 0xff);
    return out;
}
inline std::string md5_hex(const std::string& data) {
    uint32_t a0 = 0x67452301, b0 = 0xefcdab89, c0 = 0x98badcfe, d0 = 0x10325476;
    static const uint32_t K[64] = {
        0xd76aa478, 0xe8c7b756, 0x242070db, 0xc1bdceee, 0xf57c0faf, 0x4787c62a, 0xa8304613, 0xfd469501,
        0x698098d8, 0x8b44f7af, 0xffff5bb1, 0x895cd7be, 0x6b901122, 0xfd987193, 0xa679438e, 0x49b40821,
        0xf61e2562, 0xc040b340, 0x265e5a51, 0xe9b6c7aa, 0xd62f105d, 0x02441453, 0xd8a1e681, 0xe7d3fbc8,
        0x21e1cde6, 0xc33707d6, 0xf4d50d87, 0x455a14ed, 0xa9e3e905, 0xfcefa3f8, 0x676f02d9, 0x8d2a4c8a,
        0xfffa3942, 0x8771f681, 0x6d9d6122, 0xfde5380c, 0xa4beea44, 0x4bdecfa9, 0xf6bb4b60, 0xbebfbc70,
        0x289b7ec6, 0xeaa127fa, 0xd4ef3085, 0x04881d05, 0xd9d4d039, 0xe6db99e5, 0x1fa27cf8, 0xc4ac5665,
        0xf4292244, 0x432aff97, 0xab9423a7, 0xfc93a039, 0x655b59c3, 0x8f0ccc92, 0xffeff47d, 0x85845dd1,
        0x6fa87e4f, 0xfe2ce6e0, 0xa3014314, 0x4e0811a1, 0xf7537e82, 0xbd3af235, 0x2ad7d2bb, 0xeb86d391,
    };
    static const int S[64] = { 7,12,17,22,7,12,17,22,7,12,17,22,7,12,17,22, 5,9,14,20,5,9,14,20,5,9,14,20,5,9,14,20, 4,11,16,23,4,11,16,23,4,11,16,23,4,11,16,23, 6,10,15,21,6,10,15,21,6,10,15,21,6,10,15,21 };
    auto rol = [](uint32_t x, int n) { return (x << n) | (x >> (32 - n)); };
    auto le = [](const uint8_t* p) { return (uint32_t)p[0] | ((uint32_t)p[1] << 8) | ((uint32_t)p[2] << 16) | ((uint32_t)p[3] << 24); };
    std::vector<uint8_t> m(data.begin(), data.end());
    uint64_t bitlen = (uint64_t)data.size() * 8;
    m.push_back(0x80);
    while (m.size() % 64 != 56) m.push_back(0);
    for (int i = 0; i < 8; i++) m.push_back((uint8_t)(bitlen >> (i * 8)));
    for (size_t o = 0; o < m.size(); o += 64) {
        const uint8_t* p = m.data() + o;
        uint32_t M[16];
        for (int i = 0; i < 16; i++) M[i] = le(p + i * 4);
        uint32_t A = a0, B = b0, C = c0, D = d0;
        for (int i = 0; i < 64; i++) {
            uint32_t F, g;
            if (i < 16) { F = (B & C) | (~B & D); g = i; }
            else if (i < 32) { F = (D & B) | (~D & C); g = (5 * i + 1) % 16; }
            else if (i < 48) { F = B ^ C ^ D; g = (3 * i + 5) % 16; }
            else { F = C ^ (B | ~D); g = (7 * i) % 16; }
            uint32_t tmp = D;
            D = C; C = B;
            B = B + rol(A + F + K[i] + M[g], S[i]);
            A = tmp;
        }
        a0 += A; b0 += B; c0 += C; d0 += D;
    }
    const char* hex = "0123456789abcdef";
    std::string out;
    auto emit = [&](uint32_t v) {
        for (int b = 0; b < 4; b++) { uint8_t x = (uint8_t)(v >> (b * 8)); out += hex[x >> 4]; out += hex[x & 15]; }
    };
    emit(a0); emit(b0); emit(c0); emit(d0);
    return out;
}

// JWT
inline std::string jwt_sign(const Val& payload, const std::string& secret) {
    std::string h = base64_encode_url("{\"alg\":\"HS256\",\"typ\":\"JWT\"}");
    std::string b = base64_encode_url(to_json(payload));
    std::string body = h + "." + b;
    return body + "." + base64_encode_url(hmac_sha256_bin(secret, body));
}
inline bool jwt_verify(const std::string& token, const std::string& secret) {
    std::vector<std::string> parts;
    std::string cur;
    for (char c : token) { if (c == '.') { parts.push_back(cur); cur.clear(); } else cur += c; }
    parts.push_back(cur);
    if (parts.size() != 3) return false;
    std::string expect = base64_encode_url(hmac_sha256_bin(secret, parts[0] + "." + parts[1]));
    if (expect != parts[2]) return false;
    try {
        Val body = parse_json(base64_decode(parts[1]));
        const Val* exp = body.find("exp");
        if (exp && exp->is_num() && (double)exp->num() < (double)(unix_ms() / 1000)) return false;
    } catch (...) { return false; }
    return true;
}

#endif
