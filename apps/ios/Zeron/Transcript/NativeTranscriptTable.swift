// Native row virtualization owns estimates, reuse, and gesture anchoring.
// Message content remains SwiftUI; no delayed scrollTo retries or reveal gate.
import SwiftUI
import UIKit

struct NativeTranscriptTable: UIViewRepresentable {
    let rows: [TranscriptRow]
    let scroll: ScrollState
    let runwayID: String?
    let expansionHeight: CGFloat
    let bottomSpacing: CGFloat
    let reduceMotion: Bool
    let configurationID: Int
    var render: (TranscriptRow) -> AnyView

    func makeUIView(context: Context) -> TranscriptViewport {
        TranscriptViewport(scroll: scroll)
    }

    func updateUIView(_ viewport: TranscriptViewport, context: Context) {
        viewport.table.update(self)
    }
}

/// Expose realized content directly. UITableView's accessibility row proxies
/// otherwise instantiate thousands of UIHostingConfigurations just to inspect
/// their traits, defeating virtualization for assistive technology clients.
@MainActor
final class TranscriptViewport: UIView {
    let table: TranscriptTableView

    init(scroll: ScrollState) {
        table = TranscriptTableView(scroll: scroll)
        super.init(frame: .zero)
        addSubview(table)
        accessibilityIdentifier = "transcript"
        accessibilityContainerType = .list
        accessibilityCustomActions = [
            UIAccessibilityCustomAction(name: "Earlier messages") { [weak self] _ in
                self?.table.accessibilityScroll(.down) ?? false
            },
            UIAccessibilityCustomAction(name: "Later messages") { [weak self] _ in
                self?.table.accessibilityScroll(.up) ?? false
            },
        ]
    }

    required init?(coder: NSCoder) { fatalError("init(coder:) has not been implemented") }

    override func layoutSubviews() {
        super.layoutSubviews()
        table.frame = bounds
    }

    override var accessibilityElements: [Any]? {
        get { table.visibleCells.sorted { $0.frame.minY < $1.frame.minY } }
        set { }
    }

    override func accessibilityScroll(_ direction: UIAccessibilityScrollDirection) -> Bool {
        table.accessibilityScroll(direction)
    }
}

@MainActor
final class TranscriptTableView: UITableView, UITableViewDataSource, UITableViewDelegate {
    private let follow: ScrollState
    private var rows: [TranscriptRow] = []
    private var render: ((TranscriptRow) -> AnyView)?
    private var runwayID: String?
    private var expansionHeight: CGFloat = 0
    private var bottomSpacing: CGFloat = 24
    private var configurationID: Int?
    private var updating = false
    private var settling = false
    private var animatingFollow = false
    private var hasPositioned = false
    private var lastGeometry = TranscriptGeometry(contentHeight: 0, viewportHeight: 0, offset: 0, bottom: 0)
    private let runway = UIView()
    private var resizeScheduled = false

