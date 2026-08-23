//! Converts parser events into a deterministic Rowan green tree.

use crate::{GreenNode, LexToken, SourceFile, SyntaxKind};
use rowan::{GreenNodeBuilder, SyntaxKind as RowanKind};

#[derive(Clone, Debug)]
pub(super) enum Event {
    Tombstone,
    Start {
        kind: SyntaxKind,
        forward_parent: Option<u32>,
    },
    Finish,
    Token {
        token_index: u32,
        token_count: u32,
        override_kind: Option<SyntaxKind>,
    },
}

/// Sinks one complete event stream. Parser-produced token indices are checked
/// here as a last line of defense: malformed events must not silently lose text.
pub(super) fn sink(
    mut events: Vec<Event>,
    source: &SourceFile,
    tokens: &[crate::LexToken],
) -> Result<GreenNode, &'static str> {
    if events.iter().any(|event| matches!(event, Event::Tombstone)) {
        return Err("surviving parser tombstone");
    }
    let mut builder = GreenNodeBuilder::new();
    let mut starts = Vec::new();
    let mut index = 0usize;
    let mut expected_token = 0usize;
    while index < events.len() {
        match events[index].clone() {
            // Forward-parent starts were consumed earlier in this same pass.
            Event::Tombstone => {}
            Event::Start {
                kind,
                forward_parent,
            } => {
                starts.push(kind);
                let mut parent = forward_parent;
                events[index] = Event::Tombstone;
                while let Some(distance) = parent {
                    let next = index
                        .checked_add(usize::try_from(distance).map_err(|_| "parent distance")?)
                        .ok_or("parent index overflow")?;
                    let event = events
                        .get(next)
                        .cloned()
                        .ok_or("parent index out of bounds")?;
                    match event {
                        Event::Start {
                            kind,
                            forward_parent,
                        } => {
                            starts.push(kind);
                            parent = forward_parent;
                            events[next] = Event::Tombstone;
                        }
                        _ => return Err("forward parent does not point at a start event"),
                    }
                }
                for kind in starts.drain(..).rev() {
                    builder.start_node(RowanKind(kind as u16));
                }
            }
            Event::Finish => builder.finish_node(),
            Event::Token {
                token_index,
                token_count,
                override_kind,
            } => {
                let token_index = usize::try_from(token_index).map_err(|_| "token index")?;
                if token_index != expected_token {
                    return Err("parser token events are not complete and monotonic");
                }
                let token_count = usize::try_from(token_count).map_err(|_| "token count")?;
                if token_count == 0 {
                    return Err("parser token event has zero width");
                }
                let token_end = token_index
                    .checked_add(token_count)
                    .ok_or("token index overflow")?;
                let token_slice = tokens
                    .get(token_index..token_end)
                    .ok_or("token range out of bounds")?;
                let kind = token_event_kind(override_kind, token_slice)?;
                if invalid_eof_reclassification(kind, token_slice) {
                    return Err("EOF cannot be coalesced or reclassified");
                }
                push_token(&mut builder, source, kind, token_slice);
                expected_token = token_end;
            }
        }
        index += 1;
    }
    if expected_token != tokens.len() {
        return Err("parser token events do not cover the lexer stream");
    }
    Ok(builder.finish())
}

fn token_event_kind(
    override_kind: Option<SyntaxKind>,
    tokens: &[LexToken],
) -> Result<SyntaxKind, &'static str> {
    match (override_kind, tokens) {
        (None, [token]) => Ok(token.kind()),
        (None, _) => Err("compound token event requires an override kind"),
        (Some(kind), _) if kind.is_token() => Ok(kind),
        (Some(_), _) => Err("token override uses a node kind"),
    }
}

fn invalid_eof_reclassification(kind: SyntaxKind, tokens: &[LexToken]) -> bool {
    tokens.iter().any(|token| token.kind() == SyntaxKind::Eof)
        && !(tokens.len() == 1 && kind == SyntaxKind::Eof)
}

fn push_token(
    builder: &mut GreenNodeBuilder<'_>,
    source: &SourceFile,
    kind: SyntaxKind,
    tokens: &[LexToken],
) {
    if let [token] = tokens {
        builder.token(RowanKind(kind as u16), token.text(source));
    } else {
        let text: String = tokens.iter().map(|token| token.text(source)).collect();
        builder.token(RowanKind(kind as u16), &text);
    }
}
