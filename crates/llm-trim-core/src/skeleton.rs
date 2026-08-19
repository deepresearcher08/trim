use crate::lang::Language;
use crate::unit::{estimate_tokens, CodeUnit, UnitKind};
use anyhow::{anyhow, Result};
use std::collections::HashSet;
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
        Language::C => Some(match kind {
            "function_definition" => Function,
            "struct_specifier" | "union_specifier" => Struct,
            "enum_specifier" => Enum,
            "type_definition" => TypeAlias,
            _ => return None,
        }),
        Language::Cpp => Some(match kind {
            "function_definition" => Function,
            "class_specifier" => Class,
            "struct_specifier" | "union_specifier" => Struct,
            "enum_specifier" => Enum,
            "type_definition" | "alias_declaration" => TypeAlias,
            "namespace_definition" => Impl,
            _ => return None,
        }),
        Language::Java => Some(match kind {
            "class_declaration" | "record_declaration" => Class,
            "interface_declaration" => Interface,
            "enum_declaration" => Enum,
            "method_declaration" | "constructor_declaration" => Method,
            _ => return None,
        }),
        Language::CSharp => Some(match kind {
            "class_declaration" | "record_declaration" => Class,
            "struct_declaration" => Struct,
            "interface_declaration" => Interface,
            "enum_declaration" => Enum,
            "method_declaration" | "constructor_declaration" => Method,
            "property_declaration" => Const,
            _ => return None,
        }),
        Language::Ruby => Some(match kind {
            "method" | "singleton_method" => Method,
            "class" => Class,
            "module" => Impl,
            _ => return None,
        }),
        Language::Php => Some(match kind {
            "function_definition" => Function,
            "method_declaration" => Method,
            "class_declaration" => Class,
            "interface_declaration" => Interface,
            "trait_declaration" => Trait,
            "enum_declaration" => Enum,
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
    if let Some(decl) = node.child_by_field_name("declarator") {
        if let Some(id) = decl.child_by_field_name("declarator") {
            return source[id.byte_range()].to_string();
        }
        return source[decl.byte_range()].to_string();
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

/// Collect a contiguous block of comment nodes immediately preceding
/// `node` (or its parent `export_statement`), interpreted as its doc comment.
fn preceding_doc_comment(lang: Language, node: Node, source: &str) -> Option<String> {
    let target_node = if let Some(parent) = node.parent() {
        if parent.kind() == "export_statement" {
            parent
        } else {
            node
        }
    } else {
        node
    };

    let mut lines = Vec::new();
    let mut cur = target_node.prev_sibling();
    let mut expected_end_row = target_node.start_position().row;

    while let Some(n) = cur {
        let is_comment = n.kind().contains("comment");
        if !is_comment {
            break;
        }
        if n.end_position().row + 1 != expected_end_row && n.end_position().row != expected_end_row {
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

    let body_field = ["body", "block", "compound_statement"];
    let body_node = body_field
        .iter()
        .find_map(|f| node.child_by_field_name(f));

    let skeleton = match body_node {
        Some(body) => {
            let sig_end = body.start_byte();
            let sig = source[node.start_byte()..sig_end].trim_end().to_string();
            match lang {
                Language::Python => match python_docstring(node, source) {
                    Some(doc) => format!("{sig}\n    {doc}\n    ...  # body elided by trim"),
                    None => format!("{sig}\n    ...  # body elided by trim"),
                },
                Language::Ruby => {
                    format!("{sig}\n  # ... body elided by trim ...\nend")
                }
                Language::Go => {
                    format!("{sig} {{\n    // ... body elided by trim ...\n}}")
                }
                Language::Rust
                | Language::JavaScript
                | Language::TypeScript
                | Language::Tsx
                | Language::C
                | Language::Cpp
                | Language::Java
                | Language::CSharp
                | Language::Php => {
                    format!("{sig} {{\n    /* ... body elided by trim ... */\n}}")
                }
            }
        }
        None => full_text.clone(),
    };

    (full_text, skeleton)
}

/// Constructs the third tier (Compact) representation between full text
/// and bare skeleton. Preserves signature, docstring, and initial statements/lines
/// with an explicit compact elision notice, killing the hard degradation cliff.
fn build_compact(lang: Language, kind: UnitKind, node: Node, source: &str) -> String {
    let full_text = source[node.byte_range()].to_string();

    if !has_elidable_body(kind) {
        return full_text;
    }

    let body_field = ["body", "block", "compound_statement"];
    let body_node = body_field
        .iter()
        .find_map(|f| node.child_by_field_name(f));

    let body = match body_node {
        Some(b) => b,
        None => return full_text,
    };

    let full_lines: Vec<&str> = full_text.lines().collect();
    if full_lines.len() <= 8 {
        return full_text;
    }

    let sig_end = body.start_byte();
    let sig = source[node.start_byte()..sig_end].trim_end().to_string();
    let body_raw = &source[body.byte_range()];
    let body_lines: Vec<&str> = body_raw.lines().collect();

    match lang {
        Language::Python => {
            let doc = python_docstring(node, source);
            let mut prefix_lines = Vec::new();
            if let Some(d) = &doc {
                prefix_lines.push(format!("    {d}"));
            }
            let mut collected = 0;
            let mut in_doc = false;
            for line in &body_lines {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                if let Some(d) = &doc {
                    if line.contains(d.trim()) {
                        continue;
                    }
                    if trimmed.starts_with("\"\"\"") || trimmed.starts_with("'''") {
                        if !in_doc {
                            in_doc = true;
                            if trimmed.len() > 3 && (trimmed.ends_with("\"\"\"") || trimmed.ends_with("'''")) {
                                in_doc = false;
                            }
                            continue;
                        } else {
                            in_doc = false;
                            continue;
                        }
                    }
                    if in_doc {
                        continue;
                    }
                }
                prefix_lines.push(line.to_string());
                collected += 1;
                if collected >= 5 {
                    break;
                }
            }
            if prefix_lines.is_empty() {
                format!("{sig}\n    ...  # body elided by trim")
            } else {
                format!("{}\n{}\n    ...  # remaining body elided by trim", sig, prefix_lines.join("\n"))
            }
        }
        Language::Ruby => {
            let mut inner_lines = Vec::new();
            let mut collected = 0;
            for (idx, line) in body_lines.iter().enumerate() {
                if idx == 0 {
                    continue;
                }
                if idx == body_lines.len() - 1 && line.trim() == "end" {
                    continue;
                }
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                inner_lines.push(line.to_string());
                collected += 1;
                if collected >= 5 {
                    break;
                }
            }
            if inner_lines.is_empty() {
                format!("{sig}\n  # ... body elided by trim ...\nend")
            } else {
                format!("{}\n{}\n  # ... remaining body elided by trim ...\nend", sig, inner_lines.join("\n"))
            }
        }
        Language::Go => {
            let mut inner_lines = Vec::new();
            let mut collected = 0;
            for (idx, line) in body_lines.iter().enumerate() {
                if idx == 0 && line.trim() == "{" {
                    continue;
                }
                if idx == body_lines.len() - 1 && line.trim() == "}" {
                    continue;
                }
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                inner_lines.push(line.to_string());
                collected += 1;
                if collected >= 5 {
                    break;
                }
            }
            if inner_lines.is_empty() {
                format!("{sig} {{\n    // ... body elided by trim ...\n}}")
            } else {
                format!("{sig} {{\n{}\n    // ... remaining body elided by trim ...\n}}", inner_lines.join("\n"))
            }
        }
        Language::Rust
        | Language::JavaScript
        | Language::TypeScript
        | Language::Tsx
        | Language::C
        | Language::Cpp
        | Language::Java
        | Language::CSharp
        | Language::Php => {
            let mut inner_lines = Vec::new();
            let mut collected = 0;
            for (idx, line) in body_lines.iter().enumerate() {
                if idx == 0 && line.trim() == "{" {
                    continue;
                }
                if idx == body_lines.len() - 1 && line.trim() == "}" {
                    continue;
                }
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                inner_lines.push(line.to_string());
                collected += 1;
                if collected >= 5 {
                    break;
                }
            }
            if inner_lines.is_empty() {
                format!("{sig} {{\n    /* ... body elided by trim ... */\n}}")
            } else {
                format!("{sig} {{\n{}\n    /* ... remaining body elided by trim ... */\n}}", inner_lines.join("\n"))
            }
        }
    }
}

/// Extract referenced identifier/type names from inside a node subtree.
fn extract_references(node: Node, source: &str, unit_name: &str) -> Vec<String> {
    let mut refs = HashSet::new();

    fn recurse(node: Node, source: &str, unit_name: &str, refs: &mut HashSet<String>) {
        let kind = node.kind();
        if kind.contains("identifier") || kind == "type_identifier" || kind == "field_identifier" || kind == "property_identifier" {
            let text = &source[node.byte_range()];
            if text.len() >= 2 && text != unit_name && is_not_common_keyword(text) {
                refs.insert(text.to_string());
            }
        }

        let mut c = node.walk();
        for child in node.children(&mut c) {
            recurse(child, source, unit_name, refs);
        }
    }

    recurse(node, source, unit_name, &mut refs);
    let mut list: Vec<String> = refs.into_iter().collect();
    list.sort();
    list
}

fn is_not_common_keyword(s: &str) -> bool {
    !matches!(
        s,
        "let"
            | "fn"
            | "def"
            | "self"
            | "this"
            | "super"
            | "int"
            | "str"
            | "bool"
            | "float"
            | "double"
            | "void"
            | "i8"
            | "i16"
            | "i32"
            | "i64"
            | "u8"
            | "u16"
            | "u32"
            | "u64"
            | "usize"
            | "isize"
            | "f32"
            | "f64"
            | "char"
            | "String"
            | "Vec"
            | "Option"
            | "Result"
            | "Some"
            | "None"
            | "Ok"
            | "Err"
            | "true"
            | "false"
            | "nil"
            | "null"
            | "undefined"
            | "return"
            | "if"
            | "else"
            | "for"
            | "while"
            | "loop"
            | "match"
            | "switch"
            | "case"
            | "break"
            | "continue"
            | "mut"
            | "pub"
            | "public"
            | "private"
            | "protected"
            | "internal"
            | "static"
            | "final"
            | "virtual"
            | "override"
            | "abstract"
            | "var"
            | "const"
            | "function"
            | "class"
            | "struct"
            | "enum"
            | "type"
            | "interface"
            | "import"
            | "export"
            | "from"
            | "as"
            | "package"
            | "namespace"
            | "use"
            | "using"
            | "mod"
            | "crate"
            | "async"
            | "await"
            | "yield"
            | "try"
            | "catch"
            | "finally"
            | "throw"
            | "pass"
            | "end"
            | "include"
    )
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
        let full_node = if let Some(parent) = node.parent() {
            if parent.kind() == "export_statement" {
                parent
            } else {
                node
            }
        } else {
            node
        };

        let name = node_name(node, source);
        let doc = if lang == Language::Python {
            python_docstring(node, source)
        } else {
            preceding_doc_comment(lang, node, source)
        };
        let (full_text, skeleton_text) = build_skeleton(lang, kind, full_node, source);
        let skeleton_with_doc = match &doc {
            Some(d) if lang != Language::Python => format!("{d}\n{skeleton_text}"),
            _ => skeleton_text,
        };

        let compact_text = build_compact(lang, kind, full_node, source);
        let compact_with_doc = match &doc {
            Some(d) if lang != Language::Python => format!("{d}\n{compact_text}"),
            _ => compact_text,
        };

        let references = extract_references(node, source, &name);

        units.push(CodeUnit {
            id: *next_id,
            file: file.to_path_buf(),
            kind,
            name,
            doc_comment: doc,
            signature: first_line(&full_text),
            est_tokens_full: estimate_tokens(&full_text),
            est_tokens_compact: estimate_tokens(&compact_with_doc),
            est_tokens_skeleton: estimate_tokens(&skeleton_with_doc),
            full_text,
            compact_text: compact_with_doc,
            skeleton_text: skeleton_with_doc,
            start_line: full_node.start_position().row + 1,
            end_line: full_node.end_position().row + 1,
            references,
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn test_c_and_cpp_parsing() {
        let mut id = 0;
        let c_src = "int calculate_sum(int a, int b) {\n    return a + b;\n}\n";
        let c_units = extract_units(&PathBuf::from("test.c"), Language::C, c_src, &mut id).unwrap();
        assert_eq!(c_units.len(), 1);
        assert_eq!(c_units[0].name, "calculate_sum");

        let cpp_src = "class DatabaseEngine {\npublic:\n    void connect() {\n        // connect\n    }\n};\n";
        let cpp_units = extract_units(&PathBuf::from("test.cpp"), Language::Cpp, cpp_src, &mut id).unwrap();
        assert!(cpp_units.iter().any(|u| u.name == "DatabaseEngine"));
    }

    #[test]
    fn test_java_and_csharp_parsing() {
        let mut id = 0;
        let java_src = "public class AuthService {\n    public boolean validate(String token) {\n        return true;\n    }\n}\n";
        let java_units = extract_units(&PathBuf::from("AuthService.java"), Language::Java, java_src, &mut id).unwrap();
        assert!(java_units.iter().any(|u| u.name == "AuthService"));

        let cs_src = "public class TokenManager {\n    public void Invalidate() {\n        // invalidate\n    }\n}\n";
        let cs_units = extract_units(&PathBuf::from("TokenManager.cs"), Language::CSharp, cs_src, &mut id).unwrap();
        assert!(cs_units.iter().any(|u| u.name == "TokenManager"));
    }

    #[test]
    fn test_ruby_and_php_parsing() {
        let mut id = 0;
        let rb_src = "class PaymentProcessor\n  def process_transaction(amount)\n    puts amount\n  end\nend\n";
        let rb_units = extract_units(&PathBuf::from("payment.rb"), Language::Ruby, rb_src, &mut id).unwrap();
        assert!(rb_units.iter().any(|u| u.name == "PaymentProcessor"));

        let php_src = "<?php\nclass UserController {\n    public function getUser($id) {\n        return $id;\n    }\n}\n";
        let php_units = extract_units(&PathBuf::from("user.php"), Language::Php, php_src, &mut id).unwrap();
        assert!(php_units.iter().any(|u| u.name == "UserController"));
    }
}