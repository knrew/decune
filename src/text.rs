pub(crate) fn non_empty_trimmed(value: &str) -> Option<&str> {
    let value = value.trim();
    if value.is_empty() { None } else { Some(value) }
}

#[cfg(test)]
mod tests {
    use super::non_empty_trimmed;

    #[test]
    fn trims_non_empty_values_and_rejects_empty_values() {
        assert_eq!(non_empty_trimmed("  value  "), Some("value"));
        assert_eq!(non_empty_trimmed(""), None);
        assert_eq!(non_empty_trimmed(" \t\n "), None);
    }
}
