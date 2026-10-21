(function_declaration name: (identifier) @symbol.name) @symbol.def
(method_declaration name: (field_identifier) @symbol.name) @symbol.def

; only package-level declarations are symbols; each spec of a grouped declaration is its own symbol
(source_file (type_declaration (type_spec name: (type_identifier) @symbol.name) @symbol.def))
(source_file (type_declaration (type_alias name: (type_identifier) @symbol.name) @symbol.def))
(source_file (const_declaration (const_spec name: (identifier) @symbol.name) @symbol.def))
(source_file (var_declaration (var_spec name: (identifier) @symbol.name) @symbol.def))
(source_file (var_declaration (var_spec_list (var_spec name: (identifier) @symbol.name) @symbol.def)))
