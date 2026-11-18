//! Tree-sitter definition extraction and source-exact, bounded code chunks.
//!
//! This provides syntax-level symbols, not name resolution or a call graph.
//! Any syntax error makes the whole file use the explicitly marked text policy.

use std::collections::HashMap;
use std::ops::Range;

use solascope_core::{hash_text, validate_relative_path, Chunk, Error, Result};
use tree_sitter::{Language, Node, Parser};

#[derive(Debug, Clone)]
pub struct ParserOptions {
    pub enabled_languages: Vec<String>,
    pub fallback: bool,
    pub max_chunk_tokens: usize,
    pub overlap_lines: usize,
}

impl Default for ParserOptions {
    fn default() -> Self {
        Self {
            enabled_languages: ["rust", "python", "javascript", "typescript", "go"]
                .into_iter()
                .map(str::to_owned)
                .collect(),
            fallback: true,
            max_chunk_tokens: 512,
            overlap_lines: 3,
        }
    }
}

/// Parse one UTF-8 file. Byte ends are exclusive and lines are 1-based inclusive.
///
/// `max_chunk_tokens * 4` is a byte heuristic, not a tokenizer guarantee. Zero
/// tokens is invalid. Overlap is reduced as needed to preserve forward progress.
/// Disabled/unknown languages and damaged syntax return `Error::Parse` when
/// fallback is disabled; with fallback enabled their chunks have `fallback=true`.
pub fn parse_file(
    project_id: &str,
    relative_path: &str,
    source: &str,
    options: &ParserOptions,
) -> Result<Vec<Chunk>> {
    validate_relative_path(relative_path)?;
    if options.max_chunk_tokens == 0 {
        return Err(Error::InvalidInput(
            "max_chunk_tokens must be positive".into(),
        ));
    }
    if source.trim().is_empty() {
        return Ok(Vec::new());
    }
    let path = relative_path.replace('\\', "/");
    let language = language_for_path(&path);
    let mut output = Output::new(project_id, &path, source, language, options);
    let enabled = options
        .enabled_languages
        .iter()
        .any(|name| canonical_language(name) == language && language != "text");
    if !enabled {
        return output.fallback("language is unknown or disabled");
    }
    let mut parser = Parser::new();
    parser
        .set_language(&grammar(language, &path))
        .map_err(|error| {
            Error::Parse(format!(
                "cannot load {language} grammar (check Tree-sitter ABI): {error}"
            ))
        })?;
    let Some(tree) = parser.parse(source, None) else {
        return output.fallback("Tree-sitter did not produce a syntax tree");
    };
    if tree.root_node().has_error() {
        return output.fallback("Tree-sitter reported damaged syntax");
    }
    let mut covered = Vec::new();
    walk(
        tree.root_node(),
        source,
        language,
        "",
        "",
        &mut output,
        &mut covered,
    );
    // Include imports, comments, constants and executable top-level code without
    // duplicating the full bodies of classes, impl blocks or functions.
    covered.sort_by_key(|range| range.start);
    let mut position = 0;
    for range in covered {
        if position < range.start {
            output.emit_text(position..range.start, false);
        }
        position = position.max(range.end);
    }
    if position < source.len() {
        output.emit_text(position..source.len(), false);
    }
    output
        .chunks
        .sort_by_key(|chunk| (chunk.start_byte, chunk.end_byte));
    Ok(output.chunks)
}

fn canonical_language(name: &str) -> &str {
    match name {
        "rs" => "rust",
        "py" => "python",
        "js" | "jsx" => "javascript",
        "ts" | "tsx" => "typescript",
        other => other,
    }
}

fn language_for_path(path: &str) -> &'static str {
    match path.rsplit('.').next().unwrap_or("") {
        "rs" => "rust",
        "py" | "pyi" => "python",
        "js" | "jsx" | "mjs" | "cjs" => "javascript",
        "ts" | "tsx" | "mts" | "cts" => "typescript",
        "go" => "go",
        _ => "text",
    }
}

