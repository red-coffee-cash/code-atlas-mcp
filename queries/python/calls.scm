; direct calls
(call function: (identifier) @call.target_name) @call.site
(call function: (attribute object: (_) @call.receiver attribute: (identifier) @call.target_name)) @call.site

; identifiers passed as arguments (callbacks, route handlers, thread targets)
(call function: (_) @call.callback_via arguments: (argument_list (identifier) @call.callback_ref))
(call function: (_) @call.callback_via arguments: (argument_list (attribute attribute: (identifier) @call.callback_ref)))
(call function: (_) @call.callback_via arguments: (argument_list (keyword_argument value: (identifier) @call.callback_ref)))

; types in annotations (classified as param/return by position in code)
(type (identifier) @type.reference)
(type (attribute attribute: (identifier) @type.reference))
(type (subscript (identifier) @type.reference))

; imports
(import_statement) @import.path
(import_from_statement) @import.path
