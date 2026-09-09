// Composer — the floating glass shell in the t3 mobile composer's shape: a
// collapsed capsule (editor + send circle) that morphs into an expanded card
// with a toolbar ROW below it (attach circle · scrolling chips · pinned send)
// when the editor takes focus. A live run plus text queues the message;
// a live run with an empty composer offers Stop.
//
// Expansion is focus-driven like t3's, with the old deterministic content
// triggers kept as a floor (attachments, newline, >26 chars) — content-size
// measurement oscillates at the boundary, so it is never measured.

import PhotosUI
import SwiftUI

/// Shared glass shell + input + action row. `chips` render in the expanded
/// toolbar row between the attach and send circles.
struct ComposerShell<Chips: View>: View {
    @Binding var draft: String
    var placeholder = "Message"
    var sendEnabled: Bool
    var sendLabel = "Send message"
    /// Treat an empty box as sendable. Only the queue-edit path wants this:
    /// emptying a queued message is how you throw it away, so the button has
    /// to stay live with nothing typed.
    var allowEmptySend = false
    var showStop: Bool
    var busy = false
    /// New-session composers stay expanded — the picker chips ARE the page.
    var alwaysExpanded = false
    /// Hold the expanded layout while a picker sheet is up: presenting the
    /// sheet blurs the editor, and collapsing on that blur flaps the
    /// transcript's bottom inset mid-presentation (t3 derives expansion from
    /// focus OR sheet-active for exactly this reason).
    var keepExpanded = false
    var onSend: () -> Void
    var onStop: () -> Void = {}
    var onAdvanceQueue: () -> Void = {}
    /// Staged image attachments (attachment-ui.tsx AttachmentStrip inside the
    /// pill). Non-empty forces the expanded layout, like focus.
    var attachments: [StagedAttachment] = []
    /// Present the photo picker; nil hides the attach button.
    var onAttach: (() -> Void)? = nil
    var onRemoveAttachment: (String) -> Void = { _ in }
    /// Screenshot rig (-focuscomposer): take keyboard focus shortly after
    /// appearing, so the keyboard-up transcript states can be driven headless.
    var autoFocus = false
    @ViewBuilder var chips: Chips

    @Environment(\.verticalSizeClass) private var verticalSizeClass
    private var compact: Bool { verticalSizeClass == .compact }
    @State private var focused = false
    @State private var editor = ComposerEditorController()

    private var expanded: Bool {
        alwaysExpanded || keepExpanded || focused || !attachments.isEmpty
            || draft.contains("\n") || draft.count > 26
    }

    /// One animatable shape for background/glass/hairline: capsule-radius
    /// collapsed (46pt tall pill), 20pt card expanded (t3's 999↔20 morph).
    private var surfaceShape: RoundedRectangle {
        RoundedRectangle(cornerRadius: expanded ? 20 : 24)
    }

    // Switching between VStack/HStack via AnyLayout (rather than an if/else
    // that swaps container types) keeps `input`'s view identity stable across
    // the compact↔expanded flip — an if/else here would tear down and rebuild
    // the TextField, dropping keyboard focus mid-type.
    private var shellLayout: AnyLayout {
        expanded && !compact
            ? AnyLayout(VStackLayout(alignment: .leading, spacing: 0))
            : AnyLayout(HStackLayout(alignment: .center, spacing: 12))
    }

    var body: some View {
        surface
            // Focus-widen: margins pull in slightly while typing (chat-session.tsx).
            .padding(.horizontal, focused ? 10 : 16)
            .motionAnimation(Motion.resize, value: focused)
            .motionAnimation(Motion.resize, value: expanded)
            .onAppear {
                guard autoFocus else { return }
                Task { @MainActor in
                    try? await Task.sleep(nanoseconds: 1_500_000_000)
                    focused = true
                }
            }
    }

