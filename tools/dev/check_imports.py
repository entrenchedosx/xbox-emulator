#!/usr/bin/env python3
"""Resolve a PE image's imports against the DLLs that will actually be present.

Usage: check-imports.py <exe> [<search dir> ...]

Every imported module is looked up in the search directories (in order) and then
in System32. Name imports must appear in the target's export table; ordinal
imports must be inside the target's exported ordinal range. Nothing is assumed
about which libraries the emulator provides: the report says what resolves and
what does not, and from where.
"""
import mmap
import os
import struct
import sys


class Pe:
    def __init__(self, path):
        self.path = path
        f = open(path, "rb")
        self.mm = mmap.mmap(f.fileno(), 0, access=mmap.ACCESS_READ)
        d = self.mm
        e = struct.unpack_from("<I", d, 0x3C)[0]
        if d[e:e + 4] != b"PE\0\0":
            raise ValueError("not a PE image")
        nsec = struct.unpack_from("<H", d, e + 6)[0]
        optsz = struct.unpack_from("<H", d, e + 20)[0]
        opt = e + 24
        self.magic = struct.unpack_from("<H", d, opt)[0]
        self.secs = []
        for i in range(nsec):
            o = opt + optsz + i * 40
            vs, va, rs, ra = struct.unpack_from("<IIII", d, o + 8)
            self.secs.append((va, vs, ra, rs))
        self.dd = opt + (112 if self.magic == 0x20B else 96)
        self._imports = None

    def r2o(self, rva):
        for va, vs, ra, rs in self.secs:
            if va <= rva < va + max(vs, rs):
                return ra + (rva - va)
        return None

    def cstr(self, o, lim=512):
        if o is None:
            return ""
        end = self.mm.find(b"\0", o, o + lim)
        if end < 0:
            end = o + lim
        return self.mm[o:end].decode("latin1", "replace")

    def imports(self):
        if self._imports is not None:
            return self._imports
        out = []
        rva, _ = struct.unpack_from("<II", self.mm, self.dd + 8)
        if rva == 0:
            self._imports = out
            return out
        o = self.r2o(rva)
        for _ in range(256):
            ilt, _ts, _fc, name_rva, iat = struct.unpack_from("<IIIII", self.mm, o)
            if name_rva == 0:
                break
            dll = self.cstr(self.r2o(name_rva), 64)
            thunks = []
            t = self.r2o(ilt or iat)
            if t is not None:
                for _ in range(65536):
                    v = struct.unpack_from("<Q", self.mm, t)[0]
                    if v == 0:
                        break
                    if (v >> 63) & 1:
                        thunks.append(("ord", v & 0xFFFF))
                    else:
                        thunks.append(("name", self.cstr(self.r2o((v & 0x7FFFFFFF) + 2))))
                    t += 8
            out.append((dll, thunks))
            o += 20
        self._imports = out
        return out

    def exports(self):
        rva, size = struct.unpack_from("<II", self.mm, self.dd)
        if rva == 0 or size == 0:
            return {}, 0, 0
        o = self.r2o(rva)
        if o is None:
            return {}, 0, 0
        (_c, _t, _maj, _min, _name, base, nfunc, nname,
         afunc, aname, aord) = struct.unpack_from("<IIHHIIIIIII", self.mm, o)
        names = {}
        ao = self.r2o(aord)
        no = self.r2o(aname)
        if ao is not None and no is not None:
            for i in range(nname):
                idx = struct.unpack_from("<H", self.mm, ao + i * 2)[0]
                nr = struct.unpack_from("<I", self.mm, no + i * 4)[0]
                names[self.cstr(self.r2o(nr))] = idx + base
        return names, base, base + nfunc - 1


def find_module(name, dirs):
    base = name
    for ext in ("", ".dll"):
        cand = base + ext
        for d in dirs:
            p = os.path.join(d, cand)
            if os.path.isfile(p):
                return p
    return None


def main():
    if len(sys.argv) < 2:
        print(__doc__)
        return 2
    exe = sys.argv[1]
    dirs = [os.path.dirname(os.path.abspath(exe))] + [os.path.abspath(d) for d in sys.argv[2:]]
    sys32 = os.path.join(os.environ.get("WINDIR", r"C:\Windows"), "System32")
    dirs_lookup = dirs + [sys32]

    pe = Pe(exe)
    imps = pe.imports()
    total = sum(len(t) for _, t in imps)
    print(f"{os.path.basename(exe)}: {len(imps)} modules, {total} imports")
    print(f"search: {', '.join(dirs)} , System32\n")

    missing_mod, missing_fn, resolved = [], [], 0
    for dll, thunks in imps:
        path = find_module(dll, dirs_lookup)
        if path is None:
            missing_mod.append((dll, len(thunks)))
            print(f"  MODULE NOT FOUND  {dll}  ({len(thunks)} imports)")
            continue
        try:
            tp = Pe(path)
            names, lo, hi = tp.exports()
        except Exception as exc:  # unreadable target
            print(f"  unreadable {path}: {exc}")
            continue
        where = "System32" if os.path.dirname(path).lower() == sys32.lower() else "local"
        bad = []
        for kind, val in thunks:
            if kind == "name":
                if val not in names:
                    bad.append(val)
            else:
                if lo == 0 or not (lo <= val <= hi):
                    bad.append(f"#{val}")
        resolved += len(thunks) - len(bad)
        flag = "ok      " if not bad else "MISSING "
        print(f"  {flag}{dll:<34} {len(thunks):>4} imports  [{where}]")
        for b in bad:
            missing_fn.append((dll, b))
            print(f"            -> unresolved: {b}")

    print(f"\nresolved {resolved}/{total} imports")
    if missing_mod:
        print("missing modules:", ", ".join(f"{d} ({n})" for d, n in missing_mod))
    if missing_fn:
        print(f"unresolved symbols ({len(missing_fn)}):")
        for d, b in missing_fn:
            print(f"  {d}!{b}")
    return 0 if not missing_mod and not missing_fn else 1


if __name__ == "__main__":
    sys.exit(main())
