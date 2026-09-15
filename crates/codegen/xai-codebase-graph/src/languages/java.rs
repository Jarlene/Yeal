//! Java language configuration.

use crate::languages::types::TSLanguageConfig;

pub fn java_lang() -> TSLanguageConfig {
    TSLanguageConfig::new(
        vec!["Java".to_owned(), "java".to_owned()],
        vec!["java".to_owned()],
        vec![vec![
            "class".to_owned(),
            "interface".to_owned(),
            "enum".to_owned(),
            "annotation".to_owned(),
            "record".to_owned(),
            "method".to_owned(),
            "constructor".to_owned(),
            "field".to_owned(),
            "variable".to_owned(),
            "import".to_owned(),
            "package".to_owned(),
            "type".to_owned(),
        ]],
        r#"
        ; ============ DEFINITIONS ============

        ; Class definitions
        (class_declaration
            name: (identifier) @name.definition.class) @definition.class

        ; Interface definitions
        (interface_declaration
            name: (identifier) @name.definition.interface) @definition.interface

        ; Enum definitions
        (enum_declaration
            name: (identifier) @name.definition.enum) @definition.enum

        ; Annotation type definitions
        (annotation_type_declaration
            name: (identifier) @name.definition.annotation) @definition.annotation

        ; Record definitions
        (record_declaration
            name: (identifier) @name.definition.record) @definition.record

        ; Method definitions
        (method_declaration
            name: (identifier) @name.definition.method) @definition.method

        ; Constructor definitions
        (constructor_declaration
            name: (identifier) @name.definition.constructor) @definition.constructor

        ; Field definitions
        (field_declaration
            declarator: (variable_declarator
                name: (identifier) @name.definition.field)) @definition.field

        ; Local variable definitions
        (local_variable_declaration
            declarator: (variable_declarator
                name: (identifier) @name.definition.variable)) @definition.variable

        ; Parameter definitions
        (formal_parameter
            name: (identifier) @name.definition.variable) @definition.variable

        ; package demo;
        ; Note: package_declaration and import_declaration have NO fields in
        ; tree-sitter-java 0.23; their name is a plain direct child.
        (package_declaration
            (identifier) @name.definition.package) @definition.package

        (package_declaration
            (scoped_identifier
                name: (identifier) @name.definition.package)) @definition.package

        ; ============ REFERENCES ============

        ; Method invocation references: g.greet(), println(...)
        (method_invocation
            name: (identifier) @name.reference.method) @reference.method

        ; Field access references: this.name, System.out
        (field_access
            field: (identifier) @name.reference.field) @reference.field

        ; Class names used with `new`: new Greeter(...)
        (object_creation_expression
            type: (type_identifier) @name.reference.type) @reference.class

        ; Type identifier usages everywhere (variable/parameter/return types,
        ; extends/implements, casts, instanceof, generic type arguments,
        ; class literals). Catches all type positions in one pass.
        (type_identifier) @name.reference.type

        ; import java.util.List;
        (import_declaration
            (scoped_identifier
                name: (identifier) @name.reference.import)) @reference.import

        (import_declaration
            (identifier) @name.reference.import) @reference.import
        "#
        .to_owned(),
        || tree_sitter_java::LANGUAGE.into(),
    )
}

#[cfg(test)]
mod tests {
    /// Foo.java fixture, 13 lines:
    ///  1: package demo;
    ///  2:
    ///  3: public class Greeter {
    ///  4:     private String name;
    ///  5:
    ///  6:     public Greeter(String name) {
    ///  7:         this.name = name;
    ///  8:     }
    ///  9:
    /// 10:     public String greet() {
    /// 11:         return "Hello, " + name;
    /// 12:     }
    /// 13: }
    const FOO_JAVA: &str = "package demo;\n\npublic class Greeter {\n    private String name;\n\n    public Greeter(String name) {\n        this.name = name;\n    }\n\n    public String greet() {\n        return \"Hello, \" + name;\n    }\n}\n";