    /// The glass surface: collapsed = editor + send in one capsule row;
    /// expanded = attachment strip over a tall editor, with the control row
    /// (attach circle · scrolling chips · pinned send) along the card's
    /// bottom edge — everything stays inside the glass.
    private var surface: some View {
        shellLayout {
            if expanded, !attachments.isEmpty {
                AttachmentStripView(attachments: attachments, remove: onRemoveAttachment)
                    .padding(.bottom, 10)
            }
            input
                .padding(.leading, expanded ? 4 : 13)
                .padding(.trailing, expanded ? 4 : 0)
                .padding(.vertical, expanded ? 4 : 5)
                .frame(minWidth: compact ? 140 : nil,
                       minHeight: expanded && !compact ? 64 : nil, alignment: .topLeading)
            if expanded {
                HStack(spacing: 8) {
                    if onAttach != nil {
                        attachButton
                    }
                    // Chips scroll; the send button stays pinned.
                    ScrollView(.horizontal, showsIndicators: false) {
                        HStack(spacing: 8) {
                            chips
                        }
                    }
                    .scrollClipDisabled(false)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    actionButton
                }
                .padding(.top, compact ? 0 : 8)
            } else {
                actionButton
            }
        }
        .padding(.horizontal, expanded ? 12 : 5)
        .padding(.vertical, expanded ? 12 : 5)
        .background(whiteAlpha(0.04), in: surfaceShape)
        .glassEffect(.regular.interactive(), in: surfaceShape)
        .overlay(surfaceShape.strokeBorder(whiteAlpha(0.05), lineWidth: 1))
        // The whole glass surface focuses the editor, not just the TextField's
        // own text box: the collapsed pill is mostly padding, and a tap that
        // misses the text box falls through to the transcript underneath —
        // whose tap-to-blur then RESIGNS the keyboard. That's the "have to
        // press a few times" miss. Buttons and chips still win their taps.
        // Masked to .subviews while focused so cursor-placement taps inside
        // the editor stay fully native.
        .contentShape(surfaceShape)
        .gesture(TapGesture().onEnded { focused = true },
                 including: focused ? .subviews : .all)
    }

    private var input: some View {
        ComposerEditor(text: $draft, focused: $focused, placeholder: placeholder,
                       maxLines: compact ? 2 : 7, controller: editor,
                       onModifiedSubmit: {
                           guard !busy else { return }
                           editor.commit()
                           if hasContent {
                               if sendEnabled { onSend() }
                           } else {
                               onAdvanceQueue()
                           }
                           editor.apply(text: draft)
                       })
            .overlay(alignment: .topLeading) {
                if draft.isEmpty {
                    Text(placeholder)
                        .font(Theme.sans(17))
                        .foregroundStyle(Theme.text.opacity(0.3))
                        .allowsHitTesting(false)
                        .accessibilityHidden(true)
                }
            }
    }

    private var attachButton: some View {
        Button {
            onAttach?()
        } label: {
            Image(systemName: "plus")
                .font(.system(size: 16, weight: .medium))
                .foregroundStyle(Theme.textMuted)
                .frame(width: 44, height: 44)
                .background(whiteAlpha(0.06), in: Circle())
                .overlay(Circle().strokeBorder(whiteAlpha(0.08), lineWidth: 1))
                .contentShape(Circle())
        }
        .buttonStyle(.plain)
        .disabled(busy)
        .accessibilityLabel("Attach photos")
    }

    /// Attachments count as content: an image-only send is a send, never a stop.
    private var hasContent: Bool {
        allowEmptySend || !draft.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
            || !attachments.isEmpty
    }

    private var actionButton: some View {
        Button {
            if showStop, !hasContent {
                UIImpactFeedbackGenerator(style: .medium).impactOccurred()
                onStop()
            } else {
                UIImpactFeedbackGenerator(style: .light).impactOccurred()
                editor.commit()
                onSend()
                editor.apply(text: draft)
            }
        } label: {
            Group {
                if busy {
                    ProgressView()
                        .controlSize(.small)
                        .tint(Theme.bg)
                } else if showStop, !hasContent {
                    RoundedRectangle(cornerRadius: 3.5)
                        .fill(Theme.bg)
                        .frame(width: 12, height: 12)
                } else {
                    Image(systemName: "arrow.up")
                        .font(.system(size: 16, weight: .semibold))
                        .foregroundStyle(buttonActive ? Theme.bg : Theme.textFaint)
                }
            }
            .frame(width: 44, height: 44)
            .background(buttonActive ? AnyShapeStyle(Theme.text) : AnyShapeStyle(whiteAlpha(0.10)),
                        in: Circle())
            .contentShape(Circle())
        }
        .buttonStyle(.plain)
        .accessibilityLabel(showStop && !hasContent ? "Stop response" : sendLabel)
        .accessibilityIdentifier("composer-send")
        .disabled(!buttonActive)
        .motionAnimation(Motion.fadeQuick, value: showStop)
    }

