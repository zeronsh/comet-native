// LoroValue ⇄ Swift bridging: deep-value readers for the mirrors, and a
// JSON-shaped writer for nested command payloads (the Swift analogue of
// schema.rs `loro_value_from_json`).

import Foundation
import Loro

extension LoroValue {
    var stringValue: String? {
        if case .string(let v) = self { return v }
        return nil
    }

    var i64Value: Int64? {
        switch self {
        case .i64(let v): return v
        case .double(let v): return Int64(v)
        default: return nil
        }
    }

    var boolValue: Bool? {
        if case .bool(let v) = self { return v }
        return nil
    }

    var listValue: [LoroValue]? {
        if case .list(let v) = self { return v }
        return nil
    }

    var mapValue: [String: LoroValue]? {
        if case .map(let v) = self { return v }
        return nil
    }

    /// Loose JSON-ish projection for payload fields (tool call details).
    var jsonObject: Any {
        switch self {
        case .null: return NSNull()
        case .bool(let v): return v
        case .double(let v): return v
        case .i64(let v): return v
        case .binary(let v): return v
        case .string(let v): return v
        case .list(let v): return v.map(\.jsonObject)
        case .map(let v): return v.mapValues(\.jsonObject)
        case .container: return NSNull()
        }
    }

    /// Build a LoroValue from Encodable JSON (command payloads).
    static func fromJSON(_ any: Any) -> LoroValue {
        switch any {
        case is NSNull: return .null
        case let b as Bool: return .bool(value: b)
        case let n as NSNumber:
            // NSNumber bools are handled above; integers stay i64 like serde.
            if CFNumberIsFloatType(n) { return .double(value: n.doubleValue) }
            return .i64(value: n.int64Value)
        case let s as String: return .string(value: s)
        case let arr as [Any]: return .list(value: arr.map { fromJSON($0) })
        case let dict as [String: Any]: return .map(value: dict.mapValues { fromJSON($0) })
        default: return .null
        }
    }

    static func fromEncodable<T: Encodable>(_ value: T) -> LoroValue? {
        guard let data = try? JSONEncoder().encode(value),
              let obj = try? JSONSerialization.jsonObject(with: data) else { return nil }
        return fromJSON(obj)
    }
}
