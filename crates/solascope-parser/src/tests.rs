use super::*;
use std::collections::HashSet;

fn parse(path: &str, source: &str) -> Vec<Chunk> {
    parse_file("project", path, source, &ParserOptions::default()).unwrap()
}

fn symbol<'a>(chunks: &'a [Chunk], expected: &str) -> &'a Chunk {
    chunks
        .iter()
        .find(|chunk| chunk.symbol.as_deref() == Some(expected))
        .unwrap_or_else(|| {
            panic!(
                "missing {expected}; got {:?}",
                chunks.iter().map(|c| &c.symbol).collect::<Vec<_>>()
            )
        })
}

fn assert_locations(source: &str, chunks: &[Chunk], budget: usize) {
    assert!(!chunks.is_empty());
    let mut ids = HashSet::new();
    for chunk in chunks {
        assert!(ids.insert(&chunk.id), "duplicate chunk id");
        assert!(chunk.start_byte < chunk.end_byte);
        assert!(source.is_char_boundary(chunk.start_byte));
        assert!(source.is_char_boundary(chunk.end_byte));
        assert_eq!(chunk.content, source[chunk.start_byte..chunk.end_byte]);
        assert_eq!(
            chunk.start_line,
            source[..chunk.start_byte]
                .bytes()
                .filter(|b| *b == b'\n')
                .count()
                + 1
        );
        assert_eq!(
            chunk.end_line,
            source.as_bytes()[..chunk.end_byte - 1]
                .iter()
                .filter(|b| **b == b'\n')
                .count()
                + 1
        );
        assert!(
            chunk.content.len() <= budget,
            "over budget: {}",
            chunk.content.len()
        );
        assert_eq!(chunk.project_id, "project");
        assert!(!chunk.entity_id.is_empty());
        assert_ne!(chunk.id, chunk.entity_id);
        assert_eq!(
            chunk.content_hash,
            hash_text(&normalize_code(&chunk.content))
        );
        assert_eq!(chunk.embedding_hash, hash_text(&chunk.embedding_text));
        assert!(chunk.embedding_text.starts_with("code-v1\npath: "));
        assert!(!chunk.embedding_text.contains("start_line:"));
        assert!(!chunk.embedding_text.contains("end_line:"));
    }
}

#[test]
fn rust_definitions_impl_trait_and_nested_symbols() {
    let source = "mod api {\n pub struct Worker { pub value: i32 }\n pub enum State { Ready, Done }\n pub trait Run { fn run(&self); }\n impl Worker {\n  pub fn run(&self) { fn helper() {} helper(); }\n }\n type Id = u64;\n}\nfn entry() {}\n";
    let chunks = parse("src/main.rs", source);
    for (name, kind) in [
        ("api", "module"),
        ("api::Worker", "struct"),
        ("api::State", "enum"),
        ("api::Run", "trait"),
        ("api::Run::run", "method"),
        ("api::Worker::run", "method"),
        ("api::Worker::run::helper", "function"),
        ("api::Id", "type"),
        ("entry", "function"),
    ] {
        assert_eq!(symbol(&chunks, name).kind, kind);
    }
    assert_eq!(
        symbol(&chunks, "api::Worker::run").parent_symbol.as_deref(),
        Some("api::Worker")
    );
    assert!(!symbol(&chunks, "api::Run").content.contains("fn run"));
    assert!(chunks.iter().all(|chunk| !chunk.fallback));
    assert_locations(source, &chunks, 2048);
}

#[test]
fn rust_trait_impls_have_distinct_qualified_entities() {
    let source = "struct S; trait A { fn f(&self); } trait B { fn f(&self); } impl A for S { fn f(&self) {} } impl B for S { fn f(&self) {} }";
    let chunks = parse("lib.rs", source);
    let a = symbol(&chunks, "<S as A>::f");
    let b = symbol(&chunks, "<S as B>::f");
    assert_ne!(a.entity_id, b.entity_id);
    assert_eq!(a.parent_symbol.as_deref(), Some("<S as A>"));
    assert_locations(source, &chunks, 2048);
}

