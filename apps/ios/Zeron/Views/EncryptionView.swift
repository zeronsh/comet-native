import SwiftUI

/// Encrypted sync on the phone (RFC 0001 §4.2): the vault state, the
/// comparison code while an approval is pending, and the two ways in —
/// approval from an already-approved device, or the recovery kit. The
/// vault fingerprint typed here comes from the approving device (Settings →
/// Encryption → "Copy vault fingerprint", or `zeron vault status`) and pins
/// the genesis so a relay cannot hand this phone a substitute vault.
struct EncryptionView: View {
    @Environment(AppModel.self) private var model
    @Environment(\.dismiss) private var dismiss
    @State private var fingerprint = ""
    @State private var kit = ""
    @State private var error: String?
    @State private var working = false

    private var status: MobileVaultStatus? { model.vaultStatus }

    var body: some View {
        NavigationStack {
            List {
                Section {
                    LabeledContent("Status", value: statusTitle)
                    Text(statusCopy)
                        .font(.footnote)
                        .foregroundStyle(.secondary)
                    if let fingerprint = status?.fingerprint {
                        LabeledContent("Vault fingerprint") {
                            Text(fingerprint.prefix(16) + "…")
                                .font(.system(.footnote, design: .monospaced))
                                .textSelection(.enabled)
                        }
                    }
                    if let epoch = status?.epoch {
                        LabeledContent("Key epoch", value: String(epoch))
                    }
                } header: {
                    Text("End-to-end encryption")
                }

                if let code = status?.pairingCode, status?.phase == .pending {
                    Section {
                        Text(code)
                            .font(.system(size: 34, weight: .semibold, design: .monospaced))
                            .frame(maxWidth: .infinity)
                            .padding(.vertical, 8)
                        Text("On the approving device, approve only if it shows exactly this code.")
                            .font(.footnote)
                            .foregroundStyle(.secondary)
                    } header: {
                        Text("Comparison code")
                    }
                }

                if canEnroll {
                    Section {
                        TextField("Vault fingerprint (64 hex characters)", text: $fingerprint)
                            .font(.system(.footnote, design: .monospaced))
                            .textInputAutocapitalization(.never)
                            .autocorrectionDisabled()
                        Button("Approve from another device") { run { try await model.enrollVault(fingerprintHex: fingerprint) } }
                            .disabled(working || fingerprint.count < 64)
                    } header: {
                        Text("Approve this device")
                    } footer: {
                        Text("Paste the vault fingerprint shown on an approved device. That device then compares the code above before approving. An approved device can read all synced content and manage devices.")
                    }

                    Section {
                        TextField("Recovery key (XXXXX-XXXXX-…)", text: $kit)
                            .font(.system(.footnote, design: .monospaced))
                            .textInputAutocapitalization(.characters)
                            .autocorrectionDisabled()
                        Button("Use recovery key") { run { try await model.recoverVault(kit: kit, fingerprintHex: fingerprint) } }
                            .disabled(working || kit.count < 55 || fingerprint.count < 64)
                    } header: {
                        Text("Recovery")
                    } footer: {
                        Text("Enter the recovery key and the vault fingerprint from your recovery file. This adds the phone under a fresh key epoch; other devices catch up automatically.")
                    }
                }

                if let error {
                    Section {
                        Text(error).foregroundStyle(.red)
                    }
                }
            }
            .navigationTitle("Encryption")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .topBarLeading) {
                    Button("Done") { dismiss() }
                }
                ToolbarItem(placement: .topBarTrailing) {
                    Button {
                        model.refreshVault()
                    } label: {
                        if model.vaultBusy || working {
                            ProgressView()
                        } else {
                            Image(systemName: "arrow.clockwise")
                        }
                    }
                    .disabled(model.vaultBusy || working)
                }
            }
        }
    }

    private var canEnroll: Bool {
        switch status?.phase {
        case .notEnrolled, .revoked, .legacy, .pending, nil: return true
        default: return false
        }
    }

    private var statusTitle: String {
        switch status?.phase {
        case .ready: return "Encrypted"
        case .legacy: return "Not set up"
        case .notEnrolled: return "Approve this device"
        case .pending: return "Waiting for approval"
        case .locked: return "Locked"
        case .keyUpdateRequired: return "Waiting for keys"
        case .verificationFailed: return "Sync paused"
        case .revoked: return "Removed"
        case .recoveryConfirmationRequired: return "Confirm recovery kit"
        case .checking, nil: return "Checking…"
        }
    }

    private var statusCopy: String {
        if let message = status?.message { return message }
        switch status?.phase {
        case .ready:
            return "Synced content is encrypted on your devices. Only approved devices, or someone with your recovery key, can read it."
        case .legacy:
            return "This account has no encrypted vault. Set one up on a desktop (Settings → Encryption or `zeron vault setup`), then approve this phone."
        case .notEnrolled, .revoked:
            return "This account uses end-to-end encryption. Nothing syncs to this phone until an approved device admits it."
        case .pending:
            return "Open Settings → Encryption on an approved device and compare the code."
        case .locked:
            return "Secure key storage is unavailable on this phone. Existing data was retained."
        case .keyUpdateRequired:
            return "Another device changed the vault's keys; sync resumes once the new keys arrive."
        case .verificationFailed:
            return "Data from the sync backend could not be verified. Sync stays paused."
        case .recoveryConfirmationRequired:
            return "Confirm the recovery kit on the device that created the vault."
        case .checking, nil:
            return "Checking the vault for this account."
        }
    }

    private func run(_ operation: @escaping () async throws -> Void) {
        error = nil
        working = true
        Task {
            do { try await operation() } catch { self.error = error.localizedDescription }
            working = false
        }
    }
}
