//! The Actions environment: service endpoints, tokens, and cache scopes.

use anyhow::{Context, bail};

/// `ACTIONS_CACHE_MODE`, exported by newer runners.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CacheMode {
    ReadWrite,
    ReadOnly,
    WriteOnly,
    None,
}

impl CacheMode {
    pub fn parse(s: &str) -> CacheMode {
        match s.trim().to_ascii_lowercase().as_str() {
            "read" => CacheMode::ReadOnly,
            "write-only" => CacheMode::WriteOnly,
            "none" => CacheMode::None,
            // Unset or unknown modes are permissive, as in @actions/cache.
            _ => CacheMode::ReadWrite,
        }
    }

    pub fn writable(self) -> bool {
        matches!(self, CacheMode::ReadWrite | CacheMode::WriteOnly)
    }
}

#[derive(Clone)]
pub struct Env {
    pub results_url: String,
    pub runtime_token: String,
    pub cache_mode: CacheMode,
    pub github_token: Option<String>,
    pub api_url: String,
    /// `owner/repo`.
    pub repository: String,
    /// The ref whose cache scope this run writes, e.g. `refs/pull/7/merge`.
    pub git_ref: String,
    /// For pull requests, the base branch name.
    pub base_ref: Option<String>,
    /// From the event payload when present; otherwise looked up via REST.
    pub default_branch: Option<String>,
}

impl std::fmt::Debug for Env {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Env")
            .field("results_url", &self.results_url)
            .field("cache_mode", &self.cache_mode)
            .field("github_token", &self.github_token.as_ref().map(|_| "<set>"))
            .field("api_url", &self.api_url)
            .field("repository", &self.repository)
            .field("git_ref", &self.git_ref)
            .field("base_ref", &self.base_ref)
            .field("default_branch", &self.default_branch)
            .finish()
    }
}

fn var(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

impl Env {
    /// Reads the environment of an Actions job. `token_var` names the
    /// variable holding the REST token (normally `GITHUB_TOKEN`).
    pub fn from_process(token_var: &str) -> anyhow::Result<Env> {
        let results_url = var("ACTIONS_RESULTS_URL").context(
            "ACTIONS_RESULTS_URL is not set. The cache service is only reachable from \
             inside a GitHub Actions job, and runner tokens are only exported to actions \
             (use the mount action, or export them with actions/github-script)",
        )?;
        let runtime_token =
            var("ACTIONS_RUNTIME_TOKEN").context("ACTIONS_RUNTIME_TOKEN is not set")?;
        let repository = var("GITHUB_REPOSITORY").context("GITHUB_REPOSITORY is not set")?;
        let git_ref = var("GITHUB_REF").context("GITHUB_REF is not set")?;
        let default_branch = var("GITHUB_EVENT_PATH").and_then(|p| {
            let text = std::fs::read_to_string(p).ok()?;
            let json: serde_json::Value = serde_json::from_str(&text).ok()?;
            json.pointer("/repository/default_branch")?
                .as_str()
                .map(str::to_string)
        });
        Ok(Env {
            results_url,
            runtime_token,
            cache_mode: CacheMode::parse(&var("ACTIONS_CACHE_MODE").unwrap_or_default()),
            github_token: var(token_var),
            api_url: var("GITHUB_API_URL").unwrap_or_else(|| "https://api.github.com".into()),
            repository,
            git_ref,
            base_ref: var("GITHUB_BASE_REF"),
            default_branch,
        })
    }

    /// The refs whose cache entries this run can read, highest precedence first.
    pub fn scopes(&self) -> Vec<String> {
        let mut scopes = vec![self.git_ref.clone()];
        let mut push = |r: String| {
            if !scopes.contains(&r) {
                scopes.push(r);
            }
        };
        if let Some(base) = &self.base_ref {
            push(format!("refs/heads/{base}"));
        }
        if let Some(default) = &self.default_branch {
            push(format!("refs/heads/{default}"));
        }
        scopes
    }

    pub fn check_mode(&self) -> anyhow::Result<()> {
        if self.cache_mode == CacheMode::None {
            bail!("ACTIONS_CACHE_MODE=none: this run may not use the cache");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(git_ref: &str, base: Option<&str>, default: Option<&str>) -> Env {
        Env {
            results_url: String::new(),
            runtime_token: String::new(),
            cache_mode: CacheMode::ReadWrite,
            github_token: None,
            api_url: String::new(),
            repository: "o/r".into(),
            git_ref: git_ref.into(),
            base_ref: base.map(Into::into),
            default_branch: default.map(Into::into),
        }
    }

    #[test]
    fn scopes_are_ordered_and_deduplicated() {
        assert_eq!(
            env("refs/heads/main", None, Some("main")).scopes(),
            ["refs/heads/main"]
        );
        assert_eq!(
            env("refs/pull/7/merge", Some("dev"), Some("main")).scopes(),
            ["refs/pull/7/merge", "refs/heads/dev", "refs/heads/main"]
        );
        assert_eq!(
            env("refs/pull/7/merge", Some("main"), Some("main")).scopes(),
            ["refs/pull/7/merge", "refs/heads/main"]
        );
    }

    #[test]
    fn cache_modes() {
        assert_eq!(CacheMode::parse("write"), CacheMode::ReadWrite);
        assert_eq!(CacheMode::parse(""), CacheMode::ReadWrite);
        assert_eq!(CacheMode::parse("READ"), CacheMode::ReadOnly);
        assert!(!CacheMode::parse("read").writable());
        assert!(CacheMode::parse("write-only").writable());
        assert_eq!(CacheMode::parse("none"), CacheMode::None);
    }
}