#[test]
fn python_async_decorators_classes_and_nested_functions() {
    let source = "# 中文\r\n@decorate\r\nclass Outer:\r\n    @cached\r\n    async def run(self, café):\r\n        def inner():\r\n            return '你好'\r\n        return inner()\r\n    class Nested:\r\n        def method(self):\r\n            pass\r\n\r\nasync def main():\r\n    pass\r\n";
    let chunks = parse("worker.py", source);
    assert!(symbol(&chunks, "Outer")
        .content
        .starts_with("# 中文\r\n@decorate\r\nclass Outer:"));
    assert_eq!(symbol(&chunks, "Outer").start_line, 1);
    let run = symbol(&chunks, "Outer.run");
    assert!(run.content.starts_with("@cached\r\n    async def run"));
    assert_eq!(run.start_line, 4);
    assert_eq!(run.end_line, 8);
    assert_eq!(run.kind, "method");
    assert_eq!(run.parent_symbol.as_deref(), Some("Outer"));
    assert_eq!(symbol(&chunks, "Outer.run.inner").kind, "function");
    assert_eq!(
        symbol(&chunks, "Outer.Nested.method")
            .parent_symbol
            .as_deref(),
        Some("Outer.Nested")
    );
    assert!(symbol(&chunks, "main")
        .signature
        .as_deref()
        .unwrap()
        .starts_with("async def main"));
    assert!(chunks.iter().all(|chunk| !chunk.fallback));
    assert_locations(source, &chunks, 2048);
}

#[test]
fn javascript_functions_arrows_and_class_methods() {
    let source = "export function add(a, b) { return a + b; }\nconst twice = x => x * 2;\nclass Worker {\n async run(x) { return twice(x); }\n task = (x) => x + 1;\n}\nconst named = function local(x) { return x; };\n";
    let chunks = parse("worker.js", source);
    for (name, kind) in [
        ("add", "function"),
        ("twice", "function"),
        ("Worker", "class"),
        ("Worker.run", "method"),
        ("Worker.task", "method"),
        ("named", "function"),
    ] {
        assert_eq!(symbol(&chunks, name).kind, kind);
    }
    assert_eq!(
        symbol(&chunks, "Worker.task").parent_symbol.as_deref(),
        Some("Worker")
    );
    assert!(!symbol(&chunks, "Worker").content.contains("twice"));
    assert!(chunks.iter().all(|chunk| !chunk.fallback));
    assert_locations(source, &chunks, 2048);
}

#[test]
fn typescript_types_interfaces_arrows_namespace_and_tsx() {
    let source = "interface Service { run(x: number): string; }\ntype Id = string | number;\nenum State { Ready, Done }\nnamespace API { export class Worker { run(x: number): number { return x; } task = (x: number) => x; } }\nexport const twice = (x: number): number => x * 2;\n";
    let chunks = parse("worker.ts", source);
    for (name, kind) in [
        ("Service", "interface"),
        ("Service.run", "method"),
        ("Id", "type"),
        ("State", "enum"),
        ("API", "module"),
        ("API.Worker", "class"),
        ("API.Worker.run", "method"),
        ("API.Worker.task", "method"),
        ("twice", "function"),
    ] {
        assert_eq!(symbol(&chunks, name).kind, kind);
    }
    assert!(chunks.iter().all(|chunk| !chunk.fallback));
    assert_locations(source, &chunks, 2048);
    let tsx = "export const View = ({name}: {name: string}) => <div>{name}</div>;";
    let chunks = parse("view.tsx", tsx);
    assert_eq!(symbol(&chunks, "View").language, "typescript");
    assert!(!symbol(&chunks, "View").fallback);
    assert_locations(tsx, &chunks, 2048);
}

