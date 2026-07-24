// New session — a real composer page, not a form. Mirrors the old mobile
// app's canvas (faded mark + "What are we building?" + glass composer with
// picker chips) and the desktop's new-session canvas (composer expanded with
// in-pill pickers). The space already fixes device + folder; the composer
// carries the agent/model chip, and sending mints the chat, queues the first
// run, and swaps straight into the live session.

import SwiftUI

struct NewSessionView: View {
    @Environment(AppModel.self) private var model
    let spaceId: String
    @Binding var path: [Route]

    // Sticky run config (the old app persisted these to prefs.db).
    @AppStorage("newSessionHarness") private var harness = "claude-code"
    @AppStorage("newSessionModel") private var storedModel = ""
    @AppStorage("newSessionReasoning") private var storedReasoning = ""

    @State private var draft = ""
    @State private var showPicker = false
    @State private var showRefPicker = false
    @State private var showCheckoutPicker = false
    /// Live per-harness catalogs from the space's device (static fallback).
    @State private var catalogs: [String: [ModelInfo]] = [:]
    @State private var refs: [RepoRef] = []
    @State private var selectedRef: String?
    @State private var checkoutKind: CheckoutKind = .local
    @State private var busy = false
    @FocusState private var focused: Bool

    private var space: Space? {
        model.spaces.first { $0.id == spaceId }
    }

    private var models: [ModelInfo] {
        catalogs[harness] ?? HarnessCatalog.models(for: harness)
    }

    private var selectedModel: ModelInfo {
        models.first { $0.id == storedModel } ?? models[0]
    }

    private var reasoning: String? {
        if selectedModel.reasoningLevels.isEmpty { return nil }
        if selectedModel.reasoningLevels.contains(storedReasoning) { return storedReasoning }
        return HarnessCatalog.defaultReasoning(for: selectedModel)
    }

    var body: some View {
        VStack(spacing: 0) {
            // Canvas — tap dismisses the keyboard, like the old app.
            ZStack {
                Theme.bg
                VStack(spacing: 24) {
                    CometMark()
                        .frame(width: 84, height: 84)
                        .opacity(0.22)
                    Text("What are we building?")
                        .font(Theme.sans(15))
                        .foregroundStyle(Theme.textFaint)
                }
            }
            .contentShape(Rectangle())
            .onTapGesture { focused = false }

            if let space, !model.deviceOnline(space.deviceId), model.demo == nil {
                offlineNotice(space: space)
            }

            composer
                .padding(.bottom, 8)
        }
        .background(Theme.bg.ignoresSafeArea())
        .navigationTitle("New session")  // feeds the back menu
        .navigationBarTitleDisplayMode(.inline)
        .toolbar {
            ToolbarItem(placement: .principal) {
                VStack(spacing: 1) {
                    Text("New session")
                        .font(Theme.sans(13, weight: .medium))
                        .foregroundStyle(Theme.text)
                    if let space {
                        Text("\(space.displayName) · \(model.deviceName(space.deviceId))")
                            .font(Theme.sans(10.5))
                            .foregroundStyle(Theme.textMuted.opacity(0.6))
                            .lineLimit(1)
                    }
                }
            }
        }
        .sheet(isPresented: $showRefPicker) {
            RefPickerSheet(refs: refs, selected: selectedRef) { ref in
                await pickRef(ref)
            }
        }
        .sheet(isPresented: $showCheckoutPicker) {
            CheckoutPickerSheet(kind: checkoutKind,
                                selectedRefHasWorktree: selectedRefRow?.worktreePath != nil) { kind in
                pickCheckout(kind)
            }
        }
        .task(id: spaceId) {
            // Load refs for the branch chip (git spaces only).
            guard let space, space.gitDetected else { return }
            if let loaded = await model.listRefs(space: space) {
                refs = loaded
                if selectedRef == nil {
                    selectedRef = loaded.first(where: \.current)?.name ?? loaded.first?.name
                }
            }
        }
        .task(id: "\(spaceId)/\(harness)") {
            // Live model catalog from the device that will run the session.
            guard let space else { return }
            catalogs[harness] = await model.listModels(space: space, harness: harness)
        }
        .sheet(isPresented: $showPicker) {
            ModelPickerSheet(harness: $harness, modelId: Binding(
                get: { selectedModel.id },
                set: { storedModel = $0 }
            ), reasoning: Binding(
                get: { reasoning },
                set: { storedReasoning = $0 ?? "" }
            ), catalogs: catalogs)
        }
        .onAppear {
            focused = true
            if model.launchAutosend {
                model.launchAutosend = false
                draft = "Sketch the plan for porting the diff pane."
                Task { @MainActor in
                    try? await Task.sleep(nanoseconds: 800_000_000)
                    send()
                }
            }
        }
    }

