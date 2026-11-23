//! Text that came from the remote host or a file, made safe to print on a terminal.

/// `text` without terminal control sequences: C0 and C1 control characters other than `\n` and
/// `\t` (ESC starts title changes and clipboard writes), and the invisible format characters that
/// hide or reorder text on screen (soft hyphen, zero-width and bidirectional marks and overrides,
/// line and paragraph separators, word joiners, invisible operators, the byte order mark). Dropped
/// characters are replaced by `?` so the loss is visible.
pub fn sanitize(text: &str) -> String {
    text.chars()
        .map(|c| if is_unsafe(c) { '?' } else { c })
        .collect()
}

fn is_unsafe(c: char) -> bool {
    match c {
        '\n' | '\t' => false,
        '\u{00AD}'
        | '\u{061C}'
        | '\u{200B}'..='\u{200F}'
        | '\u{2028}'..='\u{202E}'
        | '\u{2060}'..='\u{206F}'
        | '\u{FEFF}' => true,
        c => c.is_control(),
    }
}

/// The line `main` prints for a failed run: every message may embed remote or file text, so it
/// passes through [`sanitize`] whole.
pub fn error_line(err: &anyhow::Error) -> String {
    format!("rr: {}", sanitize(&format!("{err:#}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_text_and_drops_control_sequences() {
        assert_eq!(sanitize("plain\ttext\nnext"), "plain\ttext\nnext");
        assert_eq!(sanitize("a\u{1b}]0;title\u{7}b"), "a?]0;title?b");
        assert_eq!(sanitize("x\u{9b}31my"), "x?31my");
        assert_eq!(sanitize("fi\u{202E}gnp.exe"), "fi?gnp.exe");
        assert_eq!(sanitize("naïve — ok"), "naïve — ok");
    }

    #[test]
    fn invisible_format_characters_cannot_hide_text() {
        for c in [
            '\u{00AD}', '\u{061C}', '\u{200B}', '\u{200C}', '\u{200D}', '\u{200E}', '\u{200F}',
            '\u{2028}', '\u{2029}', '\u{202A}', '\u{202E}', '\u{2060}', '\u{2064}', '\u{2066}',
            '\u{2069}', '\u{206F}', '\u{FEFF}',
        ] {
            assert_eq!(sanitize(&format!("a{c}b")), "a?b", "{:04X}", u32::from(c));
        }
        // neighbours of the ranges stay: en dash, a combining mark, the replacement character
        assert_eq!(
            sanitize("\u{2013}\u{0301}\u{FFFD}"),
            "\u{2013}\u{0301}\u{FFFD}"
        );
    }

    #[test]
    fn the_printed_error_chain_is_sanitized() {
        let err =
            anyhow::anyhow!("remote said \u{1b}[2J hidden\u{202E}gnp").context("sacct failed\u{7}");
        assert_eq!(
            error_line(&err),
            "rr: sacct failed?: remote said ?[2J hidden?gnp"
        );
    }
}
