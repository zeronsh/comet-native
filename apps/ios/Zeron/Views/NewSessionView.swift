// New session — a real composer page, not a form. Mirrors the old mobile
// app's canvas (faded mark + "What are we building?" + glass composer with
// picker chips) and the desktop's new-session canvas (composer expanded with
// in-pill pickers). The space already fixes device + folder; the composer
// carries the agent/model chip, and sending mints the chat, queues the first
// run, and swaps straight into the live session.

import PhotosUI
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
    @State private var showTraitPicker = false
    @State private var showOptionPicker: ModelOptionInfo?
    @State private var showRefPicker = false
    @State private var showCheckoutPicker = false
    @State private var attachments: [StagedAttachment] = []
    @State private var pickerItems: [PhotosPickerItem] = []
    @State private var showPhotoPicker = false
    @State private var attachError: String?
    /// Live harness list from the space's device (Settings → Agents gate);
    /// static pair until it loads.
    @State private var liveHarnesses: [HarnessInfo]?
    /// Live per-harness catalogs from the space's device (static fallback).
    @State private var catalogs: [String: [ModelInfo]] = [:]
    @State private var optionSelections: [String: String] = [:]
    @State private var refs: [RepoRef] = []
    @State private var selectedRef: String?
    @State private var checkoutKind: CheckoutKind = .local
    @State private var busy = false
    @FocusState private var focused: Bool
    /// Basis for the leading header's fixed width (SessionView's pattern —
    /// iOS 26 proposes leading toolbar items almost nothing).
    @State private var viewWidth: CGFloat = 0

    private var space: Space? {
        model.spaces.first { $0.id == spaceId }
    }

    private var harnesses: [HarnessInfo] {
        liveHarnesses ?? HarnessCatalog.harnesses
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

    private func selectedChoice(for option: ModelOptionInfo) -> ModelOptionChoiceInfo {
        HarnessCatalog.selectedChoice(for: option, selectedId: optionSelections[option.id])
    }

    /// Only non-default picks ride the run, matching the desktop picker.
    private var resolvedModelOptions: [String: JSONValue] {
        var result: [String: JSONValue] = [:]
        for option in selectedModel.options {
            let choice = selectedChoice(for: option)
            if choice.id != option.defaultChoice {
                result[option.id] = .string(choice.id)
            }
        }
        return result
    }

    var body: some View {
        VStack(spacing: 0) {
            // Canvas — tap dismisses the keyboard, like the old app.
            ZStack {
                Theme.bg
                VStack(spacing: 24) {
                    ZeronMark()
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

            // Where-it-runs scope row (checkout + base ref), left-aligned
            // above the composer — the composer pill keeps only the agent chip.
            if space?.gitDetected == true {
                ScrollView(.horizontal, showsIndicators: false) {
                    HStack(spacing: 8) {
                        chip(icon: checkoutIcon, label: checkoutLabel) {
                            focused = false
                            showCheckoutPicker = true
                        }
                        chip(icon: .gitBranch, label: refLabel) {
                            focused = false
                            showRefPicker = true
                        }
                    }
                    .padding(.horizontal, 16)
                }
                .padding(.bottom, 8)
            }

            composer
                .padding(.bottom, 8)
        }
        .background(Theme.bg.ignoresSafeArea())
        .onGeometryChange(for: CGFloat.self) { $0.size.width } action: { viewWidth = $0 }
        .navigationTitle("New session")  // feeds the back menu
        .navigationBarTitleDisplayMode(.inline)
        .toolbar(removing: .title)  // the leading header owns the bar
        .toolbar {
            ToolbarItem(placement: .topBarLeading) {
                VStack(alignment: .leading, spacing: 1) {
                    Text("New session")
                        .font(Theme.sans(13, weight: .medium))
                        .foregroundStyle(Theme.text)
                    if let space {
                        Text("\(space.displayName) · \(model.deviceName(space.deviceId))")
                            .font(Theme.sans(10.5))
                            .foregroundStyle(Theme.textMuted.opacity(0.6))
                            .lineLimit(1)
                            .truncationMode(.middle)
                    }
                }
                // 170: enough slack that the bar never evicts the item into
                // the "…" overflow (SessionView.headerChromeInset).
                .frame(width: max(140, viewWidth - 170), alignment: .leading)
            }
            // Bare text on the bar, not a glass capsule.
            .sharedBackgroundVisibility(.hidden)
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
        .task(id: spaceId) {
            // Live harness list + a model catalog per harness, all from the
            // device that will run the session (the picker shows one sectioned
            // list across harnesses, so it needs every catalog up front).
            guard let space else { return }
            let list = await model.listHarnesses(space: space)
            liveHarnesses = list
            if !list.contains(where: { $0.id == harness }), let first = list.first {
                harness = first.id
            }
            await withTaskGroup(of: (String, [ModelInfo]).self) { group in
                for h in list {
                    group.addTask { (h.id, await model.listModels(space: space, harness: h.id)) }
                }
                for await (id, catalog) in group {
                    catalogs[id] = catalog
                }
            }
        }
        .sheet(isPresented: $showPicker) {
            ModelPickerSheet(harness: $harness, modelId: Binding(
                get: { selectedModel.id },
                set: { storedModel = $0 }
            ), reasoning: Binding(
                get: { reasoning },
                set: { storedReasoning = $0 ?? "" }
            ), harnesses: harnesses, catalogs: catalogs)
        }
        .sheet(isPresented: $showTraitPicker) {
            TraitPickerSheet(reasoning: Binding(
                get: { reasoning },
                set: { storedReasoning = $0 ?? "" }
            ), levels: selectedModel.reasoningLevels)
        }
        .sheet(item: $showOptionPicker) { option in
            ModelOptionPickerSheet(option: option, choiceId: Binding(
                get: { selectedChoice(for: option).id },
                set: { optionSelections[option.id] = $0 }
            ))
        }
        .photosPicker(isPresented: $showPhotoPicker, selection: $pickerItems,
                      maxSelectionCount: 8, matching: .images)
        .onChange(of: pickerItems) { _, items in
            guard !items.isEmpty else { return }
            stage(items)
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
        VStack(spacing: 6) {
            if let attachError {
                Text(attachError)
                    .font(Theme.sans(12))
                    .foregroundStyle(Theme.danger)
                    .lineLimit(2)
                    .padding(.horizontal, 24)
                    .frame(maxWidth: .infinity, alignment: .leading)
            }
            ComposerShell(
                draft: $draft,
                placeholder: "Do anything…",
                sendEnabled: space != nil,
                showStop: false,
                busy: busy,
                alwaysExpanded: true,
                onSend: send,
                attachments: attachments,
                onAttach: { showPhotoPicker = true },
                onRemoveAttachment: { id in attachments.removeAll { $0.id == id } }
            ) {
                // Model + trait chips, split like the desktop's footer pickers
                // (they ride right of the shell's attach button).
                ComposerChip(label: selectedModel.label, badgeHarness: harness) {
                    focused = false
                    showPicker = true
                }
                if let reasoning {
                    ComposerChip(label: HarnessCatalog.reasoningLabel(reasoning)) {
                        focused = false
                        showTraitPicker = true
                    }
                }
                ForEach(selectedModel.options) { option in
                    ComposerChip(label: selectedChoice(for: option).label) {
                        focused = false
                        showOptionPicker = option
                    }
                }
            }
        }
    }

    /// Load picked photos into staged attachments (ComposerView.stage's twin —
    /// the upload happens on send, once the chat and its store exist).
    private func stage(_ items: [PhotosPickerItem]) {
        Task { @MainActor in
            var failed = 0
            for item in items {
                guard let data = try? await item.loadTransferable(type: Data.self),
                      let staged = StagedAttachment.stage(data: data) else {
                    failed += 1
                    continue
                }
                attachments.append(staged)
            }
            pickerItems = []
            attachError = failed > 0
                ? (failed == 1 ? "One image couldn't be attached (unsupported or over 24 MB)."
                               : "\(failed) images couldn't be attached (unsupported or over 24 MB).")
                : nil
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
            .frame(height: 40)
            .background(whiteAlpha(0.08), in: Capsule())
            .overlay(Capsule().strokeBorder(whiteAlpha(0.08), lineWidth: 1))
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
            || !attachments.isEmpty
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
    /// live session. The atomic ordering (PRs #159/#170): anything that can
    /// FAIL stages before anything is created — a legacy attachment upload
    /// aborts with no chat row anywhere — and nothing left in the pipeline
    /// blocks on a relay RPC. A New-worktree plan rides the queued Run as a
    /// WorktreeSpec the HOST materializes at drain time; the old
    /// CreateWorktree-before-send was the one new-chat path that could hang
    /// forever on a zombie link (the 2026-08-18 "Sending…" incident).
    private func send() {
        guard let space, canSend else { return }
        let prompt = draft.trimmingCharacters(in: .whitespacesAndNewlines)
        busy = true
        let config = ChatConfig(harness: harness, model: selectedModel.id,
                                reasoning: reasoning, modelOptions: resolvedModelOptions,
                                sandbox: "workspace-write")
        Task { @MainActor in
            var cwd: String?
            let branch = selectedRef
            var worktree: WorktreeSpec?
            switch checkoutKind {
            case .newWorktree:
                // Base defaults to HEAD (PR #165): a branch list that never
                // loaded over a flapping relay must not silently drop
                // isolation. cwd stays the space folder — the old-host
                // degradation path (run in main checkout, never hung).
                worktree = WorktreeSpec(repoPath: space.path, base: selectedRef ?? "HEAD")
                cwd = space.path
            case .local:
                if let reused = selectedRefRow?.worktreePath {
                    cwd = reused  // reuse the ref's existing checkout
                }
            }

            let staged = attachments
            let queued = model.hostSupportsQueuedAttachmentsOn(deviceId: space.deviceId)

            // Legacy hosts (< 0.2.12) stage attachments FIRST — before the
            // chat row or store exist — so an upload failure aborts with
            // nothing created (no stranded empty chat in the sidebar).
            var legacyPaths: [String] = []
            if !staged.isEmpty, !queued, model.demo == nil {
                guard let workspace = model.workspace else {
                    busy = false
                    return
                }
                do {
                    for att in staged {
                        let uploadId = UUID().uuidString.lowercased()
                        let uploaded = try await workspace.uploadAttachment(
                            deviceId: space.deviceId, name: att.name, data: att.data,
                            uploadId: uploadId)
                        AttachmentImageCache.shared.seed(deviceId: space.deviceId, path: uploaded,
                                                         name: att.name, data: att.data)
                        legacyPaths.append(uploaded)
                    }
                } catch {
                    attachError = "Attachment upload failed — \(error.localizedDescription)"
                    busy = false
                    return
                }
            }

            guard let chatId = model.createChat(space: space, config: config,
                                                branch: branch, cwd: cwd),
                  let chat = model.chat(id: chatId),
                  let store = model.sessionStore(for: chat) else {
                busy = false
                return
            }
            if queued, !staged.isEmpty {
                // Queued flow: pending:// refs make the whole send a durable
                // local write; the escort pushes bytes afterwards.
                let transfers = staged.map {
                    AttachmentTransfer(uploadId: UUID().uuidString.lowercased(),
                                       name: $0.name, data: $0.data)
                }
                for (transfer, att) in zip(transfers, staged) {
                    AttachmentImageCache.shared.seed(
                        deviceId: chat.deviceId,
                        path: UploadStash.pendingRef(uploadId: transfer.uploadId,
                                                     name: transfer.name),
                        name: att.name, data: att.data)
                }
                store.sendWithTransfers(prompt: prompt, chat: chat, live: false,
                                        transfers: transfers, worktree: worktree)
            } else {
                store.sendRun(prompt: legacyPaths.isEmpty ? prompt
                                  : withAttachments(text: prompt, paths: legacyPaths),
                              chat: chat, attachments: legacyPaths, worktree: worktree)
            }
            UIImpactFeedbackGenerator(style: .light).impactOccurred()
            draft = ""
            attachments = []
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

// MARK: - Composer chip

/// The composer's picker trigger chip: optional brand mark, label, chevron —
/// one per picker, split like the desktop's footer (model | traits).
struct ComposerChip: View {
    let label: String
    var badgeHarness: String?
    let action: () -> Void

    var body: some View {
        Button(action: action) {
            HStack(spacing: 6) {
                if let badgeHarness {
                    HarnessBadge(harness: badgeHarness, size: 15)
                }
                Text(label)
                    .font(Theme.sans(13, weight: .medium))
                    .foregroundStyle(Theme.text.opacity(0.9))
                    .lineLimit(1)
                Image(systemName: "chevron.down")
                    .font(.system(size: 9, weight: .semibold))
                    .foregroundStyle(Theme.textFaint)
            }
            .padding(.horizontal, 13)
            .frame(height: 40)
            .background(whiteAlpha(0.08), in: Capsule())
            .overlay(Capsule().strokeBorder(whiteAlpha(0.08), lineWidth: 1))
        }
        .buttonStyle(ChipPressButtonStyle())
    }
}

// MARK: - Model picker sheet

/// Detent bottom sheet in the t3 settings-sheet layout: one scrolling list of
/// models sectioned per harness (collapsible uppercase provider headers with
/// the brand mark — picking a model picks its harness), the selected row a
/// filled high-contrast pill with a trailing checkmark. Effort lives in its
/// own TraitPickerSheet, split like the desktop's footer pickers. Harness
/// sections collapse to one once a chat exists — harness is locked mid-chat.
struct ModelPickerSheet: View {
    @Environment(\.dismiss) private var dismiss
    @Binding var harness: String
    @Binding var modelId: String
    @Binding var reasoning: String?
    /// True when reconfiguring a live chat: the harness can't change mid-chat.
    var lockedHarness = false
    /// Harness sections to offer (the device's live list; static fallback).
    var harnesses: [HarnessInfo] = []
    /// Live per-harness catalogs from the device (static fallback when absent).
    var catalogs: [String: [ModelInfo]] = [:]

    private func models(for harness: String) -> [ModelInfo] {
        catalogs[harness] ?? HarnessCatalog.models(for: harness)
    }

    private var sections: [HarnessInfo] {
        if lockedHarness {
            return [HarnessInfo(id: harness, label: HarnessCatalog.label(for: harness))]
        }
        return harnesses.isEmpty ? HarnessCatalog.harnesses : harnesses
    }

    /// Accordion state: which harness sections show their models. Seeded with
    /// the current harness — with several agents enabled a flat list of every
    /// catalog is unmanageable (t3's collapsible provider folds).
    @State private var openSections: Set<String> = []

    var body: some View {
        NavigationStack {
            ScrollView {
                VStack(alignment: .leading, spacing: 22) {
                    VStack(alignment: .leading, spacing: 4) {
                        SheetLabel("Model")
                        ForEach(sections) { h in
                            if sections.count > 1 {
                                sectionHeader(h)
                            }
                            if sections.count == 1 || openSections.contains(h.id) {
                                ForEach(models(for: h.id)) { m in
                                    PickRow(title: m.label,
                                            subtitle: m.description,
                                            selected: harness == h.id && m.id == modelId) {
                                        select(harness: h.id, model: m)
                                    }
                                }
                            }
                        }
                    }
                    .onAppear { openSections = [harness] }
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

    /// t3's collapsible ProviderHeader: brand mark + tracked-out uppercase
    /// provider name, trailing model count + chevron; tapping folds the section.
    private func sectionHeader(_ h: HarnessInfo) -> some View {
        let open = openSections.contains(h.id)
        return Button {
            UISelectionFeedbackGenerator().selectionChanged()
            withAnimation(Motion.collapse) {
                if open { openSections.remove(h.id) } else { openSections.insert(h.id) }
            }
        } label: {
            HStack(spacing: 7) {
                HarnessBadge(harness: h.id, size: 13)
                Text(h.label.uppercased())
                    .font(Theme.sans(10.5, weight: .medium))
                    .kerning(1.2)
                    .foregroundStyle(Theme.textMuted.opacity(0.7))
                Spacer(minLength: 8)
                if !open {
                    Text("\(models(for: h.id).count)")
                        .font(Theme.sans(10.5))
                        .foregroundStyle(Theme.textFaint)
                }
                Image(systemName: open ? "chevron.up" : "chevron.down")
                    .font(.system(size: 9, weight: .semibold))
                    .foregroundStyle(Theme.textFaint)
            }
            .padding(.horizontal, 4)
            .padding(.top, 12)
            .padding(.bottom, 6)
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
    }

    private func select(harness harnessId: String, model m: ModelInfo) {
        UISelectionFeedbackGenerator().selectionChanged()
        if harness != harnessId {
            harness = harnessId
        }
        modelId = m.id
        if let current = reasoning, m.reasoningLevels.contains(current) {
            return
        }
        reasoning = HarnessCatalog.defaultReasoning(for: m)
    }

}

// MARK: - Trait (effort) picker sheet

/// The effort ladder in its own detent sheet — the composer's second picker
/// chip, split from the model list like the desktop's Traits dropdown.
struct TraitPickerSheet: View {
    @Environment(\.dismiss) private var dismiss
    @Binding var reasoning: String?
    let levels: [String]

    var body: some View {
        NavigationStack {
            ScrollView {
                VStack(alignment: .leading, spacing: 4) {
                    SheetLabel("Effort")
                    ForEach(levels, id: \.self) { level in
                        PickRow(title: HarnessCatalog.reasoningLabel(level),
                                subtitle: Self.effortHint(level),
                                selected: reasoning == level) {
                            UISelectionFeedbackGenerator().selectionChanged()
                            reasoning = level
                        }
                    }
                }
                .padding(20)
                .padding(.bottom, 12)
            }
            .background(SheetStyle.panel)
            .navigationTitle("Traits")
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

    /// One-line hints for the ladder (the special modes deserve explanation).
    static func effortHint(_ level: String) -> String? {
        switch level {
        case "minimal": return "Quickest, lightest touch"
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

// MARK: - Model option picker sheet

/// Generic harness option picker. Codex uses it for Standard/Fast; decoding
/// the wire shape keeps iOS ready for further server-advertised choices.
struct ModelOptionPickerSheet: View {
    @Environment(\.dismiss) private var dismiss
    let option: ModelOptionInfo
    @Binding var choiceId: String

    var body: some View {
        NavigationStack {
            ScrollView {
                VStack(alignment: .leading, spacing: 4) {
                    SheetLabel(option.label)
                    ForEach(option.choices) { choice in
                        PickRow(title: choice.label,
                                subtitle: hint(for: choice),
                                selected: choiceId == choice.id) {
                            UISelectionFeedbackGenerator().selectionChanged()
                            choiceId = choice.id
                        }
                    }
                }
                .padding(20)
                .padding(.bottom, 12)
            }
            .background(SheetStyle.panel)
            .navigationTitle(option.label)
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .confirmationAction) {
                    Button { dismiss() } label: {
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

    private func hint(for choice: ModelOptionChoiceInfo) -> String? {
        guard option.id == "serviceTier" else { return nil }
        switch choice.id {
        case "default": return "Standard response speed"
        case "fast": return "Faster responses with increased usage"
        default: return nil
        }
    }
}

// MARK: - Shared picker row

/// t3's ModelRow — the ONE row style every picker sheet uses (model, trait,
/// ref, checkout): the selected row is a filled high-contrast pill with a
/// trailing checkmark; unselected rows sit almost flat on the sheet. Optional
/// leading line icon and a busy spinner for rows whose pick runs async (git
/// checkouts).
struct PickRow: View {
    let title: String
    var subtitle: String?
    var icon: LineIcon?
    var busy = false
    let selected: Bool
    let action: () -> Void

    var body: some View {
        Button(action: action) {
            HStack(spacing: 10) {
                if let icon {
                    LineIconView(icon, size: 15,
                                 color: selected ? Theme.bg : Theme.textMuted)
                        .frame(width: 20)
                }
                VStack(alignment: .leading, spacing: 2) {
                    Text(title)
                        .font(Theme.sans(15, weight: .medium))
                        .foregroundStyle(selected ? Theme.bg : Theme.text)
                    if let subtitle {
                        Text(subtitle)
                            .font(Theme.sans(12))
                            .foregroundStyle(selected ? Theme.bg.opacity(0.65) : Theme.textMuted)
                    }
                }
                Spacer(minLength: 8)
                if busy {
                    ProgressView()
                        .controlSize(.small)
                        .tint(selected ? Theme.bg : Theme.textMuted)
                } else {
                    Image(systemName: "checkmark")
                        .font(.system(size: 13, weight: .semibold))
                        .foregroundStyle(Theme.bg)
                        .opacity(selected ? 1 : 0)
                }
            }
            .padding(.horizontal, 14)
            .padding(.vertical, 11)
            .frame(maxWidth: .infinity, alignment: .leading)
            .background(selected ? AnyShapeStyle(Theme.text) : AnyShapeStyle(whiteAlpha(0.03)),
                        in: RoundedRectangle(cornerRadius: 12))
            .contentShape(RoundedRectangle(cornerRadius: 12))
        }
        .buttonStyle(SheetRowButtonStyle())
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
                VStack(alignment: .leading, spacing: 4) {
                    SheetLabel("Ref")
                    if refs.isEmpty {
                        Text("Loading refs from the device…")
                            .font(Theme.sans(13))
                            .foregroundStyle(Theme.textFaint)
                            .frame(maxWidth: .infinity)
                            .padding(.vertical, 28)
                    } else {
                        ForEach(refs, id: \.name) { ref in
                            row(ref)
                        }
                    }
                    if let error {
                        Text(error)
                            .font(Theme.sans(12.5))
                            .foregroundStyle(Theme.danger)
                            .padding(.horizontal, 4)
                            .padding(.top, 4)
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
        PickRow(title: ref.name,
                subtitle: subtitle(for: ref),
                busy: switching == ref.name,
                selected: ref.name == selected) {
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
        }
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
                VStack(alignment: .leading, spacing: 4) {
                    SheetLabel("Checkout")
                    row(.local,
                        title: selectedRefHasWorktree ? "Current worktree" : "Current checkout",
                        subtitle: selectedRefHasWorktree
                            ? "Reuse the picked ref's existing worktree"
                            : "Run in the space's folder as-is")
                    row(.newWorktree, title: "New worktree",
                        subtitle: "A fresh isolated worktree created off the picked base ref")
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

    private func row(_ rowKind: CheckoutKind, title: String, subtitle: String) -> some View {
        PickRow(title: title, subtitle: subtitle, selected: rowKind == kind) {
            UISelectionFeedbackGenerator().selectionChanged()
            onPick(rowKind)
            dismiss()
        }
    }
}
