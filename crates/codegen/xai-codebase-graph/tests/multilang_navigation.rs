//! Cross-language POSITION-based navigation tests (Java/C/C++).
//!
//! These integration tests exercise the Navigator's position-based APIs
//! (`goto_definition(path, row, col)` / `get_symbol_at_position`) over
//! minimal single-file snippets and assert that clicking an indexed symbol
//! NAME resolves to its definition.
//!
//! Position resolution works by finding the smallest named node at the click
//! point and only accepting nodes whose kind is identifier-like (the
//! `is_identifier_like` whitelists in src/navigation.rs and
//! src/index_manager.rs). The two whitelists must stay in sync, and each
//! grammar's name node kinds — `identifier`/`type_identifier`/
//! `field_identifier`/`namespace_identifier` for Java/C/C++ — have to be
//! listed there or the click silently misses (NoSymbolAtPosition).

use std::collections::HashMap;
use std::fs;
use tempfile::tempdir;
use xai_codebase_graph::{IndexBuilder, LanguageRegistry, Navigator};

/// 1-indexed `(row, col)` of the first occurrence of `needle` in `content`.
fn locate(content: &str, needle: &str) -> (usize, usize) {
    for (i, line) in content.lines().enumerate() {
        if let Some(byte_col) = line.find(needle) {
            return (i + 1, byte_col + 1);
        }
    }
    panic!("`{}` not present in fixture", needle);
}

/// Write a single source file into `dir` and build an index for it.
///
/// `dir` must stay alive for as long as the returned `Navigator` is used:
/// `goto_definition`/`get_symbol_at_position` re-read the file from disk.
fn index_single_file(dir: &tempfile::TempDir, name: &str, content: &str) -> (Navigator, std::path::PathBuf) {
    let root = dir.path();
    let file = root.join(name);
    fs::write(&file, content).unwrap();
    let navigator = Navigator::new(IndexBuilder::new().build(root).unwrap());
    (navigator, file)
}

// =========================================================================
// Position-based navigation per language
// =========================================================================

#[test]
fn mlang_java_class_and_method_names_resolve_from_position() {
    // Lines: 1 class decl, 2 method decl, 3..5 method body + closing braces.
    const FOO_JAVA: &str = "public class Greeter {\n    public String greet() {\n        return \"Hello\";\n    }\n}\n";

    let dir = tempdir().unwrap();
    let (navigator, file) = index_single_file(&dir, "Foo.java", FOO_JAVA);

    // Clicking the class name on line 1 must resolve the Greeter definition.
    let (row, col) = locate(FOO_JAVA, "Greeter");
    assert_eq!(row, 1, "Greeter declaration is on line 1");
    let res = navigator
        .goto_definition(&file, row, col)
        .expect("symbol must resolve at the Greeter class name");
    assert_eq!(res.symbol, "Greeter");
    assert!(
        res.locations
            .iter()
            .any(|l| l.path.ends_with("Foo.java") && l.line == 1),
        "Greeter definition must be Foo.java:1, got {:?}",
        res.locations
    );

    // Clicking the method name on line 2 must resolve the greet definition.
    let (row, col) = locate(FOO_JAVA, "greet");
    assert_eq!(row, 2, "greet declaration is on line 2");
    let res = navigator
        .goto_definition(&file, row, col)
        .expect("symbol must resolve at the greet method name");
    assert_eq!(res.symbol, "greet");
    assert!(
        res.locations
            .iter()
            .any(|l| l.path.ends_with("Foo.java") && l.line == 2),
        "greet definition must be Foo.java:2, got {:?}",
        res.locations
    );
}

#[test]
fn mlang_c_function_and_macro_names_resolve_from_position() {
    // Lines: 1 include, 2 blank, 3 #define PI, 4 blank, 5 function def.
    const DEMO_C: &str = "#include <math.h>\n\n#define PI 3.14159\n\ndouble distance(double x) {\n    return x * PI;\n}\n";

    let dir = tempdir().unwrap();
    let (navigator, file) = index_single_file(&dir, "demo.c", DEMO_C);

    // Clicking the function name on line 5 must resolve the distance def.
    let (row, col) = locate(DEMO_C, "distance");
    assert_eq!(row, 5, "distance definition is on line 5");
    let res = navigator
        .goto_definition(&file, row, col)
        .expect("symbol must resolve at the distance function name");
    assert_eq!(res.symbol, "distance");
    assert!(
        res.locations
            .iter()
            .any(|l| l.path.ends_with("demo.c") && l.line == 5),
        "distance definition must be demo.c:5, got {:?}",
        res.locations
    );

    // Clicking the macro name on line 3 must resolve the PI #define.
    let (row, col) = locate(DEMO_C, "PI");
    assert_eq!(row, 3, "#define PI is on line 3");
    let res = navigator
        .goto_definition(&file, row, col)
        .expect("symbol must resolve at the PI macro name");
    assert_eq!(res.symbol, "PI");
    assert!(
        res.locations
            .iter()
            .any(|l| l.path.ends_with("demo.c") && l.line == 3),
        "PI definition must be demo.c:3, got {:?}",
        res.locations
    );
}

