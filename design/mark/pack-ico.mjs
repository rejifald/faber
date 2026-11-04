// Packs square PNGs into a Windows .ico (PNG-compressed entries, Vista and later).
// Usage: node pack-ico.mjs <out.ico> <size>.png...   e.g. node pack-ico.mjs icon.ico 16.png 32.png
import { readFileSync, writeFileSync } from 'node:fs';
import { basename } from 'node:path';

const ICO_HEADER_BYTES = 6;
const ICO_ENTRY_BYTES = 16;
const ICO_TYPE_ICON = 1;
const BITS_PER_PIXEL = 32;
/// An .ico entry stores its size in one byte; 0 means 256.
const MAX_BYTE_SIZE = 256;

const [out, ...inputs] = process.argv.slice(2);
if (!out || inputs.length === 0) {
  throw new Error('usage: node pack-ico.mjs <out.ico> <size>.png...');
}

const images = inputs.map((file) => {
  const size = Number.parseInt(basename(file), 10);
  if (!(size > 0 && size <= MAX_BYTE_SIZE)) throw new Error(`cannot read a size from ${file}`);
  return { size, png: readFileSync(file) };
});

const header = Buffer.alloc(ICO_HEADER_BYTES);
header.writeUInt16LE(ICO_TYPE_ICON, 2);
header.writeUInt16LE(images.length, 4);

let offset = ICO_HEADER_BYTES + ICO_ENTRY_BYTES * images.length;
const entries = images.map(({ size, png }) => {
  const entry = Buffer.alloc(ICO_ENTRY_BYTES);
  const byteSize = size === MAX_BYTE_SIZE ? 0 : size;
  entry.writeUInt8(byteSize, 0);
  entry.writeUInt8(byteSize, 1);
  entry.writeUInt16LE(1, 4);
  entry.writeUInt16LE(BITS_PER_PIXEL, 6);
  entry.writeUInt32LE(png.length, 8);
  entry.writeUInt32LE(offset, 12);
  offset += png.length;
  return entry;
});

writeFileSync(out, Buffer.concat([header, ...entries, ...images.map((image) => image.png)]));
