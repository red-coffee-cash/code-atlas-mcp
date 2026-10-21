; direct calls
(call_expression function: (identifier) @call.target_name) @call.site
(call_expression
  function: (member_expression object: (_) @call.receiver property: (property_identifier) @call.target_name)) @call.site

; identifiers passed as arguments (route handlers, middleware, promise callbacks)
(call_expression function: (_) @call.callback_via arguments: (arguments (identifier) @call.callback_ref))
(call_expression
  function: (_) @call.callback_via
  arguments: (arguments (member_expression property: (property_identifier) @call.callback_ref)))

; types in signatures (classified as param/return by position in code)
(type_identifier) @type.reference

; imports
(import_statement) @import.path
