// 鹿蕊 · Mac 快捕原型（P0-06）
// 目标不是功能齐全，而是把「⌥Space 到可输入 < 200ms」变成可测的数字。
// 做法：启动时把 NSPanel 预热好并隐藏，热键只做 show + makeFirstResponder，
// 从热键回调到可输入打一个时间戳，写进日志（不弹 UI，不打扰）。
//
// 编译：swiftc -O -o quickcapture main.swift -framework AppKit -framework Carbon
// 运行：LITMUS_VAULT=/path/to/vault ./quickcapture

import AppKit
import Carbon.HIToolbox

let vault = ProcessInfo.processInfo.environment["LITMUS_VAULT"]
    ?? (NSHomeDirectory() + "/Documents/Litmus")
let inbox = URL(fileURLWithPath: vault).appendingPathComponent("Notes/Inbox")

final class CapturePanel: NSPanel {
    override var canBecomeKey: Bool { true }
    override var canBecomeMain: Bool { false }
}

@MainActor
final class App: NSObject, NSApplicationDelegate, NSTextFieldDelegate {
    private var panel: CapturePanel!
    private var field: NSTextField!
    private var status: NSTextField!
    private var hotKeyRef: EventHotKeyRef?
    private var shownAt: CFAbsoluteTime = 0

    func applicationDidFinishLaunching(_ notification: Notification) {
        NSApp.setActivationPolicy(.accessory)
        buildPanel()
        registerHotKey()
        log("ready vault=\(vault)")
    }

    // 预热：面板、输入框、约束一次建好，热键时不再创建视图。
    private func buildPanel() {
        let rect = NSRect(x: 0, y: 0, width: 520, height: 132)
        panel = CapturePanel(contentRect: rect,
                             styleMask: [.borderless, .nonactivatingPanel],
                             backing: .buffered, defer: false)
        panel.isFloatingPanel = true
        panel.level = .floating
        panel.collectionBehavior = [.canJoinAllSpaces, .fullScreenAuxiliary, .transient]
        panel.hidesOnDeactivate = false
        panel.isReleasedWhenClosed = false
        panel.backgroundColor = .clear
        panel.hasShadow = true

        let box = NSView(frame: rect)
        box.wantsLayer = true
        box.layer?.backgroundColor = NSColor.windowBackgroundColor.cgColor
        box.layer?.cornerRadius = 14
        box.layer?.borderWidth = 1
        box.layer?.borderColor = NSColor.separatorColor.cgColor

        field = NSTextField(frame: NSRect(x: 20, y: 56, width: 480, height: 40))
        field.placeholderString = "落下什么？一句话、链接、或粘贴……"
        field.font = .systemFont(ofSize: 17)
        field.isBordered = false
        field.focusRingType = .none
        field.drawsBackground = false
        field.delegate = self

        status = NSTextField(labelWithString: "↩ 直接落卡    ⌘↩ 让鹿蕊分诊    esc 取消")
        status.frame = NSRect(x: 22, y: 20, width: 480, height: 18)
        status.font = .monospacedSystemFont(ofSize: 11, weight: .regular)
        status.textColor = .tertiaryLabelColor

        box.addSubview(field)
        box.addSubview(status)
        panel.contentView = box
    }

    private func registerHotKey() {
        let id = EventHotKeyID(signature: OSType(0x4C49544D) /* LITM */, id: 1)
        let mods = UInt32(optionKey)
        RegisterEventHotKey(UInt32(kVK_Space), mods, id, GetApplicationEventTarget(), 0, &hotKeyRef)

        var spec = EventTypeSpec(eventClass: OSType(kEventClassKeyboard),
                                 eventKind: UInt32(kEventHotKeyPressed))
        InstallEventHandler(GetApplicationEventTarget(), { _, _, _ in
            Task { @MainActor in App.shared.toggle() }
            return noErr
        }, 1, &spec, nil, nil)
    }

    static let shared = App()

    func toggle() {
        if panel.isVisible {
            hide(); return
        }
        shownAt = CFAbsoluteTimeGetCurrent()
        panel.center()
        panel.makeKeyAndOrderFront(nil)
        panel.makeFirstResponder(field)
        NSApp.activate(ignoringOtherApps: false)
        // 下一轮 runloop 才算真正「可输入」
        DispatchQueue.main.async { [weak self] in
            guard let self else { return }
            let ms = (CFAbsoluteTimeGetCurrent() - self.shownAt) * 1000
            self.log(String(format: "shown_to_focus %.1fms", ms))
        }
    }

    private func hide() {
        field.stringValue = ""
        panel.orderOut(nil)
    }

    func control(_ control: NSControl, textView: NSTextView, doCommandBy selector: Selector) -> Bool {
        if selector == #selector(NSResponder.insertNewline(_:)) {
            save(triage: NSEvent.modifierFlags.contains(.command))
            return true
        }
        if selector == #selector(NSResponder.cancelOperation(_:)) {
            hide(); return true
        }
        return false
    }

    private func save(triage: Bool) {
        let text = field.stringValue.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !text.isEmpty else { hide(); return }
        do {
            try FileManager.default.createDirectory(at: inbox, withIntermediateDirectories: true)
            let stamp = ISO8601DateFormatter().string(from: Date())
            let name = "快捕-\(stamp.replacingOccurrences(of: ":", with: "-")).md"
            let body = """
            ---
            type: thought
            state: seed
            source: quick-capture
            created: \(stamp)
            triage: \(triage ? "requested" : "none")
            ---

            \(text)
            """
            try body.write(to: inbox.appendingPathComponent(name), atomically: true, encoding: .utf8)
            log("saved \(name) triage=\(triage)")
            hide()
        } catch {
            // 失败保稿：不清空输入，给出可读原因
            status.stringValue = "没写成：\(error.localizedDescription)（内容还在）"
        }
    }

    private func log(_ line: String) {
        let path = URL(fileURLWithPath: vault).appendingPathComponent(".litmus/local/ops/quickcapture.log")
        try? FileManager.default.createDirectory(at: path.deletingLastPathComponent(), withIntermediateDirectories: true)
        let data = (line + "\n").data(using: .utf8)!
        if let h = try? FileHandle(forWritingTo: path) {
            h.seekToEndOfFile(); h.write(data); try? h.close()
        } else {
            try? data.write(to: path)
        }
    }
}

// Swift 6：顶层的 AppKit 调用全部在主线程上，用 assumeIsolated 明确这一事实。
MainActor.assumeIsolated {
    let delegate = App()
    NSApplication.shared.delegate = delegate
    NSApplication.shared.run()
}