    /// Main.java fixture, 8 lines:
    /// 1: package demo;
    /// 2:
    /// 3: public class Main {
    /// 4:     public static void main(String[] args) {
    /// 5:         Greeter g = new Greeter("world");
    /// 6:         System.out.println(g.greet());
    /// 7:     }
    /// 8: }
    const MAIN_JAVA: &str = "package demo;\n\npublic class Main {\n    public static void main(String[] args) {\n        Greeter g = new Greeter(\"world\");\n        System.out.println(g.greet());\n    }\n}\n";

    #[test]
    fn java_lang_query_and_registry() {
        let registry = crate::languages::LanguageRegistry::new();

        let config = registry
            .for_file_path(std::path::Path::new("src/Main.java"))
            .unwrap_or_else(|| panic!("language registry must resolve a Main.java path"));
        assert_eq!(
            config.primary_language_id(),
            "Java",
            "primary language id must be Java"
        );
        config
            .compile_query()
            .expect("Java definitions query must compile against the tree-sitter-java grammar");

        let supported = registry.supported_extensions();
        assert!(
            supported.contains(&"java"),
            "supported extensions must include java"
        );

        // Every kind token emitted by the query must resolve to a symbol id.
        for kind in [
            "class", "interface", "enum", "annotation", "record", "method", "constructor",
            "field", "variable", "import", "package", "type",
        ] {
            assert!(
                config.symbol_id_of(kind).is_some(),
                "kind token '{}' must exist in the namespaces list",
                kind
            );
        }
    }

    #[test]
    fn java_lang_cross_file_navigation() {
        use crate::navigation::Navigator;
        use crate::IndexBuilder;
        use std::fs;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let src = root.join("src");
        fs::create_dir_all(&src).unwrap();
        let foo_java = src.join("Foo.java");
        fs::write(&foo_java, FOO_JAVA).unwrap();
        let main_java = src.join("Main.java");
        fs::write(&main_java, MAIN_JAVA).unwrap();

        let index = IndexBuilder::new().build(root).unwrap();
        let navigator = Navigator::new(index);

        // The Greeter class (and its constructor, which shares the name) is
        // defined in Foo.java.
        let greeter_defs = navigator.goto_definition_by_name("Greeter", Some(&main_java));
        assert!(
            !greeter_defs.locations.is_empty(),
            "Greeter must have a definition, got {:?}",
            greeter_defs.locations
        );
        assert!(
            greeter_defs
                .locations
                .iter()
                .all(|l| l.path.ends_with("Foo.java")),
            "Greeter definitions must resolve to Foo.java, got {:?}",
            greeter_defs.locations
        );
        assert!(
            greeter_defs.locations.iter().any(|l| l.line == 3),
            "Greeter class declaration must be indexed on Foo.java:3, got {:?}",
            greeter_defs.locations
        );
        assert!(
            greeter_defs.locations.iter().any(|l| l.line == 6),
            "Greeter constructor must be indexed on Foo.java:6, got {:?}",
            greeter_defs.locations
        );

        // The `new Greeter("world")` usage in Main.java must be a reference.
        let greeter_refs = navigator.goto_references_by_name("Greeter", Some(&main_java), false);
        assert!(
            greeter_refs
                .locations
                .iter()
                .any(|l| l.path.ends_with("Main.java") && l.line == 5),
            "new-expression on Main.java:5 must be a Greeter reference, got {:?}",
            greeter_refs.locations
        );

        // greet() is defined in Foo.java and called from Main.java.
        let greet_defs = navigator.goto_definition_by_name("greet", Some(&main_java));
        assert!(
            greet_defs
                .locations
                .iter()
                .any(|l| l.path.ends_with("Foo.java") && l.line == 10),
            "greet must be defined on Foo.java:10, got {:?}",
            greet_defs.locations
        );
        let greet_refs = navigator.goto_references_by_name("greet", Some(&main_java), false);
        assert!(
            greet_refs
                .locations
                .iter()
                .any(|l| l.path.ends_with("Main.java") && l.line == 6),
            "g.greet() call on Main.java:6 must be a greet reference, got {:?}",
            greet_refs.locations
        );

        // The Main class itself must be a definition.
        let main_defs = navigator.goto_definition_by_name("Main", Some(&main_java));
        assert!(
            main_defs
                .locations
                .iter()
                .any(|l| l.path.ends_with("Main.java") && l.line == 3),
            "Main must be defined on Main.java:3, got {:?}",
            main_defs.locations
        );
    }
}
