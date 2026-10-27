use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Lang {
    Rust,
    TypeScript,
    Tsx,
    JavaScript,
    Python,
    Go,
    Java,
    CSharp,
    C,
    Cpp,
    Ruby,
    Php,
    Kotlin,
    Swift,
    Bash,
    Lua,
    Scala,
}

impl Lang {
    pub fn from_path(path: &Path) -> Option<Lang> {
        let ext = path.extension()?.to_str()?;
        Some(match ext {
            "rs" => Lang::Rust,
            "ts" | "mts" | "cts" => Lang::TypeScript,
            "tsx" => Lang::Tsx,
            "js" | "jsx" | "mjs" | "cjs" => Lang::JavaScript,
            "py" | "pyi" => Lang::Python,
            "go" => Lang::Go,
            "java" => Lang::Java,
            "cs" => Lang::CSharp,
            "c" | "h" => Lang::C,
            "cc" | "cpp" | "cxx" | "c++" | "hpp" | "hh" | "hxx" => Lang::Cpp,
            "rb" | "rake" | "gemspec" => Lang::Ruby,
            "php" => Lang::Php,
            "kt" | "kts" => Lang::Kotlin,
            "swift" => Lang::Swift,
            "sh" | "bash" => Lang::Bash,
            "lua" => Lang::Lua,
            "scala" | "sc" | "sbt" => Lang::Scala,
            _ => return None,
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            Lang::Rust => "rust",
            Lang::TypeScript => "typescript",
            Lang::Tsx => "tsx",
            Lang::JavaScript => "javascript",
            Lang::Python => "python",
            Lang::Go => "go",
            Lang::Java => "java",
            Lang::CSharp => "csharp",
            Lang::C => "c",
            Lang::Cpp => "cpp",
            Lang::Ruby => "ruby",
            Lang::Php => "php",
            Lang::Kotlin => "kotlin",
            Lang::Swift => "swift",
            Lang::Bash => "bash",
            Lang::Lua => "lua",
            Lang::Scala => "scala",
        }
    }

    pub fn ts_language(self) -> tree_sitter::Language {
        match self {
            Lang::Rust => tree_sitter_rust::LANGUAGE.into(),
            Lang::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            Lang::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
            Lang::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
            Lang::Python => tree_sitter_python::LANGUAGE.into(),
            Lang::Go => tree_sitter_go::LANGUAGE.into(),
            Lang::Java => tree_sitter_java::LANGUAGE.into(),
            Lang::CSharp => tree_sitter_c_sharp::LANGUAGE.into(),
            Lang::C => tree_sitter_c::LANGUAGE.into(),
            Lang::Cpp => tree_sitter_cpp::LANGUAGE.into(),
            Lang::Ruby => tree_sitter_ruby::LANGUAGE.into(),
            Lang::Php => tree_sitter_php::LANGUAGE_PHP.into(),
            Lang::Kotlin => tree_sitter_kotlin_ng::LANGUAGE.into(),
            Lang::Swift => tree_sitter_swift::LANGUAGE.into(),
            Lang::Bash => tree_sitter_bash::LANGUAGE.into(),
            Lang::Lua => tree_sitter_lua::LANGUAGE.into(),
            Lang::Scala => tree_sitter_scala::LANGUAGE.into(),
        }
    }

    pub fn is_js_family(self) -> bool {
        matches!(self, Lang::TypeScript | Lang::Tsx | Lang::JavaScript)
    }

    /// The first five languages: every refactoring tool (change_signature, add_field, move_symbol) knows them.
    /// The others get indexing, context, read_file, callers, rename and structural rewrite.
    pub fn full_refactor(self) -> bool {
        matches!(self, Lang::Rust | Lang::TypeScript | Lang::Tsx | Lang::JavaScript | Lang::Python)
    }

    /// Tier 2: indexing (functions, classes, methods, calls) and structural rewrite only; no rename.
    pub fn basic(self) -> bool {
        matches!(
            self,
            Lang::C | Lang::Cpp | Lang::Ruby | Lang::Php | Lang::Kotlin | Lang::Swift | Lang::Bash | Lang::Lua | Lang::Scala
        )
    }

    /* index into a per-thread parser array */
    pub fn slot(self) -> usize {
        self as usize
    }

    pub const ALL: [Lang; 17] = [
        Lang::Rust, Lang::TypeScript, Lang::Tsx, Lang::JavaScript, Lang::Python, Lang::Go, Lang::Java, Lang::CSharp,
        Lang::C, Lang::Cpp, Lang::Ruby, Lang::Php, Lang::Kotlin, Lang::Swift, Lang::Bash, Lang::Lua, Lang::Scala,
    ];
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_languages_by_extension() {
        assert_eq!(Lang::from_path(Path::new("a/b.rs")), Some(Lang::Rust));
        assert_eq!(Lang::from_path(Path::new("x.tsx")), Some(Lang::Tsx));
        assert_eq!(Lang::from_path(Path::new("x.mjs")), Some(Lang::JavaScript));
        assert_eq!(Lang::from_path(Path::new("x.pyi")), Some(Lang::Python));
        assert_eq!(Lang::from_path(Path::new("x.go")), Some(Lang::Go));
        assert_eq!(Lang::from_path(Path::new("x.java")), Some(Lang::Java));
        assert_eq!(Lang::from_path(Path::new("x.cs")), Some(Lang::CSharp));
        assert_eq!(Lang::from_path(Path::new("x.txt")), None);
        assert_eq!(Lang::from_path(Path::new("Makefile")), None);
    }

    #[test]
    fn every_grammar_loads() {
        crate::parse::init_allocator();
        for l in Lang::ALL {
            let mut p = tree_sitter::Parser::new();
            p.set_language(&l.ts_language()).unwrap();
        }
    }
}
