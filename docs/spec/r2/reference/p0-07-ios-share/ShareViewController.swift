// 鹿蕊 · iOS 分享扩展（P0-07，可编译骨架）
// 交互：分享面板 → 一句话（可选）→ 保存 → 关闭。AI 提议标题/标签是候选，默认可关。
import UIKit
import UniformTypeIdentifiers

final class ShareViewController: UIViewController {
    private let textView = UITextView()
    private let noteField = UITextField()
    private let status = UILabel()
    private var queue: CaptureQueue?

    override func viewDidLoad() {
        super.viewDidLoad()
        view.backgroundColor = .systemBackground
        buildUI()
        loadSharedItem()
    }

    private func buildUI() {
        textView.font = .preferredFont(forTextStyle: .body)
        textView.isEditable = false
        textView.translatesAutoresizingMaskIntoConstraints = false

        noteField.placeholder = "加一句话（可选）"
        noteField.borderStyle = .roundedRect
        noteField.translatesAutoresizingMaskIntoConstraints = false

        status.font = .preferredFont(forTextStyle: .footnote)
        status.textColor = .secondaryLabel
        status.numberOfLines = 2
        status.translatesAutoresizingMaskIntoConstraints = false

        let save = UIButton(type: .system)
        save.setTitle("保存到鹿蕊", for: .normal)
        save.addTarget(self, action: #selector(saveTapped), for: .touchUpInside)
        save.translatesAutoresizingMaskIntoConstraints = false

        let cancel = UIButton(type: .system)
        cancel.setTitle("取消", for: .normal)
        cancel.addTarget(self, action: #selector(cancelTapped), for: .touchUpInside)
        cancel.translatesAutoresizingMaskIntoConstraints = false

        for v in [textView, noteField, status, save, cancel] { view.addSubview(v) }
        NSLayoutConstraint.activate([
            textView.topAnchor.constraint(equalTo: view.safeAreaLayoutGuide.topAnchor, constant: 16),
            textView.leadingAnchor.constraint(equalTo: view.leadingAnchor, constant: 20),
            textView.trailingAnchor.constraint(equalTo: view.trailingAnchor, constant: -20),
            textView.heightAnchor.constraint(equalToConstant: 160),

            noteField.topAnchor.constraint(equalTo: textView.bottomAnchor, constant: 12),
            noteField.leadingAnchor.constraint(equalTo: textView.leadingAnchor),
            noteField.trailingAnchor.constraint(equalTo: textView.trailingAnchor),

            status.topAnchor.constraint(equalTo: noteField.bottomAnchor, constant: 10),
            status.leadingAnchor.constraint(equalTo: textView.leadingAnchor),
            status.trailingAnchor.constraint(equalTo: textView.trailingAnchor),

            save.topAnchor.constraint(equalTo: status.bottomAnchor, constant: 16),
            save.centerXAnchor.constraint(equalTo: view.centerXAnchor),
            cancel.topAnchor.constraint(equalTo: save.bottomAnchor, constant: 8),
            cancel.centerXAnchor.constraint(equalTo: view.centerXAnchor),
        ])
    }

    /// 取分享内容：优先 URL，其次纯文本，最后图片（先只入队指针，附件后续同步）。
    private func loadSharedItem() {
        guard let providers = (extensionContext?.inputItems as? [NSExtensionItem])?
            .compactMap({ $0.attachments }).flatMap({ $0 }) else { return }
        for p in providers {
            if p.hasItemConformingToTypeIdentifier(UTType.url.identifier) {
                p.loadItem(forTypeIdentifier: UTType.url.identifier) { [weak self] item, _ in
                    let s = (item as? URL)?.absoluteString ?? (item as? String)
                    DispatchQueue.main.async { self?.textView.text = s }
                }
                return
            }
            if p.hasItemConformingToTypeIdentifier(UTType.plainText.identifier) {
                p.loadItem(forTypeIdentifier: UTType.plainText.identifier) { [weak self] item, _ in
                    DispatchQueue.main.async { self?.textView.text = (item as? String) ?? "" }
                }
                return
            }
        }
    }

    @objc private func saveTapped() {
        let text = (textView.text ?? "").trimmingCharacters(in: .whitespacesAndNewlines)
        guard !text.isEmpty else {
            status.text = "没有可保存的内容"
            return
        }
        do {
            let q = try CaptureQueue()
            let bucket = ISO8601DateFormatter().string(from: Date()).prefix(16)   // 分钟桶
            let id = SHA256.hex(kind + "|" + text + "|" + bucket)
            let item = CaptureItem(id: id, kind: "text", text: text,
                                   note: noteField.text?.isEmpty == true ? nil : noteField.text,
                                   sourceApp: nil,
                                   createdAt: ISO8601DateFormatter().string(from: Date()),
                                   device: UIDevice.current.name)
            let fresh = try q.enqueue(item)
            status.text = fresh ? "已放入队列，回到 Mac 就能看到" : "这条已经在了"
            DispatchQueue.main.asyncAfter(deadline: .now() + 0.6) { [weak self] in
                self?.extensionContext?.completeRequest(returningItems: nil)
            }
        } catch {
            status.text = "\(error)"     // 失败留稿：不清空输入
        }
    }

    @objc private func cancelTapped() {
        extensionContext?.cancelRequest(withError: NSError(domain: "litmus.share", code: 0))
    }
}

enum SHA256 {
    static func hex(_ s: String) -> String {
        // 占位：正式实现用 CryptoKit 的 SHA256.hash(data:)
        String(s.unicodeScalars.reduce(UInt64(1469598103934665603), { ($0 ^ UInt64($1.value)) &* 1099511628211 }), radix: 16)
    }
}
