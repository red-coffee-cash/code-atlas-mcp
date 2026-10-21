(function_declaration name: (identifier) @symbol.name) @symbol.def
(generator_function_declaration name: (identifier) @symbol.name) @symbol.def
(class_declaration name: (type_identifier) @symbol.name) @symbol.def
(abstract_class_declaration name: (type_identifier) @symbol.name) @symbol.def
(interface_declaration name: (type_identifier) @symbol.name) @symbol.def
(enum_declaration name: (identifier) @symbol.name) @symbol.def
(method_definition name: (property_identifier) @symbol.name) @symbol.def
(lexical_declaration
  (variable_declarator name: (identifier) @symbol.name value: [(arrow_function) (function_expression)])) @symbol.def
(variable_declaration
  (variable_declarator name: (identifier) @symbol.name value: [(arrow_function) (function_expression)])) @symbol.def
(public_field_definition name: (property_identifier) @symbol.name value: [(arrow_function) (function_expression)]) @symbol.def
