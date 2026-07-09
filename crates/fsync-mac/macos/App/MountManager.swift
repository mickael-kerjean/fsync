import AppKit

enum MountManager {
    private static let binary = "/Users/m1/Downloads/fsync/target/release/filestashfs"

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
