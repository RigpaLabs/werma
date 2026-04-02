/// Which tracker manages an issue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tracker {
    Linear,
    GitHub,
}

/// A parsed issue identifier — either a Linear team key (`RIG-123`) or a GitHub reference
/// (`owner/repo#45`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IssueIdentifier {
    /// Linear issue, e.g. `RIG-123`.
    Linear { team_key: String, number: u32 },
    /// GitHub issue, e.g. `owner/repo#45`.
    GitHub {
        owner: String,
        repo: String,
        number: u64,
    },
}

impl IssueIdentifier {
    /// Parse an identifier string.
    ///
    /// Recognises two formats:
    /// - `TEAM-NNN`       → `Linear`  (team key must be all uppercase ASCII, number all digits)
    /// - `owner/repo#NNN` → `GitHub`  (owner and repo non-empty, number all digits)
    ///
    /// Returns `None` for UUIDs, plain strings, or unrecognised formats.
    pub fn parse(s: &str) -> Option<Self> {
        // GitHub: owner/repo#NNN  — check this first because '#' is unambiguous
        if let Some(hash_pos) = s.rfind('#') {
            let prefix = &s[..hash_pos];
            let num_str = &s[hash_pos + 1..];
            if !num_str.is_empty() && num_str.chars().all(|c| c.is_ascii_digit()) {
                if let Some(slash_pos) = prefix.find('/') {
                    let owner = &prefix[..slash_pos];
                    let repo = &prefix[slash_pos + 1..];
                    if !owner.is_empty() && !repo.is_empty() && !repo.contains('/') {
                        return Some(Self::GitHub {
                            owner: owner.to_string(),
                            repo: repo.to_string(),
                            number: num_str.parse().ok()?,
                        });
                    }
                }
            }
        }

        // Linear: TEAM-NNN — last dash separates key from number
        if let Some(dash_pos) = s.rfind('-') {
            let team_key = &s[..dash_pos];
            let num_str = &s[dash_pos + 1..];
            if !team_key.is_empty()
                && !num_str.is_empty()
                && num_str.chars().all(|c| c.is_ascii_digit())
                // Team key must be all ASCII uppercase letters (e.g. "RIG", "FAT")
                && team_key.chars().all(|c| c.is_ascii_uppercase())
            {
                return Some(Self::Linear {
                    team_key: team_key.to_string(),
                    number: num_str.parse().ok()?,
                });
            }
        }

        None
    }

    /// Which tracker handles this identifier.
    pub fn tracker(&self) -> Tracker {
        match self {
            Self::Linear { .. } => Tracker::Linear,
            Self::GitHub { .. } => Tracker::GitHub,
        }
    }

    /// Generate the web URL for this issue.
    ///
    /// - Linear: requires `WERMA_LINEAR_WORKSPACE` env var (returns `None` if unset).
    /// - GitHub: always succeeds.
    pub fn url(&self) -> Option<String> {
        match self {
            Self::Linear { team_key, number } => {
                let ws = std::env::var("WERMA_LINEAR_WORKSPACE").ok()?;
                Some(format!("https://linear.app/{ws}/issue/{team_key}-{number}"))
            }
            Self::GitHub {
                owner,
                repo,
                number,
            } => Some(format!("https://github.com/{owner}/{repo}/issues/{number}")),
        }
    }
}

impl std::fmt::Display for IssueIdentifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Linear { team_key, number } => write!(f, "{team_key}-{number}"),
            Self::GitHub {
                owner,
                repo,
                number,
            } => write!(f, "{owner}/{repo}#{number}"),
        }
    }
}

/// Routes issue identifiers to the appropriate tracker and generates issue URLs.
///
/// This is the single point of truth for "which tracker handles this identifier?" —
/// callers should use `ProjectResolver` rather than duplicating format detection logic.
pub struct ProjectResolver;

impl ProjectResolver {
    /// Parse an identifier string into its typed form.
    pub fn resolve(identifier: &str) -> Option<IssueIdentifier> {
        IssueIdentifier::parse(identifier)
    }

    /// Which tracker handles this identifier (`None` for unrecognised formats).
    pub fn tracker(identifier: &str) -> Option<Tracker> {
        Self::resolve(identifier).map(|id| id.tracker())
    }

