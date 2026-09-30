# lighter 0.11.1

Upgrading with Homebrew no longer strands a running machine or stops lighter starting at login.

## What went wrong

If you had run `lighter install` so that lighter starts when you log in, the login agent named the exact Homebrew keg it was installed from, such as `/opt/homebrew/Cellar/lighter/0.10.3/…`. `brew upgrade` installs the new release beside it and deletes that keg, while the machine keeps running from it. Two things then went wrong:

- **The CLI lost track of the running machine.** It recognised the machine partly by asking macOS for its executable's path, and a deleted executable has none. `lighter status`, `lighter doctor` and `lighter stop` failed with "No such file or directory", and the machine could not be stopped from the CLI.
- **lighter did not start at the next login.** The login agent still named the deleted keg, so launchd had nothing to start.

## What changed

- **A running machine whose release was deleted is still found.** `status` shows it, and `stop` and `restart` work, which moves it to the installed release.
- **`lighter install` registers Homebrew's stable path**, `/opt/homebrew/opt/lighter/…`, which Homebrew keeps pointing at the current release.
- **An existing login agent is repaired by itself.** The first lighter command you run after upgrading to 0.11.1 points an agent that names a keg at the stable path. It changes only the file: a running machine keeps running, and the next login or `lighter restart` starts 0.11.1.
- **`lighter doctor` checks the login agent**, and says so if it names a release that no longer exists.

## If you are upgrading with Homebrew from 0.11.0 or earlier

```
brew upgrade lighter
lighter restart
```

`lighter restart` moves the running machine onto 0.11.1. Containers with a restart policy come back by themselves; start the others again, as after any restart. If you upgraded to 0.11.0 with Homebrew and `lighter status` failed afterwards, this is the fix: the machine you could not stop can now be stopped.

Installs made with the install script were not affected: their login agent names a path the script keeps in place.

## Also

- The guest is unchanged from 0.11.0: Linux remains **6.18.52**, and the data epoch remains **1**.
