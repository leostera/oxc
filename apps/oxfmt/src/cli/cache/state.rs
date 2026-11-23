//! A run shape's state: what its last run read and walked, and its JS config values. Each
//! file's verdict is in the shape's [`oxc_cache::Cache`].

use serde::{Deserialize, Serialize};

use oxc_cache::Inputs;

use crate::core::FormatResult;

/// What a run of a shape leaves the next: kept in `runs/<shape>`.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunState {
    /// The JS/TS config values Node evaluated; `None` if they can't be kept.
    pub js_configs: Option<JsConfigs>,
    /// What the walk found and read; `None` if it can't be kept. It depends on the configs:
    /// kept only with them.
    pub walk: Option<Walk>,
    /// What Node read formatting files (Prettier, its plugins, the Tailwind sorter) since these
    /// inputs last changed; `None` if that can't be known.
    pub node_reads: Option<Inputs>,
}

/// The JS/TS config values Node evaluated, and what it loaded for them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JsConfigs {
    /// One per config path, sorted by it.
    pub values: Vec<JsConfigValue>,
    /// The files Node loaded evaluating them.
    pub inputs: Inputs,
}

/// A JS/TS config file's value, as Node evaluated it: JSON, or `None` for a
/// Vite+ config without a `fmt` field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JsConfigValue {
    /// The config file, absolute.
    pub path: String,
    /// Its value as JSON; `None` for a Vite+ config without a `fmt` field.
    pub value: Option<String>,
}

/// What a walk found and read: a later run whose inputs read the same replays it instead.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Walk {
    /// What the walk and config discovery read.
    pub inputs: Inputs,
    /// Whether the walk found any config, for the "No config found" hint.
    pub any_config_found: bool,
    /// The files it found, in its order: a run that formats only some reports them in it, as a
    /// full run would.
    pub files: Vec<Box<str>>,
}

/// What formatting found.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Verdict {
    /// Not formatted, or not formattable (a parse error): never cached.
    Unformatted,
    /// Already formatted, by oxfmt alone.
    Formatted,
    /// Already formatted, by way of Node (Prettier, the Tailwind sorter): kept under the
    /// fingerprint of [`RunState::node_reads`].
    FormattedWithNode,
}

impl Verdict {
    /// The verdict of formatting that gave `result`, calling into Node if `with_node`.
    pub fn of(result: &FormatResult, with_node: bool) -> Self {
        match result {
            FormatResult::Success { is_changed: false, .. } if with_node => Self::FormattedWithNode,
            FormatResult::Success { is_changed: false, .. } => Self::Formatted,
            _ => Self::Unformatted,
        }
    }
}