fn grammar(language: &str, path: &str) -> Language {
    match language {
        "rust" => tree_sitter_rust::LANGUAGE.into(),
        "python" => tree_sitter_python::LANGUAGE.into(),
        "javascript" => tree_sitter_javascript::LANGUAGE.into(),
        "typescript" if path.ends_with(".tsx") => tree_sitter_typescript::LANGUAGE_TSX.into(),
        "typescript" => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        "go" => tree_sitter_go::LANGUAGE.into(),
        _ => unreachable!("only enabled, supported languages load grammars"),
    }
}

struct Definition<'tree> {
    name: String,
    kind: &'static str,
    node: Node<'tree>,
    body: Option<Node<'tree>>,
    container: bool,
    parent_override: Option<String>,
}

fn text<'source>(node: Node<'_>, source: &'source str) -> &'source str {
    &source[node.byte_range()]
}

fn named_children(node: Node<'_>) -> Vec<Node<'_>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor).collect()
}

fn definition<'tree>(
    node: Node<'tree>,
    source: &str,
    language: &str,
    parent_kind: &str,
) -> Option<Definition<'tree>> {
    let kind = node.kind();
    let (symbol_kind, container) = match (language, kind) {
        ("rust", "function_item" | "function_signature_item") => (
            if matches!(parent_kind, "impl" | "trait") {
                "method"
            } else {
                "function"
            },
            false,
        ),
        ("rust", "struct_item") => ("struct", false),
        ("rust", "enum_item") => ("enum", false),
        ("rust", "trait_item") => ("trait", true),
        ("rust", "mod_item") => ("module", true),
        ("rust", "type_item") => ("type", false),
        ("rust", "impl_item") => ("impl", true),
        ("python", "function_definition") => (
            if parent_kind == "class" {
                "method"
            } else {
                "function"
            },
            false,
        ),
        ("python", "class_definition") => ("class", true),
        (
            "javascript" | "typescript",
            "function_declaration" | "generator_function_declaration",
        ) => ("function", false),
        (
            "javascript" | "typescript",
            "method_definition" | "method_signature" | "abstract_method_signature",
        ) => ("method", false),
        (
            "javascript" | "typescript",
            "class_declaration" | "abstract_class_declaration" | "class",
        ) => ("class", true),
        ("typescript", "interface_declaration") => ("interface", true),
        ("typescript", "type_alias_declaration") => ("type", false),
        ("typescript", "enum_declaration") => ("enum", false),
        ("typescript", "internal_module") => ("module", true),
        (
            "javascript" | "typescript",
            "variable_declarator" | "public_field_definition" | "field_definition" | "pair",
        ) => {
            let value = node.child_by_field_name("value")?;
            if !matches!(
                value.kind(),
                "arrow_function" | "function_expression" | "generator_function" | "class"
            ) {
                return None;
            }
            if value.kind() == "class" {
                ("class", true)
            } else if matches!(parent_kind, "class" | "interface") || kind == "pair" {
                ("method", false)
            } else {
                ("function", false)
            }
        }
        ("go", "function_declaration") => ("function", false),
        ("go", "method_declaration") => ("method", false),
        ("go", "type_spec" | "type_alias") => {
            let typ = node.child_by_field_name("type")?;
            match typ.kind() {
                "struct_type" => ("struct", false),
                "interface_type" => ("interface", true),
                _ => ("type", false),
            }
        }
        ("go", "method_elem") => ("method", false),
        _ => return None,
    };
    let name = if language == "rust" && kind == "impl_item" {
        let typ = text(node.child_by_field_name("type")?, source);
        if let Some(trait_node) = node.child_by_field_name("trait") {
            format!("<{typ} as {}>", text(trait_node, source))
        } else {
            typ.to_owned()
        }
    } else {
        text(
            node.child_by_field_name("name")
                .or_else(|| node.child_by_field_name("key"))
                .or_else(|| node.child_by_field_name("property"))?,
            source,
        )
        .to_owned()
    };
    let value = node.child_by_field_name("value");
    let body = node
        .child_by_field_name("body")
        .or_else(|| value.and_then(|value| value.child_by_field_name("body")))
        .or_else(|| {
            if language == "go" {
                node.child_by_field_name("type")
                    .filter(|typ| matches!(typ.kind(), "struct_type" | "interface_type"))
                    .map(|typ| {
                        named_children(typ)
                            .into_iter()
                            .find(|child| child.kind() == "field_declaration_list")
                            .unwrap_or(typ)
                    })
            } else {
                None
            }
        });
    let parent_override = if language == "go" && kind == "method_declaration" {
        let receiver = node.child_by_field_name("receiver")?;
        let parameter = named_children(receiver)
            .into_iter()
            .find(|child| child.kind() == "parameter_declaration")?;
        let typ = text(parameter.child_by_field_name("type")?, source);
        Some(typ.trim().trim_start_matches('*').trim().to_owned())
    } else {
        None
    };
    Some(Definition {
        name,
        kind: symbol_kind,
        node,
        body,
        container,
        parent_override,
    })
}

