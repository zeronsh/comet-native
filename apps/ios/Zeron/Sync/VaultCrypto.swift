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
        guard passesEd25519PointEncodingPrecheck(publicKey),
              passesEd25519PointEncodingPrecheck(signature.prefix(32)),
              passesEd25519ScalarEncodingPrecheck(signature.suffix(32)) else {
            throw VaultCryptoError.authenticationFailed
        }
        do {
            let key = try Curve25519.Signing.PublicKey(rawRepresentation: publicKey)
            guard key.isValidSignature(signature, for: message) else { throw VaultCryptoError.authenticationFailed }
        } catch {
            throw VaultCryptoError.authenticationFailed
        }
    }

    private static let ed25519FieldModulus: [UInt8] = [0xed] + Array(repeating: 0xff, count: 30) + [0x7f]

    private static let ed25519SmallOrderY: [[UInt8]] = [
        Array(repeating: 0, count: 32),
        [1] + Array(repeating: 0, count: 31),
        [0xec] + Array(repeating: 0xff, count: 30) + [0x7f],
        [
            0x26, 0xe8, 0x95, 0x8f, 0xc2, 0xb2, 0x27, 0xb0, 0x45, 0xc3, 0xf4, 0x89, 0xf2, 0xef, 0x98, 0xf0,
            0xd5, 0xdf, 0xac, 0x05, 0xd3, 0xc6, 0x33, 0x39, 0xb1, 0x38, 0x02, 0x88, 0x6d, 0x53, 0xfc, 0x05,
        ],
        [
            0xc7, 0x17, 0x6a, 0x70, 0x3d, 0x4d, 0xd8, 0x4f, 0xba, 0x3c, 0x0b, 0x76, 0x0d, 0x10, 0x67, 0x0f,
            0x2a, 0x20, 0x53, 0xfa, 0x2c, 0x39, 0xcc, 0xc6, 0x4e, 0xc7, 0xfd, 0x77, 0x92, 0xac, 0x03, 0x7a,
        ],
    ]

    private static let ed25519ScalarOrder: [UInt8] = [
        0xed, 0xd3, 0xf5, 0x5c, 0x1a, 0x63, 0x12, 0x58, 0xd6, 0x9c, 0xf7, 0xa2, 0xde, 0xf9, 0xde, 0x14,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x10,
    ]

    static func passesEd25519PointEncodingPrecheck(_ encoded: Data) -> Bool {
        guard encoded.count == 32 else { return false }
        var y = Array(encoded)
        y[31] &= 0x7f
        return y.reversed().lexicographicallyPrecedes(ed25519FieldModulus.reversed())
            && !ed25519SmallOrderY.contains(y)
    }

    static func passesEd25519ScalarEncodingPrecheck(_ encoded: Data) -> Bool {
        encoded.count == 32 && encoded.reversed().lexicographicallyPrecedes(ed25519ScalarOrder.reversed())
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
