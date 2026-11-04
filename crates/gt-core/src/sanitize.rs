pub fn sanitize_log(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_control() { '?' } else { c })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_text_passes_through() {
        assert_eq!(sanitize_log("hello world 123"), "hello world 123");
        assert_eq!(sanitize_log(""), "");
    }

    #[test]
    fn unicode_is_preserved() {
        assert_eq!(sanitize_log("h\u{e9}llo \u{1f980}"), "h\u{e9}llo \u{1f980}");
    }

    #[test]
    fn ansi_escapes_are_neutralized() {
        assert_eq!(sanitize_log("\u{1b}[2Jwiped"), "?[2Jwiped");
        assert_eq!(sanitize_log("x\u{1b}]0;title\u{07}y"), "x?]0;title?y");
    }

    #[test]
    fn newlines_and_carriage_returns_are_flattened() {
        assert_eq!(sanitize_log("a\nb\rc\td"), "a?b?c?d");
    }

    #[test]
    fn del_and_c1_controls_are_neutralized() {
        assert_eq!(sanitize_log("a\u{7f}b\u{85}c"), "a?b?c");
    }
}
