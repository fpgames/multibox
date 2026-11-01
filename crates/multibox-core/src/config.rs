//! `.multibox.toml`: optional settings at the root of a repository.
//!
//! Without the file Multibox runs on its defaults, and every key is optional.
//! A mistake in it never stops a worktree from being made: a bad key or value
//! is ignored and reported, and a file that is not valid TOML is ignored whole.

use crate::adapter::ENGINES;
use ignore::gitignore::GitignoreBuilder;
use std::fs;
use std::path::Path;
use toml::{Table, Value};

pub const CONFIG_FILE: &str = ".multibox.toml";

/// When the caches that cost real disk space travel by default: Rust's
/// `target/`, Unity's `Library/BuildCache/` and search index. Patterns in
/// `.worktreeinclude` or `carry.include` bring them along whatever this says.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum LargeCaches {
    /// Only where the filesystem shares their data instead of copying it.
    #[default]
    Auto,
    /// Copied where they cannot be shared. pnpm's `node_modules/` still stays
    /// behind there: reinstalling it is faster than a copy.
    Always,
    Never,
}

#[derive(Clone, Debug)]
pub struct Config {
    /// False makes `git multibox add` a plain `git worktree add`.
    pub enabled: bool,
    /// More ignored paths to carry, in `.worktreeinclude` syntax.
    pub include: Vec<String>,
    /// Ignored paths never to carry, whatever asked for them.
    pub exclude: Vec<String>,
    pub large_caches: LargeCaches,
    /// Engines whose adapter is turned off.
    pub disabled_engines: Vec<&'static str>,
    /// Passed to `git merge` by `finish`, after the engines' own.
    pub merge_options: Vec<String>,
    pub keep_branch: bool,
    /// Whether the file exists.
    pub found: bool,
    /// What was ignored and why.
    pub problems: Vec<String>,
}

impl Default for Config {
    fn default() -> Config {
        Config {
            enabled: true,
            include: Vec::new(),
            exclude: Vec::new(),
            large_caches: LargeCaches::Auto,
            disabled_engines: Vec::new(),
            merge_options: Vec::new(),
            keep_branch: false,
            found: false,
            problems: Vec::new(),
        }
    }
}

fn type_name(value: &Value) -> &'static str {
    match value {
        Value::String(_) => "a string",
        Value::Integer(_) => "an integer",
        Value::Float(_) => "a number",
        Value::Boolean(_) => "true or false",
        Value::Datetime(_) => "a date",
        Value::Array(_) => "a list",
        Value::Table(_) => "a table",
    }
}

impl Config {
    /// Reads the file at the root of `worktree`, or returns the defaults.
    pub fn load(worktree: &Path) -> Config {
        match fs::read_to_string(worktree.join(CONFIG_FILE)) {
            Ok(text) => Config::parse(&text),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Config::default(),
            Err(err) => Config {
                found: true,
                problems: vec![format!("unreadable, defaults used: {err}")],
                ..Config::default()
            },
        }
    }

    pub fn parse(text: &str) -> Config {
        let mut config = Config {
            found: true,
            ..Config::default()
        };
        let table: Table = match text.parse() {
            Ok(table) => table,
            Err(err) => {
                let message = err.to_string();
                let first = message.lines().next().unwrap_or_default();
                config
                    .problems
                    .push(format!("not valid TOML, defaults used: {first}"));
                return config;
            }
        };
        for (key, value) in &table {
            match (key.as_str(), value) {
                ("enabled", Value::Boolean(enabled)) => config.enabled = *enabled,
                ("carry", Value::Table(section)) => config.read_carry(section),
                ("engines", Value::Table(section)) => config.read_engines(section),
                ("finish", Value::Table(section)) => config.read_finish(section),
                ("enabled", other) => config.wrong_type("enabled", "true or false", other),
                ("carry" | "engines" | "finish", other) => {
                    config.wrong_type(key, "a table like [carry]", other);
                }
                _ => config.unknown(key),
            }
        }
        config
    }

