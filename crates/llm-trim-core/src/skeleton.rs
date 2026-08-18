use crate::lang::Language;
use crate::unit::{estimate_tokens, CodeUnit, UnitKind};
use anyhow::{anyhow, Result};
use std::path::Path;
use tree_sitter::{Node, Parser, TreeCursor};

/// Maps a Tree-Sitter node kind string to our UnitKind, per language.
/// Returning None means "not a definition we extract as its own unit"
/// (we still recurse into its children looking for nested definitions).
fn classify(lang: Language, kind: &str) -> Option<UnitKind> {
    use UnitKind::*;
    match lang {
        Language::Rust => Some(match kind {
            "function_item" => Function,
            "struct_item" => Struct,
            "enum_item" => Enum,
            "trait_item" => Trait,
            "impl_item" => Impl,
            "type_item" => TypeAlias,
            "const_item" | "static_item" => Const,
            _ => return None,
        }),
        Language::Python => Some(match kind {
            "function_definition" => Function,
            "class_definition" => Class,
            _ => return None,
        }),
        Language::JavaScript | Language::TypeScript | Language::Tsx => Some(match kind {
            "function_declaration" => Function,
            "class_declaration" => Class,
            "method_definition" => Method,
            "interface_declaration" => Interface,
            "type_alias_declaration" => TypeAlias,
            _ => return None,
        }),
        Language::Go => Some(match kind {
            "function_declaration" => Function,
            "method_declaration" => Method,
            "type_declaration" => TypeAlias,
            _ => return None,
        }),
    }
}

/// Does this node kind carry a body we should elide when skeletonizing?
/// (vs. e.g. a const item, which is short and fine to keep in full).
fn has_elidable_body(kind: UnitKind) -> bool {
    matches!(
        kind,
        UnitKind::Function
            | UnitKind::Method
            | UnitKind::Struct
            | UnitKind::Class
            | UnitKind::Trait
            | UnitKind::Interface
            | UnitKind::Impl
    )
}

fn node_name(node: Node, source: &str) -> String {
    if let Some(name_node) = node.child_by_field_name("name") {
        return source[name_node.byte_range()].to_string();
    }
    // Fallback: first identifier-ish child.
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind().contains("identifier") || child.kind() == "type_identifier" {
            return source[child.byte_range()].to_string();
        }
    }
    "<anonymous>".to_string()
}

/// Collect a contiguous block of line-comment nodes immediately preceding
/// `node` (no blank-line gap), interpreted as its doc comment.
fn preceding_doc_comment(lang: Language, node: Node, source: &str) -> Option<String> {
    let mut lines = Vec::new();
    let mut cur = node.prev_sibling();
    let mut expected_end_row = node.start_position().row; // comment must end on row - 1 initially

    while let Some(n) = cur {
        let is_comment = n.kind().contains("comment");
        if !is_comment {
            break;
        }
        if n.end_position().row + 1 != expected_end_row {
            break; // gap: not contiguous
        }
        let text = source[n.byte_range()].trim_end().to_string();
        let prefix_ok = lang
            .line_comment_prefixes()
            .iter()
            .any(|p| text.trim_start().starts_with(p));
        if !prefix_ok {
            break;
        }
        lines.push(text);
        expected_end_row = n.start_position().row;
        cur = n.prev_sibling();
    }
    if lines.is_empty() {
        return None;
    }
    lines.reverse();
    Some(lines.join("\n"))
}

/// Python-specific: docstring is the first statement in the body block.
fn python_docstring(node: Node, source: &str) -> Option<String> {
    let body = node.child_by_field_name("body")?;
    let mut cursor = body.walk();
    let first_stmt = body.children(&mut cursor).find(|c| c.kind() != "comment")?;
    if first_stmt.kind() == "expression_statement" {
        let mut c2 = first_stmt.walk();
        let string_child = first_stmt.children(&mut c2).next()?;
        if string_child.kind() == "string" {
            return Some(source[string_child.byte_range()].to_string());
        }
    }
    None
}

