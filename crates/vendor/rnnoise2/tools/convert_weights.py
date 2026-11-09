#!/usr/bin/env python3
"""Convert RNNoise C weight arrays into the upstream binary weight blob.

Usage:
  python3 -I convert_weights.py <rnnoise_data.c> <out.bin>

Reads `rnnoise_data.c` / `rnnoise_data_little.c` from the RNNoise model tarball
(see ../README.md for URL and SHA-256) and writes the same byte stream as upstream's
`dump_weights_blob` (`src/write_weights.c`) built with the default configure options,
i.e. with DISABLE_DEBUG_FLOAT defined, so the `*_weights_float` debug copies of the
int8 matrices are left out. Also writes `<out.bin>.sha256`.

Record format (`src/nnet.h` WeightHead, little endian, 64-byte header):
  char head[4] = "DNNw"; int32 version = 0; int32 type; int32 size (bytes);
  int32 block_size = size rounded up to 64; char name[44] (NUL padded, last byte 0);
  then `size` data bytes and `block_size - size` zero bytes.

Python stdlib only, deterministic (same input -> same bytes).
"""

import hashlib
import os
import re
import struct
import sys

WEIGHT_BLOCK_SIZE = 64
NAME_SIZE = 44
TYPE_CODES = {"float": 0, "int": 1, "qweight": 2, "int8": 3}
# C element type -> (struct format, byte size)
C_TYPES = {"float": ("f", 4), "int": ("i", 4), "opus_int8": ("b", 1)}
# Macros the default upstream build defines (configure.ac: --enable-dnn-debug-float=no).
INITIAL_DEFINES = {"DISABLE_DEBUG_FLOAT", "DUMP_BINARY_WEIGHTS"}

ARRAY_RE = re.compile(r"^static const (\w+) (\w+)\[(\d+)\] = \{$")
LIST_RE = re.compile(r'^\{"(\w+)",\s*WEIGHTS_(\w+)_TYPE, sizeof\((\w+)\), (\w+)\},$')
DEFINE_RE = re.compile(r"^#define (\w+)(?:\s+(\S+))?$")


def parse(path):
  """Returns the blob records in `rnnoise_arrays[]` order: (name, type_code, payload)."""
  defines = set(INITIAL_DEFINES)
  macros = {}
  arrays = {}
  records = []
  stack = []  # one bool per open #if: is this branch active
  cur = None  # (ctype, name, count, values) while inside an array body
  with open(path, encoding="ascii") as f:
    for lineno, raw in enumerate(f, 1):
      line = raw.strip()
      active = all(stack)
      if cur is not None:
        if line == "};":
          ctype, name, count, values = cur
          if len(values) != count:
            sys.exit(f"{path}:{lineno}: {name} has {len(values)} values, declared {count}")
          arrays[name] = (ctype, values)
          cur = None
        elif line:
          cur[3].extend(v for v in line.split(",") if v.strip())
        continue
      if line.startswith("#"):
        words = line.split()
        if words[0] == "#ifdef":
          stack.append(words[1] in defines)
        elif words[0] == "#ifndef":
          stack.append(words[1] not in defines)
        elif words[0] == "#else":
          stack[-1] = not stack[-1]
        elif words[0] == "#endif":
          stack.pop()
        elif words[0] == "#define":
          m = DEFINE_RE.match(line)
          if m is None:
            sys.exit(f"{path}:{lineno}: unsupported #define: {line}")
          if active:
            defines.add(m.group(1))
            macros[m.group(1)] = m.group(2)
        elif words[0] == "#include":
          pass
        else:
          sys.exit(f"{path}:{lineno}: unsupported directive: {line}")
        continue
      if not active:
        continue
      m = ARRAY_RE.match(line)
      if m:
        ctype, name, count = m.group(1), m.group(2), int(m.group(3))
        if ctype not in C_TYPES:
          sys.exit(f"{path}:{lineno}: unsupported element type {ctype}")
        cur = (ctype, name, count, [])
        continue
      m = LIST_RE.match(line)
      if m:
        name, tname, sname, dname = m.groups()
        if not (name == tname == sname == dname):
          sys.exit(f"{path}:{lineno}: inconsistent list entry: {line}")
        wtype = macros.get(f"WEIGHTS_{name}_TYPE", "")
        if not wtype.startswith("WEIGHT_TYPE_"):
          sys.exit(f"{path}:{lineno}: no type macro for {name}")
        ctype, values = arrays[name]
        fmt, _ = C_TYPES[ctype]
        if fmt == "f":
          # Literals are the exact decimal of a float32 widened to double, so the
          # double -> float32 rounding in struct.pack is exact (checked against dump_weights_blob).
          payload = struct.pack(f"<{len(values)}f", *(float(v) for v in values))
        else:
          payload = struct.pack(f"<{len(values)}{fmt}", *(int(v, 0) for v in values))
        records.append((name, TYPE_CODES[wtype[len("WEIGHT_TYPE_"):]], payload))
  if stack or cur is not None:
    sys.exit(f"{path}: unterminated #if or array")
  if not records:
    sys.exit(f"{path}: no rnnoise_arrays entries found")
  return records


def blob(records):
  out = bytearray()
  for name, type_code, payload in records:
    encoded = name.encode("ascii")
    if len(encoded) >= NAME_SIZE:
      sys.exit(f"name too long for the 44-byte header field: {name}")
    size = len(payload)
    block_size = (size + WEIGHT_BLOCK_SIZE - 1) // WEIGHT_BLOCK_SIZE * WEIGHT_BLOCK_SIZE
    out += struct.pack("<4siiii44s", b"DNNw", 0, type_code, size, block_size, encoded)
    out += payload
    out += bytes(block_size - size)
  return bytes(out)


def main():
  if len(sys.argv) != 3:
    sys.exit(__doc__)
  src, dst = sys.argv[1], sys.argv[2]
  data = blob(parse(src))
  with open(dst, "wb") as f:
    f.write(data)
  digest = hashlib.sha256(data).hexdigest()
  with open(dst + ".sha256", "w", encoding="ascii") as f:
    f.write(f"{digest}  {os.path.basename(dst)}\n")
  print(f"{dst}: {len(data)} bytes, sha256 {digest}")


if __name__ == "__main__":
  main()
