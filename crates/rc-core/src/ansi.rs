//! ANSI escape handling.
//!
//! Rendered rustc diagnostics arrive colourised. The MCP surface must be
//! plain text (escape codes are pure token waste for a coding agent), while
//! the admin log viewer keeps them for rendering.

/// Strip SGR/CSI/OSC escape sequences.
pub fn strip(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        match chars.next() {
            // CSI: ESC [ ... final-byte in @..~
            Some('[') => {
                for c in chars.by_ref() {
                    if ('\u{40}'..='\u{7e}').contains(&c) {
                        break;
                    }
                }
            }
            // OSC: ESC ] ... BEL | ESC \
            Some(']') => {
                while let Some(c) = chars.next() {
                    if c == '\u{7}' {
                        break;
                    }
                    if c == '\u{1b}' && chars.peek() == Some(&'\\') {
                        chars.next();
                        break;
                    }
                }
            }
            // Two-character escapes.
            Some(_) => {}
            None => break,
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_colour_codes() {
        assert_eq!(strip("\u{1b}[31merror\u{1b}[0m: boom"), "error: boom");
    }

    #[test]
    fn strips_osc_hyperlinks() {
        assert_eq!(strip("\u{1b}]8;;http://x\u{7}link\u{1b}]8;;\u{7}"), "link");
    }

    #[test]
    fn leaves_plain_text_alone() {
        assert_eq!(strip("error[E0308]: mismatched types"), "error[E0308]: mismatched types");
    }

    #[test]
    fn survives_a_truncated_escape() {
        assert_eq!(strip("abc\u{1b}"), "abc");
        assert_eq!(strip("abc\u{1b}[31"), "abc");
    }
}
