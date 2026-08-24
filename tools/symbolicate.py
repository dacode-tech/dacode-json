import gzip, json, sys, subprocess, bisect, collections, re

BASE = 0x100000000  # Mach-O __TEXT vmaddr for a non-PIE-slid arm64 exe

def symtab(binpath):
    out = subprocess.run(["nm","-n",binpath],capture_output=True,text=True).stdout
    addrs, names = [], []
    for line in out.split("\n"):
        m = re.match(r'^([0-9a-f]{8,16}) [tTsS] (.+)$', line)
        if m:
            addrs.append(int(m.group(1),16)); names.append(m.group(2))
    return addrs, names

def demangle(names):
    p = subprocess.run(["rustfilt"],input="\n".join(names),capture_output=True,text=True)
    if p.returncode == 0 and p.stdout.strip():
        return p.stdout.split("\n")
    return names

def main(w, binpath):
    d = json.load(gzip.open(f"/tmp/prof_{w}.json.gz"))
    addrs, names = symtab(binpath)
    libnames = {i: l.get("debugName","?") for i,l in enumerate(d.get("libs",[]))}
    self_time = collections.Counter()
    for th in d["threads"]:
        ft, tbl, st, strs = th["funcTable"], th["frameTable"], th["stackTable"], th["stringArray"]
        res = th["resourceTable"]
        for s in th["samples"]["stack"]:
            if s is None: continue
            fr = st["frame"][s]
            fn = tbl["func"][fr]
            raw = strs[ft["name"][fn]]
            ridx = ft["resource"][fn]
            lib = "?"
            if ridx is not None and ridx >= 0 and ridx < res["length"]:
                li = res.get("lib",[None])[ridx]
                if li is not None: lib = libnames.get(li,"?")
            if raw.startswith("0x") and lib == "profile":
                a = BASE + int(raw,16)
                i = bisect.bisect_right(addrs, a) - 1
                nm = names[i] if 0 <= i < len(names) else raw
                self_time[nm] += 1
            else:
                self_time[f"[{lib}] {raw}"] += 1
    n = sum(self_time.values()) or 1
    top = self_time.most_common(16)
    dem = demangle([k for k,_ in top])
    print(f"\n=== {w}   {n} samples, self time ===")
    for (k,c), pretty in zip(top, dem):
        pretty = re.sub(r'::h[0-9a-f]{16}$','',pretty.strip())
        if len(pretty) > 82: pretty = pretty[:79]+"..."
        print(f"  {100*c/n:5.1f}%  {pretty}")

main(sys.argv[1], sys.argv[2])
