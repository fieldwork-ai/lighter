import errno, os, sys, time
M = "/m/race"
waits = [0, 1, 2, 3, 4, 6, 8, 12]
res = {}
for w in waits:
    ok = 0
    for i in range(10):
        p = f"{M}/f{w}-{i}"
        fd = os.open(p, os.O_RDONLY)
        time.sleep(w / 1000)
        open(f"{p}.go", "w").close()
        while not os.path.exists(f"{p}.done"): time.sleep(0.0002)
        try:
            os.lseek(fd, 0, 0); ok += os.read(fd, 100) == b"old\n"
        except OSError: pass
        os.close(fd)
    res[w] = ok
print(" ".join(f"{w}ms:{n}/10" for w, n in res.items()))