#[test]
fn go_struct_interface_alias_function_and_receiver_method() {
    let source = "package worker\n\ntype Worker struct { Value int }\ntype Runner interface { Run() int }\ntype ID = int\nfunc New() *Worker { return &Worker{} }\nfunc (w *Worker) Run() int { return w.Value }\n";
    let chunks = parse("worker.go", source);
    for (name, kind) in [
        ("Worker", "struct"),
        ("Runner", "interface"),
        ("Runner.Run", "method"),
        ("ID", "type"),
        ("New", "function"),
        ("Worker.Run", "method"),
    ] {
        assert_eq!(symbol(&chunks, name).kind, kind);
    }
    assert_eq!(
        symbol(&chunks, "Worker.Run").parent_symbol.as_deref(),
        Some("Worker")
    );
    assert!(chunks.iter().all(|chunk| !chunk.fallback));
    assert_locations(source, &chunks, 2048);
}

#[test]
fn long_functions_are_bounded_and_syntax_aligned_without_losing_code() {
    let mut source = "fn long() {\n".to_owned();
    for n in 0..100 {
        source.push_str(&format!("    let value_{n} = {n};\n"));
    }
    source.push_str("}\n");
    let options = ParserOptions {
        max_chunk_tokens: 32,
        overlap_lines: 2,
        ..ParserOptions::default()
    };
    let chunks = parse_file("project", "long.rs", &source, &options).unwrap();
    let parts: Vec<_> = chunks
        .iter()
        .filter(|c| c.symbol.as_deref() == Some("long"))
        .collect();
    assert!(parts.len() > 2);
    assert!(parts.iter().all(|c| c.entity_id == parts[0].entity_id));
    assert!(parts
        .windows(2)
        .all(|pair| pair[0].end_byte <= pair[1].start_byte));
    for part in parts.iter().skip(1) {
        assert!(source[part.start_byte..].starts_with("let value_"));
    }
    let mut covered_end = parts[0].start_byte;
    for part in &parts {
        if part.start_byte > covered_end {
            assert!(source[covered_end..part.start_byte].trim().is_empty());
        }
        covered_end = covered_end.max(part.end_byte);
    }
    assert_eq!(covered_end, source.trim_end().len());
    assert_locations(&source, &chunks, 128);
    assert_eq!(
        chunks,
        parse_file("project", "long.rs", &source, &options).unwrap()
    );
}

#[test]
fn huge_multibyte_single_line_remains_utf8_and_budget_safe() {
    let source = format!(
        "fn unicode() {{ let message = \"{}\"; }}",
        "你好🙂".repeat(100)
    );
    let options = ParserOptions {
        max_chunk_tokens: 7,
        overlap_lines: 3,
        ..ParserOptions::default()
    };
    let chunks = parse_file("project", "unicode.rs", &source, &options).unwrap();
    assert!(chunks.len() > 10);
    assert!(chunks.iter().all(|c| c.start_line == 1 && c.end_line == 1));
    assert_eq!(
        chunks
            .iter()
            .map(|c| c.content.as_str())
            .collect::<String>(),
        source
    );
    assert_locations(&source, &chunks, 28);
}

#[test]
fn unknown_disabled_and_broken_syntax_use_explicit_fallback() {
    for (path, source) in [
        ("README.md", "# 文档\r\nHello\r\n"),
        ("bad.rs", "fn broken( {"),
        ("bad.ts", "const broken = ( => ;"),
    ] {
        let chunks = parse(path, source);
        assert!(chunks
            .iter()
            .all(|c| c.fallback && c.kind == "text" && c.symbol.is_none()));
        assert_locations(source, &chunks, 2048);
        let options = ParserOptions {
            fallback: false,
            ..ParserOptions::default()
        };
        assert!(matches!(
            parse_file("project", path, source, &options),
            Err(Error::Parse(_))
        ));
    }
    let options = ParserOptions {
        enabled_languages: vec!["python".into()],
        ..ParserOptions::default()
    };
    let chunks = parse_file("project", "valid.rs", "fn valid() {}", &options).unwrap();
    assert!(chunks.iter().all(|c| c.fallback));
}

