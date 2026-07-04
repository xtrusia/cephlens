use serde_json::Value;

pub(crate) fn shell_quote(input: &str) -> String {
    let mut out = String::from("'");
    for ch in input.chars() {
        if ch == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

pub(crate) fn short(value: &str, len: usize) -> String {
    value.chars().take(len).collect()
}

pub(crate) fn clamp_top_scroll(scroll: usize, total: usize, visible: usize) -> usize {
    scroll.min(total.saturating_sub(visible.max(1)))
}

pub(crate) fn clamp_bottom_scroll(scroll: usize, total: usize, visible: usize) -> usize {
    scroll.min(total.saturating_sub(visible.max(1)))
}

pub(crate) fn ptr_str(value: &Value, path: &str) -> String {
    value
        .pointer(path)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

pub(crate) fn ptr_u64(value: &Value, path: &str) -> u64 {
    value
        .pointer(path)
        .and_then(Value::as_u64)
        .unwrap_or_default()
}

pub(crate) fn ptr_i64(value: &Value, path: &str) -> i64 {
    value
        .pointer(path)
        .and_then(Value::as_i64)
        .unwrap_or_default()
}

pub(crate) fn ptr_f64(value: &Value, path: &str) -> f64 {
    value
        .pointer(path)
        .and_then(Value::as_f64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_quote_handles_single_quotes() {
        assert_eq!(shell_quote("echo hello"), "'echo hello'");
        assert_eq!(shell_quote("printf 'x'"), "'printf '\\''x'\\'''");
    }

    #[test]
    fn scroll_clamps_to_available_rows() {
        assert_eq!(clamp_top_scroll(99, 10, 4), 6);
        assert_eq!(clamp_top_scroll(3, 2, 4), 0);
        assert_eq!(clamp_bottom_scroll(99, 10, 4), 6);
        assert_eq!(clamp_bottom_scroll(3, 0, 4), 0);
    }
}
