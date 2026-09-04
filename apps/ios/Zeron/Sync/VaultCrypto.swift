import CryptoKit
import Foundation

enum VaultCryptoError: Error, Equatable {
    case invalidKeyLength
    case invalidNonceLength
    case invalidCiphertextLength
    case invalidSignatureLength
    case invalidOutputLength
    case sizeLimitExceeded
    case authenticationFailed
}

enum VaultCrypto {
    static func openAES256GCM(
        key: SymmetricKey, nonce: Data, aad: Data,
        ciphertextAndTag: Data, maxPlaintextBytes: Int
    ) throws -> Data {
        guard key.bitCount == 256 else { throw VaultCryptoError.invalidKeyLength }
        guard nonce.count == 12 else { throw VaultCryptoError.invalidNonceLength }
        guard ciphertextAndTag.count >= 16 else { throw VaultCryptoError.invalidCiphertextLength }
        guard ciphertextAndTag.count - 16 <= maxPlaintextBytes else { throw VaultCryptoError.sizeLimitExceeded }
        do {
            let box = try AES.GCM.SealedBox(
                nonce: AES.GCM.Nonce(data: nonce),
                ciphertext: ciphertextAndTag.dropLast(16),
                tag: ciphertextAndTag.suffix(16)
            )
            return try AES.GCM.open(box, using: key, authenticating: aad)
        } catch {
            throw VaultCryptoError.authenticationFailed
        }
    }

    static func verifyEd25519(publicKey: Data, message: Data, signature: Data) throws {
        guard publicKey.count == 32 else { throw VaultCryptoError.invalidKeyLength }
        guard signature.count == 64 else { throw VaultCryptoError.invalidSignatureLength }
        do {
            let key = try Curve25519.Signing.PublicKey(rawRepresentation: publicKey)
            guard key.isValidSignature(signature, for: message) else { throw VaultCryptoError.authenticationFailed }
        } catch {
            throw VaultCryptoError.authenticationFailed
        }
    }

    static func hkdfSHA256(
        inputKeyMaterial: SymmetricKey, salt: Data, info: Data, outputByteCount: Int
    ) throws -> SymmetricKey {
        guard (1...8160).contains(outputByteCount) else { throw VaultCryptoError.invalidOutputLength }
        return HKDF<SHA256>.deriveKey(
            inputKeyMaterial: inputKeyMaterial, salt: salt, info: info, outputByteCount: outputByteCount
        )
    }
}
