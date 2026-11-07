//! Env layers (§6.1): the one place a worker's process environment is built.
//! Each layer overrides the ones before it:
//!
//! 1. config `env`
//! 2. the project environment's `vars`
//! 3. the `env_file` values
//! 4. `INPUT_<NAME>`
//! 5. `OUTPUT_<KEY>`
//! 6. `GC_*` and the colour/terminal variables
//!
//! (The tmux server's own environment sits underneath, inherited by the
//! process.) A service uses layers 1 and 3. A pipeline step uses all six. An
//! approval message is expanded with every layer except `env_file`, because its
//! text is published unmasked. A dry run resolves exactly what the real run
//! would, from a run scope with no run id.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::LazyLock;

use regex::Regex;

use crate::config::{Config, Project};

/// Where a variable's value came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Layer {
    Config,
    Environment,
    EnvFile,
    Input,
    Output,
    Gc,
}

/// What a pipeline run adds on top of the config: its environment, inputs,
/// the outputs of earlier steps, and what `GC_*` says about it.
pub struct RunScope<'a> {
    pub env: Option<&'a str>,
    pub inputs: &'a BTreeMap<String, String>,
    pub outputs: &'a BTreeMap<String, String>,
    /// `None` for a dry run or preview: `GC_RUN_ID` is then `dry-run`.
    pub run_id: Option<i64>,
    pub output_file: &'a Path,
}

static NO_VARS: BTreeMap<String, String> = BTreeMap::new();

impl<'a> RunScope<'a> {
    /// A run that hasn't started: no id, no outputs yet, and `$GC_OUTPUT`
    /// standing in for the output file.
    pub fn preview(env: Option<&'a str>, inputs: &'a BTreeMap<String, String>) -> Self {
        RunScope { env, inputs, outputs: &NO_VARS, run_id: None, output_file: Path::new("$GC_OUTPUT") }
    }

    fn gc_vars(&self) -> BTreeMap<String, String> {
        BTreeMap::from([
            ("GC_RUN_ID".into(), self.run_id.map(|i| i.to_string()).unwrap_or_else(|| "dry-run".into())),
            ("GC_ENV".into(), self.env.unwrap_or("").into()),
            ("GC_OUTPUT".into(), self.output_file.to_string_lossy().into_owned()),
            ("FORCE_COLOR".into(), "1".into()),
            ("CLICOLOR_FORCE".into(), "1".into()),
            ("TERM".into(), "xterm-256color".into()),
            ("COLUMNS".into(), "120".into()),
        ])
    }
}

/// A resolved environment.
#[derive(Debug, Default)]
pub struct Resolved {
    vars: BTreeMap<String, String>,
    /// Names in layer order; a name moves to the layer that set it last.
    order: Vec<(String, Layer)>,
    secrets: Vec<String>,
}

impl Resolved {
    pub fn vars(&self) -> &BTreeMap<String, String> {
        &self.vars
    }

    /// Every `env_file` value, including ones a higher layer overrode: they
    /// are secrets wherever they appear.
    pub fn secrets(&self) -> &[String] {
        &self.secrets
    }

    /// Each variable's name and the layer its value came from, in layer order.
    pub fn names(&self) -> impl Iterator<Item = (&str, Layer)> {
        self.order.iter().map(|(k, l)| (k.as_str(), *l))
    }

    fn set(&mut self, layer: Layer, k: String, v: String) {
        self.order.retain(|(n, _)| *n != k);
        self.order.push((k.clone(), layer));
        self.vars.insert(k, v);
    }

    fn layer<'v>(&mut self, layer: Layer, vars: impl IntoIterator<Item = (&'v String, &'v String)>) {
        for (k, v) in vars {
            self.set(layer, k.clone(), v.clone());
        }
    }

    fn prefixed(&mut self, layer: Layer, prefix: &str, vars: &BTreeMap<String, String>) {
        for (k, v) in vars {
            self.set(layer, format!("{prefix}{}", k.to_ascii_uppercase()), v.clone());
        }
    }

    /// Layer 3. On error the other layers are still there, for a dry run.
    fn env_file(&mut self, path: Option<&Path>) -> Result<(), String> {
        let Some(path) = path else { return Ok(()) };
        for (k, v) in crate::envfile::load(path)? {
            self.secrets.push(v.clone());
            self.set(Layer::EnvFile, k, v);
        }
        Ok(())
    }
}

