; tree-sitter-md parses table cells as inline markup too.
((pipe_table_cell) @injection.content
  (#set! injection.language "markdown_inline"))
