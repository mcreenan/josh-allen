const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const { Registry } = require("vscode-textmate");
const { loadWASM } = require("vscode-oniguruma");

const root = path.resolve(__dirname, "..");
const grammarPath = path.join(root, "syntaxes", "allen.tmLanguage.json");
const specPath = path.resolve(root, "..", "..", "docs", "language-spec.md");
const grammarDefinition = JSON.parse(fs.readFileSync(grammarPath, "utf8"));
const languageConfiguration = JSON.parse(
  fs.readFileSync(path.join(root, "language-configuration.json"), "utf8")
);
const languageSpec = fs.readFileSync(specPath, "utf8");

for (const file of [
  "package.json",
  path.join("syntaxes", "allen.tmLanguage.json")
]) {
  JSON.parse(fs.readFileSync(path.join(root, file), "utf8"));
}

function wordsFromAlternation(pattern, label) {
  const match = pattern.match(/\\b\(\?:([^()]*)\)\\b/);
  assert.ok(match, `${label} must remain a flat word alternation`);
  return match[1].split("|");
}

function backtickWords(text) {
  return [...text.matchAll(/`([A-Za-z_][A-Za-z0-9_]*)`/g)].map((match) => match[1]);
}

const reservedParagraph = languageSpec.match(
  /The reserved words are ([\s\S]*?)\. `None`,/
);
assert.ok(reservedParagraph, "language spec must retain its reserved-word inventory");
const reservedWords = backtickWords(reservedParagraph[1]).sort();
const highlightedReservedWords = [
  ...wordsFromAlternation(grammarDefinition.repository.keywords.match, "keyword grammar"),
  "false",
  "true"
].sort();
assert.deepEqual(
  highlightedReservedWords,
  reservedWords,
  "TextMate reserved words must match docs/language-spec.md"
);

const builtInTypesParagraph = languageSpec.match(
  /\n(`Void`, `Bool`,[\s\S]*?) are\s+the built-in named types\./
);
assert.ok(builtInTypesParagraph, "language spec must retain its built-in type inventory");
const highlightedTypes = new Set(
  wordsFromAlternation(grammarDefinition.repository.types.match, "type grammar")
);
for (const type of backtickWords(builtInTypesParagraph[1])) {
  assert.ok(highlightedTypes.has(type), `${type} must have support.type.allen highlighting`);
}

