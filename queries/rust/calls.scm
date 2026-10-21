; direct calls
(call_expression function: (identifier) @call.target_name) @call.site
(call_expression
  function: (scoped_identifier path: (_) @call.receiver name: (identifier) @call.target_name)) @call.site
(call_expression
  function: (field_expression value: (_) @call.receiver field: (field_identifier) @call.target_name)) @call.site
(call_expression
  function: (generic_function function: (identifier) @call.target_name)) @call.site
(call_expression
  function: (generic_function
    function: (field_expression value: (_) @call.receiver field: (field_identifier) @call.target_name))) @call.site

; identifiers passed as arguments (callbacks, route handlers)
(call_expression
  function: (_) @call.callback_via
  arguments: (arguments (identifier) @call.callback_ref))
(call_expression
  function: (_) @call.callback_via
  arguments: (arguments (scoped_identifier name: (identifier) @call.callback_ref)))

; macros: scanned in code for identifiers inside the token tree
(macro_invocation macro: (_) @macro.name (token_tree) @macro.tokens) @macro.site

; types in signatures (classified as param/return by position in code)
(type_identifier) @type.reference

; imports
(use_declaration argument: (_) @import.path)
