; tree-sitter-rust's tags take every function in a block of items, a
; module's too, as a method; these take only those of impls and traits. They
; add impls, constants, and statics too. Methods come first: a definition
; goes by the first pattern that matches it.

(impl_item
  body: (declaration_list
    (function_item
      name: (identifier) @name) @definition.method))

(trait_item
  body: (declaration_list
    [
      (function_item
        name: (identifier) @name)
      (function_signature_item
        name: (identifier) @name)
    ] @definition.method))

(function_item
  name: (identifier) @name) @definition.function

(function_signature_item
  name: (identifier) @name) @definition.function

(struct_item
  name: (type_identifier) @name) @definition.struct

(enum_item
  name: (type_identifier) @name) @definition.enum

(union_item
  name: (type_identifier) @name) @definition.union

(type_item
  name: (type_identifier) @name) @definition.type

(trait_item
  name: (type_identifier) @name) @definition.trait

(impl_item
  type: (_) @name) @definition.impl

(mod_item
  name: (identifier) @name) @definition.module

(macro_definition
  name: (identifier) @name) @definition.macro

(const_item
  name: (identifier) @name) @definition.constant

(static_item
  name: (identifier) @name) @definition.constant