async function main() {
  const wasmPath = require.resolve("vscode-oniguruma/release/onig.wasm");
  const wasm = fs.readFileSync(wasmPath);
  await loadWASM(wasm.buffer.slice(wasm.byteOffset, wasm.byteOffset + wasm.byteLength));

  const registry = new Registry({
    onigLib: Promise.resolve(require("vscode-oniguruma")),
    loadGrammar: async (scopeName) => {
      assert.equal(scopeName, "source.allen");
      return grammarDefinition;
    }
  });
  const grammar = await registry.loadGrammar("source.allen");
  assert.ok(grammar, "the ALLEN TextMate grammar must load");

  const tokenized = new Map();
  for (const fixture of [
    "current.allen",
    "spec-preview.allen",
    "incomplete.allen",
    "comments.allen",
    "unterminated-comment.allen",
    "reserved-future-syntax.allen",
    "no-exception-keywords.allen",
    "operators.allen",
    "template-strings.allen",
    "unterminated-interpolation.allen",
    "control-flow-and-errors.allen",
    "template-resources.allen",
    "templates.allen",
    "l1-language.allen",
    "l2-language.allen",
    "l3-language.allen"
  ]) {
    let state = null;
    const lines = fs.readFileSync(path.join(root, "fixtures", fixture), "utf8").split("\n");
    const tokens = lines.map((line) => {
      const result = grammar.tokenizeLine(line, state);
      state = result.ruleStack;
      return { line, tokens: result.tokens };
    });
    tokenized.set(fixture, tokens);
  }

  function hasScope(fixture, text, scope) {
    return tokenized.get(fixture).some(({ line, tokens }) => tokens.some((token) =>
      line.slice(token.startIndex, token.endIndex).includes(text) && token.scopes.includes(scope)
    ));
  }

  function assertScope(fixture, text, scope) {
    assert.ok(hasScope(fixture, text, scope), `${fixture}: ${JSON.stringify(text)} needs ${scope}`);
  }

  function scopeAt(fixture, text) {
    for (const { line, tokens } of tokenized.get(fixture)) {
      const index = line.indexOf(text);
      if (index === -1) continue;
      const token = tokens.find(({ startIndex, endIndex }) =>
        startIndex <= index && endIndex > index
      );
      if (token) return token.scopes;
    }
    assert.fail(`${fixture}: ${JSON.stringify(text)} was not tokenized`);
  }

  function assertNotCommentScoped(fixture, text) {
    assert.ok(
      !scopeAt(fixture, text).some((scope) => scope.startsWith("comment.")),
      `${fixture}: ${JSON.stringify(text)} must not be comment-scoped`
    );
  }

  function assertNotKeywordScoped(fixture, text) {
    assert.ok(
      !scopeAt(fixture, text).some((scope) => scope.startsWith("keyword.")),
      `${fixture}: ${JSON.stringify(text)} must not be keyword-scoped`
    );
  }

  assertScope("current.allen", "record", "storage.type.declaration.allen");
  assertScope("current.allen", "where", "keyword.control.allen");
  assertScope("current.allen", "newtype", "storage.type.declaration.allen");
  assertScope("current.allen", "const", "storage.modifier.const.allen");
  assertScope("current.allen", "test", "keyword.control.allen");
  assertScope("current.allen", "shared answer", "string.quoted.double.allen");
  assertScope("current.allen", "SharedAnswer", "entity.name.constant.allen");
  assertScope("current.allen", "EpochSeconds", "entity.name.type.allen");
  assertScope("current.allen", "Point", "entity.name.type.allen");
  assertScope("current.allen", "import", "keyword.control.allen");
  assertScope("current.allen", "Int", "support.type.allen");
  assertScope("current.allen", "b\"", "string.quoted.double.bytes.allen");
  assertScope("current.allen", "\\x00", "constant.character.escape.allen");
  assertScope("current.allen", "Float", "support.type.allen");
  assertScope("current.allen", "Reading", "entity.name.type.allen");
  assertScope("current.allen", "export", "storage.modifier.allen");
  assertScope("current.allen", "type", "storage.type.declaration.allen");
  assertScope("current.allen", "Measurements", "entity.name.type.allen");
  assertScope("current.allen", "40", "constant.numeric.allen");
  assertScope("current.allen", "485_273", "constant.numeric.allen");
  assertScope("current.allen", "12_345.67_89e+1_0", "constant.numeric.allen");
  assertScope("current.allen", "+", "keyword.operator.allen");
  assertScope("current.allen", "mut", "keyword.control.allen");
  assertScope("current.allen", "if", "keyword.control.allen");
  assertScope("current.allen", "for", "keyword.control.allen");
  assertScope("current.allen", "in", "keyword.control.allen");
  assertScope("current.allen", "while", "keyword.control.allen");
  assertScope("current.allen", "loop", "keyword.control.allen");
  assertScope("current.allen", "break", "keyword.control.allen");
  assertScope("current.allen", "continue", "keyword.control.allen");
  assertScope("current.allen", "..", "keyword.operator.allen");
  for (const operator of ["%", "+=", "-=", "*=", "/=", "%=", "&&", "||", "??"]) {
    assertScope("operators.allen", operator, "keyword.operator.allen");
  }
  assertScope("spec-preview.allen", "manifest", "keyword.control.allen");
  assertScope("spec-preview.allen", "effects", "keyword.control.allen");
  assertScope("spec-preview.allen", "agent.ask", "support.function.effect.allen");
  assertScope("spec-preview.allen", "tool.github.create_issue@2", "support.function.effect.allen");
  assertScope("incomplete.allen", "Draft", "string.quoted.double.allen");
  assertScope("comments.allen", "A line comment", "comment.line.double-slash.allen");
  assertScope("comments.allen", "This doc-looking comment", "comment.line.double-slash.allen");
  assertScope("comments.allen", "A block comment", "comment.block.allen");
  assertScope("comments.allen", "This doc-looking block", "comment.block.allen");
  assertScope("comments.allen", "Nested block comment", "comment.block.allen");
  assertScope("comments.allen", "Back in the outer block", "comment.block.allen");
  assertScope("comments.allen", "trailing line comment", "comment.line.double-slash.allen");
  assertScope("unterminated-comment.allen", "end of file", "comment.block.allen");
  assertScope("comments.allen", "// is text", "string.quoted.double.allen");
  assertScope("comments.allen", "/* and */", "string.quoted.double.allen");
  assertScope("comments.allen", "// /* */", "string.quoted.double.bytes.allen");
  assertNotCommentScoped("comments.allen", "// is text");
  assertNotCommentScoped("comments.allen", "/* and */");
  assertNotCommentScoped("comments.allen", "// /* */");
  assertScope("template-strings.allen", "`", "string.quoted.template.allen");
  assertScope("template-strings.allen", "${", "meta.interpolation.allen");
  assertScope("template-strings.allen", "\\`", "constant.character.escape.allen");
  assertScope("template-strings.allen", "\\\"", "constant.character.escape.allen");
  assertScope("template-strings.allen", "\\\\", "constant.character.escape.allen");
  assertScope("template-strings.allen", "\\n", "constant.character.escape.allen");
  assertScope("template-strings.allen", "\\r", "constant.character.escape.allen");
  assertScope("template-strings.allen", "\\t", "constant.character.escape.allen");
  assertScope("template-strings.allen", "\\0", "constant.character.escape.allen");
  assertScope("template-strings.allen", "\\b", "constant.character.escape.allen");
  assertScope("template-strings.allen", "\\f", "constant.character.escape.allen");
  assertScope("template-strings.allen", "\\${", "constant.character.escape.allen");
  assertScope("template-strings.allen", "capability.is_granted", "support.function.effect.allen");
  assertScope("template-strings.allen", "if", "keyword.control.allen");
  assertScope("template-strings.allen", "nested", "string.quoted.template.allen");
  assertNotCommentScoped("template-strings.allen", "// not a comment");
  assertNotCommentScoped("template-strings.allen", "/* not a comment */");
  assertScope("unterminated-interpolation.allen", "value", "meta.interpolation.allen");
  assertScope("control-flow-and-errors.allen", "for", "keyword.control.allen");
  assertScope("control-flow-and-errors.allen", "Result", "support.type.allen");
  assertScope("control-flow-and-errors.allen", "Eq", "support.type.allen");
  assertScope("control-flow-and-errors.allen", "AgentError", "support.type.allen");
  assertScope("control-flow-and-errors.allen", "PermissionError", "support.type.allen");
  assertScope("control-flow-and-errors.allen", "stop", "entity.name.function.call.allen");
  assertNotKeywordScoped("control-flow-and-errors.allen", "stop");
  assertScope("templates.allen", "${", "meta.interpolation.allen");
  assertNotCommentScoped("templates.allen", "// text");
  assertScope("l1-language.allen", "r###\"", "string.quoted.raw.allen");
  assertScope("l1-language.allen", "r\"", "string.quoted.raw.allen");
  assertScope("l1-language.allen", "r################\"", "string.quoted.raw.allen");
  assertScope("l1-language.allen", "\\\\d+", "string.quoted.raw.allen");
  assertNotCommentScoped("l1-language.allen", "${name}");
  assertScope("l1-language.allen", "\"\"\"", "string.quoted.multiline.allen");
  assertScope("l1-language.allen", "${", "meta.interpolation.allen");
  assertScope("l1-language.allen", "=>", "keyword.operator.allen");
  assertScope("l1-language.allen", "values", "variable.other.property.allen");
  assertScope("l2-language.allen", "extension", "keyword.control.allen");
  assertScope("l2-language.allen", "?.", "keyword.operator.allen");
  assertScope("l2-language.allen", ">>", "keyword.operator.allen");
  assertScope("l2-language.allen", "|>", "keyword.operator.allen");
  assertScope("l2-language.allen", "..", "keyword.operator.allen");
  assertScope("l3-language.allen", "Range", "support.type.allen");
  assertScope("l3-language.allen", "Sequence", "support.type.allen");
  assertScope("l3-language.allen", "..=", "keyword.operator.allen");
  assertScope("l3-language.allen", "|", "keyword.operator.allen");
  assertScope("l3-language.allen", "local_score", "entity.name.function.allen");
  assertScope("reserved-future-syntax.allen", "if", "keyword.control.allen");
  assertScope("reserved-future-syntax.allen", "else", "keyword.control.allen");
  for (const identifier of ["try", "catch", "finally", "throw"]) {
    assertNotCommentScoped("no-exception-keywords.allen", identifier);
    assert.ok(
      !scopeAt("no-exception-keywords.allen", identifier).some((scope) => scope.startsWith("keyword.")),
      `${identifier} remains an identifier; ALLEN has no exception syntax`
    );
  }
  assert.ok(languageConfiguration.brackets.some(([open, close]) => open === "(" && close === ")"),
    "condition parentheses must remain a bracket pair");
  assert.ok(new RegExp(languageConfiguration.indentationRules.increaseIndentPattern).test("else {"),
    "an else block must increase indentation");
  assert.ok(new RegExp(languageConfiguration.indentationRules.decreaseIndentPattern).test("else {"),
    "an else branch must decrease indentation before its block opens");
  assert.ok(new RegExp(languageConfiguration.indentationRules.increaseIndentPattern).test("while (ready) {"),
    "a while block must increase indentation");
  assert.ok(new RegExp(languageConfiguration.indentationRules.increaseIndentPattern).test("for item in items {"),
    "a for block must increase indentation");
  assert.ok(new RegExp(languageConfiguration.indentationRules.increaseIndentPattern).test("loop {"),
    "an unbounded loop block must increase indentation");
  assert.ok(hasScope("template-strings.allen", "${", "meta.interpolation.allen"),
    "template interpolation must remain distinguishable from literal template text");

  console.log("ALLEN TextMate grammar validation passed.");
}

main().catch((error) => {
  console.error(error);
  process.exitCode = 1;
});
