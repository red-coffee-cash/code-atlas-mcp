; direct calls; a bare `foo` with no parens and no receiver is indistinguishable from a local
; variable, so it is deliberately not a call site
(call receiver: (_) @call.receiver method: (identifier) @call.target_name) @call.site
(call !receiver method: (identifier) @call.target_name) @call.site

; method names passed as symbols to callback-registering and reflective calls
(call
  method: (identifier) @call.callback_via
  arguments: (argument_list (simple_symbol) @call.callback_ref)
  (#any-of? @call.callback_via
    "before_action" "after_action" "around_action" "skip_before_action" "skip_after_action"
    "before_filter" "after_filter" "before_save" "after_save" "before_create" "after_create"
    "before_update" "after_update" "before_destroy" "after_destroy" "before_validation"
    "after_commit" "after_rollback" "validate" "validates_with" "method" "send" "public_send"
    "__send__" "try" "alias_method" "delegate" "set_callback" "helper_method"))

; `&:name` symbol-to-proc
(call
  method: (identifier) @call.callback_via
  arguments: (argument_list (block_argument (simple_symbol) @call.callback_ref)))

; imports
(call
  !receiver
  method: (identifier) @_m
  arguments: (argument_list . (_))
  (#any-of? @_m "require" "require_relative" "load" "autoload")) @import.path
