use std::time::Instant;

use zeron_syntax::{HighlightKind, HighlightRequest, HighlightedDocument, highlight};

fn document(source: &str, path: &str) -> HighlightedDocument {
    highlight(HighlightRequest {
        source,
        path: Some(path),
        fence_tag: None,
    })
    .unwrap()
}

fn fragments(source: &str, path: &str) -> Vec<(String, HighlightKind)> {
    let document = document(source, path);
    source
        .lines()
        .zip(document.lines)
        .flat_map(|(line, spans)| {
            spans
                .into_iter()
                .map(move |span| (line[span.range].to_owned(), span.kind))
        })
        .collect()
}

fn assert_fragments(source: &str, path: &str, expected: &[(&str, HighlightKind)]) {
    let fragments = fragments(source, path);
    for &(text, kind) in expected {
        assert!(
            fragments
                .iter()
                .any(|fragment| fragment == &(text.into(), kind)),
            "missing {text:?} as {kind:?} in {path}: {fragments:#?}"
        );
    }
}

fn assert_valid_document(source: &str, path: &str) {
    let document = document(source, path);
    assert_eq!(document.lines.len(), source.lines().count().max(1));
    for (line, spans) in source.lines().zip(document.lines) {
        assert!(
            spans
                .windows(2)
                .all(|pair| pair[0].range.end <= pair[1].range.start),
            "overlapping spans in {path}: {spans:?}"
        );
        for span in spans {
            assert!(line.is_char_boundary(span.range.start));
            assert!(line.is_char_boundary(span.range.end));
        }
    }
}

#[test]
fn rust_reference_span_snapshot() {
    let source = r#"use std::path::Path;
// quiet comment
struct Widget { field: usize }
fn build(value: usize) -> Widget {
    let label = format!("item-{value}");
    Widget { field: 42 }
}"#;
    assert_fragments(
        source,
        "src/lib.rs",
        &[
            ("use", HighlightKind::Keyword),
            ("Path", HighlightKind::Constructor),
            ("// quiet comment", HighlightKind::Comment),
            ("Widget", HighlightKind::Type),
            ("field", HighlightKind::Property),
            ("usize", HighlightKind::TypeBuiltin),
            ("build", HighlightKind::Function),
            ("value", HighlightKind::Parameter),
            ("format!", HighlightKind::Macro),
            ("\"item-{value}\"", HighlightKind::String),
            ("42", HighlightKind::Number),
        ],
    );
}

#[test]
fn typescript_composes_javascript_and_typescript_roles() {
    let source = r#"import type { Widget } from "./widget";
export function derive<T>(name: string, input: Widget): Promise<T> {
    const kind = input.kind;
    if (kind === "ready") {
        const result = parse<T>(`${name}:${kind}`, 42);
        return result;
    }
    throw new Error("bad");
}"#;
    assert_fragments(
        source,
        "src/derive.ts",
        &[
            ("import", HighlightKind::Keyword),
            ("derive", HighlightKind::Function),
            ("name", HighlightKind::Parameter),
            ("string", HighlightKind::TypeBuiltin),
            ("Widget", HighlightKind::Type),
            ("kind", HighlightKind::Property),
            ("parse", HighlightKind::Function),
            ("`${name}:${kind}`", HighlightKind::String),
            ("42", HighlightKind::Number),
            ("===", HighlightKind::Operator),
            ("(", HighlightKind::Punctuation),
            ("result", HighlightKind::Variable),
        ],
    );
}

#[test]
fn tsx_composes_javascript_jsx_and_typescript_roles() {
    let source = r#"type Props = { id: string; title: string };
export function card(props: Props): JSX.Element {
    return <main id={props.id} data-kind={props.title}>{props.title}</main>;
}"#;
    assert_fragments(
        source,
        "src/card.tsx",
        &[
            ("type", HighlightKind::Keyword),
            ("card", HighlightKind::Function),
            ("props", HighlightKind::Parameter),
            ("Props", HighlightKind::Type),
            ("string", HighlightKind::TypeBuiltin),
            ("main", HighlightKind::Tag),
            ("id", HighlightKind::Attribute),
            ("data-kind", HighlightKind::Attribute),
            ("title", HighlightKind::Property),
        ],
    );
}

