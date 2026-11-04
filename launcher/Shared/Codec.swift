// The strict codec every faber-vm wire shares: the supervisor protocol, the child protocol, the
// guest agent protocol (docs/vm-agent-protocol.md), the claim markers and the network plan.
// It is compiled into both faber-vm and the guest agent, faber-agent, so it uses Foundation and
// Darwin only, never Virtualization.
//
// JSONSerialization and JSONDecoder cannot refuse a repeated key or tell 1 from 1.0, so the
// reader below is written by hand: objects, arrays, strings and non-negative integers, and
// nothing else. src/vm/proto.rs is the reference for every edge it refuses.
//
// An error is a fixed category. None carries, and none prints, a byte from the peer: the
// peer sits across the wall, and a jit_config is a credential.

import Darwin
import Foundation

/// The largest frame, its LF included.
let maxFrame = 16384

/// Why a frame was refused. Every case is a fixed category: `invalid` takes only literals
/// from this file, never the peer's bytes.
enum ProtoError: Error, Equatable, CustomStringConvertible {
    case frameTooLarge
    case notOneLine
    case notUTF8
    case malformed
    case invalid(String)

    var description: String {
        switch self {
        case .frameTooLarge: "frame is larger than the protocol allows"
        case .notOneLine: "frame is not exactly one LF-terminated line"
        case .notUTF8: "frame is not UTF-8"
        case .malformed: "frame is not the protocol's JSON"
        case .invalid(let what): "invalid \(what)"
        }
    }
}

// MARK: - The strict JSON reader

/// An object's members in the order they came, each key once.
struct JSONObject {
    var keys: [String] = []
    var values: [String: JSON] = [:]
}

/// The JSON this protocol uses: no floats, no signs, no booleans and no null.
indirect enum JSON {
    case object(JSONObject)
    case array([JSON])
    case string(String)
    case integer(UInt64)
}

struct StrictJSONReader {
    private let bytes: [UInt8]
    private var at = 0
    private static let maxDepth = 16

    private init(_ bytes: [UInt8]) { self.bytes = bytes }

    /// One JSON value, with nothing but JSON whitespace around it.
    static func parse(_ bytes: [UInt8]) throws -> JSON {
        var reader = StrictJSONReader(bytes)
        reader.skipSpace()
        let value = try reader.value(depth: 0)
        reader.skipSpace()
        guard reader.at == bytes.count else { throw ProtoError.malformed }
        return value
    }

    private var peek: UInt8? { at < bytes.count ? bytes[at] : nil }

    private mutating func skipSpace() {
        while let c = peek, c == 0x20 || c == 0x09 || c == 0x0A || c == 0x0D { at += 1 }
    }

    private mutating func expect(_ c: UInt8) throws {
        guard peek == c else { throw ProtoError.malformed }
        at += 1
    }

    private mutating func value(depth: Int) throws -> JSON {
        guard depth < Self.maxDepth, let c = peek else { throw ProtoError.malformed }
        switch c {
        case UInt8(ascii: "{"): return .object(try object(depth: depth))
        case UInt8(ascii: "["): return .array(try array(depth: depth))
        case UInt8(ascii: "\""): return .string(try string())
        case UInt8(ascii: "0")...UInt8(ascii: "9"): return .integer(try integer())
        default: throw ProtoError.malformed  // a sign, true, false, null, or anything else
        }
    }

    private mutating func object(depth: Int) throws -> JSONObject {
        try expect(UInt8(ascii: "{"))
        var out = JSONObject()
        skipSpace()
        if peek == UInt8(ascii: "}") {
            at += 1
            return out
        }
        while true {
            skipSpace()
            let key = try string()
            guard out.values[key] == nil else { throw ProtoError.invalid("duplicate key") }
            skipSpace()
            try expect(UInt8(ascii: ":"))
            skipSpace()
            out.values[key] = try value(depth: depth + 1)
            out.keys.append(key)
            skipSpace()
            if peek == UInt8(ascii: ",") {
                at += 1
                continue
            }
            try expect(UInt8(ascii: "}"))
            return out
        }
    }

    private mutating func array(depth: Int) throws -> [JSON] {
        try expect(UInt8(ascii: "["))
        var out: [JSON] = []
        skipSpace()
        if peek == UInt8(ascii: "]") {
            at += 1
            return out
        }
        while true {
            skipSpace()
            out.append(try value(depth: depth + 1))
            skipSpace()
            if peek == UInt8(ascii: ",") {
                at += 1
                continue
            }
            try expect(UInt8(ascii: "]"))
            return out
        }
    }

