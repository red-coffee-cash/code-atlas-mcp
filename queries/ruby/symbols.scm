(method name: (_) @symbol.name) @symbol.def
(singleton_method name: (_) @symbol.name) @symbol.def

; `class Foo::Bar` is the symbol `Bar` scoped under `Foo`; modules are classes (no separate kind)
(class name: (constant) @symbol.name) @symbol.def
(class name: (scope_resolution name: (constant) @symbol.name)) @symbol.def
(module name: (constant) @symbol.name) @symbol.def
(module name: (scope_resolution name: (constant) @symbol.name)) @symbol.def

; constants at top level or directly in a class/module body; locals and @ivars are not symbols
(program (assignment left: (constant) @symbol.name) @symbol.def)
(class body: (body_statement (assignment left: (constant) @symbol.name) @symbol.def))
(module body: (body_statement (assignment left: (constant) @symbol.name) @symbol.def))
