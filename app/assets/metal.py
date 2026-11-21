# Brushed metal, Panther style: light grey, fine horizontal streaks, a touch darker at the sides.
import random, struct, zlib, sys
W, H = 1100, 760
random.seed(7)
rows = []
for y in range(H):
    n = [random.gauss(0, 1) for _ in range(W + 120)]
    # long horizontal box blur: the streaks
    L = 90
    s = sum(n[:L]); out = []
    for x in range(W):
        out.append(s / L); s += n[x + L] - n[x]
    fine = [random.gauss(0, 1) for _ in range(W)]
    row = bytearray([0])
    for x in range(W):
        t = abs(x / (W - 1) - 0.5) * 2          # 0 centre, 1 edges
        base = 226 - 24 * t ** 1.6               # #E2E2E2 centre, ~#CACACA edges
        v = base + out[x] * 34 + fine[x] * 1.2   # streak amplitude about ±4
        row.append(max(0, min(255, int(round(v)))))
    rows.append(bytes(row))
raw = b"".join(rows)
def chunk(t, d): return struct.pack(">I", len(d)) + t + d + struct.pack(">I", zlib.crc32(t + d) & 0xffffffff)
png = b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", W, H, 8, 0, 0, 0, 0)) + chunk(b"IDAT", zlib.compress(raw, 9)) + chunk(b"IEND", b"")
open(sys.argv[1], "wb").write(png)
