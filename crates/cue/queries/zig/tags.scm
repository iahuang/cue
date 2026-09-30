(function_declaration
  name: (identifier) @name) @definition.function

(variable_declaration
  (identifier) @name
  (struct_declaration)) @definition.struct

(variable_declaration
  (identifier) @name
  (enum_declaration)) @definition.enum

(variable_declaration
  (identifier) @name
  (union_declaration)) @definition.union

(variable_declaration
  (identifier) @name
  [
    (opaque_declaration)
    (error_set_declaration)
  ]) @definition.type

(test_declaration
  (string
    (string_content) @name)) @definition.test

(test_declaration
  (identifier) @name) @definition.test
