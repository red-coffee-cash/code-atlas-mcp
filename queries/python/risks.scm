(call function: (identifier) @_f (#any-of? @_f "eval" "exec" "compile" "__import__")) @risk.exec

(call
  function: (attribute object: (identifier) @_o attribute: (identifier) @_a)
  (#eq? @_o "pickle")
  (#any-of? @_a "loads" "load")) @risk.exec

(call
  function: (attribute object: (identifier) @_o)
  (#eq? @_o "subprocess")) @risk.exec

(call
  function: (attribute object: (identifier) @_o attribute: (identifier) @_a)
  (#eq? @_o "os")
  (#any-of? @_a "system" "popen" "execv" "execvp" "execl" "execlp" "spawnl" "spawnv")) @risk.exec

(call
  function: (attribute object: (identifier) @_o attribute: (identifier) @_a)
  (#any-of? @_o "yaml" "marshal" "shelve")
  (#any-of? @_a "load" "loads" "unsafe_load" "open")) @risk.deserialization

; bare `except:`
(except_clause . (block)) @risk.error_swallow

; string-built SQL handed to execute()
(call
  function: (attribute attribute: (identifier) @_m)
  arguments: (argument_list . [
    (string (interpolation))
    (binary_operator)
    (call function: (attribute attribute: (identifier) @_fmt (#eq? @_fmt "format")))
  ])
  (#any-of? @_m "execute" "executemany" "executescript" "raw")) @risk.raw_sql

(call function: (identifier) @_f (#eq? @_f "open")) @risk.io

(call
  function: (attribute object: (identifier) @_o attribute: (identifier) @_a)
  (#any-of? @_o "threading" "multiprocessing" "asyncio")
  (#any-of? @_a "Thread" "Lock" "RLock" "Semaphore" "Process" "Pool" "create_task" "gather")) @risk.concurrency
