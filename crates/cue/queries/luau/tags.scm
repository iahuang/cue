; Adds to tree-sitter-lua's tags, which Luau's grammar shares the shape of:
; type aliases, as in `type Point = { x: number }`.

(type_definition
  name: [
    (identifier) @name
    (generic_type
      .
      (identifier) @name)
  ]) @definition.type