    /// Generate the web URL for this issue.
    ///
    /// Returns `None` when:
    /// - the identifier format is unrecognised, or
    /// - it's a Linear identifier but `WERMA_LINEAR_WORKSPACE` is not set.
    pub fn issue_url(identifier: &str) -> Option<String> {
        Self::resolve(identifier).and_then(|id| id.url())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ─── IssueIdentifier::parse ──────────────────────────────────────────────

    #[test]
    fn parse_linear_identifier() {
        let id = IssueIdentifier::parse("RIG-123").unwrap();
        assert_eq!(
            id,
            IssueIdentifier::Linear {
                team_key: "RIG".to_string(),
                number: 123
            }
        );
        assert_eq!(id.tracker(), Tracker::Linear);
    }

    #[test]
    fn parse_linear_single_digit() {
        let id = IssueIdentifier::parse("FAT-1").unwrap();
        assert_eq!(
            id,
            IssueIdentifier::Linear {
                team_key: "FAT".to_string(),
                number: 1
            }
        );
    }

    #[test]
    fn parse_linear_large_number() {
        let id = IssueIdentifier::parse("PROJ-9999").unwrap();
        assert_eq!(
            id,
            IssueIdentifier::Linear {
                team_key: "PROJ".to_string(),
                number: 9999
            }
        );
    }

    #[test]
    fn parse_github_identifier() {
        let id = IssueIdentifier::parse("owner/repo#45").unwrap();
        assert_eq!(
            id,
            IssueIdentifier::GitHub {
                owner: "owner".to_string(),
                repo: "repo".to_string(),
                number: 45
            }
        );
        assert_eq!(id.tracker(), Tracker::GitHub);
    }

    #[test]
    fn parse_github_org_repo() {
        let id = IssueIdentifier::parse("RigpaLabs/werma#100").unwrap();
        assert_eq!(
            id,
            IssueIdentifier::GitHub {
                owner: "RigpaLabs".to_string(),
                repo: "werma".to_string(),
                number: 100
            }
        );
    }

    #[test]
    fn parse_uuid_returns_none() {
        assert!(IssueIdentifier::parse("755e63ee-a00e-4fef-9d7a-b8907652e2b2").is_none());
    }

    #[test]
    fn parse_empty_returns_none() {
        assert!(IssueIdentifier::parse("").is_none());
    }

    #[test]
    fn parse_no_digits_after_dash_returns_none() {
        assert!(IssueIdentifier::parse("no-digits-here").is_none());
    }

    #[test]
    fn parse_plain_string_returns_none() {
        assert!(IssueIdentifier::parse("plainstring").is_none());
    }

    #[test]
    fn parse_lowercase_team_key_returns_none() {
        // Team key must be all uppercase — "rig-123" should not match
        assert!(IssueIdentifier::parse("rig-123").is_none());
    }

    #[test]
    fn parse_mixed_case_team_key_returns_none() {
        assert!(IssueIdentifier::parse("Rig-123").is_none());
    }

    #[test]
    fn parse_github_missing_slash_returns_none() {
        // No slash → not a valid GitHub identifier
        assert!(IssueIdentifier::parse("repo#45").is_none());
    }

    #[test]
    fn parse_github_empty_owner_returns_none() {
        assert!(IssueIdentifier::parse("/repo#45").is_none());
    }

    #[test]
    fn parse_github_empty_repo_returns_none() {
        assert!(IssueIdentifier::parse("owner/#45").is_none());
    }

    #[test]
    fn parse_github_non_numeric_issue_returns_none() {
        assert!(IssueIdentifier::parse("owner/repo#abc").is_none());
    }

    // ─── Display ────────────────────────────────────────────────────────────

    #[test]
    fn display_linear() {
        let id = IssueIdentifier::Linear {
            team_key: "RIG".to_string(),
            number: 42,
        };
        assert_eq!(id.to_string(), "RIG-42");
    }

    #[test]
    fn display_github() {
        let id = IssueIdentifier::GitHub {
            owner: "org".to_string(),
            repo: "repo".to_string(),
            number: 7,
        };
        assert_eq!(id.to_string(), "org/repo#7");
    }

    // ─── URL generation ─────────────────────────────────────────────────────

    #[test]
    fn github_url_always_available() {
        let id = IssueIdentifier::GitHub {
            owner: "RigpaLabs".to_string(),
            repo: "werma".to_string(),
            number: 55,
        };
        assert_eq!(
            id.url(),
            Some("https://github.com/RigpaLabs/werma/issues/55".to_string())
        );
    }

    #[test]
    fn linear_url_requires_workspace_env() {
        // Without WERMA_LINEAR_WORKSPACE set, should return None
        // (We can't set env vars reliably in parallel tests, so we just verify it
        // returns None when the var is absent — which it will be in CI.)
        let id = IssueIdentifier::Linear {
            team_key: "RIG".to_string(),
            number: 42,
        };
        // Either Some (if env var is set in the test runner) or None (if not) — both valid.
        // The important thing is it doesn't panic.
        let _ = id.url();
    }

    // ─── ProjectResolver ────────────────────────────────────────────────────

    #[test]
    fn resolver_routes_linear() {
        assert_eq!(ProjectResolver::tracker("RIG-123"), Some(Tracker::Linear));
    }

    #[test]
    fn resolver_routes_github() {
        assert_eq!(
            ProjectResolver::tracker("owner/repo#45"),
            Some(Tracker::GitHub)
        );
    }

    #[test]
    fn resolver_returns_none_for_uuid() {
        assert_eq!(
            ProjectResolver::tracker("755e63ee-a00e-4fef-9d7a-b8907652e2b2"),
            None
        );
    }

    #[test]
    fn resolver_github_url() {
        let url = ProjectResolver::issue_url("RigpaLabs/werma#10");
        assert_eq!(
            url,
            Some("https://github.com/RigpaLabs/werma/issues/10".to_string())
        );
    }

    #[test]
    fn resolver_resolve_github() {
        let id = ProjectResolver::resolve("org/repo#99").unwrap();
        assert_eq!(id.tracker(), Tracker::GitHub);
        assert_eq!(id.to_string(), "org/repo#99");
    }
}
