use hs_compiler::ast::Module;

pub struct Builtin {
    pub module: &'static str,
    pub name: &'static str,
    pub signature: &'static str,
    pub return_type: &'static str,
    pub documentation: &'static str,
}

pub const MODULES: &[(&str, &str)] = &[
    (
        "http",
        "HTTP server types, request helpers, and response values.",
    ),
    ("postgres", "PostgreSQL connection and query operations."),
    (
        "websocket",
        "WebSocket connection, room, and messaging operations.",
    ),
    (
        "crypto",
        "Hashing, HMAC, encoding, UUID, and token helpers.",
    ),
    (
        "json",
        "JSON parsing, serialization, and object-key helpers.",
    ),
    (
        "fs",
        "Bounded filesystem read, write, list, and metadata helpers.",
    ),
    ("jwt", "JWT signing and verification helpers."),
    ("env", "Process environment value access."),
    (
        "runtime",
        "Process arguments, identity, and host information.",
    ),
    ("time", "Unix time, ISO timestamps, and delays."),
];

pub const BUILTINS: &[Builtin] = &[
    builtin(
        "crypto",
        "sha256",
        "crypto.sha256(value)",
        "Str",
        "Returns the lowercase SHA-256 hexadecimal digest of a value.",
    ),
    builtin(
        "crypto",
        "sha256bin",
        "crypto.sha256bin(value)",
        "Str",
        "Returns the SHA-256 digest bytes encoded as text.",
    ),
    builtin(
        "crypto",
        "sha1",
        "crypto.sha1(value)",
        "Str",
        "Returns the lowercase SHA-1 hexadecimal digest of a value.",
    ),
    builtin(
        "crypto",
        "md5",
        "crypto.md5(value)",
        "Str",
        "Returns the lowercase MD5 hexadecimal digest of a value.",
    ),
    builtin(
        "crypto",
        "hmac",
        "crypto.hmac(key, value)",
        "Str",
        "Computes an HMAC-SHA256 digest for a key and value.",
    ),
    builtin(
        "crypto",
        "base64",
        "crypto.base64(value)",
        "Str",
        "Encodes a value with standard Base64.",
    ),
    builtin(
        "crypto",
        "base64url",
        "crypto.base64url(value)",
        "Str",
        "Encodes a value with URL-safe Base64.",
    ),
    builtin(
        "crypto",
        "base64decode",
        "crypto.base64decode(value)",
        "Str",
        "Decodes a Base64 value.",
    ),
    builtin(
        "crypto",
        "uuid",
        "crypto.uuid()",
        "Str",
        "Returns a random UUID string.",
    ),
    builtin(
        "crypto",
        "random_hex",
        "crypto.random_hex(bytes)",
        "Str",
        "Returns random bytes encoded as hexadecimal.",
    ),
    builtin(
        "crypto",
        "token",
        "crypto.token(bytes)",
        "Str",
        "Returns a cryptographically random URL-safe token.",
    ),
    builtin(
        "json",
        "parse",
        "json.parse(text)",
        "dynamic",
        "Parses JSON text into HardScript values.",
    ),
    builtin(
        "json",
        "stringify",
        "json.stringify(value)",
        "Str",
        "Serializes a HardScript value as compact JSON.",
    ),
    builtin(
        "json",
        "keys",
        "json.keys(object)",
        "[Str]",
        "Returns the object keys as a string list.",
    ),
    builtin(
        "env",
        "get",
        "env.get(name)",
        "Str",
        "Returns an environment variable or an empty string.",
    ),
    builtin(
        "fs",
        "read",
        "fs.read(path)",
        "Str",
        "Reads a UTF-8 text file.",
    ),
    builtin(
        "fs",
        "write",
        "fs.write(path, text)",
        "nil",
        "Writes a UTF-8 text file.",
    ),
    builtin(
        "fs",
        "append",
        "fs.append(path, text)",
        "nil",
        "Appends UTF-8 text to a file.",
    ),
    builtin(
        "fs",
        "exists",
        "fs.exists(path)",
        "Bool",
        "Reports whether a filesystem path exists.",
    ),
    builtin(
        "fs",
        "is_dir",
        "fs.is_dir(path)",
        "Bool",
        "Reports whether a path is a directory.",
    ),
    builtin(
        "fs",
        "list",
        "fs.list(path)",
        "[Str]",
        "Lists directory entry names.",
    ),
    builtin(
        "fs",
        "remove",
        "fs.remove(path)",
        "nil",
        "Removes a file or empty directory.",
    ),
    builtin(
        "fs",
        "size",
        "fs.size(path)",
        "Int",
        "Returns a file size in bytes.",
    ),
    builtin(
        "jwt",
        "sign",
        "jwt.sign(payload, secret)",
        "Str",
        "Signs a payload as a compact JWT.",
    ),
    builtin(
        "jwt",
        "verify",
        "jwt.verify(token, secret)",
        "Bool",
        "Verifies a compact JWT and its signature.",
    ),
    builtin(
        "time",
        "now",
        "time.now()",
        "Int",
        "Returns the current Unix time in milliseconds.",
    ),
    builtin(
        "time",
        "iso",
        "time.iso()",
        "Str",
        "Returns the current time as an ISO-8601 string.",
    ),
    builtin(
        "time",
        "sleep",
        "time.sleep(seconds)",
        "nil",
        "Pauses the current worker for a duration.",
    ),
    builtin(
        "runtime",
        "args",
        "runtime.args()",
        "[Str]",
        "Returns command-line arguments.",
    ),
    builtin(
        "runtime",
        "argc",
        "runtime.argc()",
        "Int",
        "Returns the command-line argument count.",
    ),
    builtin(
        "runtime",
        "argv",
        "runtime.argv(index)",
        "Str",
        "Returns one command-line argument.",
    ),
    builtin(
        "runtime",
        "print",
        "runtime.print(value)",
        "nil",
        "Prints a value to standard output.",
    ),
    builtin(
        "runtime",
        "pid",
        "runtime.pid()",
        "Str",
        "Returns the process identifier.",
    ),
    builtin(
        "runtime",
        "hostname",
        "runtime.hostname()",
        "Str",
        "Returns the host name.",
    ),
    builtin(
        "runtime",
        "platform",
        "runtime.platform()",
        "Str",
        "Returns the operating-system platform.",
    ),
    builtin(
        "runtime",
        "cpus",
        "runtime.cpus()",
        "Int",
        "Returns the available CPU count.",
    ),
    builtin(
        "websocket",
        "broadcast",
        "websocket.broadcast(message)",
        "nil",
        "Broadcasts a message to connected clients.",
    ),
    builtin(
        "websocket",
        "broadcast_room",
        "websocket.broadcast_room(room, message)",
        "nil",
        "Broadcasts a message to one room.",
    ),
    builtin(
        "websocket",
        "reply",
        "websocket.reply(message)",
        "nil",
        "Replies to the current WebSocket message.",
    ),
    builtin(
        "websocket",
        "join",
        "websocket.join(room)",
        "nil",
        "Joins the current socket to a room.",
    ),
    builtin(
        "websocket",
        "leave",
        "websocket.leave()",
        "nil",
        "Leaves the current socket's room.",
    ),
    builtin(
        "websocket",
        "self",
        "websocket.self()",
        "Str",
        "Returns the current WebSocket identity.",
    ),
    builtin(
        "postgres",
        "connect",
        "postgres.connect(connection)",
        "Int",
        "Opens a PostgreSQL connection and returns its handle.",
    ),
    builtin(
        "postgres",
        "query",
        "postgres.query(handle, sql)",
        "[dynamic]",
        "Executes SQL and returns rows as HardScript values.",
    ),
];

const fn builtin(
    module: &'static str,
    name: &'static str,
    signature: &'static str,
    return_type: &'static str,
    documentation: &'static str,
) -> Builtin {
    Builtin {
        module,
        name,
        signature,
        return_type,
        documentation,
    }
}

pub fn module_name(module: &Module) -> &'static str {
    match module {
        Module::Http => "http",
        Module::Postgres => "postgres",
        Module::WebSocket => "websocket",
        Module::Crypto => "crypto",
        Module::Json => "json",
        Module::Fs => "fs",
        Module::Jwt => "jwt",
        Module::Env => "env",
        Module::Runtime => "runtime",
        Module::Time => "time",
    }
}

pub fn module_documentation(name: &str) -> Option<&'static str> {
    MODULES
        .iter()
        .find(|(module, _)| *module == name)
        .map(|(_, documentation)| *documentation)
}

pub fn find(module: &str, name: &str) -> Option<&'static Builtin> {
    BUILTINS
        .iter()
        .find(|builtin| builtin.module == module && builtin.name == name)
}

pub fn for_module(module: &str) -> impl Iterator<Item = &'static Builtin> + use<'_> {
    BUILTINS
        .iter()
        .filter(move |builtin| builtin.module == module)
}
