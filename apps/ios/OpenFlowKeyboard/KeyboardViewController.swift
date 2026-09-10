import UIKit
import OpenFlowMobileCore

/// One row, two buttons, no microphone and no model.
///
/// A keyboard extension cannot use the microphone and lives under a memory cap
/// measured in tens of megabytes (PLAN.md section 0). So this is deliberately
/// the dumbest surface in the product: it reads one small JSON file from the App
/// Group and types its contents. It never loads `ModelManager`, never touches
/// `AudioCapture`, and never sees the history list.
final class KeyboardViewController: UIInputViewController {

    private var insertButton: UIButton!
    private var openButton: UIButton!
    private var hintLabel: UILabel!

    override func viewDidLoad() {
        super.viewDidLoad()
        buildRow()
        refresh()
    }

    override func viewWillAppear(_ animated: Bool) {
        super.viewWillAppear(animated)
        refresh()
    }

    private func buildRow() {
        insertButton = Self.makeButton(title: "Insert last dictation", symbol: "text.insert")
        insertButton.addTarget(self, action: #selector(insertLast), for: .touchUpInside)

        openButton = Self.makeButton(title: "Open OpenFlow", symbol: "mic.fill")
        openButton.addTarget(self, action: #selector(openHost), for: .touchUpInside)

        hintLabel = UILabel()
        hintLabel.font = .preferredFont(forTextStyle: .caption2)
        hintLabel.textColor = .secondaryLabel
        hintLabel.textAlignment = .center
        hintLabel.numberOfLines = 2

        let row = UIStackView(arrangedSubviews: [insertButton, openButton])
        row.axis = .horizontal
        row.distribution = .fillEqually
        row.spacing = 8

        let column = UIStackView(arrangedSubviews: [row, hintLabel])
        column.axis = .vertical
        column.spacing = 6
        column.translatesAutoresizingMaskIntoConstraints = false
        view.addSubview(column)

        NSLayoutConstraint.activate([
            column.leadingAnchor.constraint(equalTo: view.leadingAnchor, constant: 12),
            column.trailingAnchor.constraint(equalTo: view.trailingAnchor, constant: -12),
            column.topAnchor.constraint(equalTo: view.topAnchor, constant: 10),
            column.bottomAnchor.constraint(equalTo: view.bottomAnchor, constant: -10),
            view.heightAnchor.constraint(equalToConstant: 96),
        ])
    }

    private static func makeButton(title: String, symbol: String) -> UIButton {
        var configuration = UIButton.Configuration.bordered()
        configuration.title = title
        configuration.image = UIImage(systemName: symbol)
        configuration.imagePadding = 6
        configuration.titleTextAttributesTransformer = UIConfigurationTextAttributesTransformer { attributes in
            var attributes = attributes
            attributes.font = .preferredFont(forTextStyle: .footnote)
            return attributes
        }
        let button = UIButton(configuration: configuration)
        button.titleLabel?.adjustsFontSizeToFitWidth = true
        return button
    }

    /// Reads only `last.json`. A 30-day history is never parsed here.
    ///
    /// Three outcomes, and they need three different sentences. Without Allow
    /// Full Access the container may fail to resolve at all, or resolve and then
    /// refuse the read -- both mean the same thing to the user, and neither is
    /// "you have not dictated anything yet". Telling someone to go and dictate
    /// when the real problem is a switch in Settings sends them round a loop
    /// that cannot end.
    private func refresh() {
        guard let store = try? TranscriptStore.shared() else {
            showFullAccessHint()
            return
        }
        switch store.readLast() {
        case .unreadable:
            showFullAccessHint()
        case .none:
            hintLabel.text = "Dictate something in OpenFlow and it will appear here."
            insertButton.isEnabled = false
        case .record(let record):
            insertButton.isEnabled = true
            hintLabel.text = preview(record.text)
        }
    }

    private func showFullAccessHint() {
        hintLabel.text = "Turn on Allow Full Access in Settings, General, Keyboard so OpenFlow can read your last dictation."
        insertButton.isEnabled = false
    }

    private func preview(_ text: String) -> String {
        let flattened = text.replacingOccurrences(of: "\n", with: " ")
        return flattened.count <= 60 ? flattened : String(flattened.prefix(60)) + "..."
    }

    @objc private func insertLast() {
        guard let store = try? TranscriptStore.shared(),
              let record = store.lastEntry() else {
            // The button is disabled in both other cases; if we get here the
            // state changed underneath us, so re-read and say why.
            refresh()
            return
        }
        textDocumentProxy.insertText(record.text)
    }

    /// Opens the host app so the user can dictate. `openURL` on the responder
    /// chain is the only route an extension has.
    @objc private func openHost() {
        guard let url = URL(string: "openflow://dictate") else { return }
        var responder: UIResponder? = self
        while let next = responder {
            if let application = next as? UIApplication {
                application.open(url, options: [:], completionHandler: nil)
                return
            }
            responder = next.next
        }
    }
}
