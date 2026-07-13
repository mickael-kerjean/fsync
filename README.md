# What is fdrive?

A cross platform drive client that does not try to own your storage, but rather connects to it wherever it already lives. From S3 and SFTP to FTP, NFS, SMB, IPFS, Azure, Google Cloud, and beyond, it is powered by <a href="https://github.com/mickael-kerjean/filestash">Filestash</a>

<p align="center">
    <img src="https://downloads.filestash.app/img/app-filestash-www-img-screenshots-sync-windows.png" alt="windows screenshot" />
    <em>Windows screenshot</em>
</p>

<p align="center">
    <img src="https://downloads.filestash.app/img/app-filestash-www-img-screenshots-sync-apple.png" alt="apple screenshot" />
    <em>Apple screenshot</em>
</p>

<p align="center">
    <img src="https://downloads.filestash.app/img/app-filestash-www-img-screenshots-sync-android.png" alt="android screenshot" />
    <em>Android screenshot</em>
</p>

<p align="center">
    <img src="https://downloads.filestash.app/img/app-filestash-www-img-screenshots-sync-linux.png" alt="linux screenshot">
    <em>Linux screenshot</em>
</p>

## Architecture

We use the hexagonal architecture / ports and adapters pattern. The core owns all policy, everything that decides *what moves where* lives there, once. Each platform adapts its own UI and filesystem technology to it.

| crate | technology |
|---|---|
| `fdrive-core` | `Engine` (ledger, conflict rules, upload scheduler), the `LocalTree` port, the Filestash HTTP sdk |
| `fdrive-linux` | FUSE, GTK |
| `fdrive-windows` | Win32, CfAPI, ReadDirectoryChangesW, IShellWindows |
| `fdrive-mac` | fuse-t |
| `fdrive-ios` | FileProvider |
| `fdrive-android` | Storage Access Framework (Kotlin wire, UniFFI) |
