//! Credential and secret scanner for prompt and cache safety.
//!
//! Scans code payloads for high-entropy tokens, API keys, and credential patterns,
//! redacting them safely before they are cached or sent to an LLM prompt.

use regex::Regex;
use serde::{Deserialize, Serialize};
use std::sync::OnceLock;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecretDetection {
    pub kind: &'static str,
    pub snippet: String,
    pub line_number: Option<usize>,
    pub file_path: Option<String>,
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
                "AWS Secret Access Key",
                Regex::new(r#"(?i)(?:aws_?secret(?:_?access)?(?:_?key)?|secret_?access_?key)\s*[:=]\s*["']?([A-Za-z0-9/+=]{40})["']?"#).unwrap(),
            ),
            (
                "GitHub Personal Access Token",
                Regex::new(r"gh[pousr]_[A-Za-z0-9_]{30,255}").unwrap(),
            ),
            (
                "Google API Key",
                Regex::new(r"AIza[0-9A-Za-z\-_]{30,45}").unwrap(),
            ),
            (
                "Groq API Key",
                Regex::new(r"gsk_[a-zA-Z0-9]{20,80}").unwrap(),
            ),
            (
                "OpenAI / Anthropic API Key",
                Regex::new(r"sk-(?:proj-|ant-)?[a-zA-Z0-9\-_]{20,100}").unwrap(),
            ),
            (
                "Stripe API Key",
                Regex::new(r"[sr]k_(?:live|test)_[0-9a-zA-Z]{24,}").unwrap(),
            ),
            (
                "Private Key",
                Regex::new(r"-----BEGIN (?:RSA |EC |OPENSSH |DSA |PGP )?PRIVATE KEY-----[\s\S]*?-{4,5}END (?:RSA |EC |OPENSSH |DSA |PGP )?PRIVATE KEY-----").unwrap(),
            ),
            (
                "Private Key",
                Regex::new(r"-----BEGIN (?:RSA |EC |OPENSSH |DSA |PGP )?PRIVATE KEY-----.*").unwrap(),
            ),
            (
                "Slack Token",
                Regex::new(r"xox[baprs]-[0-9a-zA-Z]{10,48}").unwrap(),
            ),
            (
                "Generic Secret / API Key Assignment",
                Regex::new(r#"(?i)\b[a-z0-9_]*(?:secret|password|passwd|passphrase|api_?key|auth_?token|access_?token|bearer_?token|master_?key|encryption_?key|private_?key|_key|_token|_secret|_pass)(?:_[a-z0-9]+)?\s*[:=]\s*(?:\\?["'])?([^"'\r\n\t\\; ]{8,})(?:\\?["'])?"#).unwrap(),
            ),
            (
                "Config Key Assignment",
                Regex::new(r#"(?i)\b(?:api_key|apikey|secret_key|secretkey|auth_token)\s*[:=]\s*(?:\\?["'])?([A-Za-z0-9_\-\.]{12,})(?:\\?["'])?"#).unwrap(),
            ),
        ]
    })
}

/// Calculate 1-indexed line number for a byte offset in a string.
fn offset_to_line(text: &str, offset: usize) -> usize {
    let prefix = &text[..offset.min(text.len())];
    prefix.lines().count().max(1)
}

/// Scan `text` for detected secrets, returning redacted text and a list of detections.
pub fn scan_and_redact(text: &str) -> (String, Vec<SecretDetection>) {
    scan_and_redact_file(None, text)
}

/// Scan `text` for detected secrets with an associated file path, returning redacted text and detections with line numbers.
pub fn scan_and_redact_file(file_path: Option<&str>, text: &str) -> (String, Vec<SecretDetection>) {
    let mut detections = Vec::new();
    let mut redacted = text.to_string();

    for (kind, re) in get_secret_patterns() {
        for mat in re.find_iter(text) {
            let snip = mat.as_str();
            if !snip.contains("[REDACTED") {
                let line_number = Some(offset_to_line(text, mat.start()));
                detections.push(SecretDetection {
                    kind,
                    snippet: snip.to_string(),
                    line_number,
                    file_path: file_path.map(|s| s.to_string()),
                });
            }
        }

        redacted = re
            .replace_all(&redacted, |caps: &regex::Captures| {
                if caps.len() > 1 {
                    // Match with capture group (e.g. key = "VALUE") -> replace VALUE
                    let full = caps.get(0).unwrap().as_str();
                    let secret_part = caps.get(1).unwrap().as_str();
                    if secret_part.contains("[REDACTED") {
                        full.to_string()
                    } else {
                        full.replace(secret_part, &format!("[REDACTED: {}]", kind))
                    }
                } else {
                    let matched_str = caps.get(0).unwrap().as_str();
                    if matched_str.contains("[REDACTED") {
                        matched_str.to_string()
                    } else {
                        format!("[REDACTED: {}]", kind)
                    }
                }
            })
            .to_string();
    }

    (redacted, detections)
}

