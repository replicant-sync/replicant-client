//! How `rebase` merges a list both sides changed, chosen per list path by the host.

/// Merge policy for a list both sides changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListMergePolicy {
    /// Any change from both sides collides at the list: the server's list is kept.
    Atomic,
    /// Element by element while positions line up on both sides (neither side resized the
    /// list, or one side only appended and the other kept the length); otherwise as `Atomic`.
    Append,
    /// Content-aware merge. Not supported yet: `Engine::start` refuses it.
    Full,
}

/// A JSON Pointer whose `*` segments match any one object key or array index, e.g.
/// `/tunings/*/pitches`. Literal segments are written escaped (`~0`, `~1`), as in a pointer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathPattern(pub String);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListMergeConfig {
    pub default: ListMergePolicy,
    /// For a list, the matching rule with the most literal segments wins; a tie goes to the
    /// rule listed first. No match: `default`.
    pub rules: Vec<(PathPattern, ListMergePolicy)>,
}

impl Default for ListMergeConfig {
    fn default() -> Self {
        ListMergeConfig {
            default: ListMergePolicy::Append,
            rules: Vec::new(),
        }
    }
}

impl ListMergeConfig {
    /// Refuses a policy the engine cannot apply and any malformed pattern.
    pub fn validate(&self) -> Result<(), String> {
        let mut policies =
            std::iter::once(self.default).chain(self.rules.iter().map(|(_, policy)| *policy));
        if policies.any(|policy| policy == ListMergePolicy::Full) {
            return Err("Full list merge is not supported yet".to_string());
        }
        self.rules
            .iter()
            .try_for_each(|(pattern, _)| pattern.validate())
    }

    /// The policy for the list at `list_path`, a JSON Pointer.
    pub fn policy_for(&self, list_path: &str) -> ListMergePolicy {
        let mut best: Option<(usize, ListMergePolicy)> = None;
        for (pattern, policy) in &self.rules {
            let Some(literal) = pattern.literal_segments_matching(list_path) else {
                continue;
            };
            match best {
                Some((most, _)) if most >= literal => {}
                _ => best = Some((literal, *policy)),
            }
        }
        best.map_or(self.default, |(_, policy)| policy)
    }
}

impl PathPattern {
    fn segments(&self) -> impl Iterator<Item = &str> {
        self.0.split('/').skip(1)
    }

    fn validate(&self) -> Result<(), String> {
        let pattern = &self.0;
        if !pattern.is_empty() && !pattern.starts_with('/') {
            return Err(format!(
                "list merge pattern {pattern:?} must be empty or start with '/'"
            ));
        }
        for segment in self.segments() {
            if segment.contains('*') && segment != "*" {
                return Err(format!(
                    "list merge pattern {pattern:?}: '*' must be a whole segment"
                ));
            }
            if !escapes_are_valid(segment) {
                return Err(format!(
                    "list merge pattern {pattern:?}: '~' must be followed by 0 or 1"
                ));
            }
        }
        Ok(())
    }

    /// How many literal segments match `path` when the whole pattern matches it.
    fn literal_segments_matching(&self, path: &str) -> Option<usize> {
        let pattern: Vec<&str> = self.segments().collect();
        let tokens: Vec<&str> = path.split('/').skip(1).collect();
        if pattern.len() != tokens.len() {
            return None;
        }
        let mut literal = 0;
        for (wanted, token) in pattern.iter().zip(&tokens) {
            if *wanted == "*" {
                continue;
            }
            if wanted != token {
                return None;
            }
            literal += 1;
        }
        Some(literal)
    }
}

fn escapes_are_valid(segment: &str) -> bool {
    let mut chars = segment.chars();
    while let Some(c) = chars.next() {
        if c == '~' && !matches!(chars.next(), Some('0' | '1')) {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use ListMergePolicy::{Append, Atomic, Full};

    fn lists(default: ListMergePolicy, rules: &[(&str, ListMergePolicy)]) -> ListMergeConfig {
        ListMergeConfig {
            default,
            rules: rules
                .iter()
                .map(|(pattern, policy)| (PathPattern(pattern.to_string()), *policy))
                .collect(),
        }
    }

    #[test]
    fn the_default_is_append_with_no_rules() {
        let config = ListMergeConfig::default();
        assert_eq!(config, lists(Append, &[]));
        assert_eq!(config.policy_for("/pitches"), Append);
    }

    #[test]
    fn a_more_specific_rule_beats_a_wildcard_one_in_either_order() {
        for rules in [
            [
                ("/tunings/*/pitches", Append),
                ("/tunings/0/pitches", Atomic),
            ],
            [
                ("/tunings/0/pitches", Atomic),
                ("/tunings/*/pitches", Append),
            ],
        ] {
            let config = lists(Append, &rules);
            assert_eq!(config.policy_for("/tunings/0/pitches"), Atomic);
            assert_eq!(config.policy_for("/tunings/1/pitches"), Append);
        }
    }

    #[test]
    fn a_tie_goes_to_the_rule_listed_first() {
        assert_eq!(
            lists(Append, &[("/*/0", Atomic), ("/a/*", Append)]).policy_for("/a/0"),
            Atomic
        );
        assert_eq!(
            lists(Atomic, &[("/a/*", Append), ("/*/0", Atomic)]).policy_for("/a/0"),
            Append
        );
    }

    #[test]
    fn a_wildcard_matches_exactly_one_key_or_index() {
        let config = lists(Append, &[("/tunings/*", Atomic)]);
        assert_eq!(config.policy_for("/tunings/0"), Atomic);
        assert_eq!(config.policy_for("/tunings/names"), Atomic);
        assert_eq!(config.policy_for("/tunings"), Append);
        assert_eq!(config.policy_for("/tunings/0/pitches"), Append);
    }

    #[test]
    fn full_is_refused_as_the_default_or_in_a_rule() {
        let refused = Err("Full list merge is not supported yet".to_string());
        assert_eq!(lists(Full, &[]).validate(), refused);
        assert_eq!(lists(Append, &[("/pitches", Full)]).validate(), refused);
        assert_eq!(lists(Atomic, &[("/pitches", Append)]).validate(), Ok(()));
    }

    #[test]
    fn malformed_patterns_are_refused() {
        for bad in ["pitches", "/pit*ches", "/a/~2", "/a~"] {
            assert!(
                lists(Append, &[(bad, Atomic)]).validate().is_err(),
                "{bad:?} should be refused"
            );
        }
        for good in ["", "/pitches", "/tunings/*/pitches", "/a~1b", "/*"] {
            assert_eq!(
                lists(Append, &[(good, Atomic)]).validate(),
                Ok(()),
                "{good:?}"
            );
        }
    }
}