    private var buttonActive: Bool {
        if showStop, !hasContent { return true }
        return sendEnabled && hasContent && !busy
    }
}

/// The live-chat composer: input, the photo attach button, the model + trait
/// picker chips (harness stays locked mid-chat; picks merge into the chat's
/// config row for the next dispatch), and the morphing action button.
struct ComposerView: View {
    @Environment(AppModel.self) private var model
    let store: SessionStore
    let chat: Chat
    let runLive: Bool

    @State private var text = ""
    @State private var attachments: [StagedAttachment] = []
    @State private var pickerItems: [PhotosPickerItem] = []
    @State private var showPicker = false
    @State private var uploading = false
    @State private var uploadProgress: Double?
    @State private var uploadError: String?
    @State private var showModelPicker = false
    @State private var showTraitPicker = false
    @State private var showOptionPicker: ModelOptionInfo?
    /// Live catalog for the chat's harness from its space's device.
    @State private var catalogs: [String: [ModelInfo]] = [:]
    @State private var queueEdit: QueueComposerEdit?
    private var editingQueuedId: String? { queueEdit?.lease.rowId }
    private var queueEditLease: QueueEditLease? { queueEdit?.lease }
    @State private var queueEditBusy = false
    @State private var queueEditInstanceId = UUID().uuidString.lowercased()
    /// Distinguishes a live composer from an async acquire that completed
    /// after navigation removed this view.
    @State private var queueEditorVisible = false

    private var harness: String { chat.config?.harness ?? "claude-code" }

    private var models: [ModelInfo] {
        catalogs[harness] ?? HarnessCatalog.models(for: harness)
    }

    private var currentModel: ModelInfo {
        models.first { $0.id == chat.config?.model }
            ?? models.first
            ?? HarnessCatalog.defaultModel(for: harness)
    }

    private var currentReasoning: String? {
        guard !currentModel.reasoningLevels.isEmpty else { return nil }
        if let r = chat.config?.reasoning, currentModel.reasoningLevels.contains(r) { return r }
        return HarnessCatalog.defaultReasoning(for: currentModel)
    }

    private func currentChoice(for option: ModelOptionInfo) -> ModelOptionChoiceInfo {
        HarnessCatalog.selectedChoice(
            for: option,
            selectedId: chat.config?.modelOptions[option.id]?.stringValue
        )
    }

