use crate::adapter::{self, Project};
use crate::{Error, Result};
use ignore::Match;
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use multibox_clone::{Decision, TreeFilter};
use std::fs;
use std::path::Path;

pub const INCLUDE_FILE: &str = ".worktreeinclude";

/// Decides which ignored paths travel to a new worktree.
///
/// Patterns use gitignore syntax and are evaluated in order: engine defaults,
/// then engine exclusions, then the repository's `.worktreeinclude`. The last
/// match wins, so a repository can override anything an adapter decided.
pub struct Rules {
    matcher: Gitignore,
    literal_prefixes: Vec<String>,
    matches_anywhere: bool,
}

fn glob_free_prefix(pattern: &str) -> &str {
    let end = pattern.find(['*', '?', '[']).unwrap_or(pattern.len());
    &pattern[..end]
}

impl Rules {
    pub fn new(lines: &[String]) -> Result<Rules> {
        let mut builder = GitignoreBuilder::new("");
        let mut literal_prefixes = Vec::new();
        let mut matches_anywhere = false;
        for line in lines {
            let line = line.trim_end();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            builder
                .add_line(None, line)
                .map_err(|err| Error::new(format!("invalid pattern {line:?}: {err}")))?;
            if line.starts_with('!') {
                continue;
            }
            if line.trim_end_matches('/').contains('/') {
                let anchored = line.trim_start_matches('/');
                literal_prefixes.push(glob_free_prefix(anchored).to_owned());
            } else {
                matches_anywhere = true;
            }
        }
        let matcher = builder
            .build()
            .map_err(|err| Error::new(format!("invalid patterns: {err}")))?;
        Ok(Rules {
            matcher,
            literal_prefixes,
            matches_anywhere,
        })
    }

    /// Combines adapter defaults for `projects` with the `.worktreeinclude`
    /// file at the root of `worktree`, if there is one. With `frugal` set, as
    /// in copy mode, caches that cost real disk space are left out too.
    pub fn load(worktree: &Path, projects: &[Project], frugal: bool) -> Result<Rules> {
        let mut lines: Vec<String> = projects.iter().flat_map(adapter::includes).collect();
        lines.extend(
            projects
                .iter()
                .flat_map(adapter::excludes)
                .map(|pattern| format!("!{pattern}")),
        );
        if frugal {
            lines.extend(
                projects
                    .iter()
                    .flat_map(adapter::expensive)
                    .map(|pattern| format!("!{pattern}")),
            );
        }
        match fs::read_to_string(worktree.join(INCLUDE_FILE)) {
            Ok(text) => lines.extend(text.lines().map(str::to_owned)),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(err.into()),
        }
        Rules::new(&lines)
    }

    fn matched(&self, rel: &str, is_dir: bool) -> Match<()> {
        self.matcher
            .matched_path_or_any_parents(rel, is_dir)
            .map(|_| ())
    }

    pub fn includes(&self, rel: &str, is_dir: bool) -> bool {
        matches!(self.matched(rel, is_dir), Match::Ignore(()))
    }

    /// Whether a pattern could match something below `dir`, even though `dir`
    /// itself is not selected.
    fn may_contain(&self, dir: &str) -> bool {
        if self.matches_anywhere {
            return true;
        }
        let dir = format!("{dir}/");
        self.literal_prefixes
            .iter()
            .any(|prefix| prefix.starts_with(&dir) || dir.starts_with(prefix.as_str()))
    }
}

impl TreeFilter for Rules {
    fn check(&self, rel: &str, is_dir: bool) -> Decision {
        match self.matched(rel, is_dir) {
            Match::Ignore(()) => Decision::Include,
            Match::Whitelist(()) if !is_dir || !self.may_contain(rel) => Decision::Exclude,
            Match::None if !is_dir || !self.may_contain(rel) => Decision::Exclude,
            _ => Decision::Descend,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules(lines: &[&str]) -> Rules {
        let lines: Vec<String> = lines.iter().map(|line| (*line).to_owned()).collect();
        Rules::new(&lines).unwrap()
    }

    #[test]
    fn exclusions_beat_the_directory_they_sit_in() {
        let rules = rules(&["/client/Library/", "!/client/Library/*-lock"]);
        assert!(rules.includes("client/Library/Artifacts/ab/cd", false));
        assert!(!rules.includes("client/Library/ArtifactDB-lock", false));
    }

    #[test]
    fn later_lines_override_earlier_ones() {
        let rules = rules(&["/Library/", "!/Library/Bee/", "/Library/Bee/keep"]);
        assert!(rules.includes("Library/Bee/keep", false));
        assert!(!rules.includes("Library/Bee/other", false));
    }

    #[test]
    fn walks_into_unselected_parents_of_a_selected_path() {
        let rules = rules(&["/client/Bundles/Android/"]);
        assert_eq!(rules.check("client/Bundles", true), Decision::Descend);
        assert_eq!(
            rules.check("client/Bundles/Android", true),
            Decision::Include
        );
        assert_eq!(rules.check("client/Bundles/iOS", true), Decision::Exclude);
        assert_eq!(rules.check("client/Builds", true), Decision::Exclude);
    }

    #[test]
    fn bare_names_match_at_any_depth() {
        let rules = rules(&["node_modules/"]);
        assert_eq!(rules.check("web/app", true), Decision::Descend);
        assert!(rules.includes("web/app/node_modules/left-pad/index.js", false));
    }
}