    // MARK: Composer

    private var composer: some View {
        ComposerShell(
            draft: $draft,
            placeholder: "Do anything…",
            sendEnabled: space != nil,
            showStop: false,
            busy: busy,
            onSend: send
        ) {
            // Agent chip — brand mark + model, opens the picker sheet
            // (desktop's in-pill HarnessModel trigger chip).
            Button {
                focused = false
                showPicker = true
            } label: {
                HStack(spacing: 6) {
                    HarnessBadge(harness: harness, size: 15)
                    Text(selectedModel.label)
                        .font(Theme.sans(13, weight: .medium))
                        .foregroundStyle(Theme.text.opacity(0.9))
                        .lineLimit(1)
                    if let reasoning {
                        Text(HarnessCatalog.reasoningLabel(reasoning))
                            .font(Theme.sans(12))
                            .foregroundStyle(Theme.textMuted)
                    }
                    Image(systemName: "chevron.up.chevron.down")
                        .font(.system(size: 9, weight: .medium))
                        .foregroundStyle(Theme.textFaint)
                }
                .padding(.horizontal, 13)
                .frame(height: 36)
                .background(whiteAlpha(0.10), in: Capsule())
            }
            .buttonStyle(ChipPressButtonStyle())

            // Checkout + ref chips — the desktop footer (git spaces only).
            if space?.gitDetected == true {
                chip(icon: checkoutIcon, label: checkoutLabel) {
                    focused = false
                    showCheckoutPicker = true
                }
                .layoutPriority(-1)
                chip(icon: .gitBranch, label: refLabel) {
                    focused = false
                    showRefPicker = true
                }
                .layoutPriority(-1)
            }
        }
    }

    private func chip(icon: LineIcon, label: String, action: @escaping () -> Void) -> some View {
        Button(action: action) {
            HStack(spacing: 6) {
                LineIconView(icon, size: 13, color: Theme.textMuted)
                Text(label)
                    .font(Theme.sans(13, weight: .medium))
                    .foregroundStyle(Theme.text.opacity(0.9))
                    .lineLimit(1)
            }
            .padding(.horizontal, 12)
            .frame(height: 36)
            .background(whiteAlpha(0.10), in: Capsule())
        }
        .buttonStyle(ChipPressButtonStyle())
    }

    // MARK: Checkout model (pickers.rs port)

    private var selectedRefRow: RepoRef? {
        refs.first { $0.name == selectedRef }
    }

    /// checkout_label: New worktree / Current worktree / Current checkout.
    private var checkoutLabel: String {
        switch checkoutKind {
        case .newWorktree: return "New worktree"
        case .local: return selectedRefRow?.worktreePath != nil ? "Current worktree" : "Current checkout"
        }
    }

    private var checkoutIcon: LineIcon {
        checkoutKind == .local && selectedRefRow?.worktreePath == nil ? .folder : .folderWithFiles
    }

    /// ref_label: "From <ref>" only when a NEW worktree will be created off it.
    private var refLabel: String {
        guard let name = selectedRef else { return "Select ref" }
        return checkoutKind == .newWorktree ? "From \(name)" : name
    }

    /// pick_ref (draft mode): a worktree'd ref flips to "Current worktree";
    /// base picks just record; a plain non-current ref in Local mode CHECKS
    /// OUT the space folder (it must never silently flip the mode).
    private func pickRef(_ row: RepoRef) async -> String? {
        if row.worktreePath != nil {
            selectedRef = row.name
            checkoutKind = .local
            return nil
        }
        if checkoutKind == .newWorktree || row.current {
            selectedRef = row.name
            return nil
        }
        guard let space else { return nil }
        let error = await model.switchSpaceRef(space: space, refName: row.name)
        if error == nil {
            selectedRef = row.name
            if let reloaded = await model.listRefs(space: space) {
                refs = reloaded
            }
        }
        return error
    }

    /// pick_checkout: dropping back to Local with a plain non-current ref
    /// picked drops the pick — the current branch takes over.
    private func pickCheckout(_ kind: CheckoutKind) {
        if kind == .local, checkoutKind == .newWorktree,
           let row = selectedRefRow, row.worktreePath == nil, !row.current {
            selectedRef = refs.first(where: \.current)?.name
        }
        checkoutKind = kind
    }

