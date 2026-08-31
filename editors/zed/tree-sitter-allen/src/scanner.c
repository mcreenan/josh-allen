#include "tree_sitter/parser.h"

#include <stdbool.h>
#include <stdint.h>

enum TokenType {
  BLOCK_COMMENT,
  RAW_STRING_LITERAL,
  MULTILINE_STRING_LITERAL,
};

void *tree_sitter_allen_external_scanner_create(void) { return NULL; }
void tree_sitter_allen_external_scanner_destroy(void *payload) { (void)payload; }
unsigned tree_sitter_allen_external_scanner_serialize(void *payload, char *buffer) {
  (void)payload;
  (void)buffer;
  return 0;
}
void tree_sitter_allen_external_scanner_deserialize(void *payload, const char *buffer, unsigned length) {
  (void)payload;
  (void)buffer;
  (void)length;
}

static void advance(TSLexer *lexer) { lexer->advance(lexer, false); }
static void skip(TSLexer *lexer) { lexer->advance(lexer, true); }

static bool scan_block_comment(TSLexer *lexer) {
  if (lexer->lookahead != '/') return false;
  advance(lexer);
  if (lexer->lookahead != '*') return false;
  advance(lexer);

  uint16_t depth = 1;
  while (lexer->lookahead != 0) {
    if (lexer->lookahead == '/') {
      advance(lexer);
      if (lexer->lookahead == '*') {
        advance(lexer);
        if (depth < 128) depth++;
      }
    } else if (lexer->lookahead == '*') {
      advance(lexer);
      if (lexer->lookahead == '/') {
        advance(lexer);
        depth--;
        if (depth == 0) {
          lexer->mark_end(lexer);
          return true;
        }
      }
    } else {
      advance(lexer);
    }
  }

  lexer->mark_end(lexer);
  return true;
}

static bool scan_raw_string(TSLexer *lexer) {
  if (lexer->lookahead != 'r') return false;
  advance(lexer);

  uint8_t hash_count = 0;
  while (lexer->lookahead == '#' && hash_count <= 16) {
    hash_count++;
    advance(lexer);
  }
  if (hash_count > 16 || lexer->lookahead != '"') return false;
  advance(lexer);

  while (lexer->lookahead != 0) {
    if (lexer->lookahead != '"') {
      advance(lexer);
      continue;
    }

    advance(lexer);
    uint8_t matched = 0;
    while (matched < hash_count && lexer->lookahead == '#') {
      matched++;
      advance(lexer);
    }
    if (matched == hash_count && lexer->lookahead != '#') {
      lexer->mark_end(lexer);
      return true;
    }
  }

  lexer->mark_end(lexer);
  return true;
}

static bool scan_multiline_string(TSLexer *lexer) {
  if (lexer->lookahead != '"') return false;
  advance(lexer);
  if (lexer->lookahead != '"') return false;
  advance(lexer);
  if (lexer->lookahead != '"') return false;
  advance(lexer);

  while (lexer->lookahead != 0) {
    if (lexer->lookahead != '"') {
      advance(lexer);
      continue;
    }

    advance(lexer);
    if (lexer->lookahead != '"') continue;
    advance(lexer);
    if (lexer->lookahead != '"') continue;
    advance(lexer);
    lexer->mark_end(lexer);
    return true;
  }

  lexer->mark_end(lexer);
  return true;
}

bool tree_sitter_allen_external_scanner_scan(void *payload, TSLexer *lexer, const bool *valid_symbols) {
  (void)payload;

  while (lexer->lookahead == ' ' || lexer->lookahead == '\t' ||
         lexer->lookahead == '\r' || lexer->lookahead == '\n') {
    skip(lexer);
  }

  if (valid_symbols[BLOCK_COMMENT] && lexer->lookahead == '/') {
    if (scan_block_comment(lexer)) {
      lexer->result_symbol = BLOCK_COMMENT;
      return true;
    }
  }
  if (valid_symbols[RAW_STRING_LITERAL] && lexer->lookahead == 'r') {
    if (scan_raw_string(lexer)) {
      lexer->result_symbol = RAW_STRING_LITERAL;
      return true;
    }
  }
  if (valid_symbols[MULTILINE_STRING_LITERAL] && lexer->lookahead == '"') {
    if (scan_multiline_string(lexer)) {
      lexer->result_symbol = MULTILINE_STRING_LITERAL;
      return true;
    }
  }
  return false;
}
