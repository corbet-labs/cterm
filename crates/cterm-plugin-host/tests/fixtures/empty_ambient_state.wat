(module
  (import "wasi_snapshot_preview1" "environ_sizes_get"
    (func $environ_sizes_get (param i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "fd_prestat_get"
    (func $fd_prestat_get (param i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "fd_write"
    (func $fd_write (param i32 i32 i32 i32) (result i32)))
  (memory (export "memory") 1)
  ;; Length-delimited PluginResponse v1 containing cterm:new-tab.
  (data (i32.const 64) "\13\08\01\1a\0f\0a\0dcterm:new-tab")
  (func (export "_start")
    ;; The environment count and byte count must both be zero.
    (if (call $environ_sizes_get (i32.const 0) (i32.const 4))
      (then unreachable))
    (if (i32.load (i32.const 0)) (then unreachable))
    (if (i32.load (i32.const 4)) (then unreachable))
    ;; File descriptor 3 would be the first preopened directory. It must fail.
    (if (i32.eqz (call $fd_prestat_get (i32.const 3) (i32.const 8)))
      (then unreachable))
    (i32.store (i32.const 32) (i32.const 64))
    (i32.store (i32.const 36) (i32.const 20))
    (drop (call $fd_write
      (i32.const 1) (i32.const 32) (i32.const 1) (i32.const 40)))))
