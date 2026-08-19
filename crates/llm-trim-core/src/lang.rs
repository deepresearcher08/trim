use std::path::Path;

/// Languages supported by the Tier 1 structural parser. Chosen for maximum
/// coverage of real-world codebases (per GitHub's language usage rankings)
/// while keeping the grammar set manageable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Language {
    Rust,
    Python,
    JavaScript,
    TypeScript,
    Tsx,
    Go,
    C,
    Cpp,
    Java,
    CSharp,
    Ruby,
    Php,
}

impl Language {
    pub fn from_path(path: &Path) -> Option<Self> {
        let ext = path.extension()?.to_str()?.to_ascii_lowercase();
        Some(match ext.as_str() {
            "rs" => Language::Rust,
            "py" | "pyi" => Language::Python,
            "js" | "jsx" | "mjs" | "cjs" => Language::JavaScript,
            "ts" | "mts" | "cts" => Language::TypeScript,
            "tsx" => Language::Tsx,
            "go" => Language::Go,
            "c" | "h" => Language::C,
            "cpp" | "cc" | "cxx" | "hpp" | "hxx" | "hh" | "c++" | "h++" => Language::Cpp,
            "java" => Language::Java,
            "cs" => Language::CSharp,
            "rb" | "rake" | "gemspec" => Language::Ruby,
            "php" | "phtml" | "php3" | "php4" | "php5" | "php7" | "phps" => Language::Php,
            _ => return None,
        })
    }

    pub fn ts_language(&self) -> tree_sitter::Language {
        match self {
            Language::Rust => tree_sitter_rust::language(),
            Language::Python => tree_sitter_python::language(),
            Language::JavaScript => tree_sitter_javascript::language(),
            Language::TypeScript => tree_sitter_typescript::language_typescript(),
            Language::Tsx => tree_sitter_typescript::language_tsx(),
            Language::Go => tree_sitter_go::language(),
            Language::C => tree_sitter_c::language(),
            Language::Cpp => tree_sitter_cpp::language(),
            Language::Java => tree_sitter_java::language(),
            Language::CSharp => tree_sitter_c_sharp::language(),
            Language::Ruby => tree_sitter_ruby::language(),
            Language::Php => tree_sitter_php::language(),
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            Language::Rust => "rust",
            Language::Python => "python",
            Language::JavaScript => "javascript",
            Language::TypeScript => "typescript",
            Language::Tsx => "tsx",
            Language::Go => "go",
            Language::C => "c",
            Language::Cpp => "cpp",
            Language::Java => "java",
            Language::CSharp => "csharp",
            Language::Ruby => "ruby",
            Language::Php => "php",
        }
    }

    /// Line comment prefix, used to walk upward from a definition node and
    /// pull in an attached doc comment / docstring block.
    pub fn line_comment_prefixes(&self) -> &'static [&'static str] {
        match self {
            Language::Rust => &["///", "//!", "//", "/*", "/**"],
            Language::Python => &["#"],
            Language::JavaScript | Language::TypeScript | Language::Tsx => &["//", "/*", "/**", "*"],
            Language::Go => &["//", "/*", "/**"],
            Language::C | Language::Cpp => &["//", "/*", "/**", "*"],
            Language::Java | Language::CSharp => &["//", "/*", "/**", "*", "///"],
            Language::Ruby => &["#"],
            Language::Php => &["//", "/*", "/**", "*", "#"],
        }
    }
}