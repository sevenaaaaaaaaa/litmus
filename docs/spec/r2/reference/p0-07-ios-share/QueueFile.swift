// 鹿蕊 · iOS 分享队列（P0-07）
// 只做一件事：把「从别的 App 分享过来的一段内容」安全地放进 Mac 能读到的队列里。
// 三条纪律：原子写（临时文件 + rename）、幂等（同一内容不重复入队）、失败留稿。
import Foundation

public struct CaptureItem: Codable, Sendable {
    public let id: String              // 幂等键：sha256(kind + text + 分钟桶)
    public let kind: String            // text | url | image | file
    public let text: String
    public let note: String?
    public let sourceApp: String?
    public let createdAt: String       // ISO8601
    public let device: String
    public let attachments: [String]   // App Group 内的相对路径

    public init(id: String, kind: String, text: String, note: String? = nil,
                sourceApp: String? = nil, createdAt: String, device: String,
                attachments: [String] = []) {
        self.id = id; self.kind = kind; self.text = text; self.note = note
        self.sourceApp = sourceApp; self.createdAt = createdAt
        self.device = device; self.attachments = attachments
    }
}

public enum QueueError: Error, CustomStringConvertible {
    case noAppGroup
    case writeFailed(String)

    public var description: String {
        switch self {
        case .noAppGroup: return "找不到 App Group 容器：检查 entitlements 的 group.com.understory.litmus"
        case .writeFailed(let why): return "没写成：\(why)（内容已保留，可重试）"
        }
    }
}

public final class CaptureQueue: @unchecked Sendable {
    public static let appGroup = "group.com.understory.litmus"

    private let fm = FileManager.default
    private let root: URL
    private let queueDir: URL
    private let tmpDir: URL
    private let lock = NSLock()

    public init() throws {
        guard let container = FileManager.default.containerURL(
            forSecurityApplicationGroupIdentifier: Self.appGroup
        ) else { throw QueueError.noAppGroup }
        root = container.appendingPathComponent("litmus", isDirectory: true)
        queueDir = root.appendingPathComponent("queue", isDirectory: true)
        tmpDir = root.appendingPathComponent("queue/.tmp", isDirectory: true)
        try fm.createDirectory(at: queueDir, withIntermediateDirectories: true)
        try fm.createDirectory(at: tmpDir, withIntermediateDirectories: true)
    }

    /// 原子入队：先写 .tmp，再 rename。中途被杀不会留下半个 JSON。
    @discardableResult
    public func enqueue(_ item: CaptureItem) throws -> Bool {
        lock.lock(); defer { lock.unlock() }
        let final = queueDir.appendingPathComponent("\(item.createdAt.replacingOccurrences(of: ":", with: "-"))-\(item.id).json")
        if fm.fileExists(atPath: final.path) { return false }        // 幂等：同一分钟同一内容只入队一次

        let enc = JSONEncoder()
        enc.outputFormatting = [.sortedKeys]
        let data: Data
        do { data = try enc.encode(item) }
        catch { throw QueueError.writeFailed(error.localizedDescription) }

        let tmp = tmpDir.appendingPathComponent(UUID().uuidString + ".json")
        do {
            try data.write(to: tmp, options: .atomic)
            try fm.moveItem(at: tmp, to: final)                       // 同卷 rename，原子的
        } catch {
            try? fm.removeItem(at: tmp)
            throw QueueError.writeFailed(error.localizedDescription)
        }
        return true
    }

    /// Mac 侧读出全部待处理项，按时间排序；读失败的一条不删，只跳过。
    public func drain() throws -> [CaptureItem] {
        let names = try fm.contentsOfDirectory(atPath: queueDir.path)
            .filter { $0.hasSuffix(".json") }.sorted()
        var out: [CaptureItem] = []
        for n in names {
            let url = queueDir.appendingPathComponent(n)
            guard let d = try? Data(contentsOf: url),
                  let item = try? JSONDecoder().decode(CaptureItem.self, from: d) else { continue }
            out.append(item)
        }
        return out
    }
}
