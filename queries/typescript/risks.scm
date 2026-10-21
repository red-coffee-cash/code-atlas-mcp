(assignment_expression
  left: (member_expression property: (property_identifier) @_p)
  (#any-of? @_p "innerHTML" "outerHTML")) @risk.xss

(call_expression
  function: (member_expression property: (property_identifier) @_p)
  (#any-of? @_p "insertAdjacentHTML")) @risk.xss

(call_expression function: (identifier) @_f (#any-of? @_f "eval" "Function")) @risk.exec
(new_expression constructor: (identifier) @_c (#eq? @_c "Function")) @risk.exec

(call_expression
  function: (identifier) @_f
  (#any-of? @_f "exec" "execSync" "spawn" "spawnSync" "execFile" "execFileSync")) @risk.exec

(call_expression
  function: (member_expression object: (identifier) @_o)
  (#any-of? @_o "child_process" "cp")) @risk.exec

(call_expression
  function: (member_expression object: (identifier) @_o property: (property_identifier) @_p)
  (#eq? @_o "JSON")
  (#eq? @_p "parse")) @risk.deserialization

; string-built SQL
(call_expression
  function: (member_expression property: (property_identifier) @_m)
  arguments: (arguments . [
    (template_string (template_substitution))
    (binary_expression)
  ])
  (#any-of? @_m "query" "execute" "raw" "$queryRawUnsafe" "$executeRawUnsafe")) @risk.raw_sql

(call_expression
  function: (member_expression object: (identifier) @_o)
  (#any-of? @_o "fs" "fsp")) @risk.io
