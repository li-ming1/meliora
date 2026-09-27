//! Text normalization shared by metadata tagging and library artist matching.

/// Normalizes a person name for identity comparison: lowercased, split on
/// non-alphanumerics, tokens sorted, joined with single spaces. Both the
/// scanner's artist matcher and the tag writer's artist grouping key off it,
/// so the two sides agree on what "the same artist" means.
pub fn token_key(value: &str) -> String {
    let lowered = value.to_lowercase();
    let mut tokens: Vec<&str> = lowered
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .collect();
    tokens.sort_unstable();
    tokens.join(" ")
}