fn qualified(parent: &str, name: &str, language: &str) -> String {
    if parent.is_empty() {
        name.to_owned()
    } else {
        format!(
            "{parent}{}{name}",
            if language == "rust" { "::" } else { "." }
        )
    }
}

fn walk(
    node: Node<'_>,
    source: &str,
    language: &str,
    scope: &str,
    parent_kind: &str,
    output: &mut Output<'_>,
    covered: &mut Vec<Range<usize>>,
) {
    if let Some(def) = definition(node, source, language, parent_kind) {
        let parent = def.parent_override.as_deref().unwrap_or(scope);
        let symbol = qualified(parent, &def.name, language);
        let mut range = def.node.byte_range();
        if language == "python" {
            if let Some(wrapper) = node
                .parent()
                .filter(|parent| parent.kind() == "decorated_definition")
            {
                range.start = wrapper.start_byte();
            }
        }
        if scope.is_empty() {
            covered.push(range.clone());
        }
        let header_end = def
            .body
            .map(|body| {
                if language == "go" && matches!(body.kind(), "struct_type" | "interface_type") {
                    let mut cursor = body.walk();
                    let brace = body.children(&mut cursor).find(|child| child.kind() == "{");
                    brace
                        .map(|child| child.start_byte())
                        .unwrap_or(body.start_byte())
                } else {
                    body.start_byte()
                }
            })
            .unwrap_or(range.end);
        let signature = normalize_code(source[def.node.start_byte()..header_end].trim());
        if def.container {
            // A real source slice containing the declaration header, not a
            // synthesized class body or a duplicate of all its methods.
            range.end = header_end;
            if source.as_bytes().get(header_end) == Some(&b'{') {
                range.end += 1;
            }
        }
        output.emit(
            range,
            Some(&symbol),
            def.kind,
            Some(&signature),
            nonempty(parent),
            false,
        );
        for child in named_children(node) {
            // An assigned function/class's value is anonymous; its children
            // inherit the binding's symbol rather than creating a second entity.
            walk(child, source, language, &symbol, def.kind, output, covered);
        }
    } else {
        for child in named_children(node) {
            walk(child, source, language, scope, parent_kind, output, covered);
        }
    }
}

fn nonempty(value: &str) -> Option<&str> {
    if value.is_empty() {
        None
    } else {
        Some(value)
    }
}

/// Normalize CRLF to LF; preserve all other whitespace, including whitespace
/// inside multiline literals, so unrelated edits cannot collapse cache inputs.
fn normalize_code(code: &str) -> String {
    code.replace("\r\n", "\n")
}

// Length framing prevents ambiguous identities even if names contain separators.
fn identity(parts: &[&str]) -> String {
    let mut framed = String::new();
    for part in parts {
        framed.push_str(&format!("{}:{part}", part.len()));
    }
    hash_text(&framed)
}

struct Output<'a> {
    project_id: &'a str,
    path: &'a str,
    source: &'a str,
    language: &'a str,
    options: &'a ParserOptions,
    line_starts: Vec<usize>,
    chunks: Vec<Chunk>,
    // Separate declarations may share a qualified symbol (e.g. overloads).
    // All parts of the same entity still get distinct deterministic chunk IDs.
    parts: HashMap<String, usize>,
}

impl<'a> Output<'a> {
    fn new(
        project_id: &'a str,
        path: &'a str,
        source: &'a str,
        language: &'a str,
        options: &'a ParserOptions,
    ) -> Self {
        let mut line_starts = vec![0];
        line_starts.extend(source.match_indices('\n').map(|(index, _)| index + 1));
        Self {
            project_id,
            path,
            source,
            language,
            options,
            line_starts,
            chunks: Vec::new(),
            parts: HashMap::new(),
        }
    }

