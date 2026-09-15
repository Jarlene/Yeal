//! C++ language configuration.
//!
//! Grammar notes (tree-sitter-cpp 0.23.4):
//! - Function names ride on the declarator chain: a function definition or a
//!   prototype is a `function_definition` / `declaration` whose `declarator`
//!   field holds a `function_declarator` with the callee `identifier` as its
//!   own `declarator`. Inside class bodies the same shape uses
//!   `field_identifier`.
//! - `geometry::norm(...)` calls parse as `qualified_identifier` (kind
//!   `qualified_identifier`) with `scope: (namespace_identifier)` and
//!   `name: (identifier)`; `geometry::Point` in a type position is a
//!   `qualified_identifier` (the grammar aliases `qualified_type_identifier`
//!   to that kind) whose `name` is a `type_identifier`. Type positions are
//!   therefore indexed through the unanchored `type_identifier` capture.

use crate::languages::types::TSLanguageConfig;

pub fn cpp_lang() -> TSLanguageConfig {
    TSLanguageConfig::new(
        vec!["C++".to_owned(), "cpp".to_owned(), "cxx".to_owned()],
        vec![
            "cpp".to_owned(),
            "cc".to_owned(),
            "cxx".to_owned(),
            "hpp".to_owned(),
            "hh".to_owned(),
            "hxx".to_owned(),
        ],
        vec![vec![
            "namespace".to_owned(),
            "class".to_owned(),
            "struct".to_owned(),
            "union".to_owned(),
            "enum".to_owned(),
            "function".to_owned(),
            "method".to_owned(),
            "variable".to_owned(),
            "typedef".to_owned(),
            "macro".to_owned(),
            "field".to_owned(),
            "enumerator".to_owned(),
            "type".to_owned(),
            "call".to_owned(),
            "import".to_owned(),
        ]],
        // C++ definitions/references query
        r#"
        ; ============ DEFINITIONS ============

        ; Namespaces: namespace geometry { ... }
        (namespace_definition
            name: (namespace_identifier) @name.definition.namespace) @definition.namespace

        ; Classes, structs, unions and enums. Requiring a body keeps
        ; `struct Point *p` style uses of an already-declared tag from being
        ; indexed as definitions (only `class X { ... }` bodies define).
        (class_specifier
            name: (type_identifier) @name.definition.class
            body: (_)) @definition.class

        (struct_specifier
            name: (type_identifier) @name.definition.struct
            body: (_)) @definition.struct

        (union_specifier
            name: (type_identifier) @name.definition.union
            body: (_)) @definition.union

        (enum_specifier
            name: (type_identifier) @name.definition.enum
            body: (_)) @definition.enum

        ; Enum constants
        (enumerator
            name: (identifier) @name.definition.enumerator) @definition.enumerator

        ; Free function definitions and prototypes: int main(), double norm(...);
        (function_declarator
            declarator: (identifier) @name.definition.function) @definition.function

        ; Methods defined/declared inside class bodies (declarator chain uses a
        ; field_identifier there, so it cannot collide with the free-function
        ; pattern above).
        (function_declarator
            declarator: (field_identifier) @name.definition.method) @definition.method

        ; Methods defined out of line: int geometry::norm(...) { ... }
        (function_declarator
            declarator: (qualified_identifier
                scope: (namespace_identifier)
                name: (identifier) @name.definition.method)) @definition.method

        ; C-style typedefs: typedef ... Name;
        (type_definition
            declarator: (type_identifier) @name.definition.typedef) @definition.typedef

        ; Modern alias declarations: using Name = ...;
        (alias_declaration
            name: (type_identifier) @name.definition.typedef) @definition.typedef

        ; Alias mapping for `using NewName = OldName;` when the RHS is a plain
        ; type: NewName resolves to OldName's definition.
        (alias_declaration
            name: (type_identifier) @alias.name
            type: (type_descriptor
                type: (type_identifier) @alias.original))

        ; Preprocessor macro definitions: #define X and #define F(...)
        (preproc_def
            name: (identifier) @name.definition.macro) @definition.macro

        (preproc_function_def
            name: (identifier) @name.definition.macro) @definition.macro

        ; Struct/class data members
        (field_declaration
            declarator: (field_identifier) @name.definition.field) @definition.field

        ; File-scope variable definitions only (locals inside function bodies
        ; are skipped to keep the index focused on the file surface).
        (translation_unit
            (declaration
                declarator: (identifier) @name.definition.variable)) @definition.variable

        (translation_unit
            (declaration
                declarator: (init_declarator
                    declarator: (identifier) @name.definition.variable))) @definition.variable

        ; ============ REFERENCES ============

        ; Plain function calls
        (call_expression
            function: (identifier) @name.reference.call) @reference.call

        ; Namespace-qualified calls: geometry::norm(p)
        (call_expression
            function: (qualified_identifier
                scope: (namespace_identifier) @name.reference.namespace
                name: (identifier) @name.reference.call)) @reference.call

        ; Method calls: obj.method()
        (call_expression
            function: (field_expression
                field: (field_identifier) @name.reference.call)) @reference.call

        ; Member access reads: p.x
        (field_expression
            field: (field_identifier) @name.reference.field) @reference.field

        ; Type references (params, returns, locals, template arguments, bases)
        (type_identifier) @name.reference.type

        ; Namespace-qualified type uses: geometry::Point (the type_identifier
        ; name is already covered by the pattern above; capture the scope).
        (qualified_identifier
            scope: (namespace_identifier) @name.reference.namespace
            name: (type_identifier)) @reference.type

        ; using std::cout; / using Foo; - names brought into scope
        (using_declaration
            (qualified_identifier
                name: (identifier) @name.reference.import))

        (using_declaration
            (qualified_identifier
                name: (type_identifier) @name.reference.import))

        (using_declaration
            (identifier) @name.reference.import)
        "#
        .to_owned(),
        || tree_sitter_cpp::LANGUAGE.into(),
    )
}

