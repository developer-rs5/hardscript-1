// m5.1: the validation engine. Covers every format predicate, the bounds and
// length rules, enum membership, list element types, regex, presence
// (required/nullable), strict unknown-key rejection, and the HTTP 400 body
// shape that a failed gate produces.
#include <hs_runtime.hpp>
#include <cstdio>
#include <string>

static int fails = 0;
#define CHECK(cond, msg)                                \
    do {                                                \
        if (!(cond)) {                                  \
            std::fprintf(stderr, "FAIL %s\n", msg);     \
            fails++;                                    \
        }                                               \
    } while (0)

static bool rejects(const std::string& schema, const std::string& json) {
    return !hs::valid(schema, hs::parse_json(json));
}

static bool accepts(const std::string& schema, const std::string& json) {
    return hs::valid(schema, hs::parse_json(json));
}

int main() {
    // -- format predicates -------------------------------------------------
    CHECK(hs::v_is_email("a@b.com"), "email ok");
    CHECK(hs::v_is_email("first.last+tag@sub.example.co.uk"), "email ok 2");
    CHECK(!hs::v_is_email("nope"), "email no at");
    CHECK(!hs::v_is_email("a@b"), "email no tld");
    CHECK(!hs::v_is_email("@b.com"), "email no local");
    CHECK(!hs::v_is_email("a b@c.com"), "email no space");

    CHECK(hs::v_is_url("https://example.com/x?y=1#z"), "url ok");
    CHECK(hs::v_is_url("http://localhost:8080"), "url ok 2");
    CHECK(!hs::v_is_url("notaurl"), "url bare word");

    CHECK(hs::v_is_uuid("123e4567-e89b-12d3-a456-426614174000"), "uuid ok");
    CHECK(!hs::v_is_uuid("123e4567-e89b-12d3-a456"), "uuid too short");
    CHECK(!hs::v_is_uuid("zzzzzzzz-e89b-12d3-a456-426614174000"), "uuid bad hex");

    CHECK(hs::v_is_ipv4("10.0.0.1"), "ipv4 ok");
    CHECK(hs::v_is_ipv4("255.255.255.255"), "ipv4 broadcast");
    CHECK(!hs::v_is_ipv4("256.1.1.1"), "ipv4 out of range");
    CHECK(!hs::v_is_ipv4("1.2.3"), "ipv4 too few octets");
    CHECK(hs::v_is_ipv6("::1"), "ipv6 loopback");
    CHECK(hs::v_is_ipv6("2001:db8::8a2e:370:7334"), "ipv6 full");
    CHECK(hs::v_is_ip("10.0.0.1") && hs::v_is_ip("::1"), "ip both families");
    CHECK(!hs::v_is_ip("nope"), "ip garbage");

    CHECK(hs::v_is_phone("+14155552671"), "phone e164");
    CHECK(hs::v_is_phone("4155552671"), "phone national");
    CHECK(!hs::v_is_phone("12"), "phone too short");

    // -- schema with one field per rule -----------------------------------
    hs::register_schema("Reg", {
        hs::vf("email", "Email", {hs::r_required()}),
        hs::vf("age", "Int", {hs::r_min(18), hs::r_max(120), hs::r_required()}),
        hs::vf("nick", "Str", {hs::r_length(2, 8)}),
        hs::vf("site", "Url"),
        hs::vf("ref", "Uuid"),
        hs::vf("ip", "Ip"),
        hs::vf("role", "Enum", {hs::r_enum_one("admin"), hs::r_enum_one("user")}),
        hs::vf("tags", "List", {hs::r_items("Str")}),
        hs::vf("score", "Int", {hs::r_minmax(0, 100)}),
        hs::vf("bio", "Str", {hs::r_max(280), hs::r_nullable()}),
        hs::vf("code", "Str", {hs::r_regex("^[A-Z]{3}-[0-9]{4}$")}),
    });

    const std::string good =
        R"({"email":"a@b.com","age":30,"nick":"ab","site":"https://x.io",)"
        R"("ref":"123e4567-e89b-12d3-a456-426614174000","ip":"10.0.0.1",)"
        R"("role":"admin","tags":["x","y"],"score":50,"bio":"hi","code":"ABC-1234"})";
    CHECK(accepts("Reg", good), "valid payload accepted");

    // presence: `email`/`age` are required, the rest may be absent
    CHECK(accepts("Reg", R"({"email":"a@b.com","age":18})"), "only required keys");
    hs::VResult r = hs::validate("Reg", hs::parse_json(R"({"age":18})"));
    CHECK(!r.ok && r.errors.size() == 1 && r.errors[0].field == "email",
          "missing required reported");

    // a required key that is present but null is still an error
    CHECK(rejects("Reg", R"({"email":null,"age":18})"), "required + null");

    // bounds are inclusive at the edge
    CHECK(accepts("Reg", R"({"email":"a@b.com","age":18})"), "min edge");
    CHECK(accepts("Reg", R"({"email":"a@b.com","age":120})"), "max edge");
    CHECK(rejects("Reg", R"({"email":"a@b.com","age":17})"), "below min");
    CHECK(rejects("Reg", R"({"email":"a@b.com","age":121})"), "above max");
    CHECK(rejects("Reg", R"({"email":"a@b.com","age":18.5})"), "int rejects float");

    // length counts characters, not bytes
    CHECK(accepts("Reg", R"({"email":"a@b.com","age":30,"nick":"ab"})"), "length lo edge");
    CHECK(accepts("Reg", R"({"email":"a@b.com","age":30,"nick":"abcdefgh"})"), "length hi edge");
    CHECK(rejects("Reg", R"({"email":"a@b.com","age":30,"nick":"a"})"), "length below");
    CHECK(rejects("Reg", R"({"email":"a@b.com","age":30,"nick":"abcdefghi"})"), "length above");
    CHECK(accepts("Reg", R"({"email":"a@b.com","age":30,"nick":"héllo"})"),
          "length counts codepoints not bytes");

    // enum membership
    CHECK(accepts("Reg", R"({"email":"a@b.com","age":30,"role":"user"})"), "enum member");
    CHECK(rejects("Reg", R"({"email":"a@b.com","age":30,"role":"root"})"), "enum outsider");

    // list element types
    CHECK(accepts("Reg", R"({"email":"a@b.com","age":30,"tags":[]})"), "empty list ok");
    CHECK(rejects("Reg", R"({"email":"a@b.com","age":30,"tags":[1]})"), "list wrong items");
    CHECK(rejects("Reg", R"({"email":"a@b.com","age":30,"tags":"x"})"), "list not a list");

    // minmax range
    CHECK(accepts("Reg", R"({"email":"a@b.com","age":30,"score":0})"), "range lo edge");
    CHECK(accepts("Reg", R"({"email":"a@b.com","age":30,"score":100})"), "range hi edge");
    CHECK(rejects("Reg", R"({"email":"a@b.com","age":30,"score":101})"), "above range");

    // nullable lets an explicit null through, a non-nullable field does not
    CHECK(accepts("Reg", R"({"email":"a@b.com","age":30,"bio":null})"), "nullable null ok");
    CHECK(rejects("Reg", R"({"email":null,"age":30})"), "non-nullable null");

    // regex, plus a compile-once cache shared across calls
    CHECK(accepts("Reg", R"({"email":"a@b.com","age":30,"code":"XYZ-0000"})"), "regex match");
    CHECK(rejects("Reg", R"({"email":"a@b.com","age":30,"code":"abc"})"), "regex miss");
    const size_t id1 = hs::v_regex_id("^[A-Z]{3}-[0-9]{4}$");
    const size_t id2 = hs::v_regex_id("^[A-Z]{3}-[0-9]{4}$");
    CHECK(id1 == id2, "regex cache dedups");
    CHECK(hs::v_regex_test(id1, "ABC-1234") && !hs::v_regex_test(id1, "nope"),
          "regex test via cache");

    // type mismatches name the field, the wanted type and what arrived
    hs::VResult ty = hs::validate("Reg", hs::parse_json(R"({"email":"a@b.com","age":"old"})"));
    CHECK(!ty.ok && ty.errors.size() == 1 && ty.errors[0].rule == "type" &&
              ty.errors[0].field == "age",
          "type error tagged");

    // every bad key is reported, not just the first
    hs::VResult many = hs::validate(
        "Reg", hs::parse_json(R"({"email":"x","age":5,"role":"root","score":900})"));
    CHECK(many.errors.size() == 4, "all violations reported");

    // unknown keys are ignored by default and rejected under @strict
    CHECK(accepts("Reg", R"({"email":"a@b.com","age":30,"zzz":1})"), "unknown key loose");
    hs::register_schema("Strict", {hs::vf("a", "Int")}, /*strict=*/true);
    CHECK(hs::valid("Strict", hs::parse_json(R"({"a":1})")), "strict accepts known");
    CHECK(rejects("Strict", R"({"a":1,"zzz":2})"), "strict rejects unknown");

    // the HTTP failure body a rejected route returns
    hs::Response resp = hs::validation_response(many);
    CHECK(resp.status == 400, "validation failure is 400");
    CHECK(resp.has_json, "failure body is carried as json");
    const std::string body = hs::to_json(resp.json_v);
    CHECK(body.find("\"validation_failed\"") != std::string::npos, "error tag");
    CHECK(body.find("\"count\":4") != std::string::npos, "error count");
    CHECK(body.find("\"field\":\"role\"") != std::string::npos, "field named in body");

    // validation_gate is the codegen entry point: false + a filled Response
    hs::Response gate;
    CHECK(hs::validation_gate("Reg", hs::parse_json(good), gate), "gate passes valid");
    CHECK(!hs::validation_gate("Reg", hs::parse_json(R"({"age":-1})"), gate), "gate rejects");
    CHECK(gate.status == 400, "gate writes 400");

    // an unknown schema name must not silently accept everything
    CHECK(rejects("NoSuchSchema", R"({"a":1})"), "unknown schema rejects");

    if (fails) {
        std::fprintf(stderr, "%d check(s) failed\n", fails);
        return 1;
    }
    std::printf("ok\n");
    return 0;
}