    /// Whether a project of `engine` is handled.
    pub fn engine_enabled(&self, engine: &str) -> bool {
        !self.disabled_engines.contains(&engine)
    }

    /// Every setting that differs from the defaults, on one line.
    pub fn describe(&self) -> String {
        let mut parts = Vec::new();
        if !self.enabled {
            parts.push("multibox turned off (enabled = false)".to_owned());
        }
        if !self.include.is_empty() {
            parts.push(format!("also carries {}", self.include.join(" ")));
        }
        if !self.exclude.is_empty() {
            parts.push(format!("never carries {}", self.exclude.join(" ")));
        }
        match self.large_caches {
            LargeCaches::Auto => {}
            LargeCaches::Always => parts.push("large caches copied where not shared".to_owned()),
            LargeCaches::Never => parts.push("large caches never carried".to_owned()),
        }
        if !self.disabled_engines.is_empty() {
            parts.push(format!("engines off: {}", self.disabled_engines.join(", ")));
        }
        if !self.merge_options.is_empty() {
            parts.push(format!(
                "finish merges with {}",
                self.merge_options.join(" ")
            ));
        }
        if self.keep_branch {
            parts.push("finish keeps branches".to_owned());
        }
        if parts.is_empty() {
            "all defaults".to_owned()
        } else {
            parts.join("; ")
        }
    }

    fn unknown(&mut self, key: &str) {
        self.problems.push(format!("unknown key `{key}` ignored"));
    }

    fn wrong_type(&mut self, key: &str, expected: &str, value: &Value) {
        self.problems.push(format!(
            "`{key}` must be {expected}, not {}; ignored",
            type_name(value)
        ));
    }

    fn strings(&mut self, key: &str, value: &Value) -> Vec<String> {
        let Value::Array(items) = value else {
            self.wrong_type(key, "a list of strings", value);
            return Vec::new();
        };
        let mut strings = Vec::new();
        for item in items {
            match item {
                Value::String(text) => strings.push(text.clone()),
                other => self.wrong_type(&format!("an entry of `{key}`"), "a string", other),
            }
        }
        strings
    }

    /// Keeps the patterns gitignore syntax accepts, reporting the others.
    fn patterns(&mut self, key: &str, value: &Value) -> Vec<String> {
        let mut kept = Vec::new();
        for pattern in self.strings(key, value) {
            let line = pattern.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            // Globs are only compiled when the matcher is built.
            let mut builder = GitignoreBuilder::new("");
            let checked = builder
                .add_line(None, line)
                .map(|_| ())
                .and_then(|()| builder.build().map(|_| ()));
            match checked {
                Ok(()) => kept.push(line.to_owned()),
                Err(err) => self
                    .problems
                    .push(format!("pattern {line:?} in `{key}` ignored: {err}")),
            }
        }
        kept
    }

    fn read_carry(&mut self, section: &Table) {
        for (key, value) in section {
            match (key.as_str(), value) {
                ("include", _) => self.include = self.patterns("carry.include", value),
                ("exclude", _) => {
                    // An exclusion is written as the path itself; a leading `!`
                    // would turn it back into an include.
                    self.exclude = self
                        .patterns("carry.exclude", value)
                        .into_iter()
                        .map(|pattern| pattern.trim_start_matches('!').to_owned())
                        .filter(|pattern| !pattern.is_empty())
                        .collect();
                }
                ("large-caches", Value::String(choice)) => match choice.as_str() {
                    "auto" => self.large_caches = LargeCaches::Auto,
                    "always" => self.large_caches = LargeCaches::Always,
                    "never" => self.large_caches = LargeCaches::Never,
                    _ => self.problems.push(format!(
                        "`carry.large-caches` must be \"auto\", \"always\" or \"never\", not {choice:?}; ignored"
                    )),
                },
                ("large-caches", other) => self.wrong_type(
                    "carry.large-caches",
                    "\"auto\", \"always\" or \"never\"",
                    other,
                ),
                _ => self.unknown(&format!("carry.{key}")),
            }
        }
    }

