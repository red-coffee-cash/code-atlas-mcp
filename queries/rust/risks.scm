(unsafe_block) @risk.unsafe
(function_modifiers "unsafe") @risk.unsafe

(call_expression
  function: (field_expression field: (field_identifier) @_m)
  (#any-of? @_m "unwrap" "expect" "unwrap_unchecked")) @risk.unwrap

(call_expression
  function: [
    (identifier) @_f
    (scoped_identifier name: (identifier) @_f)
  ]
  (#any-of? @_f "transmute" "transmute_copy" "from_raw_parts" "from_raw_parts_mut" "get_unchecked" "get_unchecked_mut")) @risk.memory

(call_expression
  function: (scoped_identifier
    path: (_) @_p
    name: (identifier) @_n)
  (#eq? @_p "Command")
  (#eq? @_n "new")) @risk.exec

(call_expression
  function: (scoped_identifier
    path: (_) @_p
    name: (identifier) @_n)
  (#any-of? @_p "File" "fs" "TcpStream" "TcpListener" "UdpSocket")
  (#any-of? @_n "open" "create" "read" "read_to_string" "write" "remove_file" "remove_dir_all" "connect" "bind")) @risk.io

(call_expression
  function: (scoped_identifier
    path: (_) @_p
    name: (identifier) @_n)
  (#any-of? @_p "Mutex" "RwLock" "thread" "tokio" "mpsc")
  (#any-of? @_n "new" "spawn" "channel")) @risk.concurrency

(call_expression
  function: (field_expression field: (field_identifier) @_m)
  (#any-of? @_m "lock" "try_lock")) @risk.concurrency
