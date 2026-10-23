//! Parse source-label occurrences; reference eligibility is checked separately.
use crate::contracts::{Error, Result};

pub(crate) fn labels(text: &str, source_count: usize) -> Result<Vec<usize>> {
    let mut labels = Vec::new();
    let mut tail = text;
    while let Some(start) = tail.find("[S") {
        tail = &tail[start + 1..];
        let end = tail
            .find(']')
            .ok_or_else(|| Error::new("citation_invalid", "Unclosed source citation"))?;
        for member in tail[..end].split(',') {
            let index = member
                .trim_ascii()
                .strip_prefix('S')
                .filter(|raw| !raw.is_empty() && raw.bytes().all(|byte| byte.is_ascii_digit()))
                .and_then(|raw| raw.parse::<usize>().ok())
                .filter(|n| *n > 0 && *n <= source_count)
                .ok_or_else(|| {
                    Error::new(
                        "citation_invalid",
                        "Citation references an unknown evidence label",
                    )
                })?;
            labels.push(index);
        }
        tail = &tail[end + 1..];
    }
    Ok(labels)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grouped_and_separate_labels_preserve_occurrences() {
        assert_eq!(labels("Policy [S2, S1][S1].", 2).unwrap(), [2, 1, 1]);
        assert_eq!(labels("Policy [S01,\tS2].", 2).unwrap(), [1, 2]);
    }

    #[test]
    fn a_group_cannot_hide_invalid_members() {
        for text in [
            "[S1,S999]",
            "[S1,]",
            "[S1,,S2]",
            "[S1-S2]",
            "[S1,+2]",
            "[S1,S+2]",
            "[S1 S2]",
            "[S1,S0]",
            "[S1,S２]",
            "[S1",
        ] {
            assert_eq!(
                labels(text, 2).unwrap_err().code,
                "citation_invalid",
                "{text}"
            );
        }
    }
}
