; direct calls (`go f()` and `defer f()` wrap a plain call_expression)
(call_expression function: (identifier) @call.target_name) @call.site
(call_expression function: (selector_expression operand: (_) @call.receiver field: (field_identifier) @call.target_name)) @call.site

; identifiers passed as arguments (handlers, goroutine entry points, callbacks)
(call_expression function: (_) @call.callback_via arguments: (argument_list (identifier) @call.callback_ref))
(call_expression function: (_) @call.callback_via arguments: (argument_list (selector_expression field: (field_identifier) @call.callback_ref)))

; types in parameter and result lists (classified as param/return by position in code)
(parameter_declaration type: (type_identifier) @type.reference)
(parameter_declaration type: (pointer_type (type_identifier) @type.reference))
(parameter_declaration type: (qualified_type name: (type_identifier) @type.reference))
(parameter_declaration type: (pointer_type (qualified_type name: (type_identifier) @type.reference)))
(parameter_declaration type: (slice_type element: (type_identifier) @type.reference))
(parameter_declaration type: (slice_type element: (pointer_type (type_identifier) @type.reference)))
(variadic_parameter_declaration type: (type_identifier) @type.reference)
(variadic_parameter_declaration type: (pointer_type (type_identifier) @type.reference))
(function_declaration result: (type_identifier) @type.reference)
(function_declaration result: (pointer_type (type_identifier) @type.reference))
(function_declaration result: (qualified_type name: (type_identifier) @type.reference))
(method_declaration result: (type_identifier) @type.reference)
(method_declaration result: (pointer_type (type_identifier) @type.reference))
(method_declaration result: (qualified_type name: (type_identifier) @type.reference))

; imports
(import_spec) @import.path