    /// Digits only, no leading zero, and no fraction or exponent after them.
    private mutating func integer() throws -> UInt64 {
        let start = at
        var n: UInt64 = 0
        while let c = peek, c >= UInt8(ascii: "0"), c <= UInt8(ascii: "9") {
            let (times, o1) = n.multipliedReportingOverflow(by: 10)
            let (plus, o2) = times.addingReportingOverflow(UInt64(c - UInt8(ascii: "0")))
            guard !o1, !o2 else { throw ProtoError.malformed }
            n = plus
            at += 1
        }
        if at - start > 1, bytes[start] == UInt8(ascii: "0") { throw ProtoError.malformed }
        if let c = peek, c == UInt8(ascii: ".") || c == UInt8(ascii: "e") || c == UInt8(ascii: "E") {
            throw ProtoError.malformed
        }
        return n
    }

    private mutating func hex4() throws -> UInt32 {
        guard at + 4 <= bytes.count else { throw ProtoError.malformed }
        var v: UInt32 = 0
        for _ in 0..<4 {
            let c = bytes[at]
            let d: UInt8
            switch c {
            case UInt8(ascii: "0")...UInt8(ascii: "9"): d = c - UInt8(ascii: "0")
            case UInt8(ascii: "a")...UInt8(ascii: "f"): d = c - UInt8(ascii: "a") + 10
            case UInt8(ascii: "A")...UInt8(ascii: "F"): d = c - UInt8(ascii: "A") + 10
            default: throw ProtoError.malformed
            }
            v = v << 4 | UInt32(d)
            at += 1
        }
        return v
    }

    private mutating func string() throws -> String {
        try expect(UInt8(ascii: "\""))
        var out: [UInt8] = []
        while true {
            guard let c = peek else { throw ProtoError.malformed }
            at += 1
            switch c {
            case UInt8(ascii: "\""):
                guard let s = String(validating: out, as: UTF8.self) else { throw ProtoError.notUTF8 }
                return s
            case 0..<0x20:
                throw ProtoError.malformed  // JSON escapes every control character
            case UInt8(ascii: "\\"):
                guard let e = peek else { throw ProtoError.malformed }
                at += 1
                switch e {
                case UInt8(ascii: "\""), UInt8(ascii: "\\"), UInt8(ascii: "/"): out.append(e)
                case UInt8(ascii: "b"): out.append(0x08)
                case UInt8(ascii: "f"): out.append(0x0C)
                case UInt8(ascii: "n"): out.append(0x0A)
                case UInt8(ascii: "r"): out.append(0x0D)
                case UInt8(ascii: "t"): out.append(0x09)
                case UInt8(ascii: "u"):
                    var scalar = try hex4()
                    if (0xD800..<0xDC00).contains(scalar) {
                        // A high surrogate must be followed by an escaped low one.
                        guard peek == UInt8(ascii: "\\") else { throw ProtoError.malformed }
                        at += 1
                        try expect(UInt8(ascii: "u"))
                        let low = try hex4()
                        guard (0xDC00..<0xE000).contains(low) else { throw ProtoError.malformed }
                        scalar = 0x10000 + ((scalar - 0xD800) << 10) + (low - 0xDC00)
                    } else if (0xDC00..<0xE000).contains(scalar) {
                        throw ProtoError.malformed
                    }
                    guard let u = Unicode.Scalar(scalar) else { throw ProtoError.malformed }
                    out.append(contentsOf: Array(String(Character(u)).utf8))
                default:
                    throw ProtoError.malformed
                }
            default:
                out.append(c)
            }
        }
    }
}

// MARK: - Reading typed values out of JSON

/// A closed object: every key is one the caller named, and every required key is there.
struct Fields {
    let object: JSONObject
    let what: String

    init(_ value: JSON, _ what: String, required: [String], optional: [String] = []) throws {
        guard case .object(let o) = value else { throw ProtoError.invalid("\(what): not an object") }
        for key in o.keys where !required.contains(key) && !optional.contains(key) {
            throw ProtoError.invalid("\(what): unknown field")
        }
        for key in required where o.values[key] == nil {
            throw ProtoError.invalid("\(what): missing field")
        }
        object = o
        self.what = what
    }

    func has(_ key: String) -> Bool { object.values[key] != nil }

    func int(_ key: String, _ range: ClosedRange<UInt64>) throws -> UInt64 {
        guard case .integer(let n)? = object.values[key], range.contains(n) else {
            throw ProtoError.invalid("\(what).\(key)")
        }
        return n
    }

    func string(_ key: String, _ grammar: (String) -> Bool) throws -> String {
        guard case .string(let s)? = object.values[key], grammar(s) else {
            throw ProtoError.invalid("\(what).\(key)")
        }
        return s
    }

    func optionalString(_ key: String, _ grammar: (String) -> Bool) throws -> String? {
        has(key) ? try string(key, grammar) : nil
    }

    func array(_ key: String) throws -> [JSON] {
        guard case .array(let a)? = object.values[key] else { throw ProtoError.invalid("\(what).\(key)") }
        return a
    }

    func value(_ key: String) -> JSON? { object.values[key] }

    func enumValue<E: RawRepresentable>(_ key: String, _ type: E.Type) throws -> E where E.RawValue == String {
        guard case .string(let s)? = object.values[key], let e = E(rawValue: s) else {
            throw ProtoError.invalid("\(what).\(key)")
        }
        return e
    }
}

