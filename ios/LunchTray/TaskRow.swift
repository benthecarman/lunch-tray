import SwiftUI

struct TaskRow: View {
    let item: TrayTask

    var body: some View {
        HStack(alignment: .center, spacing: 12) {
            glyph
            VStack(alignment: .leading, spacing: 3) {
                HStack(spacing: 4) {
                    if item.pinned {
                        Image(systemName: "pin.fill")
                            .font(.caption)
                            .foregroundStyle(Palette.accent)
                    }
                    Text(item.title)
                        .font(.body)
                        .foregroundStyle(muted ? Palette.textWeak : Palette.text)
                        .lineLimit(2)
                }
                TimelineView(.periodic(from: .now, by: 60)) { ctx in
                    Text(subtitle(now: ctx.date))
                        .font(.caption)
                        .foregroundStyle(Palette.textWeak)
                        .lineLimit(1)
                }
            }
            Spacer(minLength: 0)
            badges
        }
        .padding(.vertical, 2)
        .accessibilityElement(children: .combine)
    }

    private var muted: Bool { item.remote?.draft == true && item.remote?.state == .open }

    /// The leading glyph in its state color, with a mustard dot for new
    /// activity.
    private var glyph: some View {
        let (name, color): (String, Color) =
            switch item.origin {
            case .manual: ("square.and.pencil", Palette.textWeak)
            case .remote(let r):
                switch (r.kind, r.state) {
                case (.pullRequest, .open):
                    ("arrow.triangle.pull", r.draft ? Palette.textWeak : Palette.open)
                case (.pullRequest, .merged): ("arrow.triangle.merge", Palette.merged)
                case (.pullRequest, .closed): ("arrow.triangle.pull", Palette.closed)
                case (.issue, .open): ("circle.dashed", Palette.open)
                case (.issue, _): ("checkmark.circle", Palette.merged)
                }
            }
        return Image(systemName: name)
            .font(.system(size: 17, weight: .medium))
            .foregroundStyle(color)
            .frame(width: 26, height: 26)
            .overlay(alignment: .topTrailing) {
                if item.unseen {
                    Circle()
                        .fill(Palette.accent)
                        .frame(width: 8, height: 8)
                        .offset(x: 2, y: -1)
                        .accessibilityLabel("New activity")
                }
            }
    }

    @ViewBuilder private var badges: some View {
        if let r = item.remote, r.state == .open, let c = r.checks {
            VStack(alignment: .trailing, spacing: 4) {
                if c.total > 0 {
                    let (icon, color): (String, Color) =
                        if c.failed > 0 {
                            ("xmark.circle", Palette.closed)
                        } else if c.pending > 0 {
                            ("circle.dashed", Palette.accent)
                        } else {
                            ("checkmark.circle", Palette.open)
                        }
                    Label("\(c.total - c.pending)/\(c.total)", systemImage: icon)
                        .labelStyle(.titleAndIcon)
                        .font(.caption2.monospacedDigit())
                        .foregroundStyle(color)
                        .accessibilityLabel(checksLabel(c))
                }
                if c.merge == .conflicting {
                    Image(systemName: "exclamationmark.triangle")
                        .font(.caption)
                        .foregroundStyle(Palette.warning)
                        .accessibilityLabel("Conflicts with the base branch")
                }
            }
        }
    }

    private func checksLabel(_ c: Checks) -> String {
        if c.failed > 0 { return "\(c.failed) of \(c.total) checks failed" }
        if c.pending > 0 { return "\(c.pending) of \(c.total) checks still running" }
        return "All checks passed"
    }

    /// One plain sentence under the title.
    private func subtitle(now: Date) -> String {
        switch item.origin {
        case .remote(let r):
            let what =
                switch (r.state, r.draft) {
                case (.merged, _): "merged"
                case (.closed, _): "closed"
                case (.open, true): "draft, updated"
                case (.open, false): "updated"
                }
            let who = r.author.isEmpty ? "" : " by \(r.author)"
            return "\(r.label)\(who), \(what) \(ago(r.remoteUpdatedAt, now: now))"
        case .manual:
            if let first = item.notes.split(separator: "\n").first, !item.notes.isEmpty {
                return String(first)
            }
            if let link = item.link, !link.isEmpty {
                return link.replacingOccurrences(of: "https://", with: "")
                    .replacingOccurrences(of: "http://", with: "")
            }
            return "Added \(ago(item.createdAt, now: now))"
        }
    }
}
