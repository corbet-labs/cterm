(module
  (import "wasi_snapshot_preview1" "fd_write"
    (func (param i32 i32 i32 i32) (result i32)))
  (memory (export "memory") 1)
  ;; Length-delimited PluginResponse ABI v1.0 requesting cterm:new-tab.
  (data (i32.const 32) "\13\08\01\1a\0f\0a\0dcterm:new-tab")
  (func (export "_start")
    (i32.store (i32.const 0) (i32.const 32))
    (i32.store (i32.const 4) (i32.const 20))
    (drop (call 0
      (i32.const 1) (i32.const 0) (i32.const 1) (i32.const 8)))))