#[test]
fn mlang_cpp_namespace_and_class_names_resolve_from_position() {
    // Lines: 1 namespace decl, 2 blank, 3 class decl, 4..7 body, 8 close.
    const DEMO_CPP: &str = "namespace geometry {\n\n    class Point {\n    public:\n        double norm() { return 1.0; }\n    };\n\n}\n";

    let dir = tempdir().unwrap();
    let (navigator, file) = index_single_file(&dir, "demo.cpp", DEMO_CPP);

    // Clicking the namespace name on line 1 must resolve geometry (its name
    // node kind is namespace_identifier, which is_identifier_like must allow).
    let (row, col) = locate(DEMO_CPP, "geometry");
    assert_eq!(row, 1, "namespace declaration is on line 1");
    let res = navigator
        .goto_definition(&file, row, col)
        .expect("symbol must resolve at the geometry namespace name");
    assert_eq!(res.symbol, "geometry");
    assert!(
        res.locations
            .iter()
            .any(|l| l.path.ends_with("demo.cpp") && l.line == 1),
        "geometry definition must be demo.cpp:1, got {:?}",
        res.locations
    );

    // Clicking the class name on line 3 must resolve the Point definition.
    let (row, col) = locate(DEMO_CPP, "Point");
    assert_eq!(row, 3, "Point class declaration is on line 3");
    let res = navigator
        .goto_definition(&file, row, col)
        .expect("symbol must resolve at the Point class name");
    assert_eq!(res.symbol, "Point");
    assert!(
        res.locations
            .iter()
            .any(|l| l.path.ends_with("demo.cpp") && l.line == 3),
        "Point definition must be demo.cpp:3, got {:?}",
        res.locations
    );
}

// =========================================================================
// Registry invariants
// =========================================================================

#[test]
fn mlang_registry_single_owner_per_extension_and_families() {
    let registry = LanguageRegistry::new();

    // Every registered extension must be owned by exactly one config, i.e.
    // folding the per-config extension sets into the registry map must not
    // silently override an earlier registration.
    let mut owner: HashMap<&str, &str> = HashMap::new();
    for config in registry.all_configs() {
        for ext in config.file_extensions() {
            if let Some(prev) = owner.insert(ext, config.primary_language_id()) {
                panic!(
                    "extension `{}` is registered by both `{}` and `{}`",
                    ext,
                    prev,
                    config.primary_language_id()
                );
            }
        }
    }
    assert_eq!(
        owner.len(),
        registry.supported_extensions().len(),
        "registry extension map must hold exactly the union of the per-config extensions"
    );

    // Within each config family, every extension pair is the same language.
    for config in registry.all_configs() {
        let exts: Vec<&str> = config.file_extensions().iter().map(String::as_str).collect();
        for (i, a) in exts.iter().enumerate() {
            for b in &exts[i + 1..] {
                assert!(
                    registry.extensions_same_language(a, b),
                    "`{}` and `{}` both belong to `{}`",
                    a,
                    b,
                    config.primary_language_id()
                );
            }
        }
    }

    // Header extension ownership: .h belongs to C, .hpp belongs to C++.
    assert_eq!(
        registry.for_extension("h").unwrap().primary_language_id(),
        "C",
        ".h must resolve to the C config"
    );
    assert_eq!(
        registry.for_extension("hpp").unwrap().primary_language_id(),
        "C++",
        ".hpp must resolve to the C++ config"
    );
    assert!(
        registry.extensions_same_language("c", "h"),
        ".c and .h must be the same language family"
    );
    assert!(
        registry.extensions_same_language("cpp", "hpp"),
        ".cpp and .hpp must be the same language family"
    );
    assert!(
        !registry.extensions_same_language("h", "hpp"),
        ".h (C) and .hpp (C++) must not be the same language"
    );
    assert!(
        !registry.extensions_same_language("c", "cpp"),
        ".c (C) and .cpp (C++) must not be the same language"
    );
}
