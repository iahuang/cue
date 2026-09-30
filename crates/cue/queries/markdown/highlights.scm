; cue's additions to tree-sitter-md's block query: where patterns capture
; the same node, the later one wins.

; Headings are colored whole, their markers too.
(atx_heading
  [
    (atx_h1_marker)
    (atx_h2_marker)
    (atx_h3_marker)
    (atx_h4_marker)
    (atx_h5_marker)
    (atx_h6_marker)
  ] @text.title)

(setext_heading
  [
    (setext_h1_underline)
    (setext_h2_underline)
  ] @text.title)

[
  (list_marker_plus)
  (list_marker_minus)
  (list_marker_star)
  (list_marker_dot)
  (list_marker_parenthesis)
  (task_list_marker_checked)
  (task_list_marker_unchecked)
] @text.list

[
  (block_quote_marker)
  (block_continuation)
  (thematic_break)
  (fenced_code_block_delimiter)
  (info_string)
  (pipe_table_delimiter_row)
] @text.delimiter

(pipe_table_header
  (pipe_table_cell) @text.strong)

(pipe_table_header "|" @text.delimiter)

(pipe_table_row "|" @text.delimiter)

[
  (entity_reference)
  (numeric_character_reference)
] @string.escape
