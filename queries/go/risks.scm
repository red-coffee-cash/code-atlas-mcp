(call_expression
  function: (selector_expression operand: (identifier) @_o field: (field_identifier) @_f)
  (#eq? @_o "exec")
  (#any-of? @_f "Command" "CommandContext")) @risk.exec

(call_expression
  function: (selector_expression operand: (identifier) @_o field: (field_identifier) @_f)
  (#eq? @_o "syscall")
  (#any-of? @_f "Exec" "ForkExec" "StartProcess")) @risk.exec

; any use of package unsafe, in expressions or in types
(selector_expression operand: (identifier) @_o (#eq? @_o "unsafe")) @risk.unsafe
(qualified_type package: (package_identifier) @_p (#eq? @_p "unsafe")) @risk.unsafe

(selector_expression
  operand: (identifier) @_o field: (field_identifier) @_f
  (#eq? @_o "unsafe")
  (#eq? @_f "Pointer")) @risk.memory
(qualified_type
  package: (package_identifier) @_p name: (type_identifier) @_n
  (#eq? @_p "unsafe")
  (#eq? @_n "Pointer")) @risk.memory
(qualified_type
  package: (package_identifier) @_p name: (type_identifier) @_n
  (#eq? @_p "reflect")
  (#any-of? @_n "SliceHeader" "StringHeader")) @risk.memory

(call_expression
  function: (selector_expression operand: (identifier) @_o field: (field_identifier) @_f)
  (#eq? @_o "os")
  (#any-of? @_f "Open" "OpenFile" "Create" "ReadFile" "WriteFile" "Remove" "RemoveAll" "Rename" "Mkdir" "MkdirAll")) @risk.io

(call_expression
  function: (selector_expression operand: (identifier) @_o)
  (#eq? @_o "ioutil")) @risk.io

(call_expression
  function: (selector_expression operand: (identifier) @_o field: (field_identifier) @_f)
  (#eq? @_o "net")
  (#any-of? @_f "Dial" "DialTimeout" "DialTCP" "DialUDP" "Listen")) @risk.io

(call_expression
  function: (selector_expression operand: (identifier) @_o field: (field_identifier) @_f)
  (#eq? @_o "http")
  (#any-of? @_f "Get" "Post" "PostForm" "Head")) @risk.io

; string-built SQL. `sink` is a `+` concatenation or an `fmt.Sprintf` call; the query's argument
; position differs per API, so each gets its own pattern.
; database/sql, sqlx and gorm: the query is the first argument
(call_expression
  function: (selector_expression field: (field_identifier) @_m)
  arguments: (argument_list . [
    (binary_expression operator: "+")
    (call_expression function: (selector_expression operand: (identifier) @_p field: (field_identifier) @_s (#eq? @_p "fmt") (#eq? @_s "Sprintf")))
  ])
  (#any-of? @_m "Query" "QueryRow" "Exec" "Prepare" "Queryx" "QueryRowx" "MustExec" "Raw")) @risk.raw_sql

; the same with a leading context.Context
(call_expression
  function: (selector_expression field: (field_identifier) @_m)
  arguments: (argument_list . (_) . [
    (binary_expression operator: "+")
    (call_expression function: (selector_expression operand: (identifier) @_p field: (field_identifier) @_s (#eq? @_p "fmt") (#eq? @_s "Sprintf")))
  ])
  (#any-of? @_m "QueryContext" "QueryRowContext" "ExecContext" "PrepareContext" "QueryxContext" "QueryRowxContext" "MustExecContext")) @risk.raw_sql

; sqlx Get/Select(&dest, query, ...): the `&dest` keeps redis/gin/http `Get` calls out
(call_expression
  function: (selector_expression field: (field_identifier) @_m)
  arguments: (argument_list . (unary_expression operator: "&") . [
    (binary_expression operator: "+")
    (call_expression function: (selector_expression operand: (identifier) @_p field: (field_identifier) @_s (#eq? @_p "fmt") (#eq? @_s "Sprintf")))
  ])
  (#any-of? @_m "Get" "Select")) @risk.raw_sql

; sqlx GetContext/SelectContext(ctx, &dest, query, ...)
(call_expression
  function: (selector_expression field: (field_identifier) @_m)
  arguments: (argument_list . (_) . (unary_expression operator: "&") . [
    (binary_expression operator: "+")
    (call_expression function: (selector_expression operand: (identifier) @_p field: (field_identifier) @_s (#eq? @_p "fmt") (#eq? @_s "Sprintf")))
  ])
  (#any-of? @_m "GetContext" "SelectContext")) @risk.raw_sql

(go_statement) @risk.concurrency
(channel_type) @risk.concurrency
(qualified_type package: (package_identifier) @_p (#eq? @_p "sync")) @risk.concurrency
(selector_expression operand: (identifier) @_o (#any-of? @_o "sync" "atomic")) @risk.concurrency

; `_ = f()` discards the result (an error, usually); `x, _ := f()` is idiomatic and not flagged
(assignment_statement
  left: (expression_list . (identifier) @_i .)
  right: (expression_list (call_expression))
  (#eq? @_i "_")) @risk.error_swallow

(call_expression function: (identifier) @_f (#eq? @_f "recover")) @risk.error_swallow

(call_expression
  function: (selector_expression operand: (identifier) @_o field: (field_identifier) @_f)
  (#eq? @_o "gob")
  (#eq? @_f "NewDecoder")) @risk.deserialization

(call_expression
  function: (selector_expression operand: (identifier) @_o field: (field_identifier) @_f)
  (#eq? @_o "template")
  (#any-of? @_f "HTML" "JS" "JSStr" "HTMLAttr" "URL" "CSS")) @risk.xss