    var body: some View {
        // Subscribe to the connectivity pulse so the degraded caption follows
        // the graced stream (1Hz only while something is degraded/pending).
        let _ = model.connectivity.pulse
        return VStack(spacing: 6) {
            if let error = uploadError ?? store.queueActionError {
                Text(error)
                    .font(Theme.sans(12))
                    .foregroundStyle(Theme.danger)
                    .lineLimit(2)
                    .padding(.horizontal, 24)
                    .frame(maxWidth: .infinity, alignment: .leading)
            }
            if uploading, let uploadProgress {
                Text("Uploading… \(Int(uploadProgress * 100))%")
                    .font(Theme.sans(11))
                    .foregroundStyle(Theme.textMuted)
                    .monospacedDigit()
                    .padding(.horizontal, 24)
                    .frame(maxWidth: .infinity, alignment: .leading)
            }
            // Pre-send honesty (composer.rs degraded notice, post-#170 copy):
            // one quiet caption line, never a warning box — the send still
            // works, it just queues.
            if model.chatDeliveryDegraded(chat) {
                Text(model.connectivity.state == .offline
                    ? "Offline — messages will send when you're back online."
                    : "Messages will send once the connection recovers.")
                    .font(Theme.sans(11))
                    .foregroundStyle(Theme.textFaint)
                    .padding(.horizontal, 24)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .transition(.opacity)
            }
            // What is waiting to be sent, stacked directly above the box it
            // was typed in.
            if !store.queue.isEmpty {
                QueuePanel(store: store,
                           editingId: editingQueuedId,
                           onEdit: { item in Task { await beginQueueEdit(item) } },
                           onCancelEdit: { Task { await cancelQueueEdit() } },
                           supportsActions: supportsQueueActions,
                           onAction: performQueueAction)
            }
            if editingQueuedId != nil {
                Button(queueEdit?.terminal == true ? "Copy edit and stop editing" : "Stop editing") {
                    if queueEdit?.terminal == true {
                        UIPasteboard.general.string = text
                        finishLocalQueueEdit(.finished, failure: "")
                    } else {
                        Task { await cancelQueueEdit() }
                    }
                }
                .buttonStyle(.plain)
                .font(Theme.sans(12))
                .foregroundStyle(Theme.textMuted)
                .disabled(queueEditBusy)
                .accessibilityHint(queueEdit?.terminal == true ? "Copies the edit and restores your previous draft" : "")
            }
            ComposerShell(
                draft: $text,
                placeholder: editingQueuedId == nil ? "Message" : "Edit queued message",
                sendEnabled: true,
                sendLabel: editingQueuedId != nil ? "Save queued message"
                    : (runLive ? "Queue message" : "Send message"),
                allowEmptySend: editingQueuedId != nil,
                showStop: runLive,
                busy: uploading || queueEditBusy,
                keepExpanded: showModelPicker || showTraitPicker || showOptionPicker != nil,
                onSend: send,
                onStop: { store.sendInterrupt() },
                onAdvanceQueue: advanceQueue,
                attachments: editingQueuedId == nil ? attachments : [],
                onAttach: editingQueuedId == nil ? { showPicker = true } : nil,
                onRemoveAttachment: { id in attachments.removeAll { $0.id == id } },
                autoFocus: model.launchFocusComposer
            ) {
                if let pullRequest = model.changeRequest(for: chat) {
                    PullRequestBadge(summary: pullRequest, surface: .composer)
                }
                if let branch = chat.branch?.trimmingCharacters(in: .whitespacesAndNewlines),
                   !branch.isEmpty {
                    BranchContextChip(branch: branch)
                }
                ComposerChip(label: currentModel.label, badgeHarness: harness) {
                    showModelPicker = true
                }
                if let currentReasoning {
                    ComposerChip(label: HarnessCatalog.reasoningLabel(currentReasoning)) {
                        showTraitPicker = true
                    }
                }
                ForEach(currentModel.options) { option in
                    ComposerChip(label: currentChoice(for: option).label) {
                        showOptionPicker = option
                    }
                }
            }
        }
        .photosPicker(isPresented: $showPicker, selection: $pickerItems,
                      maxSelectionCount: 8, matching: .images)
        .onChange(of: pickerItems) { _, items in
            guard !items.isEmpty else { return }
            stage(items)
        }
        .sheet(isPresented: $showModelPicker) {
            ModelPickerSheet(
                harness: .constant(harness),
                modelId: Binding(
                    get: { currentModel.id },
                    set: { writeConfig(model: $0, reasoning: chat.config?.reasoning) }
                ),
                reasoning: Binding(
                    get: { chat.config?.reasoning },
                    set: { writeConfig(model: chat.config?.model, reasoning: $0) }
                ),
                lockedHarness: true,
                catalogs: catalogs
            )
        }
        .sheet(isPresented: $showTraitPicker) {
            TraitPickerSheet(
                reasoning: Binding(
                    get: { currentReasoning },
                    set: { writeConfig(model: chat.config?.model, reasoning: $0) }
                ),
                levels: currentModel.reasoningLevels
            )
        }
        .sheet(item: $showOptionPicker) { option in
            ModelOptionPickerSheet(option: option, choiceId: Binding(
                get: { currentChoice(for: option).id },
                set: { writeOption(option: option, choiceId: $0) }
            ))
        }
        .task(id: "\(chat.id)/\(harness)") {
            guard let space = model.space(for: chat) else { return }
            catalogs[harness] = await model.listModels(space: space, harness: harness)
        }
        .task(id: queueEdit?.terminal == true ? nil : queueEditLease?.leaseId) {
            guard let lease = queueEditLease, queueEdit?.terminal != true else { return }
            while !Task.isCancelled, queueEditLease?.leaseId == lease.leaseId {
                try? await Task.sleep(for: .seconds(20))
                guard !Task.isCancelled else { return }
                if !(await store.renewQueuedEdit(lease)) {
                    uploadError = "Edit protection may have expired — review before sending."
                }
            }
        }
        .onAppear {
            queueEditorVisible = true
            if model.launchSheet == "config" {
                model.launchSheet = nil
                showModelPicker = true
            }
        }
        .onDisappear {
            queueEditorVisible = false
            guard let lease = queueEditLease else { return }
            text = queueEdit?.originalDraft ?? ""
            queueEdit = nil
            queueEditBusy = false
            Task { _ = await store.finishQueuedEdit(lease, action: "cancel") }
        }
    }

