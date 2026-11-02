//! C language configuration.

use crate::languages::types::TSLanguageConfig;

pub fn c_lang() -> TSLanguageConfig {
    TSLanguageConfig::new(
        vec!["C".to_owned(), "c".to_owned()],
        vec!["c".to_owned(), "h".to_owned()],
        vec![vec![
            "function".to_owned(),
            "variable".to_owned(),
            "struct".to_owned(),
            "union".to_owned(),
            "enum".to_owned(),
            "typedef".to_owned(),
            "macro".to_owned(),
            "field".to_owned(),
            "type".to_owned(),
            "enumerator".to_owned(),
        ]],
        r#"
        ; ============ DEFINITIONS ============

        ; Function definitions and prototypes
        (function_declarator
            declarator: (identifier) @name.definition.function) @definition.function

        ; Struct/union/enum definitions. Requiring a body keeps tag uses such
        ; as `struct Point *p` from being indexed as definitions.
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

        ; Typedef names: typedef ... Name;
        (type_definition
            declarator: (type_identifier) @name.definition.typedef) @definition.typedef

        ; Preprocessor macro definitions: #define X and #define F(...)
        (preproc_def
            name: (identifier) @name.definition.macro) @definition.macro

        (preproc_function_def
            name: (identifier) @name.definition.macro) @definition.macro

        ; File-scope variable definitions only (declarations nested in function
        ; bodies are skipped to keep the index focused on the file surface).
        (translation_unit
            (declaration
                declarator: (identifier) @name.definition.variable)) @definition.variable

        (translation_unit
            (declaration
                declarator: (init_declarator
                    declarator: (identifier) @name.definition.variable))) @definition.variable

        ; Struct/union member fields
        (field_declaration
            declarator: (field_identifier) @name.definition.field) @definition.field

        ; ============ REFERENCES ============

        ; Function calls
        (call_expression
            function: (identifier) @name.reference.function) @reference.function

        ; Method-style calls: obj->method() / obj.method()
        (call_expression
            function: (field_expression
                field: (field_identifier) @name.reference.function)) @reference.function

        ; Member access: a.x
        (field_expression
            field: (field_identifier) @name.reference.field) @reference.field

        ; Type references in parameters and declarations
        (parameter_declaration
            type: (type_identifier) @name.reference.type) @reference.type

        (declaration
            type: (type_identifier) @name.reference.type) @reference.type

        ; Type references inside casts, sizeof, and compound literals
        (cast_expression
            type: (type_descriptor
                type: (type_identifier) @name.reference.type)) @reference.type

        (sizeof_expression
            type: (type_descriptor
                type: (type_identifier) @name.reference.type)) @reference.type

        (compound_literal_expression
            type: (type_descriptor
                type: (type_identifier) @name.reference.type)) @reference.type
        "#
        .to_owned(),
        || tree_sitter_c::LANGUAGE.into(),
    )
}

#[cfg(test)]
mod tests {
    /// mathlib.h fixture, 13 lines:
    ///  1: #ifndef MATHLIB_H
    ///  2: #define MATHLIB_H
    ///  3:
    ///  4: #define PI 3.14159
    ///  5:
    ///  6: typedef struct Point {
    ///  7:     double x;
    ///  8:     double y;
    ///  9: } Point;
    /// 10:
    /// 11: double distance(Point a, Point b);
    /// 12:
    /// 13: #endif
    const MATHLIB_H: &str = "#ifndef MATHLIB_H\n#define MATHLIB_H\n\n#define PI 3.14159\n\ntypedef struct Point {\n    double x;\n    double y;\n} Point;\n\ndouble distance(Point a, Point b);\n\n#endif\n";

    /// mathlib.c fixture, 7 lines:
    ///  1: #include "mathlib.h"
    ///  2:
    ///  3: double distance(Point a, Point b) {
    ///  4:     double dx = a.x - b.x;
    ///  5:     double dy = a.y - b.y;
    ///  6:     return PI * (dx > dy ? dx : dy);
    ///  7: }
    const MATHLIB_C: &str = "#include \"mathlib.h\"\n\ndouble distance(Point a, Point b) {\n    double dx = a.x - b.x;\n    double dy = a.y - b.y;\n    return PI * (dx > dy ? dx : dy);\n}\n";

