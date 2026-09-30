; cue's own, after tree-sitter-svelte-ng's injections.scm, which inherits
; Neovim's `html_tags` for scripts and styles: without it, every script and
; style would be JavaScript.

((script_element
  (raw_text) @injection.content)
  (#set! injection.language "javascript"))

((script_element
  (start_tag
    (attribute
      (attribute_name) @_attr
      (quoted_attribute_value
        (attribute_value) @_lang)))
  (raw_text) @injection.content)
  (#eq? @_attr "lang")
  (#any-of? @_lang "ts" "typescript")
  (#set! injection.language "typescript"))

; SCSS and Less too: CSS's grammar gets most of them right.
((style_element
  (raw_text) @injection.content)
  (#set! injection.language "css"))