    fn fallback(mut self, reason: &str) -> Result<Vec<Chunk>> {
        if !self.options.fallback {
            return Err(Error::Parse(format!("{}: {reason}", self.path)));
        }
        self.emit_text(0..self.source.len(), true);
        Ok(self.chunks)
    }

    fn emit_text(&mut self, range: Range<usize>, fallback: bool) {
        self.emit(range, None, "text", None, None, fallback);
    }

    fn line(&self, byte: usize) -> usize {
        self.line_starts.partition_point(|start| *start <= byte)
    }

    fn emit(
        &mut self,
        mut range: Range<usize>,
        symbol: Option<&str>,
        kind: &str,
        signature: Option<&str>,
        parent: Option<&str>,
        fallback: bool,
    ) {
        // Avoid whitespace-only chunks and keep location metadata on real code.
        let slice = &self.source[range.clone()];
        range.start += slice.len() - slice.trim_start().len();
        range.end = range.start + slice.trim().len();
        if range.is_empty() {
            return;
        }
        let entity_id = identity(&[self.project_id, self.path, symbol.unwrap_or("<file>"), kind]);
        let budget = self.options.max_chunk_tokens.saturating_mul(4);
        for span in windows(self.source, range, budget, self.options.overlap_lines) {
            let content = self.source[span.clone()].to_owned();
            let normalized = normalize_code(&content);
            let embedding_text = format!(
                "code-v1\npath: {}\nlanguage: {}\nsymbol: {}\nsignature: {}\ncode:\n{}",
                self.path,
                self.language,
                symbol.unwrap_or(""),
                signature.unwrap_or(""),
                normalized
            );
            let part = self.parts.entry(entity_id.clone()).or_default();
            let id = identity(&[&entity_id, &part.to_string()]);
            *part += 1;
            self.chunks.push(Chunk {
                id,
                entity_id: entity_id.clone(),
                project_id: self.project_id.to_owned(),
                relative_path: self.path.to_owned(),
                language: self.language.to_owned(),
                symbol: symbol.map(str::to_owned),
                kind: kind.to_owned(),
                signature: signature.map(str::to_owned),
                parent_symbol: parent.map(str::to_owned),
                start_line: self.line(span.start),
                end_line: self.line(span.end - 1),
                start_byte: span.start,
                end_byte: span.end,
                content_hash: hash_text(&normalized),
                embedding_hash: hash_text(&embedding_text),
                content,
                embedding_text,
                fallback,
            });
        }
    }
}

/// Prefer whole lines (and therefore statement boundaries in common layouts).
/// A single line exceeding the budget is split only at valid UTF-8 boundaries.
fn windows(
    source: &str,
    range: Range<usize>,
    budget: usize,
    overlap_lines: usize,
) -> Vec<Range<usize>> {
    let mut spans = Vec::new();
    let mut start = range.start;
    while start < range.end {
        let mut limit = start.saturating_add(budget).min(range.end);
        while !source.is_char_boundary(limit) {
            limit -= 1;
        }
        // With a positive token budget (at least four bytes), one UTF-8 scalar
        // always fits. Avoid splitting a short last line unnecessarily.
        let end = if limit == range.end {
            limit
        } else {
            source[start..limit]
                .rfind('\n')
                .map(|index| start + index + 1)
                .unwrap_or(limit)
        };
        spans.push(start..end);
        if end == range.end {
            break;
        }
        let mut next = end;
        if overlap_lines > 0 {
            let prefix = &source[start..end];
            // Newline at the exclusive end belongs to the preceding line.
            let prefix = prefix.strip_suffix('\n').unwrap_or(prefix);
            for (count, (index, _)) in prefix.rmatch_indices('\n').enumerate() {
                if count >= overlap_lines {
                    break;
                }
                let candidate = start + index + 1;
                // Reduce the number of overlap lines to fit the next window.
                if end - candidate >= budget / 2 {
                    break;
                }
                next = candidate;
            }
        }
        start = next;
    }
    spans
}

#[cfg(test)]
mod tests;