    init(scroll: ScrollState) {
        follow = scroll
        super.init(frame: .zero, style: .plain)
        dataSource = self
        delegate = self
        register(UITableViewCell.self, forCellReuseIdentifier: "message")
        rowHeight = UITableView.automaticDimension
        estimatedRowHeight = 140
        separatorStyle = .none
        sectionHeaderHeight = 0
        sectionFooterHeight = 0
        sectionHeaderTopPadding = 0
        backgroundColor = .clear
        contentInsetAdjustmentBehavior = .never
        automaticallyAdjustsScrollIndicatorInsets = false
        keyboardDismissMode = .interactive
        alwaysBounceVertical = true
        clipsToBounds = false
        runway.backgroundColor = .clear
        runway.frame.size.height = bottomSpacing
        tableFooterView = runway
        follow.nativeScrollView = self
        follow.refreshLayout = { [weak self] in self?.scheduleResize() }
        follow.jumpToLatest = { [weak self] animated in self?.requestLatest(animated: animated) }
        let tap = UITapGestureRecognizer(target: self, action: #selector(dismissKeyboard))
        tap.cancelsTouchesInView = false
        addGestureRecognizer(tap)
    }

    override func accessibilityScroll(_ direction: UIAccessibilityScrollDirection) -> Bool {
        let step: CGFloat
        switch direction {
        case .up, .next: step = 1
        case .down, .previous: step = -1
        default: return false
        }
        let target = min(max(0, contentOffset.y + step * max(44, bounds.height - 80)),
                         max(0, contentSize.height - bounds.height))
        guard abs(target - contentOffset.y) > 0.5 else { return false }
        animatingFollow = false
        follow.interactionEpoch &+= 1
        follow.pinned = false
        setContentOffset(CGPoint(x: 0, y: target), animated: false)
        layoutIfNeeded()
        if step > 0, contentSize.height - bounds.height - contentOffset.y <= TranscriptView.stickThreshold {
            follow.arm()
            goToLatest(animated: false)
        }
        UIAccessibility.post(notification: .pageScrolled,
                             argument: step > 0 ? "Later messages" : "Earlier messages")
        return true
    }

    required init?(coder: NSCoder) { fatalError("init(coder:) has not been implemented") }

    @objc private func dismissKeyboard() {
        window?.endEditing(true)
    }

    func update(_ input: NativeTranscriptTable) {
        render = input.render
        let previous = rows
        let previousIDs = previous.map(\.id)
        let nextIDs = input.rows.map(\.id)
        let newSubmission = input.runwayID != nil && input.runwayID != runwayID && hasPositioned
        let presentationChanged = configurationID != input.configurationID
        configurationID = input.configurationID
        runwayID = input.runwayID
        expansionHeight = input.expansionHeight
        bottomSpacing = input.bottomSpacing
        let anchor = visibleAnchor()
        rows = input.rows
        guard window != nil, bounds.width > 0 else {
            reloadData()
            setNeedsLayout()
            return
        }
        updating = true
        UIView.performWithoutAnimation {
            if previousIDs != nextIDs {
                if hasPositioned, nextIDs.starts(with: previousIDs), !previousIDs.isEmpty {
                    insertRows(at: (previousIDs.count..<nextIDs.count).map { IndexPath(row: $0, section: 0) }, with: .none)
                } else {
                    reloadData()
                }
            } else {
                let visible = indexPathsForVisibleRows ?? []
                let changed = visible.filter {
                    presentationChanged || previous[$0.row].version != rows[$0.row].version
                }
                if !changed.isEmpty { reconfigureRows(at: changed) }
            }
            layoutIfNeeded()
            updateRunway()
            if let anchor, hasPositioned,
               previousIDs != nextIDs || (presentationChanged && !follow.pinned) {
                restore(anchor)
            }
        }
        updating = false
        guard !rows.isEmpty, bounds.height > 0 else { return }
        if !hasPositioned {
            hasPositioned = true
            goToLatest(animated: false)
        } else if newSubmission {
            follow.arm()
            requestLatest(animated: !input.reduceMotion)
        } else if follow.pinned, !userOwnsScroll, !animatingFollow {
            goToLatest(animated: false)
        }
    }

    override func layoutSubviews() {
        super.layoutSubviews()
        guard window != nil, !updating, !settling, !rows.isEmpty else { return }
        if !hasPositioned {
            hasPositioned = true
            let interaction = follow.interactionEpoch
            DispatchQueue.main.async { [weak self] in
                guard let self, self.follow.interactionEpoch == interaction, self.follow.pinned else { return }
                self.goToLatest(animated: false)
            }
        }
        settling = true
        updateRunway()
        if follow.pinned, !userOwnsScroll, !animatingFollow { alignBottom(animated: false) }
        settling = false
    }

    private var userOwnsScroll: Bool {
        isTracking || isDragging || isDecelerating || follow.userScrolling
    }

    private func visibleAnchor() -> (id: String, offset: CGFloat)? {
        guard let index = indexPathsForVisibleRows?.min(), index.row < rows.count else { return nil }
        return (rows[index.row].id, rectForRow(at: index).minY - contentOffset.y)
    }

    private func restore(_ anchor: (id: String, offset: CGFloat)) {
        guard let index = rows.firstIndex(where: { $0.id == anchor.id }) else { return }
        let rect = rectForRow(at: IndexPath(row: index, section: 0))
        let visibleOffset = max(anchor.offset, -max(0, rect.height - 44))
        let target = min(max(0, rect.minY - visibleOffset), max(0, contentSize.height - bounds.height))
        setContentOffset(CGPoint(x: 0, y: target), animated: false)
    }

    private func updateRunway() {
        var height = bottomSpacing
        if let id = runwayID, let index = rows.firstIndex(where: { $0.entryId == id }) {
            let start = rectForRow(at: IndexPath(row: index, section: 0)).minY
            let end = rectForRow(at: IndexPath(row: rows.count - 1, section: 0)).maxY
            let turnHeight = end - start
            height = max(bottomSpacing, bounds.height + expansionHeight - turnHeight)
        }
        guard height.isFinite, abs(runway.frame.height - height) > 0.5 else { return }
        runway.frame.size.height = height
        tableFooterView = runway
    }

    private func alignBottom(animated: Bool) {
        let target = max(0, contentSize.height - bounds.height)
        guard abs(contentOffset.y - target) > 0.5 else {
            animatingFollow = false
            return
        }
        if animated { animatingFollow = true }
        setContentOffset(CGPoint(x: 0, y: target), animated: animated)
    }

    private func requestLatest(animated: Bool) {
        // Explicit send/jump takes over from momentum immediately.
        setContentOffset(contentOffset, animated: false)
        follow.userScrolling = false
        follow.userDragging = false
        animatingFollow = false
        goToLatest(animated: animated)
    }

    private func goToLatest(animated: Bool) {
        guard window != nil, !rows.isEmpty, !userOwnsScroll else { return }
        // Resolve the final cell before calculating its footer reservation.
        // UIKit keeps this cell realized when estimates above it change.
        if !animated {
            settling = true
            scrollToRow(at: IndexPath(row: rows.count - 1, section: 0), at: .bottom, animated: false)
            layoutIfNeeded()
            updateRunway()
            alignBottom(animated: false)
            settling = false
        } else {
            animatingFollow = true
            layoutIfNeeded()
            updateRunway()
            alignBottom(animated: true)
        }
    }

    private func scheduleResize() {
        guard !resizeScheduled else { return }
        resizeScheduled = true
        DispatchQueue.main.async { [weak self] in
            guard let self else { return }
            self.resizeScheduled = false
            guard self.window != nil else { return }
            self.updating = true
            UIView.performWithoutAnimation {
                self.beginUpdates()
                self.endUpdates()
                self.layoutIfNeeded()
                self.updateRunway()
            }
            self.updating = false
            if self.follow.pinned, !self.userOwnsScroll, !self.animatingFollow {
                self.goToLatest(animated: false)
            }
        }
    }

    func tableView(_ tableView: UITableView, numberOfRowsInSection section: Int) -> Int { rows.count }
    func tableView(_ tableView: UITableView, heightForHeaderInSection section: Int) -> CGFloat { .leastNormalMagnitude }
    func tableView(_ tableView: UITableView, heightForFooterInSection section: Int) -> CGFloat { .leastNormalMagnitude }

    func tableView(_ tableView: UITableView, cellForRowAt indexPath: IndexPath) -> UITableViewCell {
        let cell = dequeueReusableCell(withIdentifier: "message", for: indexPath)
        let row = rows[indexPath.row]
        cell.accessibilityIdentifier = row.id
        cell.selectionStyle = .none
        cell.backgroundColor = .clear
        cell.contentView.backgroundColor = .clear
        let content = render?(row)
        cell.contentConfiguration = UIHostingConfiguration {
            content.id(row.id).ignoresSafeArea()
        }.margins(.all, 0)
        return cell
    }

    func scrollViewWillBeginDragging(_ scrollView: UIScrollView) {
        animatingFollow = false
        follow.interactionEpoch &+= 1
        follow.userScrolling = true
        follow.userDragging = true
    }

    func scrollViewDidScroll(_ scrollView: UIScrollView) {
        let geometry = TranscriptGeometry(contentHeight: contentSize.height, viewportHeight: bounds.height,
            offset: contentOffset.y, bottom: contentSize.height - contentOffset.y - bounds.height)
        follow.observe(old: lastGeometry, new: geometry)
        lastGeometry = geometry
    }

    func scrollViewDidEndDragging(_ scrollView: UIScrollView, willDecelerate decelerate: Bool) {
        if !decelerate { finishGesture() }
    }

    func scrollViewDidEndDecelerating(_ scrollView: UIScrollView) { finishGesture() }

    private func finishGesture() {
        follow.endGesture()
        if follow.pinned { goToLatest(animated: false) }
    }

    func scrollViewDidEndScrollingAnimation(_ scrollView: UIScrollView) {
        animatingFollow = false
        if follow.pinned { goToLatest(animated: false) }
    }
}
