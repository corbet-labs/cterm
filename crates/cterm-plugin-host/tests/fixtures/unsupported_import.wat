(module
  (import "cterm_ambient" "open_everything" (func $open_everything))
  (func (export "_start")
    (call $open_everything)))