    /// Merge a model/effort change into the chat's config row (LWW; the host
    /// picks it up on the next run dispatch). Compatible model options survive
    /// edits; changing model prunes traits the destination does not advertise.
    private func writeConfig(model newModel: String?, reasoning newReasoning: String?) {
        var config = chat.config ?? ChatConfig(harness: harness, model: nil,
                                               reasoning: nil, sandbox: "workspace-write")
        let changedModel = newModel != nil && newModel != config.model
        config.model = newModel
        config.reasoning = newReasoning
        if changedModel, let newModel,
           let target = models.first(where: { $0.id == newModel }) {
            var compatible: [String: JSONValue] = [:]
            for option in target.options {
                guard let selected = config.modelOptions[option.id]?.stringValue,
                      selected != option.defaultChoice,
                      option.choices.contains(where: { $0.id == selected }) else { continue }
                compatible[option.id] = .string(selected)
            }
            config.modelOptions = compatible
        }
        model.setChatConfig(chatId: chat.id, config: config)
    }

    private func writeOption(option: ModelOptionInfo, choiceId: String) {
        var config = chat.config ?? ChatConfig(harness: harness, model: currentModel.id,
                                               reasoning: currentReasoning,
                                               sandbox: "workspace-write")
        if choiceId == option.defaultChoice {
            config.modelOptions.removeValue(forKey: option.id)
        } else {
            config.modelOptions[option.id] = .string(choiceId)
        }
        model.setChatConfig(chatId: chat.id, config: config)
    }

