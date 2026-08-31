; Comments and literals
[
  (line_comment)
  (block_comment)
] @comment

[
  (string_literal)
  (bytes_literal)
  (raw_string_literal)
  (multiline_string_literal)
  (template_literal)
] @string

(template_escape_sequence) @string.escape

[
  (integer_literal)
  (float_literal)
] @number

[
  (boolean_literal)
  (none_literal)
  (unit_literal)
] @constant.builtin

; Declarations
(function_declaration name: (identifier) @function)
(local_function_declaration name: (identifier) @function)
(test_declaration name: (string_literal) @function)
(const_declaration name: (identifier) @constant)

[
  (record_declaration name: (type_identifier) @type)
  (enum_declaration name: (type_identifier) @type)
  (type_alias_declaration name: (type_identifier) @type)
  (newtype_declaration name: (type_identifier) @type)
]

(enum_variant name: (type_identifier) @constructor)
(generic_parameter name: (type_identifier) @type.parameter)
(parameter name: (identifier) @variable.parameter)

[
  (record_field name: (identifier) @property)
  (record_value_field name: (identifier) @property)
  (pattern_field name: (identifier) @property)
  (manifest_field name: (_) @property)
  (prompt_field name: (_) @property)
  (argument label: (identifier) @property)
]

; Types and constructors
(type_identifier) @type

((type_identifier) @type.builtin
  (#any-of? @type.builtin
    "Bool" "Int" "Float" "String" "Bytes" "Void" "Never"
    "List" "Map" "Option" "Result" "Range" "Sequence" "unknown"
    "Prompt" "Future" "Task" "Workspace" "ExternalFsAccess" "SubAgent"
    "ExternalFileRequest" "ExternalDirectoryRequest" "HttpResponse"
    "FileError" "NetworkError" "ExecError" "TimeError" "ParseError"
    "FormatError" "DecodeError" "TranscriptPart" "TranscriptMessage"
    "TranscriptSnapshot" "Eq"))

(record_literal type: (_) @constructor)
(constructor_pattern constructor: (_) @constructor)

; Calls, members, and effects
(call_expression function: (identifier) @function.call)
(call_expression function: (type_identifier) @constructor)
(call_expression
  function: (field_expression field: (identifier) @function.method))
(call_expression
  function: (optional_field_expression field: (identifier) @function.method))

[
  (field_expression field: (identifier) @property)
  (optional_field_expression field: (identifier) @property)
]

(effect_identifier) @function.special
(map_identifier) @keyword

((identifier) @constant.builtin
  (#any-of? @constant.builtin "Some" "Ok" "Err"))

((identifier) @function.builtin
  (#any-of? @function.builtin
    "decode" "length" "narrow" "to_bytes" "to_float" "to_int"
    "to_string" "to_unknown"))

; Keywords
[
  "as"
  "async"
  "await"
  "break"
  "const"
  "continue"
  "effects"
  "else"
  "enum"
  "export"
  "extension"
  "fn"
  "for"
  "from"
  "if"
  "import"
  "in"
  "let"
  "loop"
  "manifest"
  "match"
  "mut"
  "newtype"
  "prompt"
  "record"
  "return"
  "returns"
  "spawn"
  "test"
  "type"
  "where"
  "while"
] @keyword

; Operators and punctuation
[
  "!" "-" "+" "*" "/" "%"
  "=" "+=" "-=" "*=" "/=" "%="
  "==" "!=" "<" "<=" ">" ">="
  "&&" "||" "??" "?." "?"
  ".." "..=" ">>" "|>" "=>" "|"
] @operator

[
  "(" ")" "[" "]" "{" "}"
] @punctuation.bracket

[
  "," "." ":" ";"
] @punctuation.delimiter

[
  "${" "`"
] @punctuation.special
