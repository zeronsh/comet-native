; Comments

[
  (line_comment)
  (block_comment)
  (shebang)
] @comment

; Literals

[
  (string_literal)
  (multiline_string_literal)
  (character_literal)
] @string

(escape_sequence) @string.escape

[
  (number_literal)
  (float_literal)
] @number

; Keep the generic identifier capture before structural refinements. Tree-sitter
; resolves later matching patterns for the same range as the more specific role.
(identifier) @variable

((identifier) @boolean
  (#any-of? @boolean "true" "false"))

((identifier) @constant
  (#eq? @constant "null"))

; Declarations and references

(class_declaration
  name: (identifier) @type)

(object_declaration
  name: (identifier) @type)

(companion_object
  name: (identifier) @type)

(type_alias
  type: (identifier) @type)

(type_parameter
  (identifier) @type)

(user_type
  (identifier) @type)

((user_type
  (identifier) @type.builtin)
  (#any-of? @type.builtin
    "Any" "Boolean" "Byte" "Char" "Double" "Float" "Int" "Long"
    "Nothing" "Short" "String" "Unit" "UByte" "UInt" "ULong" "UShort"))

(function_declaration
  name: (identifier) @function)

(call_expression
  .
  (identifier) @function.call)

(call_expression
  .
  (navigation_expression
    (_)
    .
    (identifier) @function.call))

(constructor_invocation
  (user_type
    (identifier) @constructor))

(parameter
  (identifier) @variable.parameter)

(class_parameter
  (identifier) @variable.parameter)

(lambda_parameters
  (variable_declaration
    (identifier) @variable.parameter))

(catch_block
  (identifier) @variable.parameter)

(setter
  (identifier) @variable.parameter)

(class_parameter
  ["val" "var"]
  (identifier) @property)

(class_body
  (property_declaration
    (variable_declaration
      (identifier) @property)))

(source_file
  (property_declaration
    (variable_declaration
      (identifier) @property)))

(value_argument
  (identifier) @property
  "=")

(property_declaration
  (modifiers
    (property_modifier))
  (variable_declaration
    (identifier) @constant))

(enum_entry
  (identifier) @constant)

; Annotations and labels

(annotation
  "@" @punctuation.special)

(annotation
  (constructor_invocation
    (user_type
      (identifier) @attribute)))

(annotation
  (user_type
    (identifier) @attribute))

(label) @label

[
  (this_expression)
  (super_expression)
] @variable.builtin

; Keywords and modifiers

[
  "package"
  "import"
  "as"
  "class"
  "interface"
  "object"
  "companion"
  "fun"
  "val"
  "var"
  "typealias"
  "constructor"
  "init"
  "return"
  "return@"
  "throw"
  "if"
  "else"
  "when"
  "for"
  "while"
  "do"
  "try"
  "catch"
  "finally"
  "in"
  "!in"
  "is"
  "!is"
  "by"
  "where"
  "dynamic"
  "abstract"
  "actual"
  "annotation"
  "const"
  "crossinline"
  "data"
  "enum"
  "expect"
  "external"
  "final"
  "infix"
  "inline"
  "inner"
  "internal"
  "lateinit"
  "noinline"
  "open"
  "operator"
  "override"
  "private"
  "protected"
  "public"
  "sealed"
  "suspend"
  "tailrec"
  "value"
  "vararg"
] @keyword

(reification_modifier) @keyword

; Operators

[
  "!"
  "!!"
  "!="
  "!=="
  "%"
  "%="
  "&"
  "&&"
  "*"
  "*="
  "+"
  "++"
  "+="
  "-"
  "--"
  "-="
  "->"
  "/"
  "/="
  ".."
  "..<"
  "<"
  "<="
  "="
  "=="
  "==="
  ">"
  ">="
  "?:"
  "||"
  "as?"
] @operator

; Punctuation

[
  "("
  ")"
  "["
  "]"
  "{"
  "}"
] @punctuation.bracket

[
  ","
  "."
  "?."
  ":"
  "::"
  ";"
  "?"
] @punctuation.delimiter

[
  "$"
  "${"
  "@"
] @punctuation.special
