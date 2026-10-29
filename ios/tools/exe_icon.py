"""The icon groups in a Windows PE's resources, each as an .ico: exe_icon.py <exe> <out dir>.

Run on the player's own WoW.exe by ios/make-icon.sh; nothing it reads or writes is committed."""
import struct, sys, os

data = open(sys.argv[1], 'rb').read()
out = sys.argv[2]
pe = struct.unpack_from('<I', data, 0x3c)[0]
assert data[pe:pe+4] == b'PE\0\0'
nsec = struct.unpack_from('<H', data, pe+6)[0]
optsz = struct.unpack_from('<H', data, pe+20)[0]
opt = pe + 24
magic = struct.unpack_from('<H', data, opt)[0]
dd = opt + (96 if magic == 0x10b else 112)
rsrc_rva = struct.unpack_from('<I', data, dd + 2*8)[0]
secs = []
for i in range(nsec):
    s = pe + 24 + optsz + i*40
    vsize, va, rawsz, rawptr = struct.unpack_from('<IIII', data, s+8)
    secs.append((va, max(vsize, rawsz), rawptr))
def off(rva):
    for va, sz, raw in secs:
        if va <= rva < va + sz:
            return rva - va + raw
    raise ValueError(hex(rva))
base = off(rsrc_rva)
def entries(d):
    named, ids = struct.unpack_from('<HH', data, base + d + 12)
    for i in range(named + ids):
        name, target = struct.unpack_from('<II', data, base + d + 16 + i*8)
        yield name, target
def leaves(d):
    for _, t in entries(d):
        if t & 0x80000000:
            yield from leaves(t & 0x7fffffff)
        else:
            rva, size = struct.unpack_from('<II', data, base + t)
            yield data[off(rva):off(rva)+size]
types = {name: t for name, t in entries(0)}
icons = {}
for name, t in entries(types[3] & 0x7fffffff):
    icons[name] = next(leaves(t & 0x7fffffff))
for gi, (gname, gt) in enumerate(entries(types[14] & 0x7fffffff)):
    grp = next(leaves(gt & 0x7fffffff))
    _, _, count = struct.unpack_from('<HHH', grp, 0)
    items = [struct.unpack_from('<BBBBHHIH', grp, 6 + i*14) for i in range(count)]
    header = struct.pack('<HHH', 0, 1, count)
    dirs, blobs, at = b'', b'', 6 + 16*count
    for w, h, colors, res, planes, bpp, size, iid in items:
        blob = icons[iid]
        dirs += struct.pack('<BBBBHHII', w, h, colors, res, planes, bpp, len(blob), at)
        blobs += blob; at += len(blob)
        print(f'group {gi}: {w or 256}x{h or 256} {bpp} bpp')
    open(os.path.join(out, f'group{gi}.ico'), 'wb').write(header + dirs + blobs)
