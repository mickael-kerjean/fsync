# What is fdrive?

A cross platform drive client that does not try to own your storage, but rather connects to it wherever it already lives. From S3 and SFTP to FTP, NFS, SMB, IPFS, Azure, Google Cloud, and beyond, it is powered by <a href="https://github.com/mickael-kerjean/filestash">Filestash</a>

<p align="center">
    <img src="https://downloads.filestash.app/img/app-filestash-www-img-screenshots-sync-windows.png" alt="windows screenshot" />
    <em>Windows screenshot of <a href="https://webdav.filestash.app">this webdav</a></em>
</p>

<p align="center">
    <img src="https://downloads.filestash.app/img/app-filestash-www-img-screenshots-sync-apple.png" alt="apple screenshot" />
    <em>Apple screenshot of <a href="https://webdav.filestash.app">this webdav</a></em>
</p>

<p align="center">
    <img src="https://downloads.filestash.app/img/app-filestash-www-img-screenshots-sync-android.png" alt="android screenshot" />
    <em>Android screenshot of <a href="https://webdav.filestash.app">this webdav</a></em>
</p>

<p align="center">
    <img src="https://downloads.filestash.app/img/app-filestash-www-img-screenshots-sync-linux.png" alt="linux screenshot">
    <em>Linux screenshot of <a href="https://webdav.filestash.app">this webdav</a></em>
</p>

## Architecture

We use the hexagonal architecture / ports and adapters pattern. The core owns all policy, everything that decides *what moves where* lives there, once. Each platform adapts its own UI and filesystem technology to it.

Inside the core, sync is a journal: filesystem events are recorded, coalesced into plans (an editor's whole save dance collapses into a single upload), and replayed against the server under leases, a plan only lands if the server still holds the version we last saw, anything newer wins and becomes a conflict you can act on.

| crate | technology |
|---|---|
| `fdrive-core` | `model` (the sync vocabulary: `Operation`, `Plan`, `Fate`, `Conflict`), `engine` (the journal and its state, plan replay, conflict rules, cache policy), the `LocalTree` port, the Filestash HTTP sdk |
| `fdrive-linux` | FUSE, GTK |
| `fdrive-windows` | Win32, CfAPI, ReadDirectoryChangesW, IShellWindows |
| `fdrive-mac` | fuse-t |
| `fdrive-ios` | FileProvider |
| `fdrive-android` | Storage Access Framework (Kotlin wire, UniFFI) |

## Features

- [X] Sane Architecture: one Rust core makes every decision and each platform only implements the lipstick, so sync behaves the same everywhere
- [X] Delta sync: only ship the bytes that changed, not the whole file
- [X] Lives in the tray: the tray icon shows the status, synced, syncing or in trouble at a glance
- [X] Files on demand: a file only downloads when you open it, with both content and listings cached so browsing stays snappy and the next open is instant
- [X] Streaming: large files open immediately, reads are served as the bytes arrive
- [X] Offline mode: cached files stay readable and editable, changes upload once the link returns
- [X] Conflict handling: your work is never lost, when both sides changed you get a `(conflicted copy)` so both versions survive
- [X] Coalesced uploads: the journal folds editor save dances and rapid edits into the fewest server operations, and retries back off instead of hammering the server
- [X] Thumbnails: they are generated on the server through fine tuned C code that works fast!
- [X] Safe deletes: removes and renames carry a lease, they only apply if the server still holds the version you last saw, so nothing you have not seen can ever be destroyed
- [X] Crash safe: unpushed edits survive crashes and restarts
- [X] Reset friendly: rage deleting the local cache partially or entirely is not undefined behavior
- [X] Live view: changes made elsewhere show up in the folder you are browsing, no manual refresh
- [X] Pinning: mark a folder always available offline, it syncs down ahead of time and survives cache cleanup (`setfattr -n user.fdrive.pin -v always <dir>` on linux)
- [X] Ignore list: `node_modules`, `.DS_Store` and friends stay home by default, adjustable via `fdrive.toml`
- [X] Login: done through your server's own login page, password, LDAP, SSO, 2FA all just work
- [X] No Electron: native everything from the tray, filesystem integration into one single binary
- [ ] Profiles: connect to several servers / accounts in the same time
- [ ] Deep integration with Filestash for file locks
- [ ] Deep integration with Filestash for file versioning
- [ ] Deep integration with Filestash for search
- [ ] Explorer actions: surface share links and friends right from the file manager
- [ ] MacOS FileProvider: fuse-t fills the gap until we pay Apple the $100 a year it takes to get there
- [ ] Testing: test on all possible devices / configuration
- [ ] Support for delta download: same as the existing upload but for download. Awaiting for server support
- [ ] MDM integration: preconfigure the client and roll it out across a fleet
- [ ] full POSIX compliance
