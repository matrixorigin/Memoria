/// Lightweight regex-based entity extraction — mirrors Python's entity_extractor.py
/// No LLM, no network. Runs on every memory_store automatically.

/// Known tech terms (lowercase) that are common English words needing explicit listing.
const TECH_TERMS: &[&str] = &[
    "python", "rust", "go", "java", "ruby", "swift",
    "flask", "spring", "express", "gin", "lambda",
    "terraform", "ansible", "docker", "git", "ruff", "black", "jest", "mocha",
    "k8s", "aws", "gcp", "s3", "ec2", "ecs", "eks",
];

/// Service/component suffixes that make a hyphenated name a "project" entity.
const SERVICE_SUFFIXES: &[&str] = &[
    "service", "server", "api", "gateway", "proxy", "mesh",
    "pipeline", "worker", "queue", "cache", "store", "db",
    "manager", "controller", "handler", "client", "sdk", "cli", "ui", "app",
];

/// Common English words to exclude from capitalized-word extraction.
const COMMON_ENGLISH: &[&str] = &[
    "the", "a", "an", "is", "are", "was", "were", "be", "been", "being",
    "have", "has", "had", "do", "does", "did", "will", "would", "could",
    "should", "may", "might", "shall", "can", "need", "must", "let",
    "we", "i", "you", "he", "she", "it", "they", "my", "our", "your",
    "this", "that", "these", "those", "if", "then", "else", "when",
    "where", "how", "what", "which", "who", "why", "not", "no", "yes",
    "all", "each", "every", "both", "few", "more", "most", "other",
    "some", "such", "only", "same", "so", "than", "too", "very", "just",
    "but", "and", "or", "nor", "for", "yet", "after", "before", "since",
    "while", "about", "above", "below", "to", "from", "up", "down",
    "in", "out", "on", "off", "over", "under", "again", "once",
    "here", "there", "any", "new", "old", "also", "back", "now", "well",
    "way", "use", "note", "see", "check", "run", "try", "make", "sure",
    "first", "next", "last", "step", "error", "warning", "info",
    "please", "thanks", "monday", "tuesday", "wednesday", "thursday",
    "friday", "saturday", "sunday",
    "january", "february", "march", "april", "june", "july", "august",
    "september", "october", "november", "december",
];

#[derive(Debug, Clone)]
pub struct ExtractedEntity {
    pub name: String,       // canonical lowercase
    pub display: String,    // original casing
    pub entity_type: String, // tech, person, repo, project, concept
}

