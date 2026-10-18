use std::sync::atomic::{AtomicUsize, Ordering};

use regex::Regex;
use url::Url;

pub(crate) const MAX_MESSAGE_BYTES: usize = 64 * 1024;
pub(crate) const MAX_URL_BYTES: usize = 8 * 1024;
const MAX_MATCHES: usize = 32;

#[derive(Default)]
pub(crate) struct Counters {
    pub(crate) oversized: AtomicUsize,
    pub(crate) invalid_urls: AtomicUsize,
    pub(crate) limited: AtomicUsize,
    pub(crate) duplicates: AtomicUsize,
    pub(crate) encrypted_discarded: AtomicUsize,
}

impl Counters {
    pub(crate) fn report(&self) {
        for (reason, counter) in [
            ("oversized messages", &self.oversized),
            ("invalid or oversized URLs", &self.invalid_urls),
            ("messages exceeding the URL match limit", &self.limited),
            (
                "encrypted events expired, oversized or discarded",
                &self.encrypted_discarded,
            ),
        ] {
            let count = counter.swap(0, Ordering::Relaxed);
            if count > 0 {
                tracing::error!(count, reason, "Harvest input discarded");
            }
        }
        let count = self.duplicates.swap(0, Ordering::Relaxed);
        if count > 0 {
            tracing::debug!(count, "Ignored repeated Matrix event IDs");
        }
    }
}

pub(crate) fn extract_urls<'a>(regex: &Regex, text: &'a str, counters: &Counters) -> Vec<&'a str> {
    let mut urls = Vec::new();
    for (index, capture) in regex.captures_iter(text).enumerate() {
        if index == MAX_MATCHES {
            counters.limited.fetch_add(1, Ordering::Relaxed);
            break;
        }
        let Some(value) = capture.get(1).map(|value| value.as_str()) else {
            continue;
        };
        if value.len() > MAX_URL_BYTES
            || value
                .chars()
                .any(|ch| ch.is_control() || ch.is_whitespace())
            || !Url::parse(value).is_ok_and(|url| {
                matches!(url.scheme(), "http" | "https") && url.host_str().is_some()
            })
        {
            counters.invalid_urls.fetch_add(1, Ordering::Relaxed);
            continue;
        }
        // Matrix bodies are plain text: no Slack label/entity decoding, and no
        // rewriting query strings or Unicode in the stored URL.
        urls.push(value);
    }
    urls
}

/// Prevent control sequences and multiline log injection; never split UTF-8.
pub(crate) fn log_text(text: &str, max_bytes: usize) -> String {
    let mut result = String::new();
    for ch in text.chars() {
        let ch = if ch.is_control() || ch.is_whitespace() {
            ' '
        } else {
            ch
        };
        if result.len() + ch.len_utf8() > max_bytes {
            break;
        }
        result.push(ch);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matrix_urls_remain_exact_and_invalid_captures_are_skipped() {
        let regex = Regex::new("<([^>]+)>").unwrap();
        let counters = Counters::default();
        assert_eq!(
            extract_urls(
                &regex,
                "<https://example.org/é?a=1&amp;b=2> <file:///etc/passwd> <https://?> <https://example.org/a\nb>",
                &counters
            ),
            ["https://example.org/é?a=1&amp;b=2"]
        );
        assert_eq!(counters.invalid_urls.load(Ordering::Relaxed), 3);
        assert!(extract_urls(&Regex::new("(https)?www").unwrap(), "www", &counters).is_empty());
        assert!(extract_urls(&Regex::new("()").unwrap(), "x", &counters).is_empty());
    }

    #[test]
    fn match_work_and_url_lengths_are_bounded_without_truncation() {
        let regex = Regex::new("<(https?://[^>]+)>").unwrap();
        let counters = Counters::default();
        let prefix = "https://example.org/";
        let exact = format!("{prefix}{}", "a".repeat(MAX_URL_BYTES - prefix.len()));
        assert_eq!(
            extract_urls(&regex, &format!("<{exact}>"), &counters),
            [exact.as_str()]
        );
        assert!(extract_urls(&regex, &format!("<{exact}a>"), &counters).is_empty());
        assert_eq!(
            extract_urls(&regex, &"<https://example.org/> ".repeat(100), &counters).len(),
            MAX_MATCHES
        );
        assert_eq!(counters.limited.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn console_text_is_single_line_bounded_and_unicode_safe() {
        assert_eq!(log_text("a\r\n\u{1b}b", 20), "a   b");
        assert_eq!(log_text("ééé", 5), "éé");
    }
}
