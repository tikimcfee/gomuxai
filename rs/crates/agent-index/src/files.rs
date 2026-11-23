//! What a file is, from its name: its file kind and language, and whether the
//! index may hold it at all (docs/INDEX.md, "What gets indexed").

/// Files larger than this get an entity and no content.
pub const MAX_CONTENT: u64 = 1 << 20;

/// Whether a file must never be indexed, tracked or not: `.env*`, key and
/// certificate files (`*.pem`, `*.key`, `*.p12`, `*.pfx`), and SSH keys
/// (`id_rsa`, `id_ed25519`, ... and their `.pub`). Not every `id_*`: that
/// would drop `id_map.rs`.
pub fn is_secret(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    n.starts_with(".env")
        || ["id_rsa", "id_dsa", "id_ecdsa", "id_ed25519", "id_xmss"]
            .iter()
            .any(|k| n == *k || n.starts_with(&format!("{k}.")) || n.starts_with(&format!("{k}_")))
        || [".pem", ".key", ".p12", ".pfx", ".keystore", ".jks"]
            .iter()
            .any(|e| n.ends_with(e))
}

/// Directories never walked outside git: VCS internals and build output.
pub const SKIP_DIRS: &[&str] = &[
    ".git",
    "target",
    "node_modules",
    ".venv",
    "venv",
    "__pycache__",
    ".pixi",
    ".mypy_cache",
    ".pytest_cache",
    ".tox",
    "dist",
    "build",
    ".gomuxai",
    ".cache",
];

/// Whether bytes look binary: a NUL in the first 8 KB.
pub fn is_binary(head: &[u8]) -> bool {
    head.iter().take(8192).any(|&b| b == 0)
}

fn ext(name: &str) -> String {
    match name.rsplit_once('.') {
        Some((stem, e)) if !stem.is_empty() => e.to_ascii_lowercase(),
        _ => String::new(),
    }
}

/// A file's language, from its extension (or name: `Makefile`).
pub fn language(name: &str) -> &'static str {
    let lower = name.to_ascii_lowercase();
    if lower == "makefile" || lower == "gnumakefile" || lower.ends_with(".mk") {
        return "make";
    }
    if lower == "dockerfile" {
        return "docker";
    }
    match ext(name).as_str() {
        "rs" => "rust",
        "go" => "go",
        "py" | "pyi" => "python",
        "js" | "mjs" | "cjs" | "jsx" => "javascript",
        "ts" | "tsx" | "mts" | "cts" => "typescript",
        "swift" => "swift",
        "sh" | "bash" | "zsh" | "fish" => "shell",
        "c" | "h" => "c",
        "cc" | "cpp" | "cxx" | "hpp" | "hh" => "cpp",
        "m" | "mm" => "objc",
        "java" => "java",
        "kt" | "kts" => "kotlin",
        "rb" => "ruby",
        "lua" => "lua",
        "zig" => "zig",
        "wgsl" => "wgsl",
        "glsl" | "metal" | "hlsl" | "vert" | "frag" => "shader",
        "md" | "markdown" => "markdown",
        "rst" => "rst",
        "toml" => "toml",
        "yaml" | "yml" => "yaml",
        "json" | "jsonl" => "json",
        "html" | "htm" => "html",
        "css" | "scss" => "css",
        "sql" => "sql",
        "nix" => "nix",
        _ => "",
    }
}

/// Whether a path is a test by its name or place: `x_test.rs`, `test_x.py`,
/// `x.test.ts`, `x.spec.js`, `XTests.swift`, or anything under `tests/`.
pub fn is_test(rel: &str) -> bool {
    let name = rel.rsplit('/').next().unwrap_or(rel);
    let stem = name.split('.').next().unwrap_or(name);
    if rel
        .split('/')
        .any(|c| c == "tests" || c == "test" || c == "__tests__" || c == "testdata")
    {
        return true;
    }
    stem.ends_with("_test")
        || stem.starts_with("test_")
        || name.contains(".test.")
        || name.contains(".spec.")
        || (language(name) == "swift" && stem.ends_with("Tests"))
}

/// Directories that hold what tests read, not what they test.
pub const FIXTURE_DIRS: &[&str] = &[
    "testdata",
    "test-data",
    "test_data",
    "fixtures",
    "fixture",
    "__fixtures__",
    "__snapshots__",
    "snapshots",
    "golden",
    "corpus",
];

