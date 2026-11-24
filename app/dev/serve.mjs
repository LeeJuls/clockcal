// Dev-only static server: serves ../src and injects dev/mock.js (fake Tauri IPC) into index.html.
// Usage: node dev/serve.mjs [port]   then open http://127.0.0.1:<port>/?s=normal
import http from "node:http";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const src = path.resolve(here, "..", "src");
const port = Number(process.argv[2] || 5179);
const types = { ".html": "text/html; charset=utf-8", ".js": "text/javascript; charset=utf-8", ".css": "text/css; charset=utf-8" };

http.createServer((req, res) => {
  const url = new URL(req.url, "http://x");
  let file;
  if (url.pathname === "/__dev/mock.js") file = path.join(here, "mock.js");
  else {
    const rel = url.pathname === "/" ? "index.html" : decodeURIComponent(url.pathname.slice(1));
    file = path.resolve(src, rel);
    if (!file.startsWith(src)) { res.writeHead(403).end(); return; }
  }
  fs.readFile(file, (err, buf) => {
    if (err) { res.writeHead(404).end("not found"); return; }
    let body = buf;
    if (path.basename(file) === "index.html") {
      body = Buffer.from(buf.toString("utf8").replace("<script src=\"strings.js\">", "<script src=\"/__dev/mock.js\"></script>\n    <script src=\"strings.js\">"));
    }
    res.writeHead(200, { "content-type": types[path.extname(file)] || "application/octet-stream", "cache-control": "no-store" });
    res.end(body);
  });
}).listen(port, "127.0.0.1", () => console.log(`dev server http://127.0.0.1:${port}/`));