    /// Load picked photos into staged attachments (HEIC transcodes to JPEG;
    /// unsupported/oversized picks surface as an error line).
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
            if failed > 0 {
                uploadError = failed == 1
                    ? "One image couldn't be attached (unsupported or over 24 MB)."
                    : "\(failed) images couldn't be attached (unsupported or over 24 MB)."
            } else {
                uploadError = nil
            }
        }
    }

    /// Retype a queued message in the composer. Whatever was already in the box
    /// steps aside rather than being lost.
    @MainActor
    private func beginQueueEdit(_ item: QueuedMessage) async {
        guard editingQueuedId == nil, !queueEditBusy else { return }
        guard model.hostSupportsQueueEditLease(chat) else {
            uploadError = "Update the chat host to edit queued messages safely."
            return
        }
        queueEditBusy = true
        let result = await store.beginQueuedEdit(id: item.id, instanceId: queueEditInstanceId)
        queueEditBusy = false
        switch result {
        case .acquired(let lease):
            guard queueEditorVisible else {
                // Navigation won the race with the host ACK. Release the
                // newly acquired lease instead of leaving a ghost editor.
                _ = await store.finishQueuedEdit(lease, action: "cancel")
                return
            }
            queueEdit = QueueComposerEdit(lease: lease, originalDraft: text,
                                          hasAttachments: !item.attachments.isEmpty)
            text = MessageQueue.visibleText(lease.text, attachments: item.attachments)
            uploadError = nil
        case .locked:
            uploadError = "That queued message is being edited on another device."
        case .missing:
            uploadError = "That queued message is no longer available."
        case .unavailable:
            uploadError = "Connect to the chat host to edit this message."
        }
    }

    @MainActor
    private func cancelQueueEdit() async {
        guard let lease = queueEditLease, !queueEditBusy else { return }
        queueEditBusy = true
        let result = await store.finishQueuedEdit(lease, action: "cancel")
        queueEditBusy = false
        finishLocalQueueEdit(result, failure: "Couldn't cancel the protected edit.")
    }

    @MainActor
    private func commitQueueEdit() async {
        guard let lease = queueEditLease, !queueEditBusy else { return }
        let edited = queueEdit?.textToCommit(text)
        queueEditBusy = true
        let result: QueueEditFinishResult
        if let edited {
            result = await store.finishQueuedEdit(lease, action: "commit", text: edited)
        } else {
            result = await store.finishQueuedEdit(lease, action: "discard")
        }
        queueEditBusy = false
        finishLocalQueueEdit(result, failure: "Couldn't save the protected edit.")
    }

    @MainActor
    private func finishLocalQueueEdit(_ result: QueueEditFinishResult, failure: String) {
        queueEdit?.receive(result)
        switch result {
        case .finished:
            text = queueEdit?.originalDraft ?? ""
            queueEdit = nil
            uploadError = nil
        case .conflict:
            uploadError = "This message changed on another device; your edit is still here."
        case .missing:
            uploadError = "Message removed. Stop editing to copy your text and restore your draft."
        case .lost:
            uploadError = "Edit protection changed. Stop editing to copy your text and restore your draft."
        case .unavailable:
            uploadError = failure
        }
    }

    private func send() {
        // Editing a queued row: the button saves that row rather than sending
        // anything new.
        if editingQueuedId != nil {
            Task { await commitQueueEdit() }
            return
        }
        let submittedText = text
        let prompt = submittedText.trimmingCharacters(in: .whitespacesAndNewlines)
        let staged = attachments
        guard !runLive || model.hostSupportsMessageQueue(chat, attachments: !staged.isEmpty) else {
            uploadError = "Update the chat's engine to queue messages during a response."
            return
        }
        guard !prompt.isEmpty || !staged.isEmpty else { return }

        if staged.isEmpty {
            deliver(content: prompt, paths: [])
            clearDraft(matching: submittedText)
            return
        }
        if model.hostSupportsQueuedAttachments(chat)
            && !(runLive && model.hostSupportsMessageQueue(chat, attachments: true)) {
            // Queued flow (host ≥ 0.2.12): the send is a durable local write
            // NOW — pending:// refs in the doc, bytes escorted afterwards
            // with retry-forever — so an image send survives a dead link
            // exactly like a text send does.
            let transfers = staged.map {
                AttachmentTransfer(uploadId: UUID().uuidString.lowercased(),
                                   name: $0.name, data: $0.data)
            }
            for (transfer, att) in zip(transfers, staged) {
                // Seed under the pending ref — the echo's thumbnail renders
                // from local bytes while the upload crosses the relay.
                AttachmentImageCache.shared.seed(
                    deviceId: chat.deviceId,
                    path: UploadStash.pendingRef(uploadId: transfer.uploadId, name: transfer.name),
                    name: att.name, data: att.data)
            }
            store.sendWithTransfers(prompt: prompt, chat: chat, live: runLive,
                                    transfers: transfers)
            attachments = []
            clearDraft(matching: submittedText)
            return
        }
        // Legacy host-staged flow (host < 0.2.12): upload first, send after —
        // the refs trailer needs the committed paths. Bounded by the
        // whole-attachment deadline; progress narrates instead of a bare
        // spinner (a lawful crawl must never read as a hang).
        uploading = true
        uploadError = nil
        uploadProgress = 0
        let progressBinding = $uploadProgress
        Task { @MainActor in
            defer {
                uploading = false
                uploadProgress = nil
            }
            do {
                var paths: [String] = []
                let total = staged.count
                for (ix, att) in staged.enumerated() {
                    let path = try await store.uploadAttachment(name: att.name, data: att.data) { fraction in
                        progressBinding.wrappedValue = (Double(ix) + fraction) / Double(total)
                    }
                    // Seed the cache so our own bubble renders from local
                    // bytes instead of a round-trip.
                    AttachmentImageCache.shared.seed(deviceId: chat.deviceId, path: path,
                                                     name: att.name, data: att.data)
                    paths.append(path)
                }
                deliver(content: withAttachments(text: prompt, paths: paths), paths: paths)
                attachments = []
                clearDraft(matching: submittedText)
            } catch {
                uploadError = "Attachment upload failed — \(error.localizedDescription)"
            }
        }
    }

    private func deliver(content: String, paths: [String]) {
        // All active-turn messages wait in the shared queue.
        if runLive {
            let queueText = model.hostSupportsCleanQueueAttachmentText(chat)
                ? MessageQueue.visibleText(content, attachments: paths)
                : content
            store.enqueueMessage(text: queueText, attachments: paths, holdForTurnEnd: true)
        } else {
            store.sendRun(prompt: content, chat: chat, attachments: paths)
        }
    }

    private var supportsQueueActions: Bool {
        model.workspace?.deviceSupports(chat.deviceId, EngineCapability.messageQueueActionsV1) ?? false
    }

    private func advanceQueue() {
        guard editingQueuedId == nil, !queueEditBusy, !uploading,
              let head = store.queue.first,
              let action = MessageQueue.primaryAction(
                for: head,
                supportsActions: supportsQueueActions,
                pending: store.queueActionsPending.contains(head.id)) else { return }
        performQueueAction(head, action)
    }

    private func performQueueAction(_ item: QueuedMessage, _ action: QueueAction) {
        guard supportsQueueActions, !queueEditBusy else { return }
        uploadError = nil
        // Discard while the lease still blocks delivery; cancelling it first
        // would open a window in which the host could send the old text.
        if action == .remove, editingQueuedId == item.id, let lease = queueEditLease {
            queueEditBusy = true
            Task { @MainActor in
                let result = await store.finishQueuedEdit(lease, action: "discard")
                queueEditBusy = false
                finishLocalQueueEdit(result, failure: "Couldn't remove the protected message.")
            }
            return
        }
        Task { await store.performQueueAction(id: item.id, action: action) }
    }

    private func clearDraft(matching submittedText: String) {
        // A legacy attachment upload can finish after the user edits the draft.
        guard text == submittedText else { return }
        text = ""
    }
}

