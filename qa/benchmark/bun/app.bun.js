Bun.serve({
  port: 8080,
  hostname: "127.0.0.1",
  fetch(req) {
    const url = new URL(req.url);
    const path = url.pathname;
    if (req.method === "GET" && path === "/") return Response.json({ ok: true });
    const m = path.match(/^\/hello\/(.+)$/);
    if (req.method === "GET" && m) return Response.json({ hello: m[1] });
    if (req.method === "POST" && path === "/echo") {
      return req.json().then((b) => Response.json({ echo: b }));
    }
    return new Response("not found", { status: 404 });
  },
});