#[cfg(test)]
mod tests {
    /// geometry.hpp fixture, 10 lines:
    ///  1: #pragma once
    ///  2:
    ///  3: namespace geometry {
    ///  4:     struct Point {
    ///  5:         double x;
    ///  6:         double y;
    ///  7:     };
    ///  8:
    ///  9:     double norm(Point p);
    /// 10: }
    const GEOMETRY_HPP: &str = "#pragma once\n\nnamespace geometry {\n    struct Point {\n        double x;\n        double y;\n    };\n\n    double norm(Point p);\n}\n";

    /// geometry.cpp fixture, 8 lines:
    ///  1: #include "geometry.hpp"
    ///  2: #include <cmath>
    ///  3:
    ///  4: namespace geometry {
    ///  5:     double norm(Point p) {
    ///  6:         return std::sqrt(p.x * p.x + p.y * p.y);
    ///  7:     }
    ///  8: }
    const GEOMETRY_CPP: &str = "#include \"geometry.hpp\"\n#include <cmath>\n\nnamespace geometry {\n    double norm(Point p) {\n        return std::sqrt(p.x * p.x + p.y * p.y);\n    }\n}\n";

    /// main.cpp fixture, 7 lines:
    ///  1: #include "geometry.hpp"
    ///  2:
    ///  3: int main() {
    ///  4:     geometry::Point p{3.0, 4.0};
    ///  5:     double n = geometry::norm(p);
    ///  6:     return 0;
    ///  7: }
    const MAIN_CPP: &str = "#include \"geometry.hpp\"\n\nint main() {\n    geometry::Point p{3.0, 4.0};\n    double n = geometry::norm(p);\n    return 0;\n}\n";

    #[test]
    fn cpp_lang_query_and_registry() {
        let registry = crate::languages::LanguageRegistry::new();

        for ext in ["cpp", "hpp"] {
            let config = registry
                .for_extension(ext)
                .unwrap_or_else(|| panic!("language registry must resolve .{}", ext));
            assert_eq!(config.primary_language_id(), "C++");
            config
                .compile_query()
                .expect("C++ query must compile against the tree-sitter-cpp grammar");
            assert!(
                config.file_extensions().iter().any(|e| e == ext),
                "C++ config must own the .{} extension",
                ext
            );
        }

        let cpp = registry
            .for_id("C++")
            .expect("registry must expose the C++ language id");
        for ext in ["cpp", "cc", "cxx", "hpp", "hh", "hxx"] {
            assert!(
                cpp.file_extensions().iter().any(|e| e == ext),
                "C++ config must own the .{} extension",
                ext
            );
        }
        assert!(
            !cpp.file_extensions().iter().any(|e| e == "h"),
            "C++ must not own the .h extension"
        );
        // .h is owned by the C language.
        let c_lang = registry
            .for_extension("h")
            .expect("registry must resolve .h");
        assert_eq!(c_lang.primary_language_id(), "C");

        // Every kind token emitted by the queries is present in the taxonomy.
        let namespaces = cpp.namespaces().first().expect("one namespace group");
        let query = cpp.compile_query().expect("query must compile");
        for capture in query.capture_names() {
            let kind = if let Some(kind) = capture.strip_prefix("name.definition.") {
                kind
            } else if let Some(kind) = capture.strip_prefix("name.reference.") {
                kind
            } else {
                continue; // anchor / alias captures carry no kind token
            };
            assert!(
                namespaces.iter().any(|k| k == kind),
                "capture @name.definition/reference.{} has no matching token in the namespaces taxonomy",
                kind
            );
        }
    }