/// Whether a file is test data: anything under a fixture directory, or a data
/// file (`*.jsonl`, `*.txt`, `*.json`, ...) under a test directory. Such files
/// answer many words (a fixture is named for the feature it records), so
/// search ranks them down and dreams don't summarize them.
pub fn is_fixture(rel: &str) -> bool {
    let parts: Vec<&str> = rel.split('/').collect();
    let dirs = &parts[..parts.len().saturating_sub(1)];
    if dirs
        .iter()
        .any(|d| FIXTURE_DIRS.contains(&d.to_ascii_lowercase().as_str()))
    {
        return true;
    }
    let in_tests = dirs.iter().any(|d| {
        matches!(
            d.to_ascii_lowercase().as_str(),
            "tests" | "test" | "__tests__" | "spec"
        )
    });
    in_tests
        && matches!(
            ext(parts.last().copied().unwrap_or("")).as_str(),
            "jsonl"
                | "json"
                | "txt"
                | "csv"
                | "tsv"
                | "yaml"
                | "yml"
                | "xml"
                | "html"
                | "snap"
                | "golden"
                | "out"
                | "expected"
                | "log"
                | "bin"
                | "dat"
        )
}

/// A file's kind: `source`, `test`, `fixture`, `doc`, `config`, `build`,
/// `data`, `asset`.
pub fn file_kind(rel: &str) -> &'static str {
    let name = rel.rsplit('/').next().unwrap_or(rel);
    let lower = name.to_ascii_lowercase();
    const BUILD: &[&str] = &[
        "makefile",
        "gnumakefile",
        "cargo.toml",
        "cargo.lock",
        "build.rs",
        "go.mod",
        "go.sum",
        "package.json",
        "package-lock.json",
        "bun.lockb",
        "bun.lock",
        "yarn.lock",
        "pnpm-lock.yaml",
        "pyproject.toml",
        "setup.py",
        "setup.cfg",
        "pixi.toml",
        "pixi.lock",
        "uv.lock",
        "cmakelists.txt",
        "dockerfile",
        "justfile",
        "package.swift",
        "build.gradle",
        "flake.nix",
        "tsconfig.json",
    ];
    if BUILD.contains(&lower.as_str()) || lower.ends_with(".mk") {
        return "build";
    }
    let e = ext(name);
    let lang = language(name);
    let code = !lang.is_empty()
        && !matches!(
            lang,
            "markdown" | "rst" | "toml" | "yaml" | "json" | "html" | "css" | "sql" | "nix"
        );
    if is_fixture(rel) {
        return "fixture";
    }
    if code && is_test(rel) {
        return "test";
    }
    match e.as_str() {
        "md" | "markdown" | "rst" | "txt" | "adoc" | "org" => return "doc",
        "toml" | "yaml" | "yml" | "ini" | "conf" | "cfg" | "plist" | "service" | "timer"
        | "editorconfig" | "properties" | "nix" => return "config",
        "json" if !rel.contains("data/") => return "config",
        "json" | "jsonl" | "csv" | "tsv" | "db" | "sqlite" | "parquet" | "arrow" | "npy"
        | "xml" | "lock" => return "data",
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "svg" | "ico" | "icns" | "bmp" | "ttf"
        | "otf" | "woff" | "woff2" | "mp3" | "wav" | "ogg" | "flac" | "mp4" | "mov" | "webm"
        | "pdf" | "wasm" | "glb" | "gltf" | "obj" | "zip" | "gz" | "tar" | "xcf" | "psd" => {
            return "asset";
        }
        _ => {}
    }
    if lower == "license" || lower.starts_with("readme") || lower.starts_with("changelog") {
        return "doc";
    }
    if lower.starts_with('.') {
        return "config";
    }
    if code || e == "html" || e == "css" || e == "sql" {
        return "source";
    }
    "data"
}

/// Whether a directory's name says it is kept out of the way: a `_` or `__`
/// prefix, `archive`, `old`, `attic`, `deprecated`, `legacy`.
pub fn is_out_of_the_way(component: &str) -> bool {
    let l = component.to_ascii_lowercase();
    l.starts_with('_')
        || matches!(
            l.as_str(),
            "archive" | "archives" | "old" | "attic" | "deprecated" | "legacy" | "_archive"
        )
}