/// A service's environment: config `env` and its `env_file`.
pub fn service(config: &Config, env_file: Option<&Path>) -> Result<Resolved, String> {
    let mut r = Resolved::default();
    r.layer(Layer::Config, config.env.iter().map(|(k, v)| (k, v)));
    r.env_file(env_file)?;
    Ok(r)
}

/// A `run:` step's environment.
pub fn step(config: &Config, project: &Project, scope: &RunScope, env_file: Option<&Path>) -> Result<Resolved, String> {
    match step_preview(config, project, scope, env_file) {
        (r, None) => Ok(r),
        (_, Some(e)) => Err(e),
    }
}

/// [`step`] for a dry run: an unreadable `env_file` is reported next to the
/// other layers instead of replacing them.
pub fn step_preview(config: &Config, project: &Project, scope: &RunScope, env_file: Option<&Path>) -> (Resolved, Option<String>) {
    let mut r = Resolved::default();
    r.layer(Layer::Config, config.env.iter().map(|(k, v)| (k, v)));
    if let Some(e) = scope.env.and_then(|e| project.environment(e)) {
        r.layer(Layer::Environment, e.vars.iter().map(|(k, v)| (k, v)));
    }
    let err = r.env_file(env_file).err();
    r.prefixed(Layer::Input, "INPUT_", scope.inputs);
    r.prefixed(Layer::Output, "OUTPUT_", scope.outputs);
    r.layer(Layer::Gc, &scope.gc_vars());
    (r, err)
}

/// An `approve:` step's message with `$NAME` / `${NAME}` expanded from every
/// layer but `env_file`. Unknown names stay as written.
pub fn approval_message(config: &Config, project: &Project, scope: &RunScope, message: &str) -> String {
    let (r, _) = step_preview(config, project, scope, None);
    expand(message, r.vars())
}

static VAR_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\$\{([A-Za-z_][A-Za-z0-9_]*)\}|\$([A-Za-z_][A-Za-z0-9_]*)").unwrap());