    private var canSend: Bool {
        guard !busy, space != nil else { return false }
        return !draft.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
    }

    private func offlineNotice(space: Space) -> some View {
        Text("\(model.deviceName(space.deviceId)) is offline — the run will start when it reconnects.")
            .font(Theme.sans(12))
            .foregroundStyle(Theme.warning.opacity(0.9))
            .frame(maxWidth: .infinity, alignment: .leading)
            .padding(.horizontal, 14)
            .padding(.vertical, 8)
            .background(Theme.warning.opacity(0.1), in: RoundedRectangle(cornerRadius: 12))
            .padding(.horizontal, 12)
            .padding(.bottom, 8)
    }

    /// Mint the chat per the checkout plan, queue the first run, swap to the
    /// live session (composer.rs on-send: current checkout as-is, reuse the
    /// picked ref's worktree, or CreateWorktree off the base first).
    private func send() {
        guard let space, canSend else { return }
        let prompt = draft.trimmingCharacters(in: .whitespacesAndNewlines)
        busy = true
        let config = ChatConfig(harness: harness, model: selectedModel.id,
                                reasoning: reasoning, sandbox: "workspace-write")
        Task { @MainActor in
            var cwd: String?
            var branch = selectedRef
            switch checkoutKind {
            case .newWorktree:
                if let base = selectedRef {
                    guard let worktreePath = await model.createWorktree(space: space, base: base) else {
                        busy = false
                        return
                    }
                    cwd = worktreePath
                    branch = base
                }
            case .local:
                if let worktree = selectedRefRow?.worktreePath {
                    cwd = worktree  // reuse the ref's existing checkout
                }
            }
            guard let chatId = model.createChat(space: space, config: config,
                                                branch: branch, cwd: cwd),
                  let chat = model.chat(id: chatId),
                  let store = model.sessionStore(for: chat) else {
                busy = false
                return
            }
            store.sendRun(prompt: prompt, chat: chat)
            UIImpactFeedbackGenerator(style: .light).impactOccurred()
            draft = ""
            busy = false
            // Replace the canvas with the live session (in-place swap, no
            // back-through-canvas).
            if path.last == .newSession(spaceId: spaceId) {
                path.removeLast()
            }
            path.append(.chat(chatId))
        }
    }
}

// MARK: - Model / effort picker sheet

/// Detent bottom sheet in the old app's ModelEffortMenu layout: harness tabs
/// (hidden once a chat exists — harness is locked, like the old app), a
/// grouped card of models, and the effort ladder in the same select-row style.
/// Mid-session checkout context: the read-only kind label plus the live ref
/// list (the desktop keeps its branch selector interactive mid-session).
struct SessionCheckoutContext {
    var isWorktree: Bool
    var cwd: String
    var refs: [RepoRef]
    var currentBranch: String?
    /// Returns git's error to surface inline, or nil on success.
    var onPick: (RepoRef) async -> String?
}

struct ModelPickerSheet: View {
    @Environment(\.dismiss) private var dismiss
    @Binding var harness: String
    @Binding var modelId: String
    @Binding var reasoning: String?
    /// True when reconfiguring a live chat: the harness can't change mid-chat.
    var lockedHarness = false
    /// Live per-harness catalogs from the device (static fallback when absent).
    var catalogs: [String: [ModelInfo]] = [:]
    /// Present on live git chats: checkout label + switchable refs.
    var checkout: SessionCheckoutContext?

    private func models(for harness: String) -> [ModelInfo] {
        catalogs[harness] ?? HarnessCatalog.models(for: harness)
    }

    @State private var switching: String?
    @State private var switchError: String?

