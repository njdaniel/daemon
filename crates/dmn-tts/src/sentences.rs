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
//!
//! #122: Chinese and Japanese end sentences with `。`, `！` or `？` and no space after, so a
//! Chinese turn was one generation, and Qwen3-TTS pauses up to 0.74 s between its sentences:
//! close enough to the 0.8 s cut that one of 24 test renders lost two of its three sentences.
//! Those marks end a sentence too, and a wide character (Han, kana, Hangul) counts twice toward
//! [`MIN_CHARS`], so a ten-character Chinese sentence stands on its own.

/// A piece shorter than this many characters is not a sentence of its own.
pub(crate) const MIN_CHARS: usize = 20;

/// A full-width sentence end, which needs no space after it (#122).
fn wide_end(c: char) -> bool {
    matches!(c, '。' | '！' | '？')
}

/// Han, kana, Hangul and full-width forms: one carries about as much speech as two Latin
/// letters' worth of [`MIN_CHARS`] (#122).
fn is_wide(c: char) -> bool {
    matches!(c, '\u{3000}'..='\u{9FFF}' | '\u{AC00}'..='\u{D7AF}' | '\u{F900}'..='\u{FAFF}' | '\u{FF00}'..='\u{FFEF}')
}

/// `text`'s length toward [`MIN_CHARS`]: a wide character counts twice.
fn weight(text: &str) -> usize {
    text.chars().map(|c| if is_wide(c) { 2 } else { 1 }).sum()
}

/// Join `piece` onto `into`: with a space, or none after a full-width end.
fn join(into: &mut String, piece: &str) {
    if !into.ends_with(wide_end) {
        into.push(' ');
    }
    into.push_str(piece);
}

/// `text` as sentences, in order, each trimmed; joined (with a space, none after a full-width
/// end) they are `text` with its whitespace runs between sentences collapsed. Empty for blank
/// text.
pub(crate) fn sentences(text: &str) -> Vec<String> {
    let mut pieces: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut chars = text.trim().chars().peekable();
    while let Some(c) = chars.next() {
        current.push(c);
        let ends = wide_end(c)
            || (matches!(c, '.' | '!' | '?' | '…')
                && chars.peek().is_some_and(|n| n.is_whitespace()));
        if ends {
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
            join(&mut carry, piece);
        }
        if weight(&carry) >= MIN_CHARS {
            out.push(std::mem::take(&mut carry));
        }
    }
    if !carry.is_empty() {
        match out.last_mut() {
            Some(last) => join(last, &carry),
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

    /// #122: `。！？` end a sentence with no space after; a ten-character Chinese sentence is
    /// one of its own, a two-character one joins its neighbour with no space.
    #[test]
    fn full_width_ends_split_chinese() {
        assert_eq!(
            sentences(
                "我会先用一个消息队列来缓冲请求。然后每个服务独立扩展！最后我们用缓存降低压力？"
            ),
            [
                "我会先用一个消息队列来缓冲请求。",
                "然后每个服务独立扩展！",
                "最后我们用缓存降低压力？"
            ]
        );
        assert_eq!(
            sentences("好的。我觉得这个方案可行。"),
            ["好的。我觉得这个方案可行。"]
        );
        assert_eq!(sentences("我觉得这个方案可行。"), ["我觉得这个方案可行。"]);
    }

    #[test]
    fn one_sentence_and_blank_text() {
        assert_eq!(sentences("  Hola, ¿qué tal?  "), ["Hola, ¿qué tal?"]);
        assert_eq!(sentences("Hi."), ["Hi."], "a short text is still spoken");
        assert!(sentences("   ").is_empty());
    }
}
