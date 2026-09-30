; From tree-sitter-bibtex (https://github.com/latex-lsp/tree-sitter-bibtex),
; queries/highlights.scm at 8d04ed27b3bc7929f14b7df9236797dab9f3fa66. MIT License.

[
  (string_type)
  (preamble_type)
  (entry_type)
] @keyword

[
  (junk)
  (comment)
] @comment

[
  "="
  "#"
] @operator

(command) @function.builtin

(number) @number

(field
  name: (identifier) @variable.builtin)

(token
  (identifier) @variable.parameter)

[
  (brace_word)
  (quote_word)
] @string

[
  (key_brace)
  (key_paren)
] @attribute

(string
  name: (identifier) @constant)

[
  "{"
  "}"
  "("
  ")"
] @punctuation.bracket

"," @punctuation.delimiter
