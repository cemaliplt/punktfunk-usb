#if os(iOS)
import Foundation
import os.log
import PunktfunkCore

private let logger = Logger(subsystem: "io.unom.punktfunk", category: "USBRelay")

/// Manages the in-process USB tunnel relay (`punktfunk_usb_*`).
public final class USBRelay: @unchecked Sendable {
    public static let shared = USBRelay()

    private let lock = NSLock()
    private var relay: OpaquePointer?

    public static let defaultTCPPort: UInt16 = 9779
    public static let defaultUDPPort: UInt16 = 9777

    private init() {}

    /// Start the relay if not already running.
    public func start(tcpPort: UInt16 = defaultTCPPort, udpPort: UInt16 = defaultUDPPort) {
        lock.lock()
        defer { lock.unlock() }

        if relay != nil {
            return
        }

        var out: OpaquePointer?
        let rc = punktfunk_usb_start(tcpPort, udpPort, &out)
        if rc == 0, let handle = out {
            relay = handle
            logger.info("USB tunnel relay started (TCP \(tcpPort), UDP \(udpPort))")
        } else {
            logger.error("Failed to start USB tunnel relay (TCP \(tcpPort), UDP \(udpPort)), error code: \(rc)")
        }
    }

    /// Stop the relay if running.
    public func stop() {
        lock.lock()
        defer { lock.unlock() }

        if let handle = relay {
            punktfunk_usb_stop(handle)
            relay = nil
            logger.info("USB tunnel relay stopped")
        }
    }

    /// Restart the relay (stops then starts).
    public func restart(tcpPort: UInt16 = defaultTCPPort, udpPort: UInt16 = defaultUDPPort) {
        stop()
        start(tcpPort: tcpPort, udpPort: udpPort)
    }

    /// Current statistics snapshot, if running.
    public func stats() -> PfUsbStats? {
        lock.lock()
        defer { lock.unlock() }

        guard let handle = relay else { return nil }
        var s = PfUsbStats()
        let rc = punktfunk_usb_stats(handle, &s)
        return rc == 0 ? s : nil
    }

    public var isRunning: Bool {
        lock.lock()
        defer { lock.unlock() }
        return relay != nil
    }
}
#endif
