(module
  (memory 1)
  (func (export "_start")
    ;; The initial page plus 256 more pages exceeds the 16 MiB store limit.
    (drop (memory.grow (i32.const 256)))))