/// Format a scan report suitable for stderr or MCP logging.
pub fn format_scan_report(detections: &[SecretDetection]) -> String {
    if detections.is_empty() {
        return "No secrets detected.".to_string();
    }

    let mut report = format!("Detected and redacted {} potential secret(s):\n", detections.len());
    for (i, d) in detections.iter().enumerate() {
        let loc = match (&d.file_path, d.line_number) {
            (Some(f), Some(l)) => format!("{}:{}", f, l),
            (Some(f), None) => f.clone(),
            (None, Some(l)) => format!("line {}", l),
            (None, None) => "unknown location".to_string(),
        };
        report.push_str(&format!("  {}. [{}] at {}\n", i + 1, d.kind, loc));
    }
    report
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
const aws_secret = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";
let password = "P@ssw0rd-NotReal-12345";
const groq_key = "gsk_1234567890abcdefghijklmnopqrstuvwxyz1234567890";
const gemini_key = "AIzaSyD-1234567890abcdefghijklmnopqrstuv";
const openai_key = "sk-proj-1234567890abcdefghijklmnopqrstuvwxyz";
let secret = 'default';
let token = 0;
fn getToken() {}
let apiKeyFromName = true;
let apiKeyFromName = "innocent_var_name";
let moniker = "keyboard_value_name";
"#;
        let (clean, detections) = scan_and_redact(code);
        assert!(detections.len() >= 8, "Expected >= 8 detections, got {}", detections.len());
        assert!(!clean.contains("AKIA1234567890ABCDEF"));
        assert!(!clean.contains("wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY"));
        assert!(!clean.contains("P@ssw0rd-NotReal-12345"));
        assert!(!clean.contains("gsk_1234567890abcdefghijklmnopqrstuvwxyz1234567890"));
        assert!(!clean.contains("AIzaSyD-1234567890abcdefghijklmnopqrstuv"));
        assert!(!clean.contains("sk-proj-1234567890abcdefghijklmnopqrstuvwxyz"));
        assert!(clean.contains("[REDACTED: AWS Access Key]"));
        assert!(clean.contains("[REDACTED: GitHub Personal Access Token]"));
        assert!(clean.contains("[REDACTED: Groq API Key]"));
        assert!(clean.contains("[REDACTED: Google API Key]"));
        assert!(clean.contains("[REDACTED: OpenAI / Anthropic API Key]"));
        assert!(clean.contains("secret = 'default'"), "Innocent short value must not be redacted");
        assert!(clean.contains("let apiKeyFromName = true;"), "Innocent identifier must not be redacted");
        assert!(clean.contains("let apiKeyFromName = \"innocent_var_name\";"), "Identifier containing 'key' but not used as a key must not be redacted");
        assert!(clean.contains("let moniker = \"keyboard_value_name\";"), "Innocent identifier containing 'key' must not be redacted");
    }

    #[test]
    fn test_pem_private_key_full_block_redaction() {
        // Multi-line PEM block with END marker must be fully redacted
        let multi_line = r#"const KEY = "-----BEGIN RSA PRIVATE KEY-----
MIIEowIBAAKCAQEA0examplebody
abc123def456ghi789
-----END RSA PRIVATE KEY-----";
"#;
        let (clean, detections) = scan_and_redact(multi_line);
        assert!(!clean.contains("MIIEowIBAAKCAQEA0examplebody"));
        assert!(!clean.contains("abc123def456ghi789"));
        assert!(!clean.contains("BEGIN RSA PRIVATE KEY"));
        assert!(!clean.contains("END RSA PRIVATE KEY"));
        assert!(clean.contains("[REDACTED: Private Key]"));
        assert!(detections.iter().any(|d| d.kind == "Private Key"));

        // Single-line PEM with body and END marker on same line
        let single_line = r#"const K = "-----BEGIN OPENSSH PRIVATE KEY-----MIIEowIBAAKCAQEA0----END OPENSSH PRIVATE KEY-----";"#;
        let (clean2, _) = scan_and_redact(single_line);
        assert!(!clean2.contains("MIIEowIBAAKCAQEA0"));
        assert!(!clean2.contains("BEGIN OPENSSH PRIVATE KEY"));

        // Truncated header (no END marker) - header + rest of line must not leak
        let truncated = r#"const PEM = "-----BEGIN EC PRIVATE KEY-----MIIEowIBAAKCAQEA0...";"#;
        let (clean3, detections3) = scan_and_redact(truncated);
        assert!(!clean3.contains("MIIEowIBAAKCAQEA0"));
        assert!(!clean3.contains("BEGIN EC PRIVATE KEY"));
        assert!(detections3.iter().any(|d| d.kind == "Private Key"));
    }

    #[test]
    fn test_scan_report_formatting() {
        let code = r#"
const gemini = "AIzaSyD-1234567890abcdefghijklmnopqrstuv";
const groq = "gsk_1234567890abcdefghijklmnopqrstuvwxyz1234567890";
"#;
        let (_, detections) = scan_and_redact_file(Some("src/keys.ts"), code);
        let report = format_scan_report(&detections);
        assert!(report.contains("Detected and redacted 2 potential secret(s)"));
        assert!(report.contains("src/keys.ts:2"));
        assert!(report.contains("src/keys.ts:3"));
    }
}