    var body: some View {
        NavigationStack {
            ScrollView {
                VStack(alignment: .leading, spacing: 22) {
                    if !lockedHarness {
                        HStack(spacing: 8) {
                            ForEach(HarnessCatalog.harnesses) { h in
                                harnessTab(h)
                            }
                            Spacer(minLength: 0)
                        }
                    }

                    VStack(alignment: .leading, spacing: 8) {
                        SheetLabel("Model")
                        SheetCard {
                            let models = models(for: harness)
                            ForEach(Array(models.enumerated()), id: \.element.id) { ix, m in
                                SheetSelectRow(title: m.label,
                                               subtitle: m.description,
                                               selected: m.id == modelId,
                                               leading: nil) {
                                    select(model: m)
                                }
                                if ix < models.count - 1 {
                                    SheetSeparator()
                                }
                            }
                        }
                    }

                    if let m = selectedModel, !m.reasoningLevels.isEmpty {
                        VStack(alignment: .leading, spacing: 8) {
                            SheetLabel("Effort")
                            SheetCard {
                                ForEach(Array(m.reasoningLevels.enumerated()), id: \.element) { ix, level in
                                    SheetSelectRow(title: HarnessCatalog.reasoningLabel(level),
                                                   subtitle: Self.effortHint(level),
                                                   selected: reasoning == level,
                                                   leading: nil) {
                                        reasoning = level
                                    }
                                    if ix < m.reasoningLevels.count - 1 {
                                        SheetSeparator()
                                    }
                                }
                            }
                        }
                    }

                    if let checkout {
                        checkoutSection(checkout)
                    }
                }
                .padding(20)
                .padding(.bottom, 12)
            }
            .background(SheetStyle.panel)
            .navigationTitle("Select model")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .confirmationAction) {
                    Button {
                        dismiss()
                    } label: {
                        Image(systemName: "xmark")
                            .font(.system(size: 13, weight: .semibold))
                    }
                    .accessibilityLabel("Close")
                }
            }
        }
        .presentationDetents([.medium, .large])
        .presentationDragIndicator(.visible)
        .presentationCornerRadius(32)
        .preferredColorScheme(.dark)
    }

    private var selectedModel: ModelInfo? {
        models(for: harness).first { $0.id == modelId }
    }

    private func harnessTab(_ h: HarnessInfo) -> some View {
        let selected = harness == h.id
        return Button {
            guard harness != h.id else { return }
            UISelectionFeedbackGenerator().selectionChanged()
            harness = h.id
            let fallback = HarnessCatalog.defaultModel(for: h.id)
            modelId = fallback.id
            reasoning = HarnessCatalog.defaultReasoning(for: fallback)
        } label: {
            HStack(spacing: 7) {
                HarnessBadge(harness: h.id, size: 15, dimmed: !selected)
                Text(h.label)
                    .font(Theme.sans(13, weight: .medium))
                    .foregroundStyle(selected ? Theme.text : Theme.textMuted)
            }
            .padding(.horizontal, 14)
            .frame(height: 36)
            .background(selected ? whiteAlpha(0.15) : whiteAlpha(0.05), in: Capsule())
        }
        .buttonStyle(.plain)
    }

    private func select(model m: ModelInfo) {
        modelId = m.id
        if let current = reasoning, m.reasoningLevels.contains(current) {
            return
        }
        reasoning = HarnessCatalog.defaultReasoning(for: m)
    }

    /// Checkout: read-only kind (fixed at creation — resume is cwd-scoped)
    /// plus the LIVE ref list: retarget onto a ref's worktree, or git-checkout
    /// in the session's own folder.
    @ViewBuilder
    private func checkoutSection(_ checkout: SessionCheckoutContext) -> some View {
        VStack(alignment: .leading, spacing: 8) {
            SheetLabel("Checkout")
            SheetCard {
                HStack(spacing: 12) {
                    LineIconView(checkout.isWorktree ? .folderWithFiles : .folder,
                                 size: 16, color: Theme.textMuted)
                        .frame(width: 22)
                    VStack(alignment: .leading, spacing: 2) {
                        Text(checkout.isWorktree ? "Worktree" : "Local checkout")
                            .font(Theme.sans(15))
                            .foregroundStyle(Theme.text)
                        Text(checkout.cwd)
                            .font(Theme.mono(11.5))
                            .foregroundStyle(Theme.textMuted)
                            .lineLimit(1)
                            .truncationMode(.head)
                    }
                    Spacer(minLength: 0)
                }
                .padding(.horizontal, 16)
                .padding(.vertical, 11)
            }
        }

        VStack(alignment: .leading, spacing: 8) {
            SheetLabel("Ref")
            if checkout.refs.isEmpty {
                Text("Loading refs from the device…")
                    .font(Theme.sans(13))
                    .foregroundStyle(Theme.textFaint)
                    .frame(maxWidth: .infinity)
                    .padding(.vertical, 20)
            } else {
                SheetCard {
                    ForEach(Array(checkout.refs.enumerated()), id: \.element.name) { ix, ref in
                        refRow(ref, checkout: checkout)
                        if ix < checkout.refs.count - 1 {
                            SheetSeparator()
                        }
                    }
                }
            }
            if let switchError {
                Text(switchError)
                    .font(Theme.sans(12.5))
                    .foregroundStyle(Theme.danger)
                    .padding(.horizontal, 4)
            }
        }
    }

    private func refRow(_ ref: RepoRef, checkout: SessionCheckoutContext) -> some View {
        let selected = ref.name == checkout.currentBranch
        return Button {
            guard switching == nil, !selected else { return }
            UISelectionFeedbackGenerator().selectionChanged()
            switchError = nil
            switching = ref.name
            Task { @MainActor in
                let result = await checkout.onPick(ref)
                switching = nil
                switchError = result
            }
        } label: {
            HStack(spacing: 12) {
                LineIconView(.gitBranch, size: 15, color: Theme.textMuted)
                    .frame(width: 20)
                VStack(alignment: .leading, spacing: 2) {
                    Text(ref.name)
                        .font(Theme.sans(15))
                        .foregroundStyle(Theme.text)
                    if let subtitle = refSubtitle(ref, checkout: checkout) {
                        Text(subtitle)
                            .font(Theme.sans(12.5))
                            .foregroundStyle(Theme.textMuted)
                    }
                }
                Spacer(minLength: 8)
                if switching == ref.name {
                    ProgressView()
                        .controlSize(.small)
                        .tint(Theme.textMuted)
                } else {
                    Image(systemName: "checkmark")
                        .font(.system(size: 14, weight: .semibold))
                        .foregroundStyle(Theme.text)
                        .opacity(selected ? 1 : 0)
                }
            }
            .padding(.horizontal, 16)
            .padding(.vertical, 11)
            .contentShape(Rectangle())
        }
        .buttonStyle(SheetRowButtonStyle())
    }

    private func refSubtitle(_ ref: RepoRef, checkout: SessionCheckoutContext) -> String? {
        if ref.worktreePath == checkout.cwd { return "This session's worktree" }
        if let worktree = ref.worktreePath, worktree != checkout.cwd { return "Switches to its worktree" }
        if ref.current { return "Main checkout" }
        return nil
    }

    /// One-line hints for the ladder (the special modes deserve explanation).
    static func effortHint(_ level: String) -> String? {
        switch level {
        case "low": return "Fastest responses"
        case "medium": return "Balanced speed and depth"
        case "high": return "Thorough reasoning"
        case "xhigh": return "Extended reasoning"
        case "max": return "Maximum reasoning budget"
        case "ultra": return "Highest Codex tier"
        case "ultracode": return "X-High plus the ultracode setting"
        case "ultrathink": return "Deep-thinking prompt mode"
        default: return nil
        }
    }
}

