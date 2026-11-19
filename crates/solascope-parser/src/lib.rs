//! Tree-sitter definition extraction and source-exact, bounded code chunks.
//!
//! This provides syntax-level symbols, not name resolution or a call graph.
//! Any syntax error makes the whole file use the explicitly marked text policy.

use std::cmp::Reverse;
use std::collections::HashMap;
use std::ops::Range;

use solascope_core::{hash_text, validate_relative_path, Chunk, Error, Result};
use tree_sitter::{Language, Node, Parser};

/// Bump whenever the chunks produced for an unchanged input change.
pub const CHUNKER_VERSION: u32 = 2;

#[derive(Debug, Clone)]
pub struct ParserOptions {
    pub enabled_languages: Vec<String>,
    pub fallback: bool,
    pub max_chunk_tokens: usize,
    /// Lines repeated at the start of the next chunk when a `text` chunk, or a
    /// definition that offers no syntax boundary, is cut at a line boundary.
    /// Chunks that end at a syntax boundary do not overlap.
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
/// A definition's chunk starts at the comments, attributes and decorators that
/// directly precede it and covers its whole declaration. A container (`impl`,
/// `trait`, `class`, `interface`, `module`) becomes a header chunk, its member
/// definitions, and further parts of the container for everything else in its
/// body, so every alphanumeric character of a parsed file is in some chunk.
///
/// `max_chunk_tokens * 4` is a byte heuristic, not a tokenizer guarantee. Zero
/// tokens is invalid. A definition over that budget is cut where a syntax node
/// begins a line: of those in the second half of the budget the outermost wins,
/// then the last. Such parts do not overlap. Where no node qualifies the cut
/// falls on a line boundary, and inside a single over-long line on a UTF-8
/// boundary. `overlap_lines` applies to line-boundary cuts and to `text` chunks
/// only, and is reduced as needed to preserve forward progress.
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
    let root = tree.root_node();
    if root.has_error() {
        return output.fallback("Tree-sitter reported damaged syntax");
    }
    output.root = Some(root);
    let mut covered = Vec::new();
    let top = [root];
    let place = Place {
        siblings: &top,
        index: 0,
        parent: None,
    };
    walk(place, source, language, "", "", &mut output, &mut covered);
    // Include imports, comments, constants and executable top-level code without
    // duplicating the full bodies of classes, impl blocks or functions.
    for gap in uncovered(0..source.len(), covered) {
        output.emit_text(gap, false);
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

/// A node together with its siblings and ancestors. Tree-sitter finds a parent
/// or a previous sibling by searching down from the root, so the walk carries
/// this context along instead of asking the tree again for every definition.
#[derive(Clone, Copy)]
struct Place<'a, 'tree> {
    /// Every child of the parent, anonymous tokens and comments included.
    siblings: &'a [Node<'tree>],
    index: usize,
    parent: Option<&'a Place<'a, 'tree>>,
}

impl<'tree> Place<'_, 'tree> {
    fn node(&self) -> Node<'tree> {
        self.siblings[self.index]
    }

    /// The node that ends right before this one starts: the previous sibling,
    /// or that of the nearest ancestor that does not start at the same byte.
    fn previous(self) -> Option<Self> {
        let mut place = self;
        while place.index == 0 {
            let parent = *place.parent?;
            if parent.node().start_byte() != place.node().start_byte() {
                return None;
            }
            place = parent;
        }
        Some(Self {
            index: place.index - 1,
            ..place
        })
    }
}

/// A definition whose children are being walked.
struct Open {
    symbol: String,
    kind: &'static str,
    container: Option<Container>,
}

/// What a container's parts share once its header chunk is out.
struct Container {
    body: Range<usize>,
    /// The closing brace of the body, which belongs to no part.
    close: Option<Range<usize>>,
    signature: String,
    parent: Option<String>,
}