// MARK: - Question panel (composer.rs Wizard)

struct QuestionPanel: View {
    let requestId: String
    let questions: [UserInputQuestion]
    let respond: (String, [UserInputAnswer]) -> Void

    @State private var page = 0
    @State private var picked: [String: Set<String>] = [:]  // questionId → labels
    @State private var typed: [String: String] = [:]
    @State private var autoAdvanceTask: Task<Void, Never>?

    var body: some View {
        // `questions[min(page, count - 1)]` traps on an empty list (count - 1
        // is -1). A request whose questions fail to decode reaches here empty,
        // so this crashed the app on any session holding one.
        if questions.isEmpty {
            EmptyView()
        } else {
            panel(for: questions[min(max(page, 0), questions.count - 1)])
        }
    }

    private func panel(for question: UserInputQuestion) -> some View {
        VStack(alignment: .leading, spacing: 12) {
            HStack {
                Text(question.header.uppercased())
                    .font(Theme.sans(10.5, weight: .medium))
                    .kerning(1)
                    .foregroundStyle(Theme.textMuted.opacity(0.6))
                Spacer()
                if questions.count > 1 {
                    Text("\(page + 1)/\(questions.count)")
                        .font(Theme.sans(10))
                        .foregroundStyle(Theme.textMuted)
                        .padding(.horizontal, 6)
                        .frame(height: 20)
                        .background(whiteAlpha(0.06), in: RoundedRectangle(cornerRadius: 6))
                }
            }

            Text(question.question)
                .font(Theme.sans(15, weight: .medium))
                .foregroundStyle(Theme.text)
                .fixedSize(horizontal: false, vertical: true)

            if question.multiSelect == true {
                Text("Select one or more options.")
                    .font(Theme.sans(12))
                    .foregroundStyle(Theme.textMuted)
            }

            VStack(spacing: 4) {
                ForEach(Array(question.options.enumerated()), id: \.offset) { ix, option in
                    optionRow(question: question, ix: ix, option: option)
                }
            }

            VStack(alignment: .leading, spacing: 6) {
                Rectangle().fill(whiteAlpha(0.06)).frame(height: 1)
                TextField("Or type your own answer", text: Binding(
                    get: { typed[question.id] ?? "" },
                    set: { typed[question.id] = $0 }
                ))
                .font(Theme.sans(13))
                .foregroundStyle(Theme.text)
                .padding(.top, 6)
            }

            HStack {
                if page > 0 {
                    Button("Back") {
                        page -= 1
                    }
                    .font(Theme.sans(13, weight: .medium))
                    .foregroundStyle(Theme.textMuted)
                }
                Spacer()
                Button(page < questions.count - 1 ? "Next" : "Submit") {
                    advance()
                }
                .font(Theme.sans(13, weight: .medium))
                .foregroundStyle(Theme.bg)
                .padding(.horizontal, 16)
                .frame(height: 34)
                .background(Theme.text, in: Capsule())
                .opacity(canAdvance(question) ? 1 : 0.4)
                .disabled(!canAdvance(question))
            }
        }
        .padding(16)
        .glassEffect(.regular, in: RoundedRectangle(cornerRadius: 26))
        .overlay(RoundedRectangle(cornerRadius: 26).strokeBorder(whiteAlpha(0.05), lineWidth: 1))
        .padding(.horizontal, 12)
        .transition(.opacity)
    }