fn expand(msg: &str, vars: &BTreeMap<String, String>) -> String {
    VAR_RE
        .replace_all(msg, |c: &regex::Captures| {
            let name = c.get(1).or_else(|| c.get(2)).unwrap().as_str();
            vars.get(name).cloned().unwrap_or_else(|| c[0].to_string())
        })
        .into_owned()
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    const SRC: &str = r#"
env:
  REGION: eu
  SHARED: from-config
projects:
  p:
    path: /tmp
    environments:
      prod: { vars: { DB: ledger, SHARED: from-env } }
    pipelines:
      x:
        environments: [prod]
        steps:
          - run: "true"
"#;

    fn config() -> Config {
        let r = crate::config::load_str(SRC);
        r.config.unwrap_or_else(|| panic!("{:?}", r.diagnostics))
    }

    fn env_file(body: &str) -> tempfile::NamedTempFile {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(body.as_bytes()).unwrap();
        f
    }

    fn map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn layers_override_in_order() {
        let c = config();
        let p = c.project("p").unwrap();
        let f = env_file("SHARED=from-file\nTOKEN=s3cr3t-token\nINPUT_BRANCH=file-branch\nGC_ENV=file-env\n");
        let inputs = map(&[("branch", "main")]);
        let outputs = map(&[("bm", "0001")]);
        let scope = RunScope { env: Some("prod"), inputs: &inputs, outputs: &outputs, run_id: Some(7), output_file: Path::new("/o") };
        let r = step(&c, p, &scope, Some(f.path())).unwrap();
        let v = r.vars();
        assert_eq!(v["REGION"], "eu");
        assert_eq!(v["DB"], "ledger");
        assert_eq!(v["SHARED"], "from-file");
        assert_eq!(v["TOKEN"], "s3cr3t-token");
        assert_eq!(v["INPUT_BRANCH"], "main");
        assert_eq!(v["OUTPUT_BM"], "0001");
        assert_eq!(v["GC_ENV"], "prod");
        assert_eq!(v["GC_RUN_ID"], "7");
        assert_eq!(v["GC_OUTPUT"], "/o");
        let layer = |name: &str| r.names().find(|(n, _)| *n == name).unwrap().1;
        assert_eq!(layer("SHARED"), Layer::EnvFile);
        assert_eq!(layer("INPUT_BRANCH"), Layer::Input);
        assert_eq!(layer("GC_ENV"), Layer::Gc);
        let layers: Vec<Layer> = r.names().map(|(_, l)| l).collect();
        assert!(layers.is_sorted(), "names are in layer order: {layers:?}");
        assert_eq!(r.names().count(), v.len(), "each name once");
    }

    #[test]
    fn secrets_are_every_env_file_value_even_overridden() {
        let c = config();
        let p = c.project("p").unwrap();
        let f = env_file("TOKEN=s3cr3t-token\nGC_ENV=hidden-value\n");
        let scope = RunScope::preview(Some("prod"), &NO_VARS);
        let r = step(&c, p, &scope, Some(f.path())).unwrap();
        assert_eq!(r.secrets(), ["s3cr3t-token", "hidden-value"]);
        assert_eq!(r.vars()["GC_ENV"], "prod");
        let svc = service(&c, Some(f.path())).unwrap();
        assert_eq!(svc.secrets(), ["s3cr3t-token", "hidden-value"]);
        assert!(step(&c, p, &scope, None).unwrap().secrets().is_empty());
    }

    #[test]
    fn service_gets_config_and_env_file_only() {
        let c = config();
        let f = env_file("SHARED=from-file\n");
        let r = service(&c, Some(f.path())).unwrap();
        assert_eq!(r.vars(), &map(&[("REGION", "eu"), ("SHARED", "from-file")]));
    }

    #[test]
    fn bad_env_file_fails_a_step_but_not_a_preview() {
        let c = config();
        let p = c.project("p").unwrap();
        let f = env_file("not a line\n");
        let scope = RunScope::preview(Some("prod"), &NO_VARS);
        assert!(step(&c, p, &scope, Some(f.path())).is_err());
        assert!(service(&c, Some(f.path())).is_err());
        let (r, err) = step_preview(&c, p, &scope, Some(f.path()));
        assert!(err.unwrap().contains("expected KEY=value"));
        assert_eq!(r.vars()["DB"], "ledger");
        assert_eq!(r.vars()["GC_ENV"], "prod");
    }

    #[test]
    fn approval_messages_expand_without_env_file() {
        let c = config();
        let p = c.project("p").unwrap();
        let inputs = map(&[("pr", "42")]);
        let scope = RunScope::preview(Some("prod"), &inputs);
        assert_eq!(
            approval_message(&c, p, &scope, "Merge PR #$INPUT_PR into $DB on ${GC_ENV}? $OUTPUT_BM $UNKNOWN"),
            "Merge PR #42 into ledger on prod? $OUTPUT_BM $UNKNOWN"
        );
    }

    #[test]
    fn a_preview_resolves_what_the_run_will() {
        let c = config();
        let p = c.project("p").unwrap();
        let f = env_file("TOKEN=s3cr3t-token\n");
        let inputs = map(&[("branch", "main")]);
        let real = RunScope { env: Some("prod"), inputs: &inputs, outputs: &NO_VARS, run_id: Some(7), output_file: Path::new("/o") };
        let mut real = step(&c, p, &real, Some(f.path())).unwrap().vars().clone();
        let mut dry = step(&c, p, &RunScope::preview(Some("prod"), &inputs), Some(f.path())).unwrap().vars().clone();
        for k in ["GC_RUN_ID", "GC_OUTPUT"] {
            assert_ne!(real.remove(k), dry.remove(k));
        }
        assert_eq!(real, dry);
    }
}
