use crate::{
    cache::{Cache, digest},
    error::RuntimeError,
    generated::Status,
    permissions::PermissionSet,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashMap},
    path::Path,
    sync::Arc,
    time::Instant,
};
use url::Url;

#[derive(Serialize, Deserialize)]
struct Transformed {
    text: String,
    source_map: Option<String>,
}
#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Graph {
    modules: BTreeMap<String, ModuleHint>,
    edges: BTreeMap<String, String>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ModuleHint {
    source_hash: String,
    transform_key: String,
}
pub struct Loader {
    permissions: Arc<PermissionSet>,
    cache: Cache,
    pub maps: HashMap<String, String>,
    eval: Option<(String, String)>,
    pub transform_hits: u64,
    pub module_count: usize,
    graph_key: Option<String>,
    previous: Graph,
    graph: Graph,
    pub graph_hits: u64,
    pub load_micros: u64,
    pub transform_micros: u64,
}
impl Loader {
    pub fn new(
        permissions: Arc<PermissionSet>,
        cache: Cache,
        eval: Option<(String, String)>,
    ) -> Self {
        Self {
            permissions,
            cache,
            maps: Default::default(),
            eval,
            transform_hits: 0,
            module_count: 0,
            graph_key: None,
            previous: Graph::default(),
            graph: Graph::default(),
            graph_hits: 0,
            load_micros: 0,
            transform_micros: 0,
        }
    }
    pub fn resolve(specifier: &str, referrer: &str) -> Result<String, RuntimeError> {
        let url = if let Ok(url) = Url::parse(specifier) {
            url
        } else {
            if !specifier.starts_with("./")
                && !specifier.starts_with("../")
                && !specifier.starts_with('/')
            {
                return Err(RuntimeError::new(
                    Status::Unsupported,
                    "unresolved bare module specifier",
                ));
            }
            let base = Url::parse(referrer)
                .map_err(|_| RuntimeError::new(Status::InvalidArgument, "invalid referrer"))?;
            base.join(specifier)
                .map_err(|e| RuntimeError::new(Status::InvalidArgument, e.to_string()))?
        };
        if url.scheme() != "file" || url.query().is_some() || url.fragment().is_some() {
            return Err(RuntimeError::new(
                Status::Unsupported,
                "only local file modules without query/fragment are supported",
            ));
        }
        Ok(url.to_string())
    }
    pub fn resolve_import(
        &mut self,
        specifier: &str,
        referrer: &str,
    ) -> Result<String, RuntimeError> {
        if let Some(key) = crate::npm::resolve(&self.permissions, specifier, referrer)? {
            return Ok(key);
        }
        // Package resolution always reopens manifests under current grants.
        // Only deterministic local URL resolution can reuse a graph edge.
        let edge = serde_json::to_string(&(referrer, specifier)).unwrap();
        let resolved = match self.previous.edges.get(&edge) {
            Some(key) => key.clone(),
            None => Self::resolve(specifier, referrer)?,
        };
        if self.graph.edges.len() < 8192 {
            self.graph.edges.insert(edge, resolved.clone());
        }
        Ok(resolved)
    }
    pub fn load(&mut self, key: &str) -> Result<String, RuntimeError> {
        let started = Instant::now();
        if self.graph_key.is_none() {
            let graph_key = digest(format!("ogma-graph-v1:deno_ast=0.49.0:{key}").as_bytes());
            self.previous = self
                .cache
                .get(&graph_key)
                .and_then(|bytes| serde_json::from_slice::<Graph>(&bytes).ok())
                .filter(|graph| graph.modules.len() <= 4096 && graph.edges.len() <= 8192)
                .unwrap_or_default();
            self.graph_key = Some(graph_key);
        }
        self.module_count += 1;
        if self.module_count > 4096 {
            return Err(RuntimeError::new(
                Status::ResourceLimit,
                "module graph exceeds 4096 modules",
            ));
        }
        let url = Url::parse(key)
            .map_err(|e| RuntimeError::new(Status::InvalidArgument, e.to_string()))?;
        let path = url.to_file_path().map_err(|_| {
            RuntimeError::new(Status::Unsupported, "only local modules are supported")
        })?;
        // Always open source under current grants before consulting the cache.
        let source = match &self.eval {
            Some((entry, source)) if entry == key => source.clone(),
            _ => self.permissions.read_text(Path::new(&path))?,
        };
        let source_hash = digest(source.as_bytes());
        let cache_key = match self.previous.modules.get(key) {
            Some(hint) if hint.source_hash == source_hash => {
                self.graph_hits += 1;
                hint.transform_key.clone()
            }
            _ => {
                digest(format!("ogma-transform-v2:deno_ast=0.49.0:{url}\0{source_hash}").as_bytes())
            }
        };
        let cached = self
            .cache
            .get(&cache_key)
            .and_then(|bytes| serde_json::from_slice::<Transformed>(&bytes).ok());
        let transformed = match cached {
            Some(value) => {
                self.transform_hits += 1;
                value
            }
            None => {
                let transform_started = Instant::now();
                let (text, source_map) = Self::transform(&url, source)?;
                self.transform_micros += transform_started.elapsed().as_micros() as u64;
                let result = Transformed { text, source_map };
                if let Ok(bytes) = serde_json::to_vec(&result) {
                    self.cache.put(&cache_key, &bytes);
                }
                result
            }
        };
        self.graph.modules.insert(
            key.into(),
            ModuleHint {
                source_hash,
                transform_key: cache_key,
            },
        );
        if let Some(map) = transformed.source_map {
            self.maps.insert(key.into(), map);
        }
        self.load_micros += started.elapsed().as_micros() as u64;
        Ok(transformed.text)
    }
    pub fn transform(url: &Url, source: String) -> Result<(String, Option<String>), RuntimeError> {
        let path = url.to_file_path().map_err(|_| {
            RuntimeError::new(Status::Unsupported, "only local modules are supported")
        })?;
        let media_type = deno_ast::MediaType::from_path(&path);
        use deno_ast::MediaType;
        match media_type {
            MediaType::JavaScript | MediaType::Mjs => return Ok((source, None)),
            MediaType::TypeScript | MediaType::Mts => {}
            _ => {
                return Err(RuntimeError::new(
                    Status::Unsupported,
                    "only .js/.mjs/.ts/.mts ESM modules are supported",
                ));
            }
        }
        let parsed = deno_ast::parse_module(deno_ast::ParseParams {
            specifier: url.clone(),
            text: source.into(),
            media_type,
            capture_tokens: false,
            scope_analysis: false,
            maybe_syntax: None,
        })
        .map_err(|e| RuntimeError::new(Status::Execution, e.to_string()))?;
        let emitted = parsed
            .transpile(
                &deno_ast::TranspileOptions::default(),
                &deno_ast::TranspileModuleOptions::default(),
                &deno_ast::EmitOptions {
                    source_map: deno_ast::SourceMapOption::Separate,
                    inline_sources: true,
                    ..Default::default()
                },
            )
            .map_err(|e| RuntimeError::new(Status::Execution, e.to_string()))?
            .into_source();
        Ok((emitted.text, emitted.source_map))
    }
}

impl Drop for Loader {
    fn drop(&mut self) {
        if !self.graph.modules.is_empty()
            && let Some(key) = &self.graph_key
            && let Ok(bytes) = serde_json::to_vec(&self.graph)
        {
            self.cache.put(key, &bytes);
        }
    }
}