    private func optionRow(question: UserInputQuestion, ix: Int, option: String) -> some View {
        let isPicked = (typed[question.id] ?? "").isEmpty
            && picked[question.id, default: []].contains(option)
        return Button {
            pick(question: question, option: option)
        } label: {
            HStack(spacing: 10) {
                VStack(alignment: .leading, spacing: 2) {
                    Text(option)
                        .font(Theme.sans(13.5, weight: .medium))
                        .foregroundStyle(Theme.text)
                        .multilineTextAlignment(.leading)
                }
                Spacer(minLength: 0)
                if ix < 9 {
                    Text("\(ix + 1)")
                        .font(Theme.sans(11))
                        .foregroundStyle(Theme.textMuted)
                        .frame(width: 22, height: 22)
                        .background(whiteAlpha(0.06), in: RoundedRectangle(cornerRadius: 6))
                }
            }
            .padding(.horizontal, 14)
            .padding(.vertical, 10)
            .background(isPicked ? whiteAlpha(0.09) : whiteAlpha(0.025),
                        in: RoundedRectangle(cornerRadius: 12))
            .overlay(RoundedRectangle(cornerRadius: 12)
                .strokeBorder(isPicked ? whiteAlpha(0.16) : .clear, lineWidth: 1))
        }
        .buttonStyle(.plain)
    }

    private func pick(question: UserInputQuestion, option: String) {
        typed[question.id] = nil
        if question.multiSelect == true {
            var set = picked[question.id, default: []]
            if set.contains(option) { set.remove(option) } else { set.insert(option) }
            picked[question.id] = set
        } else {
            picked[question.id] = [option]
            // Single-select auto-advances after 220ms (AUTO_ADVANCE_MS).
            autoAdvanceTask?.cancel()
            autoAdvanceTask = Task {
                try? await Task.sleep(nanoseconds: 220_000_000)
                guard !Task.isCancelled else { return }
                advance()
            }
        }
    }

    private func canAdvance(_ question: UserInputQuestion) -> Bool {
        !(typed[question.id] ?? "").isEmpty || !picked[question.id, default: []].isEmpty
    }

    private func advance() {
        let question = questions[min(page, questions.count - 1)]
        guard canAdvance(question) else { return }
        if page < questions.count - 1 {
            page += 1
            return
        }
        let answers = questions.map { q -> UserInputAnswer in
            let typedAnswer = (typed[q.id] ?? "").trimmingCharacters(in: .whitespacesAndNewlines)
            if !typedAnswer.isEmpty {
                return UserInputAnswer(questionId: q.id, labels: [typedAnswer])
            }
            return UserInputAnswer(questionId: q.id, labels: Array(picked[q.id, default: []]))
        }
        respond(requestId, answers)
    }
}
