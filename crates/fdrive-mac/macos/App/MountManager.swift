import AppKit

enum MountManager {
    private static var binary: String {
        if let bundled = Bundle.main.url(forResource: "filestashfs", withExtension: nil) {
            return bundled.path
        }
        if let env = ProcessInfo.processInfo.environment["FILESTASH_FUSE_BIN"], !env.isEmpty {
            return env
        }
        return (NSHomeDirectory() as NSString)
            .appendingPathComponent("Downloads/fsync/target/release/filestashfs")
    }

    static let mountPoint = (NSHomeDirectory() as NSString).appendingPathComponent("Filestash")

    private static var process: Process?

    static func mount(server: String, token: String) throws {
        unmount()
        try FileManager.default.createDirectory(
            atPath: mountPoint, withIntermediateDirectories: true)
        let p = Process()
        p.executableURL = URL(fileURLWithPath: binary)
        p.arguments = ["-f", "-s", "-o", "volname=Filestash", mountPoint]
        var env = ProcessInfo.processInfo.environment
        env["FILESTASH_URL"] = server
        env["FILESTASH_TOKEN"] = token
        p.environment = env
        try p.run()
        process = p
    }

    static func unmount() {
        let umount = Process()
        umount.executableURL = URL(fileURLWithPath: "/sbin/umount")
        umount.arguments = [mountPoint]
        try? umount.run()
        umount.waitUntilExit()
        process?.terminate()
        process = nil
    }

    @MainActor
    static func open() {
        NSWorkspace.shared.open(URL(fileURLWithPath: mountPoint))
    }
}