/// `{"<name>":<value>}`: exactly one key, the operation's name.
func singleMember(_ value: JSON, _ what: String) throws -> (String, JSON) {
    guard case .object(let o) = value, o.keys.count == 1, let key = o.keys.first, let v = o.values[key] else {
        throw ProtoError.invalid("\(what): not one operation")
    }
    return (key, v)
}

/// `{}` and nothing else.
func requireEmpty(_ value: JSON, _ what: String) throws {
    _ = try Fields(value, what, required: [])
}

// MARK: - Grammars

func isAlnum(_ c: UInt8) -> Bool {
    (c >= 0x30 && c <= 0x39) || (c >= 0x41 && c <= 0x5A) || (c >= 0x61 && c <= 0x7A)
}

func matches(_ s: String, max: Int, first: (UInt8) -> Bool, rest: (UInt8) -> Bool) -> Bool {
    let b = Array(s.utf8)
    guard let head = b.first, b.count <= max, first(head) else { return false }
    return b.dropFirst().allSatisfy(rest)
}

/// A release version as faber stamps it (the supervisor's, the agent's in a manifest): 1 to 64 bytes of `[0-9A-Za-z.+-]`.
func isSupervisorVersion(_ s: String) -> Bool {
    let ok = { (c: UInt8) in isAlnum(c) || c == 0x2E || c == 0x2B || c == 0x2D }
    return matches(s, max: 64, first: ok, rest: ok)
}

/// 1 to 8 192 bytes of the base64 alphabet.
func isJitConfig(_ s: String) -> Bool {
    let ok = { (c: UInt8) in isAlnum(c) || c == 0x2B || c == 0x2F || c == 0x3D }
    return matches(s, max: 8192, first: ok, rest: ok)
}

/// A GitHub JIT runner configuration: a credential. It prints, dumps and reflects as a
/// redaction, so no log line, error or `dump` can carry it. Keep it out of argv, env and files.
struct JitConfig: CustomStringConvertible, CustomDebugStringConvertible, CustomReflectable, Sendable {
    private let secret: String

    init?(_ s: String) {
        guard isJitConfig(s) else { return nil }
        secret = s
    }

    /// The secret itself, for the one place that hands it on.
    func expose() -> String { secret }

    var description: String { "JitConfig(<redacted, \(secret.utf8.count) bytes>)" }
    var debugDescription: String { description }
    var customMirror: Mirror { Mirror(self, children: [:]) }
}

/// Check the framing and return the body: at most `cap` bytes (maxFrame unless a wire says less), one LF at the end and no
/// other LF or CR, UTF-8, and no whitespace at either edge.
func frameBody(_ line: [UInt8], cap: Int = maxFrame) throws -> [UInt8] {
    guard line.count <= cap else { throw ProtoError.frameTooLarge }
    guard line.last == 0x0A else { throw ProtoError.notOneLine }
    let body = Array(line.dropLast())
    guard !body.contains(0x0A), !body.contains(0x0D) else { throw ProtoError.notOneLine }
    guard String(validating: body, as: UTF8.self) != nil else { throw ProtoError.notUTF8 }
    let edge = { (c: UInt8?) in c.map { $0 == 0x20 || $0 == 0x09 || $0 == 0x0C } ?? false }
    guard !edge(body.first), !edge(body.last) else { throw ProtoError.invalid("whitespace around frame") }
    return body
}

/// Compact JSON, fields in the document's order, no whitespace, no null.
struct JSONWriter {
    private(set) var bytes: [UInt8] = []

    mutating func raw(_ s: String) { bytes.append(contentsOf: Array(s.utf8)) }

    mutating func string(_ s: String) {
        bytes.append(0x22)
        for c in s.utf8 {
            switch c {
            case 0x22: raw("\\\"")
            case 0x5C: raw("\\\\")
            case 0x0A: raw("\\n")
            case 0x0D: raw("\\r")
            case 0x09: raw("\\t")
            case 0x08: raw("\\b")
            case 0x0C: raw("\\f")
            case 0..<0x20: raw(String(format: "\\u%04x", c))
            default: bytes.append(c)
            }
        }
        bytes.append(0x22)
    }

    mutating func key(_ k: String) {
        string(k)
        bytes.append(0x3A)
    }

    /// `"key":"value"` with a comma before it unless it opens an object.
    mutating func field(_ k: String, _ v: String, first: Bool = false) {
        if !first { bytes.append(0x2C) }
        key(k)
        string(v)
    }

    mutating func field(_ k: String, _ v: UInt64, first: Bool = false) {
        if !first { bytes.append(0x2C) }
        key(k)
        raw(String(v))
    }

    /// The frame and its LF, refused past `cap` (maxFrame unless a wire says less).
    func terminated(cap: Int = maxFrame) throws -> [UInt8] {
        let out = bytes + [0x0A]
        guard out.count <= cap else { throw ProtoError.frameTooLarge }
        return out
    }
}
