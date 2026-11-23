//! Tests for the items after the review: Python and WGSL seeds, fixture
//! files, and the full-text list.

use crate::extract::extract;

#[test]
fn python_docstrings_after_shebangs_comments_and_boms() {
    for text in [
        "#!/usr/bin/env python3\n# -*- coding: utf-8 -*-\n# Copyright someone\n\n\"\"\"Loads rows.\n\nMore.\"\"\"\nimport os\n",
        "\u{feff}\"\"\"Loads rows.\"\"\"\n",
        "#!/usr/bin/env python3\n\n'''\nLoads rows.\n'''\n",
        "# a comment\n\"Loads rows.\"\nimport os\n",
    ] {
        let x = extract("tool.py", "python", text, true);
        assert_eq!(x.seed.as_deref(), Some("Loads rows."), "{text:?}");
    }
    // Code first: no module docstring.
    let x = extract(
        "tool.py",
        "python",
        "import os\n\"\"\"Not a docstring.\"\"\"\n",
        true,
    );
    assert_eq!(x.seed, None);
}

#[test]
fn wgsl_seeds_and_symbols() {
    let text = "// Lays out glyph quads.\n// One instance per glyph.\n\nstruct Glyph {\n  pos: vec2f,\n}\n\n@group(0) @binding(0) var<uniform> u: Uniforms;\nconst MAX_GLYPHS: u32 = 4096u;\n\n/// The vertex stage.\n@vertex\nfn vs_main(@builtin(vertex_index) i: u32) -> @builtin(position) vec4f {\n  return vec4f();\n}\nfn helper() {}\n";
    let x = extract(
        "glyph.wgsl",
        crate::files::language("glyph.wgsl"),
        text,
        true,
    );
    assert_eq!(
        x.seed.as_deref(),
        Some("Lays out glyph quads. One instance per glyph.")
    );
    let syms: Vec<(&str, &str)> = x
        .symbols
        .iter()
        .map(|s| (s.kind.as_str(), s.name.as_str()))
        .collect();
    assert_eq!(
        syms,
        vec![
            ("struct", "Glyph"),
            ("var", "u"),
            ("var", "MAX_GLYPHS"),
            ("fn", "vs_main"),
            ("fn", "helper")
        ]
    );
    assert_eq!(x.symbols[3].doc, "The vertex stage.");
    assert_eq!(crate::files::file_kind("shaders/glyph.wgsl"), "source");
}
