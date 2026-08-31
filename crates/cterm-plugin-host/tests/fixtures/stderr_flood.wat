(module
  (import "wasi_snapshot_preview1" "fd_write"
    (func $fd_write (param i32 i32 i32 i32) (result i32)))
  (memory (export "memory") 1)
  ;; One iovec for 65,520 zero-initialized bytes at address 16.
  (data (i32.const 0) "\10\00\00\00\f0\ff\00\00")
  (func (export "_start")
    (loop $flood
      (drop (call $fd_write
        (i32.const 2) (i32.const 0) (i32.const 1) (i32.const 8)))
      (br $flood))))
