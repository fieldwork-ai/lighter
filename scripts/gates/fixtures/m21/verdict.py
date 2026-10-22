"""verdict.py <result.json>: one line per probe, `ok <what>` or `bad <what> (read ..., then ...)`."""
import json
import sys

WANT = {
    "host-delete-unread": ("old", "deleted on the Mac, open and unread"),
    "host-delete-read": ("old", "deleted on the Mac, open and read"),
    "host-replace-unread": ("old", "saved over on the Mac, open and unread"),
    "host-replace-read": ("old", "saved over on the Mac, open and read"),
    "host-rename-unread": ("old", "moved on the Mac, open"),
    "host-inplace-read": ("new", "edited in place on the Mac, open: the edit is read at once"),
    "guest-delete-unread": ("old", "deleted in the container, open"),
    "guest-replace-unread": ("old", "replaced in the container, open"),
    "guest-tmpfile": ("same", "opened, unlinked and used as scratch in the container"),
}
NAMES = {"old-contents": "old", "new-contents": "new"}

result = json.load(open(sys.argv[1]))
for case, (want, what) in WANT.items():
    got = result[case]
    now = NAMES.get(got["now"].strip(), got["now"].strip())
    later = NAMES.get(got.get("later", got["now"]).strip(), got.get("later", got["now"]).strip())
    if now == want and later == want:
        print("ok", what)
    else:
        print("bad", f"{what} (read {now}, then {later})")
