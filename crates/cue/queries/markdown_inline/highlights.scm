; cue's additions to tree-sitter-md's inline query: where patterns capture
; the same node, the later one wins.

[
  (emphasis_delimiter)
  (code_span_delimiter)
  (latex_span_delimiter)
] @text.delimiter

(strikethrough) @text.strike

(email_autolink) @text.uri

(latex_block) @text.literal

[
  (entity_reference)
  (numeric_character_reference)
] @string.escape

(image
  [
    "!"
    "["
    "]"
    "("
    ")"
  ] @text.delimiter)

(inline_link
  [
    "["
    "]"
    "("
    ")"
  ] @text.delimiter)

(shortcut_link
  [
    "["
    "]"
  ] @text.delimiter)

(full_reference_link
  [
    "["
    "]"
  ] @text.delimiter)

(collapsed_reference_link
  [
    "["
    "]"
  ] @text.delimiter)
