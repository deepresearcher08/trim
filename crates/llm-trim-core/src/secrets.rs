//! Opt-in credential and secret scanner for prompt safety.
//!
//! Scans code payloads for high-entropy tokens, API keys, and credential patterns,
//! redacting them safely before they are sent to an LLM prompt.

use regex::Regex;
use std::sync::OnceLock;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretDetection {
    pub kind: &'static str,
    pub snippet: String,
}

static SECRET_PATTERNS: OnceLock<Vec<(&'static str, Regex)>> = OnceLock::new();

fn get_secret_patterns() -> &'static [(&'static str, Regex)] {
    SECRET_PATTERNS.get_or_init(|| {
        vec![
            (
                "AWS Access Key",
                Regex::new(r"AKIA[0-9A-Z]{16}").unwrap(),
            ),
            (
                "GitHub Personal Access Token",
                Regex::new(r"gh[pousr]_[A-Za-z0-9_]{30,255}").unwrap(),
            ),
            (
                "Private Key Header",
                Regex::new(r"-----BEGIN (?:RSA |EC |OPENSSH |DSA |PGP )?PRIVATE KEY-----").unwrap(),
            ),
            (
                "Slack Token",
                Regex::new(r"xox[baprs]-[0-9a-zA-Z]{10,48}").unwrap(),
            ),
            (
                "Generic Secret / API Key Assignment",
                Regex::new(r#"(?i)(?:api_?key|auth_?token|secret_?key|private_?key|access_?token|db_?password)\s*[:=]\s*["']([A-Za-z0-9_\-\.]{8,})["']"#).unwrap(),
            ),
        ]
    })
}

/// Scan `text` for detected secrets, returning redacted text and a list of detections.
pub fn scan_and_redact(text: &str) -> (String, Vec<SecretDetection>) {
    let mut detections = Vec::new();
    let mut redacted = text.to_string();

    for (kind, re) in get_secret_patterns() {
        for mat in re.find_iter(text) {
            detections.push(SecretDetection {
                kind,
                snippet: mat.as_str().to_string(),
            });
        }

        redacted = re
            .replace_all(&redacted, |caps: &regex::Captures| {
                if caps.len() > 1 {
                    // Match with capture group (e.g. key = "VALUE") -> replace VALUE
                    let full = caps.get(0).unwrap().as_str();
                    let secret_part = caps.get(1).unwrap().as_str();
                    full.replace(secret_part, &format!("[REDACTED_SECRET: {}]", kind))
                } else {
                    format!("[REDACTED_SECRET: {}]", kind)
                }
            })
            .to_string();
    }

    (redacted, detections)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_secret_redaction() {
        let code = r#"
const AWS_KEY = "AKIA1234567890ABCDEF";
const github_token = "ghp_1234567890abcdefghijklmnopqrstuvwx";
let apiKey = "secret_key_abcdef1234567890";
"#;
        let (clean, detections) = scan_and_redact(code);
        assert!(detections.len() >= 3, "Expected >= 3 detections, got {}", detections.len());
        assert!(!clean.contains("AKIA1234567890ABCDEF"));
        assert!(clean.contains("[REDACTED_SECRET: AWS Access Key]"));
        assert!(clean.contains("[REDACTED_SECRET: GitHub Personal Access Token]"));
    }
}
