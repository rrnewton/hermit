//! Internal text-contract helpers.

/// Whether a character belongs to the whitespace set used by trimming and quoting.
pub(crate) fn is_whitespace(character: char) -> bool {
    character.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&character)
}

/// Trim the full contract whitespace set from both ends.
pub(crate) fn trim(value: &str) -> &str {
    value.trim_matches(is_whitespace)
}

/// Render a string using the stable single/double-quoted diagnostic convention.
pub(crate) fn string_repr(value: &str) -> String {
    let quote = if value.contains('\'') && !value.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut rendered = String::with_capacity(value.len() + 2);
    rendered.push(quote);
    for character in value.chars() {
        match character {
            '\\' => rendered.push_str("\\\\"),
            '\n' => rendered.push_str("\\n"),
            '\r' => rendered.push_str("\\r"),
            '\t' => rendered.push_str("\\t"),
            character if character == quote => {
                rendered.push('\\');
                rendered.push(character);
            }
            character
                if character.is_control() || (character != ' ' && character.is_whitespace()) =>
            {
                let scalar = character as u32;
                if scalar <= 0xff {
                    rendered.push_str(&format!("\\x{scalar:02x}"));
                } else if scalar <= 0xffff {
                    rendered.push_str(&format!("\\u{scalar:04x}"));
                } else {
                    rendered.push_str(&format!("\\U{scalar:08x}"));
                }
            }
            character => rendered.push(character),
        }
    }
    rendered.push(quote);
    rendered
}

/// Split all Unicode line boundaries recognized by the line-oriented contract.
pub(crate) fn split_lines(value: &str) -> impl Iterator<Item = &str> {
    value.split(|character| {
        matches!(
            character,
            '\n' | '\r'
                | '\u{000b}'
                | '\u{000c}'
                | '\u{001c}'
                | '\u{001d}'
                | '\u{001e}'
                | '\u{0085}'
                | '\u{2028}'
                | '\u{2029}'
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chooses_and_escapes_quotes_and_controls() {
        assert_eq!(string_repr("simple"), "'simple'");
        assert_eq!(string_repr("it's"), "\"it's\"");
        assert_eq!(string_repr("both'\""), "'both\\'\"'");
        assert_eq!(string_repr("a\n\\b"), "'a\\n\\\\b'");
    }

    #[test]
    fn splits_ascii_and_unicode_line_boundaries() {
        assert_eq!(
            split_lines("a\r\nb\u{b}c\u{85}d\u{2028}e").collect::<Vec<_>>(),
            ["a", "", "b", "c", "d", "e"]
        );
    }

    #[test]
    fn trimming_includes_ascii_information_separators() {
        assert_eq!(trim("\u{1c} value \u{1f}"), "value");
    }
}