// MARK: - Ref picker sheet

/// Base-ref selector (the desktop footer's branch popover): branch rows with
/// current-checkout / worktree markers. Picks that require a git checkout run
/// inline — a spinner on the row, git's error surfaced in place on failure
/// (dirty tree, held ref), success dismisses.
struct RefPickerSheet: View {
    @Environment(\.dismiss) private var dismiss
    let refs: [RepoRef]
    let selected: String?
    /// Returns an error message to keep the sheet open, or nil to close.
    let onPick: (RepoRef) async -> String?

    @State private var switching: String?
    @State private var error: String?

    var body: some View {
        NavigationStack {
            ScrollView {
                VStack(alignment: .leading, spacing: 8) {
                    SheetLabel("Ref")
                    if refs.isEmpty {
                        Text("Loading refs from the device…")
                            .font(Theme.sans(13))
                            .foregroundStyle(Theme.textFaint)
                            .frame(maxWidth: .infinity)
                            .padding(.vertical, 28)
                    } else {
                        SheetCard {
                            ForEach(Array(refs.enumerated()), id: \.element.name) { ix, ref in
                                row(ref)
                                if ix < refs.count - 1 {
                                    SheetSeparator()
                                }
                            }
                        }
                    }
                    if let error {
                        Text(error)
                            .font(Theme.sans(12.5))
                            .foregroundStyle(Theme.danger)
                            .padding(.horizontal, 4)
                    }
                }
                .padding(20)
            }
            .background(SheetStyle.panel)
            .navigationTitle("Select ref")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .confirmationAction) {
                    Button {
                        dismiss()
                    } label: {
                        Image(systemName: "xmark")
                            .font(.system(size: 13, weight: .semibold))
                    }
                    .accessibilityLabel("Close")
                }
            }
        }
        .presentationDetents([.medium])
        .presentationDragIndicator(.visible)
        .presentationCornerRadius(32)
        .preferredColorScheme(.dark)
    }

    private func row(_ ref: RepoRef) -> some View {
        Button {
            guard switching == nil else { return }
            UISelectionFeedbackGenerator().selectionChanged()
            error = nil
            switching = ref.name
            Task { @MainActor in
                let result = await onPick(ref)
                switching = nil
                if let result {
                    error = result
                } else {
                    dismiss()
                }
            }
        } label: {
            HStack(spacing: 12) {
                LineIconView(.gitBranch, size: 15, color: Theme.textMuted)
                    .frame(width: 20)
                VStack(alignment: .leading, spacing: 2) {
                    Text(ref.name)
                        .font(Theme.sans(15))
                        .foregroundStyle(Theme.text)
                    if let subtitle = subtitle(for: ref) {
                        Text(subtitle)
                            .font(Theme.sans(12.5))
                            .foregroundStyle(Theme.textMuted)
                    }
                }
                Spacer(minLength: 8)
                if switching == ref.name {
                    ProgressView()
                        .controlSize(.small)
                        .tint(Theme.textMuted)
                } else {
                    Image(systemName: "checkmark")
                        .font(.system(size: 14, weight: .semibold))
                        .foregroundStyle(Theme.text)
                        .opacity(ref.name == selected ? 1 : 0)
                }
            }
            .padding(.horizontal, 16)
            .padding(.vertical, 11)
            .contentShape(Rectangle())
        }
        .buttonStyle(SheetRowButtonStyle())
    }

    private func subtitle(for ref: RepoRef) -> String? {
        if ref.current { return "Current checkout" }
        if ref.worktreePath != nil { return "Checked out in a worktree" }
        return nil
    }
}

