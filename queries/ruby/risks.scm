(call !receiver method: (identifier) @_m (#any-of? @_m "system" "exec" "spawn" "syscall" "eval")) @risk.exec
(call method: (identifier) @_m (#any-of? @_m "instance_eval" "class_eval" "module_eval" "constantize" "safe_constantize")) @risk.exec
(subshell) @risk.exec
(call receiver: (constant) @_r method: (identifier) @_m (#eq? @_r "Kernel") (#any-of? @_m "system" "exec" "spawn" "eval" "open")) @risk.exec
(call receiver: (constant) @_r (#eq? @_r "Open3")) @risk.exec
(call receiver: (constant) @_r method: (identifier) @_m (#eq? @_r "IO") (#eq? @_m "popen")) @risk.exec

; send/public_send with a non-literal method name
(call
  method: (identifier) @_m
  arguments: (argument_list . [(identifier) (instance_variable) (call) (string (interpolation))])
  (#any-of? @_m "send" "public_send" "__send__")) @risk.exec

(call receiver: (constant) @_r method: (identifier) @_m
  (#any-of? @_r "Marshal")
  (#any-of? @_m "load" "restore")) @risk.deserialization
(call receiver: (constant) @_r method: (identifier) @_m
  (#any-of? @_r "YAML" "Psych")
  (#any-of? @_m "load" "unsafe_load" "load_file" "unsafe_load_file")) @risk.deserialization
(call receiver: (constant) @_r method: (identifier) @_m
  (#eq? @_r "Oj")
  (#any-of? @_m "load" "object_load")) @risk.deserialization
(call receiver: (constant) @_r method: (identifier) @_m
  arguments: (argument_list . [(identifier) (instance_variable) (call) (binary) (string (interpolation))])
  (#eq? @_r "ERB")
  (#eq? @_m "new")) @risk.deserialization

; string-built SQL
(call
  method: (identifier) @_m
  arguments: (argument_list . [(string (interpolation)) (binary operator: "+") (binary operator: "%")])
  (#any-of? @_m "execute" "exec_query" "select_all" "select_one" "select_value" "select_values" "select_rows"
    "find_by_sql" "count_by_sql" "where" "not" "order" "reorder" "pluck" "joins" "having" "group" "from"
    "exists?" "update_all" "delete_all")) @risk.raw_sql

(call !receiver method: (identifier) @_m (#eq? @_m "raw")) @risk.xss
(call method: (identifier) @_m (#eq? @_m "html_safe")) @risk.xss
(call
  method: (identifier) @_m
  arguments: (argument_list (string (interpolation)))
  (#eq? @_m "content_tag")) @risk.xss

(call receiver: (constant) @_r method: (identifier) @_m
  (#any-of? @_r "File" "IO" "Dir" "FileUtils" "Tempfile" "Pathname")
  (#any-of? @_m "open" "read" "write" "readlines" "foreach" "binread" "binwrite" "delete" "unlink" "rename"
    "chmod" "chown" "mkdir" "rmdir" "new" "copy" "cp" "cp_r" "mv" "move" "rm" "rm_f" "rm_r" "rm_rf" "mkdir_p"
    "touch" "glob" "entries" "children" "ln_s" "symlink" "chdir" "truncate" "sysopen" "create" "read_lines")) @risk.io
(call receiver: (constant) @_r method: (identifier) @_m (#eq? @_r "URI") (#eq? @_m "open")) @risk.io
(call receiver: (scope_resolution scope: (constant) @_s name: (constant) @_n) (#eq? @_s "Net") (#any-of? @_n "HTTP" "FTP" "SMTP" "IMAP" "POP3")) @risk.io
(call !receiver method: (identifier) @_m (#eq? @_m "open")) @risk.io

((constant) @risk.concurrency
  (#any-of? @risk.concurrency "Thread" "Mutex" "Monitor" "Queue" "SizedQueue" "ConditionVariable" "Ractor"))
(scope_resolution scope: (constant) @_s (#eq? @_s "Concurrent")) @risk.concurrency

; empty rescue body, `rescue Exception`, and `expr rescue nil`
(rescue !body) @risk.error_swallow
(rescue exceptions: (exceptions (constant) @_c (#eq? @_c "Exception"))) @risk.error_swallow
(rescue_modifier handler: (nil)) @risk.error_swallow

((constant) @risk.unsafe (#any-of? @risk.unsafe "Fiddle" "FFI"))