    /// main.c fixture, 8 lines:
    ///  1: #include "mathlib.h"
    ///  2:
    ///  3: int main(void) {
    ///  4:     Point a = {0.0, 0.0};
    ///  5:     Point b = {3.0, 4.0};
    ///  6:     double d = distance(a, b);
    ///  7:     return 0;
    ///  8: }
    const MAIN_C: &str = "#include \"mathlib.h\"\n\nint main(void) {\n    Point a = {0.0, 0.0};\n    Point b = {3.0, 4.0};\n    double d = distance(a, b);\n    return 0;\n}\n";

    #[test]
    fn c_lang_query_and_registry() {
        let registry = crate::languages::LanguageRegistry::new();

        for ext in ["c", "h"] {
            let config = registry
                .for_extension(ext)
                .unwrap_or_else(|| panic!("language registry must resolve .{}", ext));
            assert_eq!(config.primary_language_id(), "C");
            config
                .compile_query()
                .expect("C definitions query must compile against the tree-sitter-c grammar");
            assert!(
                config.file_extensions().iter().any(|e| e == ext),
                "C config must own the .{} extension",
                ext
            );
        }

        let supported = registry.supported_extensions();
        assert!(supported.contains(&"c"), "supported extensions must include c");
        assert!(supported.contains(&"h"), "supported extensions must include h");
    }

    #[test]
    fn c_lang_cross_file_navigation() {
        use crate::navigation::Navigator;
        use crate::IndexBuilder;
        use std::fs;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("mathlib.h"), MATHLIB_H).unwrap();
        fs::write(root.join("mathlib.c"), MATHLIB_C).unwrap();
        let main_c = root.join("main.c");
        fs::write(&main_c, MAIN_C).unwrap();

        let index = IndexBuilder::new().build(root).unwrap();
        let navigator = Navigator::new(index);

        // Definitions exist for PI, Point, and distance.
        let pi_defs = navigator.goto_definition_by_name("PI", Some(&main_c));
        assert!(
            pi_defs
                .locations
                .iter()
                .any(|l| l.path.ends_with("mathlib.h") && l.line == 4),
            "PI should be defined as a macro on mathlib.h:4, got {:?}",
            pi_defs.locations
        );

        let point_defs = navigator.goto_definition_by_name("Point", Some(&main_c));
        assert!(
            point_defs
                .locations
                .iter()
                .any(|l| l.path.ends_with("mathlib.h")),
            "Point should be defined in mathlib.h, got {:?}",
            point_defs.locations
        );

        let distance_defs = navigator.goto_definition_by_name("distance", Some(&main_c));
        assert!(
            distance_defs
                .locations
                .iter()
                .any(|l| l.path.ends_with("mathlib.c") && l.line == 3),
            "distance should be defined on mathlib.c:3, got {:?}",
            distance_defs.locations
        );

        // The distance call in main.c must be a reference.
        let distance_refs = navigator.goto_references_by_name("distance", Some(&main_c), false);
        assert!(
            distance_refs
                .locations
                .iter()
                .any(|l| l.path.ends_with("main.c") && l.line == 6),
            "distance call site on main.c:6 must be a reference, got {:?}",
            distance_refs.locations
        );

        // Point must be referenced from the .c files that use it as a type.
        let point_refs = navigator.goto_references_by_name("Point", Some(&main_c), false);
        assert!(
            point_refs.locations.iter().any(|l| l.path.ends_with("main.c")),
            "Point must be referenced in main.c, got {:?}",
            point_refs.locations
        );
        assert!(
            point_refs
                .locations
                .iter()
                .any(|l| l.path.ends_with("mathlib.c")),
            "Point must be referenced in mathlib.c, got {:?}",
            point_refs.locations
        );
    }
}