pub fn extract_entities(text: &str) -> Vec<ExtractedEntity> {
    let mut seen = std::collections::HashSet::new();
    let mut entities = Vec::new();

    let mut add = |name: &str, display: &str, etype: &str| {
        let key = name.to_lowercase();
        if key.len() >= 2 && seen.insert(key.clone()) {
            entities.push(ExtractedEntity {
                name: key,
                display: display.to_string(),
                entity_type: etype.to_string(),
            });
        }
    };

    // 1. Known tech terms
    let text_lower = text.to_lowercase();
    for term in TECH_TERMS {
        // word boundary check
        if let Some(pos) = text_lower.find(term) {
            let before_ok = pos == 0 || !text_lower.as_bytes()[pos - 1].is_ascii_alphanumeric();
            let after_pos = pos + term.len();
            let after_ok = after_pos >= text_lower.len() || !text_lower.as_bytes()[after_pos].is_ascii_alphanumeric();
            if before_ok && after_ok {
                add(term, term, "tech");
            }
        }
    }

    // 2. Capitalized words (likely tech proper nouns): CamelCase or Title Case
    // Pattern: word starting with uppercase, 2+ chars
    let words: Vec<&str> = text.split_whitespace().collect();
    for word in &words {
        // Strip trailing punctuation
        let w = word.trim_end_matches(|c: char| !c.is_alphanumeric());
        if w.len() < 2 { continue; }
        let first = w.chars().next().unwrap_or_default();
        if !first.is_uppercase() { continue; }
        let lower = w.to_lowercase();
        if COMMON_ENGLISH.contains(&lower.as_str()) { continue; }
        if TECH_TERMS.contains(&lower.as_str()) { continue; } // already added
        // Must have at least one lowercase letter (not pure acronym like "THE")
        if w.chars().all(|c| c.is_uppercase() || !c.is_alphabetic()) {
            // Pure acronym: 2-8 chars
            if w.len() >= 2 && w.len() <= 8 && w.chars().all(|c| c.is_ascii_uppercase()) {
                add(&lower, w, "tech");
            }
            continue;
        }
        add(&lower, w, "tech");
    }

    // 3. @mentions
    let mut i = 0;
    let bytes = text.as_bytes();
    while i < bytes.len() {
        if bytes[i] == b'@' {
            let start = i + 1;
            let end = text[start..].find(|c: char| !c.is_alphanumeric() && c != '_' && c != '-' && c != '.')
                .map(|n| start + n)
                .unwrap_or(text.len());
            if end > start {
                let name = &text[start..end];
                add(&name.to_lowercase(), name, "person");
            }
        }
        i += 1;
    }

    // 4. owner/repo patterns (word/word)
    let repo_re_simple = text.split_whitespace().filter(|w| {
        let parts: Vec<&str> = w.split('/').collect();
        parts.len() == 2 && parts[0].len() >= 2 && parts[1].len() >= 2
            && parts[0].chars().all(|c| c.is_alphanumeric() || c == '-' || c == '_' || c == '.')
            && parts[1].chars().all(|c| c.is_alphanumeric() || c == '-' || c == '_' || c == '.')
    });
    for repo in repo_re_simple {
        let clean = repo.trim_end_matches(|c: char| !c.is_alphanumeric() && c != '/');
        add(&clean.to_lowercase(), clean, "repo");
    }

    // 5. CamelCase identifiers (2+ humps)
    for word in &words {
        let w = word.trim_end_matches(|c: char| !c.is_alphanumeric());
        if w.len() < 4 { continue; }
        // Must have at least 2 uppercase letters not at start
        let upper_count = w.chars().skip(1).filter(|c| c.is_uppercase()).count();
        if upper_count >= 1 && w.chars().next().map(|c| c.is_uppercase()).unwrap_or(false) {
            let lower = w.to_lowercase();
            if !COMMON_ENGLISH.contains(&lower.as_str()) {
                add(&lower, w, "project");
            }
        }
    }

    // 6. Hyphenated service names (auth-service, payment-api, etc.)
    for word in &words {
        let w = word.trim_end_matches(|c: char| !c.is_alphanumeric() && c != '-');
        if !w.contains('-') { continue; }
        let parts: Vec<&str> = w.split('-').collect();
        if parts.len() < 2 { continue; }
        if parts.iter().any(|p| SERVICE_SUFFIXES.contains(p)) {
            add(&w.to_lowercase(), w, "project");
        }
    }

    // 7. Backtick terms `like-this`
    let mut rest = text;
    while let Some(start) = rest.find('`') {
        rest = &rest[start + 1..];
        if let Some(end) = rest.find('`') {
            let term = &rest[..end];
            if term.len() >= 2 && term.len() <= 30 {
                add(&term.to_lowercase(), term, "project");
            }
            rest = &rest[end + 1..];
        } else {
            break;
        }
    }

    entities
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_known_tech_terms() {
        let e = extract_entities("We use docker and rust for this project");
        let names: Vec<&str> = e.iter().map(|x| x.name.as_str()).collect();
        assert!(names.contains(&"docker"), "{names:?}");
        assert!(names.contains(&"rust"), "{names:?}");
    }

    #[test]
    fn test_capitalized_proper_noun() {
        let e = extract_entities("MatrixOne is a distributed database");
        let names: Vec<&str> = e.iter().map(|x| x.name.as_str()).collect();
        assert!(names.contains(&"matrixone"), "{names:?}");
    }

    #[test]
    fn test_at_mention() {
        let e = extract_entities("Ask @alice about this");
        let names: Vec<&str> = e.iter().map(|x| x.name.as_str()).collect();
        assert!(names.contains(&"alice"), "{names:?}");
    }

    #[test]
    fn test_repo_pattern() {
        let e = extract_entities("See matrixorigin/matrixone for details");
        let names: Vec<&str> = e.iter().map(|x| x.name.as_str()).collect();
        assert!(names.contains(&"matrixorigin/matrixone"), "{names:?}");
    }

    #[test]
    fn test_service_name() {
        let e = extract_entities("The auth-service handles login");
        let names: Vec<&str> = e.iter().map(|x| x.name.as_str()).collect();
        assert!(names.contains(&"auth-service"), "{names:?}");
    }

    #[test]
    fn test_backtick() {
        let e = extract_entities("Run `cargo test` to verify");
        let names: Vec<&str> = e.iter().map(|x| x.name.as_str()).collect();
        assert!(names.contains(&"cargo test"), "{names:?}");
    }
}
