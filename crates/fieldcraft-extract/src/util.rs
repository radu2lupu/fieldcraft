/// Extract the first `{...}` JSON object from a string, handling nested braces.
pub fn extract_json_object(s: &str) -> Option<&str> {
    let start = s.find('{')?;
    let mut depth = 0i32;
    let mut end = None;
    for (i, ch) in s[start..].char_indices() {
        match ch {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    end = Some(start + i);
                    break;
                }
            }
            _ => {}
        }
    }
    end.map(|e| &s[start..=e])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_simple() {
        let s = r#"{"key": "value"}"#;
        assert_eq!(extract_json_object(s), Some(r#"{"key": "value"}"#));
    }

    #[test]
    fn test_extract_nested() {
        let s = r#"Sure! {"outer": {"inner": "value"}} done"#;
        assert_eq!(
            extract_json_object(s),
            Some(r#"{"outer": {"inner": "value"}}"#)
        );
    }

    #[test]
    fn test_extract_no_json() {
        assert_eq!(extract_json_object("no json here"), None);
    }

    #[test]
    fn test_extract_with_prefix() {
        let s = r#"Here is the result: {"a": 1}"#;
        assert_eq!(extract_json_object(s), Some(r#"{"a": 1}"#));
    }
}
