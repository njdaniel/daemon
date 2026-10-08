//! Text → the sentences the client asks the server for one at a time (#113).
//!
//! Two reasons. The trailing-silence cut ([`crate::tail`]) ends a whole generation, and inside
//! one generation of two sentences the model sometimes pauses about a second between them, the
//! same length as the silence before its end-of-text burst; one sentence per generation makes
//! "a long silence after speech" mean "the end". And the first sentence of a long turn is
//! rendered and starts playing before the rest has been rendered at all (the server's streaming
//! route would otherwise merge sentences into chunks of up to 200 characters first).
//!
//! The split is the server's own (`app/core/text_processing.py`: after `.`, `!` or `?` and
//! whitespace; `…` too), and a piece shorter than [`MIN_CHARS`] joins the next one (or the one
//! before, at the end), so "z. B." or "Ja." never becomes a generation of its own.

/// A piece shorter than this many characters is not a sentence of its own.
pub(crate) const MIN_CHARS: usize = 20;

/// `text` as sentences, in order, each trimmed; joined with spaces they are `text` with its
/// whitespace runs between sentences collapsed. Empty for blank text.
pub(crate) fn sentences(text: &str) -> Vec<String> {
    let mut pieces: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut chars = text.trim().chars().peekable();
    while let Some(c) = chars.next() {
        current.push(c);
        if matches!(c, '.' | '!' | '?' | '…') && chars.peek().is_some_and(|n| n.is_whitespace()) {
            while chars.peek().is_some_and(|n| n.is_whitespace()) {
                chars.next();
            }
            pieces.push(std::mem::take(&mut current));
        }
    }
    if !current.trim().is_empty() {
        pieces.push(current);
    }
    let mut out: Vec<String> = Vec::new();
    let mut carry = String::new();
    for piece in pieces {
        let piece = piece.trim();
        if carry.is_empty() {
            carry = piece.to_owned();
        } else {
            carry.push(' ');
            carry.push_str(piece);
        }
        if carry.chars().count() >= MIN_CHARS {
            out.push(std::mem::take(&mut carry));
        }
    }
    if !carry.is_empty() {
        match out.last_mut() {
            Some(last) => {
                last.push(' ');
                last.push_str(&carry);
            }
            None => out.push(carry),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_at_sentence_ends_like_the_server() {
        assert_eq!(
            sentences(
                "Zuerst würde ich die Anforderungen klären. Dann nehme ich einen Token-Bucket!  Wie viele Anfragen pro Sekunde?"
            ),
            [
                "Zuerst würde ich die Anforderungen klären.",
                "Dann nehme ich einen Token-Bucket!",
                "Wie viele Anfragen pro Sekunde?"
            ]
        );
        // A period inside a number is not an end.
        assert_eq!(
            sentences("Version 1.27 ist seit gestern stabil."),
            ["Version 1.27 ist seit gestern stabil."]
        );
    }

    #[test]
    fn short_pieces_join_a_neighbour() {
        assert_eq!(
            sentences("Ja. Genau das würde ich auch machen. Z. B. mit Redis und einem Cache."),
            [
                "Ja. Genau das würde ich auch machen.",
                "Z. B. mit Redis und einem Cache."
            ]
        );
        // A short last piece joins the one before.
        assert_eq!(
            sentences("Genau das würde ich auch machen. Danke."),
            ["Genau das würde ich auch machen. Danke."]
        );
    }

    #[test]
    fn one_sentence_and_blank_text() {
        assert_eq!(sentences("  Hola, ¿qué tal?  "), ["Hola, ¿qué tal?"]);
        assert_eq!(sentences("Hi."), ["Hi."], "a short text is still spoken");
        assert!(sentences("   ").is_empty());
    }
}