fn walk(
    place: Place<'_, '_>,
    source: &str,
    language: &str,
    scope: &str,
    parent_kind: &str,
    output: &mut Output<'_>,
    covered: &mut Vec<Range<usize>>,
) {
    let open = open(place, source, language, scope, parent_kind, output, covered);
    // What a definition contains is scoped by it. A container collects its
    // members; any other definition's chunk already holds all of its children.
    let mut members = Vec::new();
    let (scope, parent_kind, covered) = match &open {
        Some(open) => (open.symbol.as_str(), open.kind, &mut members),
        None => (scope, parent_kind, covered),
    };
    let children = children(place.node());
    for index in 0..children.len() {
        if children[index].is_named() {
            let child = Place {
                siblings: &children,
                index,
                parent: Some(&place),
            };
            // An assigned function/class's value is anonymous; its children
            // inherit the binding's symbol rather than creating a second entity.
            walk(child, source, language, scope, parent_kind, output, covered);
        }
    }
    let Some(Open {
        symbol,
        kind,
        container: Some(container),
    }) = open
    else {
        return;
    };
    // Fields, constants, member comments and whatever else is no definition
    // of its own stay searchable as further parts of the container.
    members.extend(container.close);
    for part in uncovered(container.body, members) {
        if source[part.clone()].chars().any(char::is_alphanumeric) {
            output.emit(
                part,
                Some(&symbol),
                kind,
                Some(&container.signature),
                container.parent.as_deref(),
                false,
            );
        }
    }
}

/// Emit the chunk of the definition at `place`, or the header chunk if it is a
/// container, and claim the definition's range in `covered`.
fn open(
    place: Place<'_, '_>,
    source: &str,
    language: &str,
    scope: &str,
    parent_kind: &str,
    output: &mut Output<'_>,
    covered: &mut Vec<Range<usize>>,
) -> Option<Open> {
    let def = definition(place.node(), source, language, parent_kind)?;
    let parent = def.parent_override.as_deref().unwrap_or(scope);
    let symbol = qualified(parent, &def.name, language);
    let outer = declaration(place, language);
    let range = leading_start(outer, source, language)..outer.node().end_byte();
    // The separator behind a member or declarator is not worth a part of its own.
    let separator = outer
        .siblings
        .get(outer.index + 1)
        .filter(|next| !next.is_named() && matches!(next.kind(), ";" | ","));
    covered.push(range.start..separator.map_or(range.end, |token| token.end_byte()));
    let header_end = def.body.map(|body| header_end(def.node, body, language));
    let signature = normalize_code(
        source[def.node.start_byte()..header_end.unwrap_or(def.node.end_byte())].trim(),
    );
    let parent = nonempty(parent);
    if !def.container {
        output.emit(
            range,
            Some(&symbol),
            def.kind,
            Some(&signature),
            parent,
            false,
        );
        return Some(Open {
            symbol,
            kind: def.kind,
            container: None,
        });
    }
    // A real source slice containing the declaration header, not a
    // synthesized class body or a duplicate of all its methods.
    let mut header = range.clone();
    if let Some(end) = header_end {
        header.end = end + usize::from(source.as_bytes().get(end) == Some(&b'{'));
    }
    output.emit(
        header.clone(),
        Some(&symbol),
        def.kind,
        Some(&signature),
        parent,
        false,
    );
    let close = def
        .body
        .and_then(|body| body.child(body.child_count().checked_sub(1)?))
        .filter(|last| !last.is_named() && last.kind() == "}");
    let container = Container {
        body: header.end..range.end,
        close: close.map(|brace| brace.byte_range()),
        signature,
        parent: parent.map(str::to_owned),
    };
    Some(Open {
        symbol,
        kind: def.kind,
        container: Some(container),
    })
}