// MARK: - Checkout picker sheet

/// Where the session runs (the desktop's checkout popover): the space's
/// folder as-is (or the picked ref's existing worktree), or a fresh isolated
/// worktree created off the base ref on send.
struct CheckoutPickerSheet: View {
    @Environment(\.dismiss) private var dismiss
    let kind: CheckoutKind
    let selectedRefHasWorktree: Bool
    let onPick: (CheckoutKind) -> Void

    var body: some View {
        NavigationStack {
            ScrollView {
                VStack(alignment: .leading, spacing: 8) {
                    SheetLabel("Checkout")
                    SheetCard {
                        row(.local,
                            icon: selectedRefHasWorktree ? .folderWithFiles : .folder,
                            title: selectedRefHasWorktree ? "Current worktree" : "Current checkout",
                            subtitle: selectedRefHasWorktree
                                ? "Reuse the picked ref's existing worktree"
                                : "Run in the space's folder as-is")
                        SheetSeparator()
                        row(.newWorktree, icon: .folderWithFiles, title: "New worktree",
                            subtitle: "A fresh isolated worktree created off the picked base ref")
                    }
                }
                .padding(20)
            }
            .background(SheetStyle.panel)
            .navigationTitle("Checkout")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .confirmationAction) {
                    Button {
                        dismiss()
                    } label: {
                        Image(systemName: "xmark")
                            .font(.system(size: 13, weight: .semibold))
                    }
                    .accessibilityLabel("Close")
                }
            }
        }
        .presentationDetents([.medium])
        .presentationDragIndicator(.visible)
        .presentationCornerRadius(32)
        .preferredColorScheme(.dark)
    }

    private func row(_ rowKind: CheckoutKind, icon: LineIcon, title: String, subtitle: String) -> some View {
        Button {
            UISelectionFeedbackGenerator().selectionChanged()
            onPick(rowKind)
            dismiss()
        } label: {
            HStack(spacing: 12) {
                LineIconView(icon, size: 16, color: Theme.textMuted)
                    .frame(width: 22)
                VStack(alignment: .leading, spacing: 2) {
                    Text(title)
                        .font(Theme.sans(15))
                        .foregroundStyle(Theme.text)
                    Text(subtitle)
                        .font(Theme.sans(12.5))
                        .foregroundStyle(Theme.textMuted)
                }
                Spacer(minLength: 8)
                Image(systemName: "checkmark")
                    .font(.system(size: 14, weight: .semibold))
                    .foregroundStyle(Theme.text)
                    .opacity(rowKind == kind ? 1 : 0)
            }
            .padding(.horizontal, 16)
            .padding(.vertical, 11)
            .contentShape(Rectangle())
        }
        .buttonStyle(SheetRowButtonStyle())
    }
}
