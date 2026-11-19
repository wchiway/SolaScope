use solascope_core::Chunk;
use solascope_parser::{parse_file, ParserOptions};

fn parsed(path: &str, source: &str, options: &ParserOptions) -> Vec<Chunk> {
    let chunks = parse_file("project", path, source, options).unwrap();
    assert!(chunks.iter().all(|chunk| !chunk.fallback));
    assert_coverage(source, &chunks, options.max_chunk_tokens * 4);
    chunks
}

fn assert_coverage(source: &str, chunks: &[Chunk], budget: usize) {
    for chunk in chunks {
        assert!(source.is_char_boundary(chunk.start_byte));
        assert!(source.is_char_boundary(chunk.end_byte));
        assert_eq!(chunk.content, source[chunk.start_byte..chunk.end_byte]);
        assert!(chunk.content.len() <= budget);
        assert_eq!(
            chunk.start_line,
            source.as_bytes()[..chunk.start_byte]
                .iter()
                .filter(|byte| **byte == b'\n')
                .count()
                + 1
        );
        assert_eq!(
            chunk.end_line,
            source.as_bytes()[..chunk.end_byte - 1]
                .iter()
                .filter(|byte| **byte == b'\n')
                .count()
                + 1
        );
    }
    for (byte, character) in source
        .char_indices()
        .filter(|(_, character)| character.is_alphanumeric())
    {
        assert!(
            chunks
                .iter()
                .any(|chunk| chunk.start_byte <= byte
                    && chunk.end_byte >= byte + character.len_utf8()),
            "uncovered {character:?} at byte {byte}: {:?}",
            &source[byte..]
        );
    }
}

fn named<'a>(chunks: &'a [Chunk], symbol: &str) -> &'a Chunk {
    chunks
        .iter()
        .find(|chunk| chunk.symbol.as_deref() == Some(symbol))
        .unwrap_or_else(|| panic!("missing {symbol}"))
}

#[test]
fn leading_comments_wrappers_and_container_members_are_covered_in_all_languages() {
    let cases = [
        (
            "model.rs",
            "// free_doc\n\n/// type_doc\n#[derive(Debug)]\npub struct Model {\n    /// field_doc\n    value: i32,\n}\nimpl Model {\n    /// const_doc\n    pub const LIMIT: usize = 4;\n    /// method_doc\n    #[inline]\n    pub fn read(&self) -> i32 { self.value }\n}\n/// function_doc\n#[inline]\npub fn build() {}\n",
            "Model", "Model::read", "build",
        ),
        (
            "model.py",
            "# free_doc\n\n# type_doc\n@decorate\nclass Model:\n    # field_doc\n    value = 1\n    # method_doc\n    @cached\n    def read(self):\n        return self.value\n# function_doc\n@cached\ndef build():\n    pass\n",
            "Model", "Model.read", "build",
        ),
        (
            "model.js",
            "// free_doc\n\n/** type_doc */\nexport class Model {\n  /** field_doc */\n  value = 1;\n  /** method_doc */\n  read() { return this.value; }\n}\n/** function_doc */\nexport const build = () => 1;\n",
            "Model", "Model.read", "build",
        ),
        (
            "model.ts",
            "// free_doc\n\n/** type_doc */\nexport class Model {\n  /** field_doc */\n  private readonly value: number = 1;\n  /** method_doc */\n  read(): number { return this.value; }\n}\n/** function_doc */\nexport const build = (): number => 1;\n",
            "Model", "Model.read", "build",
        ),
        (
            "model.tsx",
            "// free_doc\n\n/** type_doc */\nexport interface Model {\n  /** field_doc */\n  value: string;\n  /** method_doc */\n  read(): string;\n}\n/** function_doc */\nexport const build = () => <div>hello</div>;\n",
            "Model", "Model.read", "build",
        ),
        (
            "model.go",
            "package model\n// free_doc\n\n// type_doc\ntype Model struct {\n    // field_doc\n    Value int\n}\n// method_doc\nfunc (model *Model) Read() int { return model.Value }\n// function_doc\nfunc Build() {}\n",
            "Model", "Model.Read", "Build",
        ),
    ];
    for (path, source, typ, method, function) in cases {
        let chunks = parsed(path, source, &ParserOptions::default());
        assert!(named(&chunks, typ).content.contains("type_doc"), "{path}");
        assert!(
            named(&chunks, method).content.contains("method_doc"),
            "{path}"
        );
        assert!(
            named(&chunks, function).content.contains("function_doc"),
            "{path}"
        );
        assert!(
            chunks
                .iter()
                .any(|chunk| chunk.symbol.as_deref() == Some(typ)
                    && chunk.content.contains("field_doc")),
            "{path}"
        );
        assert!(
            chunks
                .iter()
                .any(|chunk| chunk.kind == "text" && chunk.content.contains("free_doc")),
            "{path}"
        );
        assert!(!named(&chunks, typ).content.contains("free_doc"), "{path}");
        assert!(
            !chunks
                .iter()
                .any(|chunk| chunk.kind == "text" && chunk.content.contains("function_doc")),
            "{path}"
        );
        if path.ends_with(".rs") {
            assert!(chunks
                .iter()
                .any(|chunk| chunk.symbol.as_deref() == Some(typ)
                    && chunk.kind == "impl"
                    && chunk.content.contains("const_doc")));
            assert!(named(&chunks, function).content.contains("#[inline]"));
        }
        if path.ends_with(".py") {
            assert!(named(&chunks, typ).content.contains("@decorate"));
            assert!(named(&chunks, method).content.contains("@cached"));
        }
        if path.ends_with(".js") || path.ends_with(".ts") || path.ends_with(".tsx") {
            assert!(named(&chunks, function)
                .content
                .contains("export const build"));
        }
        if path.ends_with(".go") {
            assert!(named(&chunks, typ).content.contains("type Model"));
        }
    }
}

#[test]
fn syntax_boundaries_prefer_outer_statements_over_later_nested_ones() {
    let source = "fn sample() {\n    let a = 123456789;\n    let b = 123456789;\n    if a > b {\n        let inner_a = 111;\n        let inner_b = 222;\n        let inner_c = 333;\n    }\n    let tail = 999;\n}\n";
    let options = ParserOptions {
        max_chunk_tokens: 32,
        overlap_lines: 4,
        ..Default::default()
    };
    let chunks = parsed("sample.rs", source, &options);
    let parts: Vec<_> = chunks
        .iter()
        .filter(|chunk| chunk.symbol.as_deref() == Some("sample"))
        .collect();
    assert!(parts.len() > 1);
    assert!(parts
        .windows(2)
        .all(|parts| parts[0].end_byte <= parts[1].start_byte));
    assert_eq!(parts[1].start_byte, source.find("if a > b").unwrap());
}

#[test]
fn text_fallback_keeps_overlap_and_preserves_unicode_content() {
    let source = (0..60)
        .map(|line| format!("中文 文档 line_{line} data\n"))
        .collect::<String>();
    let options = ParserOptions {
        max_chunk_tokens: 32,
        overlap_lines: 2,
        ..Default::default()
    };
    let chunks = parse_file("project", "notes.txt", &source, &options).unwrap();
    assert!(chunks
        .iter()
        .all(|chunk| chunk.fallback && chunk.kind == "text"));
    assert!(chunks
        .windows(2)
        .any(|parts| parts[1].start_byte < parts[0].end_byte));
    assert_coverage(&source, &chunks, 128);
}