    fn read_engines(&mut self, section: &Table) {
        for (key, value) in section {
            let Some(engine) = ENGINES.iter().find(|engine| **engine == key.as_str()) else {
                self.problems.push(format!(
                    "unknown engine `engines.{key}` ignored; known: {}",
                    ENGINES.join(", ")
                ));
                continue;
            };
            match value {
                Value::Boolean(true) => self.disabled_engines.retain(|known| known != engine),
                Value::Boolean(false) => {
                    if !self.disabled_engines.contains(engine) {
                        self.disabled_engines.push(engine);
                    }
                }
                other => self.wrong_type(&format!("engines.{key}"), "true or false", other),
            }
        }
    }

    fn read_finish(&mut self, section: &Table) {
        for (key, value) in section {
            match (key.as_str(), value) {
                ("merge-options", _) => {
                    self.merge_options = self.strings("finish.merge-options", value);
                }
                ("keep-branch", Value::Boolean(keep)) => self.keep_branch = *keep,
                ("keep-branch", other) => {
                    self.wrong_type("finish.keep-branch", "true or false", other);
                }
                _ => self.unknown(&format!("finish.{key}")),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_file_means_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config::load(dir.path());
        assert!(!config.found);
        assert!(config.enabled);
        assert_eq!(config.large_caches, LargeCaches::Auto);
        assert!(config.problems.is_empty());
    }

    #[test]
    fn reads_every_section() {
        let config = Config::parse(
            r#"
            enabled = true
            [carry]
            include = ["/bin/*/*/", ".env.local"]
            exclude = ["!/Library/Search/"]
            large-caches = "always"
            [engines]
            pnpm = false
            unity = true
            [finish]
            merge-options = ["--no-ff"]
            keep-branch = true
            "#,
        );
        assert!(config.problems.is_empty(), "{:?}", config.problems);
        assert_eq!(config.include, ["/bin/*/*/", ".env.local"]);
        assert_eq!(config.exclude, ["/Library/Search/"]);
        assert_eq!(config.large_caches, LargeCaches::Always);
        assert!(!config.engine_enabled("pnpm"));
        assert!(config.engine_enabled("unity"));
        assert_eq!(config.merge_options, ["--no-ff"]);
        assert!(config.keep_branch);
    }

    #[test]
    fn mistakes_are_reported_and_skipped() {
        let config = Config::parse(
            r#"
            enable = false
            [carry]
            includes = ["x/"]
            include = ["ok/", 3]
            large-caches = "sometimes"
            [engines]
            godot = false
            cargo = "no"
            "#,
        );
        assert!(config.enabled);
        assert_eq!(config.include, ["ok/"]);
        assert_eq!(config.large_caches, LargeCaches::Auto);
        assert!(config.engine_enabled("cargo"));
        let problems = config.problems.join("\n");
        for expected in [
            "`enable`",
            "`carry.includes`",
            "an entry of `carry.include`",
            "\"sometimes\"",
            "`engines.godot`",
            "`engines.cargo`",
        ] {
            assert!(
                problems.contains(expected),
                "{expected} not in:\n{problems}"
            );
        }
    }

    #[test]
    fn invalid_toml_falls_back_to_defaults() {
        let config = Config::parse("[carry\ninclude = [");
        assert!(config.found);
        assert!(config.enabled);
        assert!(config.include.is_empty());
        assert_eq!(config.problems.len(), 1);
        assert!(config.problems[0].contains("not valid TOML"));
    }

    #[test]
    fn an_invalid_pattern_is_dropped_alone() {
        // Accepted line by line, rejected only once compiled.
        let config = Config::parse("[carry]\ninclude = [\"/ok/\", \"/bad/{x\"]\n");
        assert_eq!(config.include, ["/ok/"]);
        assert_eq!(config.problems.len(), 1);
        assert!(config.problems[0].contains("/bad/{x"));
    }
}
