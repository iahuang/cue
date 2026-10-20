; cue's additions to tree-sitter-python's query: where patterns capture the
; same node, the later one wins.

; Operators spelled as words read as keywords, as in `for x in xs`.
[
  "and"
  "in"
  "is"
  "not"
  "or"
  "is not"
  "not in"
] @keyword.operator
