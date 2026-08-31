/**
 * @file Tree-sitter grammar for ALLEN 0.1 source files.
 * @author Matt Creenan
 * @license MIT
 */

/// <reference types="tree-sitter-cli/dsl" />
// @ts-check

const PREC = {
  RANGE: 1,
  COALESCE: 2,
  PIPELINE: 3,
  COMPOSITION: 4,
  OR: 5,
  AND: 6,
  EQUALITY: 7,
  COMPARISON: 8,
  ADD: 9,
  MULTIPLY: 10,
  UNARY: 11,
  POSTFIX: 12,
};

module.exports = grammar({
  name: "allen",

  externals: ($) => [
    $.block_comment,
    $.raw_string_literal,
    $.multiline_string_literal,
  ],

  extras: ($) => [/[\s\uFEFF\u2060\u200B]/, $.line_comment, $.block_comment],

  word: ($) => $.identifier,

  supertypes: ($) => [$._declaration, $._statement, $._expression, $._type, $._pattern],

  conflicts: ($) => [
    [$.record_literal, $.block],
    [$.record_type_body, $.record_literal],
    [$.tuple_type, $.unit_literal],
    [$.parameters, $.function_type],
    [$.qualified_type, $.qualified_identifier],
    [$.qualified_type, $._expression, $.qualified_identifier],
    [$.unary_expression, $.comparison_expression, $.call_expression],
    [$.binary_expression, $.comparison_expression, $.call_expression],
    [$.conditional_statement, $._expression],
    [$._expression, $.record_value_field],
    [$._expression, $.qualified_identifier],
    [$._expression, $.record_literal],
    [$._expression, $.generic_type],
    [$._expression, $._type],
    [$.map_literal, $.map_identifier],
  ],

  rules: {
    source_file: ($) => seq(
      optional($.manifest),
      repeat($.import_declaration),
      repeat($._declaration),
    ),

    manifest: ($) => seq(
      "manifest",
      "{",
      optional(optionalCommaSep1($.manifest_field)),
      "}",
    ),

    manifest_field: ($) => choice(
      seq(field("name", "language"), ":", field("value", $.string_literal)),
      seq(field("name", "entry"), ":", field("value", $.identifier)),
      seq(field("name", "capabilities"), ":", "[", optional(commaSep1($.capability)), optional(","), "]"),
      seq(field("name", "http_origins"), ":", "[", optional(commaSep1($.string_literal)), optional(","), "]"),
      seq(field("name", "tools"), ":", "{", field("name", "required"), ":", "[", optional(commaSep1($.tool_requirement)), optional(","), "]", optional(","), "}"),
    ),

    capability: ($) => seq($.effect_identifier, optional(seq("(", $.identifier, ")"))),

    tool_requirement: ($) => seq(
      "{",
      field("name", "name"), ":", $.string_literal,
      ",",
      field("name", "version"), ":", $.string_literal,
      optional(","),
      "}",
    ),

    import_declaration: ($) => seq(
      "import",
      optional("extension"),
      "{",
      commaSep1($.import_specifier),
      optional(","),
      "}",
      "from",
      field("source", $.string_literal),
      ";",
    ),

    import_specifier: ($) => seq(
      field("name", $.identifier),
      optional(seq("as", field("alias", $.identifier))),
    ),

    _declaration: ($) => choice(
      $.record_declaration,
      $.enum_declaration,
      $.type_alias_declaration,
      $.newtype_declaration,
      $.const_declaration,
      $.function_declaration,
      $.test_declaration,
    ),

    record_declaration: ($) => seq(
      optional("export"),
      "record",
      field("name", $.type_identifier),
      field("body", $.record_type_body),
      optional(seq("where", "{", field("invariant", $._expression), "}")),
    ),

    enum_declaration: ($) => seq(
      optional("export"),
      "enum",
      field("name", $.type_identifier),
      "{",
      optional(optionalCommaSep1($.enum_variant)),
      "}",
    ),

    enum_variant: ($) => seq(
      field("name", $.type_identifier),
      optional(choice(
        seq("(", commaSep1($._type), optional(","), ")"),
        $.record_type_body,
      )),
    ),

    type_alias_declaration: ($) => seq(
      optional("export"),
      "type",
      field("name", $.type_identifier),
      "=",
      field("value", $._type),
    ),

    newtype_declaration: ($) => seq(
      optional("export"),
      "newtype",
      field("name", $.type_identifier),
      "=",
      field("value", $._type),
    ),

    const_declaration: ($) => seq(
      optional("export"),
      "const",
      field("name", $.identifier),
      ":",
      field("type", $._type),
      "=",
      field("value", $._expression),
      ";",
    ),

    function_declaration: ($) => seq(
      optional("export"),
      optional("async"),
      "fn",
      field("name", $.identifier),
      optional($.generic_parameters),
      field("parameters", $.parameters),
      "returns",
      field("return_type", $._type),
      optional($.effect_clause),
      field("body", $.block),
    ),

    local_function_declaration: ($) => seq(
      "fn",
      field("name", $.identifier),
      field("parameters", $.parameters),
      "returns",
      field("return_type", $._type),
      optional($.effect_clause),
      field("body", $.block),
    ),

    test_declaration: ($) => seq(
      "test",
      field("name", $.string_literal),
      optional($.effect_clause),
      field("body", $.block),
    ),

    generic_parameters: ($) => seq(
      "<",
      commaSep1($.generic_parameter),
      optional(","),
      ">",
    ),

    generic_parameter: ($) => seq(field("name", $.type_identifier), ":", field("constraint", $.type_identifier)),

    parameters: ($) => seq("(", optional(commaSep1($.parameter)), optional(","), ")"),

    parameter: ($) => seq(
      field("name", $.identifier),
      ":",
      field("type", $._type),
      optional(seq("=", field("default", $._expression))),
    ),

    effect_clause: ($) => seq(
      "effects",
      "[",
      optional(commaSep1($.effect_identifier)),
      optional(","),
      "]",
    ),

    block: ($) => seq("{", repeat($._statement), optional($._expression), "}"),

    _statement: ($) => choice(
      $.binding_statement,
      $.assignment_statement,
      $.return_statement,
      $.break_statement,
      $.continue_statement,
      $.while_statement,
      $.loop_statement,
      $.for_statement,
      $.local_function_declaration,
      $.conditional_statement,
    ),

    binding_statement: ($) => seq(
      field("kind", choice("let", "mut")),
      field("name", $.identifier),
      optional(seq(":", field("type", $._type))),
      "=",
      field("value", $._expression),
      ";",
    ),

    assignment_statement: ($) => seq(
      field("left", $.identifier),
      field("operator", choice("=", "+=", "-=", "*=", "/=", "%=")),
      field("right", $._expression),
      ";",
    ),

    return_statement: ($) => seq("return", optional($._expression), ";"),
    break_statement: (_) => seq("break", ";"),
    continue_statement: (_) => seq("continue", ";"),

    while_statement: ($) => seq("while", "(", field("condition", $._expression), ")", field("body", $.block)),
    loop_statement: ($) => seq("loop", field("body", $.block)),
    for_statement: ($) => seq("for", field("binding", $.loop_binding), "in", field("iterable", $._expression), field("body", $.block)),

    loop_binding: ($) => choice(
      $.identifier,
      "_",
      seq("(", choice($.identifier, "_"), ",", optional(commaSep1(choice($.identifier, "_"))), optional(","), ")"),
    ),

    conditional_statement: ($) => prec.dynamic(-1, $.if_expression),

    _type: ($) => choice(
      $.type_identifier,
      $.qualified_type,
      $.generic_type,
      $.tuple_type,
      $.record_type,
      $.function_type,
    ),

    qualified_type: ($) => seq($.type_identifier, repeat1(seq(".", $.type_identifier))),

    generic_type: ($) => prec.dynamic(1, seq(
      field("name", choice($.type_identifier, $.qualified_type)),
      "<",
      commaSep1($._type),
      optional(","),
      ">",
    )),

    tuple_type: ($) => seq(
      "(",
      optional(seq($._type, ",", optional(commaSep1($._type)), optional(","))),
      ")",
    ),

    record_type: ($) => $.record_type_body,
    record_type_body: ($) => seq("{", optional(optionalCommaSep1($.record_field)), "}"),
    record_field: ($) => seq(field("name", $.identifier), ":", field("type", $._type)),

    function_type: ($) => prec.right(seq(
      "fn",
      "(",
      optional(commaSep1($._type)),
      optional(","),
      ")",
      "returns",
      field("return_type", $._type),
      optional($.effect_clause),
    )),

    _expression: ($) => choice(
      $.identifier,
      $.type_identifier,
      $.map_identifier,
      $.integer_literal,
      $.float_literal,
      $.string_literal,
      $.bytes_literal,
      $.raw_string_literal,
      $.multiline_string_literal,
      $.template_literal,
      $.boolean_literal,
      $.none_literal,
      $.unit_literal,
      $.parenthesized_expression,
      $.list_literal,
      $.map_literal,
      $.tuple_expression,
      $.record_literal,
      $.if_expression,
      $.match_expression,
      $.closure_expression,
      $.short_closure_expression,
      $.prompt_expression,
      $.await_block,
      $.unary_expression,
      $.binary_expression,
      $.comparison_expression,
      $.call_expression,
      $.field_expression,
      $.optional_field_expression,
      $.index_expression,
      $.try_expression,
      $.range_expression,
    ),

    integer_literal: (_) => token(/[0-9](?:_?[0-9])*/),
    float_literal: (_) => token(/[0-9](?:_?[0-9])*\.[0-9](?:_?[0-9])*(?:[eE][+-]?[0-9](?:_?[0-9])*)?/),
    boolean_literal: (_) => choice("true", "false"),
    none_literal: (_) => "None",
    unit_literal: (_) => seq("(", ")"),

    string_literal: (_) => token(/"(?:[^"\\\r\n\x00-\x1f]|\\["\\nrt0bf])*"/),

    bytes_literal: (_) => token(/b"(?:[\x20-\x21\x23-\x5b\x5d-\x7e]|\\(?:["\\nrt0bf]|x[0-9A-Fa-f]{2}))*"/),

    template_literal: ($) => seq(
      "`",
      repeat(choice($.template_chars, $.template_escape_sequence, $.template_interpolation)),
      "`",
    ),

    // Keep invalid bare line breaks inside the template node so editor highlighting
    // recovers locally; the ALLEN compiler still reports them as source errors.
    template_chars: (_) => token.immediate(prec(1, /(?:[^\\`$\x00-\x09\x0b\x0c\x0e-\x1f]+|\$[^{])+/)),
    template_escape_sequence: (_) => token.immediate(/\\(?:["`\\nrt0bf]|\$\{)/),
    template_interpolation: ($) => seq("${", $._expression, "}"),

    list_literal: ($) => seq("[", optional(commaSep1($.list_item)), optional(","), "]"),
    list_item: ($) => choice($._expression, seq("..", $._expression)),

    map_literal: ($) => seq("map", "{", optional(optionalCommaSep1($.map_item)), "}"),
    map_item: ($) => choice(seq(field("key", $._expression), ":", field("value", $._expression)), seq("..", $._expression)),

    tuple_expression: ($) => seq(
      "(",
      $._expression,
      ",",
      optional(commaSep1($._expression)),
      optional(","),
      ")",
    ),

    parenthesized_expression: ($) => seq("(", $._expression, ")"),

    record_literal: ($) => seq(
      optional(field("type", choice($.type_identifier, $.qualified_identifier))),
      "{",
      optional(seq($.record_update_base, optional(","))),
      optional(optionalCommaSep1($.record_value_field)),
      "}",
    ),

    record_update_base: ($) => seq("..", $._expression),
    record_value_field: ($) => choice(
      seq(field("name", $.identifier), ":", field("value", $._expression)),
      field("name", $.identifier),
    ),

    if_expression: ($) => prec.right(seq(
      "if",
      "(",
      field("condition", $._expression),
      ")",
      field("consequence", $.block),
      optional(seq("else", field("alternative", choice($.if_expression, $.block)))),
    )),

    match_expression: ($) => seq(
      "match",
      field("value", $._expression),
      "{",
      repeat1(seq($.match_arm, optional(","))),
      "}",
    ),

    match_arm: ($) => seq(field("pattern", $._pattern), "=>", field("value", $._expression)),

    _pattern: ($) => choice($._pattern_primary, $.or_pattern),

    _pattern_primary: ($) => choice(
      $.wildcard_pattern,
      $.identifier,
      $.boolean_literal,
      $.none_literal,
      $.literal_pattern,
      $.constructor_pattern,
      $.range_pattern,
    ),

    wildcard_pattern: (_) => "_",
    literal_pattern: ($) => choice($.integer_literal, $.float_literal, $.string_literal, $.bytes_literal, $.raw_string_literal),
    constructor_pattern: ($) => seq(
      field("constructor", choice($.type_identifier, $.qualified_identifier)),
      optional(choice(
        seq("(", commaSep1($._pattern), optional(","), ")"),
        $.record_pattern_body,
      )),
    ),
    record_pattern_body: ($) => seq("{", optional(optionalCommaSep1($.pattern_field)), "}"),
    pattern_field: ($) => choice(
      seq(field("name", $.identifier), ":", field("pattern", $._pattern)),
      field("name", $.identifier),
    ),
    or_pattern: ($) => prec.left(seq($._pattern_primary, repeat1(seq("|", $._pattern_primary)))),
    range_pattern: ($) => prec.left(PREC.RANGE, seq($.literal_pattern, choice("..", "..="), $.literal_pattern)),

    closure_expression: ($) => seq(
      "fn",
      field("parameters", $.parameters),
      "returns",
      field("return_type", $._type),
      optional($.effect_clause),
      field("body", $.block),
    ),

    short_closure_expression: ($) => seq(
      "fn",
      "(",
      optional(commaSep1($.identifier)),
      optional(","),
      ")",
      "=>",
      field("body", $._expression),
    ),

    prompt_expression: ($) => seq("prompt", "{", optional(optionalCommaSep1($.prompt_field)), "}"),
    prompt_field: ($) => choice(
      seq(field("name", choice("system", "context", "data")), ":", field("value", $._expression)),
      seq(field("name", "output"), ":", field("value", $._type)),
      seq(field("name", "policy"), ":", "{", field("name", "max_attempts"), ":", $.integer_literal, optional(","), "}"),
    ),

    await_block: ($) => seq("await", $.block),

    unary_expression: ($) => prec.right(PREC.UNARY, seq(
      field("operator", choice("!", "-", "await", "spawn")),
      field("argument", $._expression),
    )),

    binary_expression: ($) => choice(
      ...[
        ["??", PREC.COALESCE, "right"],
        ["|>", PREC.PIPELINE, "left"],
        [">>", PREC.COMPOSITION, "left"],
        ["||", PREC.OR, "left"],
        ["&&", PREC.AND, "left"],
        ["==", PREC.EQUALITY, "left"],
        ["!=", PREC.EQUALITY, "left"],
        ["+", PREC.ADD, "left"],
        ["-", PREC.ADD, "left"],
        ["*", PREC.MULTIPLY, "left"],
        ["/", PREC.MULTIPLY, "left"],
        ["%", PREC.MULTIPLY, "left"],
      ].map(([operator, precedence, associativity]) =>
        associativity === "right"
          ? prec.right(precedence, seq(field("left", $._expression), field("operator", operator), field("right", $._expression)))
          : prec.left(precedence, seq(field("left", $._expression), field("operator", operator), field("right", $._expression))),
      ),
    ),

    comparison_expression: ($) => prec.left(PREC.COMPARISON, seq(
      field("left", $._expression),
      repeat1(seq(field("operator", choice("<", "<=", ">", ">=")), field("right", $._expression))),
    )),

    call_expression: ($) => prec.left(PREC.POSTFIX, seq(
      field("function", $._expression),
      optional($.type_arguments),
      field("arguments", $.arguments),
      optional(choice($.closure_expression, $.short_closure_expression)),
    )),

    type_arguments: ($) => prec.dynamic(2, seq("<", $._type, ">")),
    arguments: ($) => seq("(", optional(commaSep1($.argument)), optional(","), ")"),
    argument: ($) => choice(
      "_",
      $._expression,
      seq(field("label", $.identifier), ":", field("value", choice($._expression, "_"))),
    ),

    field_expression: ($) => prec.left(PREC.POSTFIX, seq(field("value", $._expression), ".", field("field", $.identifier))),
    optional_field_expression: ($) => prec.left(PREC.POSTFIX, seq(field("value", $._expression), "?.", field("field", $.identifier))),
    index_expression: ($) => prec.left(PREC.POSTFIX, seq(field("value", $._expression), "[", field("index", $._expression), "]")),
    try_expression: ($) => prec.left(PREC.POSTFIX, seq(field("value", $._expression), "?")),

    range_expression: ($) => prec.left(PREC.RANGE, seq(
      field("start", $._expression),
      field("operator", choice("..", "..=")),
      field("end", $._expression),
    )),

    qualified_identifier: ($) => seq($.type_identifier, repeat1(seq(".", choice($.identifier, $.type_identifier)))),

    line_comment: (_) => token(seq("//", /.*/)),
    map_identifier: (_) => "map",
    effect_identifier: (_) => token(/[a-z][a-z0-9_]*(?:\.[a-z][a-z0-9_]*)+(?:@[1-9][0-9]*)?/),
    type_identifier: (_) => token(/[A-Z][A-Za-z0-9_]*/),
    identifier: (_) => token(/[A-Za-z_][A-Za-z0-9_]*/),
  },
});

function commaSep1(rule) {
  return seq(rule, repeat(seq(",", rule)));
}

function optionalCommaSep1(rule) {
  return seq(rule, repeat(seq(optional(","), rule)), optional(","));
}
