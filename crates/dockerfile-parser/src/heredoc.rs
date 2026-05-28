//! HEREDOC support for Dockerfile parser.

use super::parse::{ParseError, Result};

/// Check if a line starts a HEREDOC operator (<< or <<-).
pub fn is_heredoc_start(line: &str) -> bool {
    line.trim().starts_with("<<") && !line.trim().starts_with("<<<")
}

/// Extract the heredoc delimiter from a line like "<<EOF" or "<<'EOF'".
pub fn extract_heredoc_delimiter(line: &str) -> String {
    let trimmed = line.trim();
    let start_pos = trimmed.find("<<").unwrap() + 2;
    let delimiter_part = &trimmed[start_pos..];
    
    // Handle quoted delimiters like <<'EOF' or <<"EOF"
    let delimiter = delimiter_part.trim();
    if delimiter.starts_with('"') || delimiter.starts_with('\'') {
        let quote_char = delimiter.chars().next().unwrap();
        delimiter[1..].split(quote_char).next().unwrap_or("").to_string()
    } else {
        delimiter.split_whitespace().next().unwrap_or("").to_string()
    }
}

/// Parse HEREDOC content from lines.
pub fn parse_heredoc_content(lines: &[&str], start_line: usize, delimiter: &str) -> Result<(String, usize)> {
    let mut content = String::new();
    let mut line_idx = start_line;
    
    while line_idx < lines.len() {
        let line = lines[line_idx];
        // Check if line exactly matches the delimiter (no leading/trailing whitespace)
        if line.trim_end() == delimiter {
            break;
        }
        content.push_str(line);
        content.push('\n');
        line_idx += 1;
    }
    
    // Remove the trailing newline we added
    if content.ends_with('\n') {
        content.pop();
    }
    
    if line_idx >= lines.len() {
        return Err(ParseError::Syntax {
            line: start_line,
            message: format!("HEREDOC delimiter '{}' not found", delimiter),
        });
    }
    
    Ok((content, line_idx))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_heredoc_start() {
        assert!(is_heredoc_start("<<EOF"));
        assert!(is_heredoc_start("<<'SCRIPT'"));
        assert!(!is_heredoc_start("echo <<"));
        assert!(!is_heredoc_start("echo <<<"));
    }

    #[test]
    fn test_extract_heredoc_delimiter() {
        assert_eq!(extract_heredoc_delimiter("RUN <<EOF"), "EOF");
        assert_eq!(extract_heredoc_delimiter("RUN <<'SCRIPT'"), "SCRIPT");
        assert_eq!(extract_heredoc_delimiter("RUN <<\"CONFIG\""), "CONFIG");
    }

    #[test]
    fn test_parse_heredoc_content() {
        let lines = vec![
            "echo \"Hello World\"",
            "echo \"Line 2\"",
            "EOF"
        ];
        let (content, end_line) = parse_heredoc_content(&lines, 0, "EOF").unwrap();
        assert_eq!(content, "echo \"Hello World\"\necho \"Line 2\"");
        assert_eq!(end_line, 2);
    }
}