#[test]
fn comments_and_top_level_code_remain_searchable_without_definitions() {
    for (path, source) in [
        ("empty.rs", "// searchable documentation\n"),
        ("script.py", "# heading\nprint('hello')\n"),
        ("script.js", "// start\nconsole.log('hello');\n"),
    ] {
        let chunks = parse(path, source);
        assert!(chunks.iter().all(|c| !c.fallback && c.kind == "text"));
        assert!(
            chunks
                .iter()
                .map(|c| c.content.as_str())
                .collect::<String>()
                .contains("hello")
                || path == "empty.rs"
        );
        assert_locations(source, &chunks, 2048);
    }
    let source = "// before\nfn one() {}\n// after\nconst VALUE: i32 = 1;";
    let chunks = parse("lib.rs", source);
    assert!(symbol(&chunks, "one")
        .content
        .starts_with("// before\nfn one"));
    assert!(!chunks
        .iter()
        .any(|c| c.kind == "text" && c.content.contains("// before")));
    assert!(chunks
        .iter()
        .any(|c| c.kind == "text" && c.content.contains("const VALUE")));
    assert_locations(source, &chunks, 2048);
}

#[test]
fn line_movement_reuses_input_but_path_changes_do_not() {
    let source = "fn f() { println!(\"你好\"); }";
    let first_chunks = parse("src/lib.rs", source);
    let first = symbol(&first_chunks, "f");
    let moved_chunks = parse("src/lib.rs", &format!("\n\n{source}"));
    let moved = symbol(&moved_chunks, "f");
    assert_eq!(first.entity_id, moved.entity_id);
    assert_eq!(first.id, moved.id);
    assert_eq!(first.content_hash, moved.content_hash);
    assert_eq!(first.embedding_hash, moved.embedding_hash);
    assert_eq!(moved.start_line, first.start_line + 2);
    let renamed_chunks = parse("new/lib.rs", source);
    let renamed = symbol(&renamed_chunks, "f");
    assert_eq!(first.content_hash, renamed.content_hash);
    assert_ne!(first.entity_id, renamed.entity_id);
    assert_ne!(first.embedding_text, renamed.embedding_text);
    assert_ne!(first.embedding_hash, renamed.embedding_hash);
    let edited_chunks = parse("src/lib.rs", "fn f() { println!(\"changed\"); }");
    let edited = symbol(&edited_chunks, "f");
    assert_eq!(first.entity_id, edited.entity_id);
    assert_ne!(first.content_hash, edited.content_hash);
    assert_ne!(first.embedding_hash, edited.embedding_hash);
}

#[test]
fn empty_files_options_and_path_validation() {
    assert!(parse("empty.rs", "").is_empty());
    assert!(parse("empty.rs", " \r\n\t").is_empty());
    let defaults = ParserOptions::default();
    assert_eq!(defaults.enabled_languages.len(), 5);
    assert_eq!(defaults.max_chunk_tokens, 512);
    assert_eq!(defaults.overlap_lines, 3);
    assert!(defaults.fallback);
    let zero = ParserOptions {
        max_chunk_tokens: 0,
        ..defaults.clone()
    };
    assert!(matches!(
        parse_file("project", "x.rs", "fn x() {}", &zero),
        Err(Error::InvalidInput(_))
    ));
    assert!(parse_file("project", "../x.rs", "fn x() {}", &defaults).is_err());
    let aliases = ParserOptions {
        enabled_languages: vec!["rs".into()],
        ..defaults
    };
    assert!(!parse_file("project", "x.rs", "fn x() {}", &aliases).unwrap()[0].fallback);
}

#[test]
fn crlf_content_hash_and_embedding_are_normalized_consistently() {
    let lf = parse("lib.rs", "fn f() {\n    let café = \"好\";\n}\n");
    let crlf_source = "fn f() {\r\n    let café = \"好\";\r\n}\r\n";
    let crlf = parse("lib.rs", crlf_source);
    assert_eq!(
        symbol(&lf, "f").content_hash,
        symbol(&crlf, "f").content_hash
    );
    assert_eq!(
        symbol(&lf, "f").embedding_text,
        symbol(&crlf, "f").embedding_text
    );
    assert_locations(crlf_source, &crlf, 2048);
}
