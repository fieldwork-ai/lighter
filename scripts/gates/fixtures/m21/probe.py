"""The container half of the semantics probes: opens each case's file, says
ready, waits for the Mac to act, then reads through the descriptor it kept.

Results go to /m/result.json as {case: {now, later}}, each a string: the bytes
read, or the errno name."""
import errno, json, os, sys, time

M = "/m"
OLD, NEW = b"old-contents\n", b"new-contents\n"


def read_fd(fd):
    try:
        os.lseek(fd, 0, 0)
        return os.read(fd, 4096).decode(errors="replace")
    except OSError as e:
        return errno.errorcode.get(e.errno, str(e.errno))


def path(case):
    return f"{M}/{case}/f"


# Mac-side cases: the Mac acts between "ready" and "go".
HOST = {
    "host-delete-unread": False,      # opened, never read, then rm on the Mac
    "host-delete-read": True,         # read once first, so the guest has it cached
    "host-replace-unread": False,     # atomic save on the Mac: write tmp, rename over
    "host-replace-read": True,
    "host-rename-unread": False,      # moved to another folder on the Mac
    "host-inplace-read": True,        # same file, new contents written in place: should show new
}


def guest_cases():
    out = {}
    for case in ("guest-delete-unread", "guest-replace-unread", "guest-tmpfile"):
        os.makedirs(f"{M}/{case}", exist_ok=True)
    # Deleted by the guest while open.
    p = path("guest-delete-unread")
    with open(p, "wb") as f:
        f.write(OLD)
    fd = os.open(p, os.O_RDONLY)
    os.unlink(p)
    time.sleep(0.5)
    out["guest-delete-unread"] = {"now": read_fd(fd)}
    os.close(fd)
    # Another file renamed over it by the guest while open.
    p = path("guest-replace-unread")
    with open(p, "wb") as f:
        f.write(OLD)
    fd = os.open(p, os.O_RDONLY)
    with open(p + ".tmp", "wb") as f:
        f.write(NEW)
    os.rename(p + ".tmp", p)
    time.sleep(0.5)
    out["guest-replace-unread"] = {"now": read_fd(fd)}
    os.close(fd)
    # The temp-file idiom: open, unlink, then use it as scratch.
    p = path("guest-tmpfile")
    fd = os.open(p, os.O_RDWR | os.O_CREAT, 0o600)
    os.unlink(p)
    blob = os.urandom(1 << 20)
    try:
        os.write(fd, blob)
        time.sleep(0.5)
        os.lseek(fd, 0, 0)
        got = b""
        while len(got) < len(blob):
            chunk = os.read(fd, len(blob) - len(got))
            if not chunk:
                break
            got += chunk
        out["guest-tmpfile"] = {"now": "same" if got == blob else f"differs ({len(got)} bytes)"}
    except OSError as e:
        out["guest-tmpfile"] = {"now": errno.errorcode.get(e.errno, str(e.errno))}
    os.close(fd)
    return out


def main():
    fds = {}
    for case, read_first in HOST.items():
        fd = os.open(path(case), os.O_RDONLY)
        if read_first:
            read_fd(fd)
        fds[case] = fd
    open(f"{M}/ready", "w").close()
    while not os.path.exists(f"{M}/go"):
        time.sleep(0.01)
    result = {case: {"now": read_fd(fd)} for case, fd in fds.items()}
    time.sleep(1.0)
    for case, fd in fds.items():
        result[case]["later"] = read_fd(fd)
    result.update(guest_cases())
    with open(f"{M}/result.json.tmp", "w") as f:
        json.dump(result, f, indent=1)
    os.rename(f"{M}/result.json.tmp", f"{M}/result.json")


main()