/// Constructs skeleton representation for a syntax node by extracting
/// declaration signatures and inserting explicit elision markers for body
/// blocks. Operates strictly on complete AST nodes to ensure syntactic
/// clarity and prevent partial statement truncation.
fn build_skeleton(lang: Language, kind: UnitKind, node: Node, source: &str) -> (String, String) {
    let full_text = source[node.byte_range()].to_string();

    if !has_elidable_body(kind) {
        return (full_text.clone(), full_text);
    }

    // Find the body/block child to elide. Field name differs by grammar;
    // try the common candidates in order.
    let body_field = ["body", "block"];
    let body_node = body_field
        .iter()
        .find_map(|f| node.child_by_field_name(f));

    let skeleton = match body_node {
        Some(body) => {
            let sig_end = body.start_byte();
            let sig = source[node.start_byte()..sig_end].trim_end().to_string();
            match lang {
                // Python's body colon is already part of the signature
                // slice (it's the last token before the block), and the
                // grammar is indentation-, not brace-, delimited.
                Language::Python => {
                    // The docstring lives *inside* the body block, so a
                    // plain body-elision would silently swallow it too --
                    // pull it back out and keep it above the elision
                    // marker rather than losing real documentation.
                    match python_docstring(node, source) {
                        Some(doc) => format!("{sig}\n    {doc}\n    ...  # body elided by trim"),
                        None => format!("{sig}\n    ...  # body elided by trim"),
                    }
                }
                _ => {
                    format!("{sig} {{\n    /* ... body elided by trim ... */\n}}")
                }
            }
        }
        None => full_text.clone(),
    };

    (full_text, skeleton)
}

pub fn extract_units(
    file: &Path,
    lang: Language,
    source: &str,
    next_id: &mut usize,
) -> Result<Vec<CodeUnit>> {
    let mut parser = Parser::new();
    parser
        .set_language(lang.ts_language())
        .map_err(|e| anyhow!("grammar load failed: {e}"))?;
    let tree = parser
        .parse(source, None)
        .ok_or_else(|| anyhow!("tree-sitter failed to parse {}", file.display()))?;

    let mut units = Vec::new();
    let mut cursor = tree.walk();
    walk(&mut cursor, lang, source, file, &mut units, next_id);
    Ok(units)
}

fn walk(
    cursor: &mut TreeCursor,
    lang: Language,
    source: &str,
    file: &Path,
    units: &mut Vec<CodeUnit>,
    next_id: &mut usize,
) {
    let node = cursor.node();

    if let Some(kind) = classify(lang, node.kind()) {
        let name = node_name(node, source);
        let doc = if lang == Language::Python {
            python_docstring(node, source)
        } else {
            preceding_doc_comment(lang, node, source)
        };
        let (full_text, skeleton_text) = build_skeleton(lang, kind, node, source);
        let skeleton_with_doc = match &doc {
            Some(d) if lang != Language::Python => format!("{d}\n{skeleton_text}"),
            _ => skeleton_text,
        };

        units.push(CodeUnit {
            id: *next_id,
            file: file.to_path_buf(),
            kind,
            name,
            doc_comment: doc,
            signature: first_line(&full_text),
            est_tokens_full: estimate_tokens(&full_text),
            est_tokens_skeleton: estimate_tokens(&skeleton_with_doc),
            full_text,
            skeleton_text: skeleton_with_doc,
            start_line: node.start_position().row + 1,
            end_line: node.end_position().row + 1,
        });
        *next_id += 1;
    }

    if cursor.goto_first_child() {
        loop {
            walk(cursor, lang, source, file, units, next_id);
            if !cursor.goto_next_sibling() {
                break;
            }
        }
        cursor.goto_parent();
    }
}

fn first_line(s: &str) -> String {
    s.lines().next().unwrap_or("").trim().to_string()
}