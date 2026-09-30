; cue's addition, before tree-sitter-make's query, which only colors the
; standard targets: they keep their color, as later patterns win.

(rule
  (targets
    (word) @function))