    #[test]
    fn cpp_lang_cross_file_navigation() {
        use crate::navigation::Navigator;
        use crate::IndexBuilder;
        use std::fs;

        // Guard the fixtures against accidental edits that would shift the
        // line numbers asserted below.
        assert_eq!(GEOMETRY_HPP.lines().count(), 10, "geometry.hpp must have 10 lines");
        assert_eq!(GEOMETRY_CPP.lines().count(), 8, "geometry.cpp must have 8 lines");
        assert_eq!(MAIN_CPP.lines().count(), 7, "main.cpp must have 7 lines");

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("geometry.hpp"), GEOMETRY_HPP).unwrap();
        fs::write(root.join("geometry.cpp"), GEOMETRY_CPP).unwrap();
        let main_cpp = root.join("main.cpp");
        fs::write(&main_cpp, MAIN_CPP).unwrap();

        let index = IndexBuilder::new().build(root).unwrap();
        let navigator = Navigator::new(index);

        // Point is defined by `struct Point` in geometry.hpp:4.
        let point_defs = navigator.goto_definition_by_name("Point", Some(&main_cpp));
        assert!(
            point_defs
                .locations
                .iter()
                .any(|l| l.path.ends_with("geometry.hpp") && l.line == 4),
            "Point should be defined on geometry.hpp:4, got {:?}",
            point_defs.locations
        );

        // norm is defined in geometry.cpp:5; the geometry.hpp:9 prototype may
        // also count as a declaration.
        let norm_defs = navigator.goto_definition_by_name("norm", Some(&main_cpp));
        assert!(
            norm_defs
                .locations
                .iter()
                .any(|l| l.path.ends_with("geometry.cpp") && l.line == 5),
            "norm should be defined on geometry.cpp:5, got {:?}",
            norm_defs.locations
        );

        // The geometry namespace name is a definition in both the header and
        // the implementation.
        let geometry_defs = navigator.goto_definition_by_name("geometry", Some(&main_cpp));
        assert!(
            geometry_defs
                .locations
                .iter()
                .any(|l| l.path.ends_with("geometry.hpp") && l.line == 3),
            "geometry should be defined on geometry.hpp:3, got {:?}",
            geometry_defs.locations
        );
        assert!(
            geometry_defs
                .locations
                .iter()
                .any(|l| l.path.ends_with("geometry.cpp") && l.line == 4),
            "geometry should be defined on geometry.cpp:4, got {:?}",
            geometry_defs.locations
        );

        // The norm call site in main.cpp:5 must be a reference.
        let norm_refs = navigator.goto_references_by_name("norm", Some(&main_cpp), false);
        assert!(
            norm_refs
                .locations
                .iter()
                .any(|l| l.path.ends_with("main.cpp") && l.line == 5),
            "norm call site on main.cpp:5 must be a reference, got {:?}",
            norm_refs.locations
        );

        // Point must be referenced from the .cpp files that use it as a type:
        // main.cpp:4 (geometry::Point) and geometry.cpp:5 (the norm parameter).
        let point_refs = navigator.goto_references_by_name("Point", Some(&main_cpp), false);
        assert!(
            point_refs
                .locations
                .iter()
                .any(|l| l.path.ends_with("main.cpp") && l.line == 4),
            "Point must be referenced on main.cpp:4, got {:?}",
            point_refs.locations
        );
        assert!(
            point_refs
                .locations
                .iter()
                .any(|l| l.path.ends_with("geometry.cpp") && l.line == 5),
            "Point must be referenced on geometry.cpp:5, got {:?}",
            point_refs.locations
        );
    }
}
