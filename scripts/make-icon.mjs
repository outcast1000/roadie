// Placeholder app icon: a 1024px PNG drawn with a minimal encoder so the repo
// needs no image tooling. Run `node scripts/make-icon.mjs app-icon.png` then
// `cargo tauri icon app-icon.png -o src-tauri/icons`. Replace with real art.
import zlib from "node:zlib";
import fs from "node:fs";

const W = 1024;
const crcTable = [];
for (let n = 0; n < 256; n++) {
  let c = n;
  for (let k = 0; k < 8; k++) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
  crcTable[n] = c >>> 0;
}
const crc = (b) => {
  let c = 0xffffffff;
  for (const x of b) c = crcTable[(c ^ x) & 0xff] ^ (c >>> 8);
  return (c ^ 0xffffffff) >>> 0;
};
const chunk = (t, d) => {
  const l = Buffer.alloc(4);
  l.writeUInt32BE(d.length);
  const td = Buffer.concat([Buffer.from(t), d]);
  const c = Buffer.alloc(4);
  c.writeUInt32BE(crc(td));
  return Buffer.concat([l, td, c]);
};
const raw = Buffer.alloc((W * 4 + 1) * W);
const amber = [0xf5, 0xa5, 0x24];
for (let y = 0; y < W; y++) {
  raw[y * (W * 4 + 1)] = 0;
  for (let x = 0; x < W; x++) {
    const o = y * (W * 4 + 1) + 1 + x * 4;
    const dx = x - 512, dy = y - 512, r = Math.hypot(dx, dy);
    let [R, G, B] = [0x1f, 0x24, 0x2b];
    let A = r > 500 ? 0 : 255;
    const stem = Math.abs(dx + 120) < 70 && Math.abs(dy) < 300;
    const bowl = r < 300 && r > 230 && dy < 0;
    const leg = dy > 40 && dy < 300 && Math.abs(dx - 110 - (dy - 40) * 0.5) < 60;
    if (A && (stem || bowl || leg)) [R, G, B] = amber;
    raw[o] = R; raw[o + 1] = G; raw[o + 2] = B; raw[o + 3] = A;
  }
}
const ihdr = Buffer.alloc(13);
ihdr.writeUInt32BE(W, 0);
ihdr.writeUInt32BE(W, 4);
ihdr[8] = 8; ihdr[9] = 6;
fs.writeFileSync(
  process.argv[2] ?? "app-icon.png",
  Buffer.concat([
    Buffer.from([137, 80, 78, 71, 13, 10, 26, 10]),
    chunk("IHDR", ihdr),
    chunk("IDAT", zlib.deflateSync(raw)),
    chunk("IEND", Buffer.alloc(0)),
  ]),
);