fn children(node: Node<'_>) -> Vec<Node<'_>> {
    let mut cursor = node.walk();
    node.children(&mut cursor).collect()
}

/// Where a declaration's header ends and its body begins.
fn header_end(node: Node<'_>, body: Node<'_>, language: &str) -> usize {
    match language {
        "go" if matches!(body.kind(), "struct_type" | "interface_type") => {
            let mut cursor = body.walk();
            let brace = body.children(&mut cursor).find(|child| child.kind() == "{");
            brace.map_or(body.start_byte(), |child| child.start_byte())
        }
        // A Python block has no opening token: it starts at its first
        // statement, behind any comment that follows the colon.
        "python" => {
            let mut cursor = node.walk();
            node.children(&mut cursor)
                .take_while(|child| child.end_byte() <= body.start_byte())
                .filter(|child| !child.is_extra())
                .last()
                .map_or(body.start_byte(), |child| child.end_byte())
        }
        _ => body.start_byte(),
    }
}

/// The outermost node that declares nothing but this definition. The chunk is
/// then a complete declaration (`export const f = () => {};`, `@decorator` plus
/// `def`, `type T struct {}`) and its leading comments are found in front of it.
fn declaration<'a, 'tree>(place: Place<'a, 'tree>, language: &str) -> Place<'a, 'tree> {
    let mut outer = place;
    while let Some(parent) = outer.parent {
        let node = outer.node();
        let wrapper = parent.node();
        let only = |kinds: &[&str]| {
            let declared = outer.siblings.iter();
            declared.filter(|node| kinds.contains(&node.kind())).count() == 1
        };
        let wraps = match (language, wrapper.kind()) {
            ("python", "decorated_definition") => true,
            ("javascript" | "typescript", "lexical_declaration" | "variable_declaration") => {
                only(&["variable_declarator"])
            }
            ("javascript" | "typescript", "export_statement") => ["declaration", "value"]
                .iter()
                .any(|field| wrapper.child_by_field_name(field) == Some(node)),
            ("typescript", "ambient_declaration") => true,
            // A namespace that is not exported parses as an expression statement.
            ("typescript", "expression_statement") => node.kind() == "internal_module",
            ("go", "type_declaration") => only(&["type_spec", "type_alias"]),
            _ => false,
        };
        if !wraps {
            break;
        }
        outer = *parent;
    }
    outer
}

/// Where the comments, attributes and decorators that belong to a declaration
/// begin. They must directly precede it: a blank line ends the run, and so
/// does a comment that merely trails other code on its line.
fn leading_start(outer: Place<'_, '_>, source: &str, language: &str) -> usize {
    let mut first = outer;
    while let Some(previous) = first.previous() {
        let node = previous.node();
        if !attaches(node, language) {
            break;
        }
        let Some(gap) = source.get(text_end(node, source)..first.node().start_byte()) else {
            break;
        };
        let line_breaks = gap.matches('\n').count();
        if !gap.trim().is_empty() || line_breaks > 1 {
            break;
        }
        let comment = matches!(node.kind(), "comment" | "line_comment" | "block_comment");
        if comment && line_breaks == 1 && trails_code(previous, source, language) {
            break;
        }
        first = previous;
    }
    first.node().start_byte()
}

/// Whether a node in front of a declaration can belong to it.
fn attaches(node: Node<'_>, language: &str) -> bool {
    match (language, node.kind()) {
        // `//!` and `/*! */` document the enclosing item, not the next one.
        ("rust", "line_comment" | "block_comment") => node.child_by_field_name("inner").is_none(),
        ("rust", "attribute_item") => true,
        ("javascript" | "typescript", "decorator") => true,
        ("python" | "javascript" | "typescript" | "go", "comment") => true,
        _ => false,
    }
}

/// Whether a comment shares its line with code in front of it, as in
/// `let x = 1; // why`. Attributes and comments before it do not count.
fn trails_code(comment: Place<'_, '_>, source: &str, language: &str) -> bool {
    let mut first = comment;
    while let Some(previous) = first.previous() {
        let node = previous.node();
        let gap = source.get(text_end(node, source)..first.node().start_byte());
        if gap.is_none_or(|gap| gap.contains('\n')) {
            return false;
        }
        if !attaches(node, language) {
            return true;
        }
        first = previous;
    }
    false
}

/// A node's end without the line break some grammars include in it (a Rust
/// doc comment ends behind its newline).
fn text_end(node: Node<'_>, source: &str) -> usize {
    node.start_byte() + text(node, source).trim_end().len()
}

/// The parts of `within` that no range in `covered` claims, in source order.
fn uncovered(within: Range<usize>, mut covered: Vec<Range<usize>>) -> Vec<Range<usize>> {
    covered.sort_by_key(|range| range.start);
    let mut gaps = Vec::new();
    let mut position = within.start;
    for range in covered {
        let end = range.start.min(within.end);
        if position < end {
            gaps.push(position..end);
        }
        position = position.max(range.end);
    }
    if position < within.end {
        gaps.push(position..within.end);
    }
    gaps
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
    /// The root of a clean syntax tree; none while only text can be emitted.
    root: Option<Node<'a>>,
    /// Where a definition may be cut, found when the first one is over budget.
    boundaries: Option<Vec<Boundary>>,
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
            root: None,
            boundaries: None,
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
        // Only definitions are cut at syntax boundaries; `text` has none.
        let mut boundaries: &[Boundary] = &[];
        if symbol.is_some() && range.len() > budget {
            if let Some(root) = self.root {
                boundaries = self.boundaries.get_or_insert_with(|| {
                    line_boundaries(root, self.language, self.source, &self.line_starts)
                });
            }
        }
        let spans = windows(
            self.source,
            range,
            budget,
            self.options.overlap_lines,
            boundaries,
        );
        for span in spans {
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

/// A line that begins with a syntax node.
struct Boundary {
    line_start: usize,
    token_start: usize,
    /// How deeply the node is nested; statements of one block share a depth.
    depth: usize,
}

/// Every line whose first token starts a named syntax node, in source order.
/// Where several nodes start at that token the outermost one counts.
fn line_boundaries(
    root: Node<'_>,
    language: &str,
    source: &str,
    line_starts: &[usize],
) -> Vec<Boundary> {
    let first_token = |row: usize| {
        let start = *line_starts.get(row)?;
        let end = line_starts.get(row + 1).copied().unwrap_or(source.len());
        let line = &source[start..end];
        Some((start, start + line.len() - line.trim_start().len()))
    };
    let mut boundaries: Vec<Boundary> = Vec::new();
    let mut cursor = root.walk();
    let mut depth = 0;
    loop {
        let node = cursor.node();
        let row = node.start_position().row;
        let mut descend = node.end_position().row > row;
        if let Some((line_start, token)) = first_token(row) {
            let seen = |boundaries: &[Boundary]| {
                boundaries
                    .last()
                    .is_some_and(|last| last.line_start == line_start)
            };
            // A statement sequence without an opening token starts at its first
            // statement, which must not look shallower than the ones after it.
            let sequence = matches!(
                (language, node.kind()),
                ("python", "block") | ("go", "statement_list")
            );
            if node.start_byte() == token && !seen(&boundaries) {
                if node.is_named() && !sequence {
                    boundaries.push(Boundary {
                        line_start,
                        token_start: token,
                        depth,
                    });
                } else {
                    descend = true;
                }
            }
        }
        // A node within one line holds nothing that begins another line.
        if descend && cursor.goto_first_child() {
            depth += 1;
            continue;
        }
        while !cursor.goto_next_sibling() {
            if !cursor.goto_parent() {
                return boundaries;
            }
            depth -= 1;
        }
    }
}

fn syntax_boundary(boundaries: &[Boundary], from: usize, to: usize) -> Option<usize> {
    let first = boundaries.partition_point(|boundary| boundary.token_start < from);
    boundaries[first..]
        .iter()
        .take_while(|boundary| boundary.token_start <= to)
        .min_by_key(|boundary| (boundary.depth, Reverse(boundary.token_start)))
        .map(|boundary| boundary.token_start)
}

/// Cut a range into spans of at most `budget` bytes. A span preferably ends at
/// one of `boundaries` in the second half of the budget, without overlap.
/// Otherwise it ends after a whole line (and therefore at a statement boundary
/// in common layouts) and the next span repeats `overlap_lines` of it.
/// A single line exceeding the budget is split only at valid UTF-8 boundaries.
fn windows(
    source: &str,
    range: Range<usize>,
    budget: usize,
    overlap_lines: usize,
    boundaries: &[Boundary],
) -> Vec<Range<usize>> {
    let mut spans = Vec::new();
    let mut start = range.start;
    while start < range.end {
        let mut limit = start.saturating_add(budget).min(range.end);
        while !source.is_char_boundary(limit) {
            limit -= 1;
        }
        if limit < range.end {
            let half = start.saturating_add(budget / 2);
            if let Some(end) = syntax_boundary(boundaries, half, limit) {
                spans.push(start..end);
                start = end;
                continue;
            }
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
