// Bundle the 3D office into the server's static folder, plus a gzipped copy the
// server sends to browsers that accept it (560 KB → about 140 KB).
import { build } from "esbuild";
import { readFileSync, writeFileSync } from "node:fs";
import { gzipSync } from "node:zlib";

const out = "../crates/agora-server/static/office.js";
await build({
  entryPoints: ["src/office.js"], bundle: true, minify: true, format: "esm", target: "es2022",
  legalComments: "none", outfile: out, logLevel: "error",
});
const js = readFileSync(out);
writeFileSync(out + ".gz", gzipSync(js, { level: 9, mtime: 0 }));
console.log(`office.js ${js.length} B, gzipped ${gzipSync(js, { level: 9 }).length} B`);