#[test]
fn kotlin_project_query_covers_structural_roles() {
    let source = r#"package demo

// Structural Kotlin fixture
@Deprecated("old")
@Marker
open class Parent
data class Greeter(private val prefix: String) : Parent() {
    val version: Int = 1

    suspend fun greet(name: String, enabled: Boolean): String {
        if (enabled && version > 0) println("$prefix, $name")
        return prefix
    }
}

val greeter = Greeter("Hello")
val enabled = true
val missing = null

fun exercise(receiver: Greeter) {
    genericCall<String>()
    lambdaCall { value -> value }
    receiver.genericMethod<String>()
    receiver.lambdaMethod { value -> value }
    Uppercase()
}
"#;
    assert_fragments(
        source,
        "src/Greeter.kt",
        &[
            ("package", HighlightKind::Keyword),
            ("// Structural Kotlin fixture", HighlightKind::Comment),
            ("Deprecated", HighlightKind::Attribute),
            ("Marker", HighlightKind::Attribute),
            ("data", HighlightKind::Keyword),
            ("Greeter", HighlightKind::Type),
            ("Greeter", HighlightKind::Function),
            ("Parent", HighlightKind::Constructor),
            ("prefix", HighlightKind::Property),
            ("String", HighlightKind::TypeBuiltin),
            ("version", HighlightKind::Property),
            ("Int", HighlightKind::TypeBuiltin),
            ("1", HighlightKind::Number),
            ("suspend", HighlightKind::Keyword),
            ("greet", HighlightKind::Function),
            ("name", HighlightKind::Parameter),
            ("Boolean", HighlightKind::TypeBuiltin),
            ("&&", HighlightKind::Operator),
            ("println", HighlightKind::Function),
            ("\"$prefix, $name\"", HighlightKind::String),
            ("true", HighlightKind::Boolean),
            ("null", HighlightKind::Constant),
            ("greeter", HighlightKind::Property),
            ("genericCall", HighlightKind::Function),
            ("lambdaCall", HighlightKind::Function),
            ("genericMethod", HighlightKind::Function),
            ("lambdaMethod", HighlightKind::Function),
            ("Uppercase", HighlightKind::Function),
            ("(", HighlightKind::Punctuation),
        ],
    );
    assert!(
        !fragments(source, "src/Greeter.kt")
            .contains(&("Uppercase".into(), HighlightKind::Constructor)),
        "capitalized function calls must not be guessed to be constructors"
    );
    assert!(
        !fragments(source, "src/Greeter.kt")
            .contains(&("receiver".into(), HighlightKind::Function)),
        "only the terminal navigation member is a function call"
    );
}

#[test]
fn dockerfile_uses_bounded_shell_and_structured_data_injections() {
    let source = r#"FROM alpine
ENV NAME=World
RUN echo "hello $NAME" && printf '%s\n' "$NAME"
RUN <<SCRIPT
touch /tmp/ready
SCRIPT
COPY <<EOF config.json
{"enabled": true}
EOF
COPY <<EOF config.yaml
yaml_enabled: true
EOF
COPY <<EOF config.toml
toml_enabled = true
EOF
COPY <<EOF config.xml
<root />
EOF
"#;
    assert_fragments(
        source,
        "Dockerfile",
        &[
            ("FROM", HighlightKind::Keyword),
            ("NAME", HighlightKind::Property),
            ("echo", HighlightKind::Function),
            ("&&", HighlightKind::Operator),
            ("'%s\\n'", HighlightKind::String),
            ("touch", HighlightKind::Function),
            ("true", HighlightKind::Constant),
            ("yaml_enabled", HighlightKind::Property),
            ("toml_enabled = true", HighlightKind::Property),
            ("<root />", HighlightKind::String),
        ],
    );
    assert!(
        !fragments(source, "Dockerfile")
            .iter()
            .any(|(text, kind)| text == "root" && *kind == HighlightKind::Tag),
        "unsupported XML heredocs must remain parent-highlighted"
    );
    assert_valid_document(source, "Dockerfile");
}

#[test]
fn affected_languages_keep_utf8_boundaries_for_incomplete_code() {
    for (path, source) in [
        (
            "src/broken.ts",
            "export function café(name: string) {\n const label = `héllo ${name\n if (name ===",
        ),
        (
            "src/Broken.kt",
            "fun café(name: String) {\n val label = \"héllo $name\n if (name ==",
        ),
    ] {
        assert_valid_document(source, path);
    }
}

#[test]
fn every_span_is_valid_and_non_overlapping_for_incomplete_unicode() {
    let source = "fn café(value: &str) {\n let text = r#\"héllo\nworld\"#;\n if value {";
    let document = highlight(HighlightRequest {
        source,
        path: Some("broken.rs"),
        fence_tag: None,
    })
    .unwrap();
    for (line, spans) in source.lines().zip(document.lines) {
        assert!(
            spans
                .windows(2)
                .all(|pair| pair[0].range.end <= pair[1].range.start)
        );
        for span in spans {
            assert!(line.is_char_boundary(span.range.start));
            assert!(line.is_char_boundary(span.range.end));
        }
    }
}

#[test]
#[ignore = "diagnostic benchmark; run explicitly when changing parsers or queries"]
fn benchmark_small_medium_large_and_incomplete_documents() {
    for (name, source) in [
        ("small", "fn main() { println!(\"hi\"); }".to_string()),
        ("medium", "fn item() -> usize { 42 }\n".repeat(2_000)),
        ("large", "struct Item { value: usize }\n".repeat(20_000)),
        (
            "incomplete",
            "fn broken( { let value = \"open".repeat(2_000),
        ),
    ] {
        let started = Instant::now();
        let document = highlight(HighlightRequest {
            source: &source,
            path: Some("bench.rs"),
            fence_tag: None,
        })
        .unwrap();
        eprintln!(
            "{name}: bytes={} spans={} elapsed_us={}",
            source.len(),
            document.lines.iter().map(Vec::len).sum::<usize>(),
            started.elapsed().as_micros()
        );
    }
}

#[test]
fn reference_contains_expected_visual_roles() {
    let spans = fragments("fn call() { let value = 42; }", "main.rs");
    for kind in [
        HighlightKind::Keyword,
        HighlightKind::Function,
        HighlightKind::Number,
    ] {
        assert!(spans.iter().any(|(_, actual)| *actual == kind));
    }
}
