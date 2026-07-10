#![allow(dead_code)]

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub(crate) struct ShelfId(String);

impl ShelfId {
    pub(crate) fn new(raw: impl Into<String>) -> Result<Self> {
        Ok(Self(validate_id(raw.into(), "shelf")?))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<&str> for ShelfId {
    type Error = anyhow::Error;

    fn try_from(value: &str) -> Result<Self> {
        Self::new(value)
    }
}

impl<'de> Deserialize<'de> for ShelfId {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        Self::new(raw).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub(crate) struct PackId(String);

impl PackId {
    pub(crate) fn new(raw: impl Into<String>) -> Result<Self> {
        Ok(Self(validate_id(raw.into(), "pack")?))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<&str> for PackId {
    type Error = anyhow::Error;

    fn try_from(value: &str) -> Result<Self> {
        Self::new(value)
    }
}

impl<'de> Deserialize<'de> for PackId {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        Self::new(raw).map_err(serde::de::Error::custom)
    }
}

fn validate_id(raw: String, label: &str) -> Result<String> {
    let value = raw.trim();
    if value.is_empty() {
        bail!("{label} id is empty");
    }
    if value.starts_with('/') || value.ends_with('/') || value.contains("//") {
        bail!("{label} id '{value}' has an empty segment");
    }
    let ok = value.bytes().all(|b| {
        b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'-' | b'_' | b'/' | b'.')
    });
    if !ok {
        bail!("{label} id '{value}' must be lowercase ascii");
    }
    Ok(value.to_string())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ShelfScope {
    Core,
    User,
    Project,
    Run,
}

impl ShelfScope {
    fn rank(self) -> u8 {
        match self {
            Self::Core => 0,
            Self::User => 1,
            Self::Project => 2,
            Self::Run => 3,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ShelfOrigin {
    pub(crate) scope: ShelfScope,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) path: Option<PathBuf>,
}

impl ShelfOrigin {
    pub(crate) fn core() -> Self {
        Self {
            scope: ShelfScope::Core,
            path: None,
        }
    }

    pub(crate) fn project(path: impl Into<PathBuf>) -> Self {
        Self {
            scope: ShelfScope::Project,
            path: Some(path.into()),
        }
    }
}

impl Default for ShelfOrigin {
    fn default() -> Self {
        Self::core()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ShelfMode {
    Passive,
    #[default]
    OnDemand,
    Always,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct ShelfManifest {
    pub(crate) id: ShelfId,
    pub(crate) name: String,
    pub(crate) description: String,
    #[serde(default)]
    pub(crate) origin: ShelfOrigin,
    #[serde(default)]
    pub(crate) mode: ShelfMode,
    #[serde(default)]
    pub(crate) packs: Vec<PackManifest>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct PackManifest {
    pub(crate) id: PackId,
    pub(crate) name: String,
    pub(crate) version: String,
    pub(crate) description: String,
    #[serde(default)]
    pub(crate) abilities: Vec<Ability>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "ability", rename_all = "snake_case")]
pub(crate) enum Ability {
    Tool(ToolAbility),
    Command(CommandAbility),
    Hook(HookAbility),
    Context(ContextAbility),
}

impl Ability {
    fn key(&self) -> (&'static str, &str) {
        match self {
            Self::Tool(tool) => ("tool", &tool.name),
            Self::Command(command) => ("command", &command.name),
            Self::Hook(hook) => ("hook", &hook.name),
            Self::Context(context) => ("context", &context.name),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct ToolAbility {
    pub(crate) name: String,
    pub(crate) description: String,
    pub(crate) schema: Value,
    #[serde(default)]
    pub(crate) grants: Vec<Grant>,
    pub(crate) exposure: Exposure,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct CommandAbility {
    pub(crate) name: String,
    pub(crate) usage: String,
    pub(crate) description: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct HookAbility {
    pub(crate) name: String,
    pub(crate) signals: Vec<SignalKind>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ContextAbility {
    pub(crate) name: String,
    pub(crate) description: String,
    pub(crate) budget: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Grant {
    Read,
    Write,
    Network,
    Process,
    Secret,
    Browser,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Exposure {
    Hidden,
    OnDemand,
    Visible,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SignalKind {
    Load,
    Prompt,
    Tool,
    Turn,
    Compact,
    Shutdown,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "signal", rename_all = "snake_case")]
pub(crate) enum Signal {
    Load,
    Prompt {
        text: String,
    },
    Tool {
        phase: ToolPhase,
        name: String,
        input: Value,
        #[serde(skip_serializing_if = "Option::is_none")]
        output: Option<String>,
    },
    Turn {
        phase: TurnPhase,
    },
    Compact {
        phase: CompactPhase,
    },
    Shutdown,
}

impl Signal {
    fn kind(&self) -> SignalKind {
        match self {
            Self::Load => SignalKind::Load,
            Self::Prompt { .. } => SignalKind::Prompt,
            Self::Tool { .. } => SignalKind::Tool,
            Self::Turn { .. } => SignalKind::Turn,
            Self::Compact { .. } => SignalKind::Compact,
            Self::Shutdown => SignalKind::Shutdown,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ToolPhase {
    Before,
    After,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TurnPhase {
    Start,
    End,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CompactPhase {
    Start,
    End,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "effect", rename_all = "snake_case")]
pub(crate) enum Effect {
    Note { text: String },
    Context { text: String, priority: i16 },
    Block { reason: String },
    RewriteTool { input: Value },
    State { key: String, value: Value },
}

impl Effect {
    fn stops_flow(&self) -> bool {
        matches!(self, Self::Block { .. })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ShelfFrame {
    pub(crate) root: PathBuf,
    pub(crate) session_id: Option<String>,
    pub(crate) turn: u64,
}

impl ShelfFrame {
    pub(crate) fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            session_id: None,
            turn: 0,
        }
    }
}

pub(crate) trait Shelf: Send + Sync {
    fn manifest(&self) -> &ShelfManifest;

    fn on_signal(&self, _signal: &Signal, _frame: &ShelfFrame) -> Result<Vec<Effect>> {
        Ok(Vec::new())
    }
}

pub(crate) struct ShelfRegistry {
    shelves: Vec<Box<dyn Shelf>>,
}

impl ShelfRegistry {
    pub(crate) fn new() -> Self {
        Self {
            shelves: Vec::new(),
        }
    }

    pub(crate) fn discover(root: &Path) -> Self {
        let mut registry = Self::new();
        for candidate in shelf_manifest_candidates(root) {
            let Ok(mut shelf) = StaticShelf::from_json_file(&candidate.path) else {
                continue;
            };
            shelf.manifest.origin = candidate.origin;
            registry.register(shelf);
        }
        registry
    }

    pub(crate) fn register(&mut self, shelf: impl Shelf + 'static) {
        self.shelves.push(Box::new(shelf));
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.shelves.is_empty()
    }

    pub(crate) fn manifests(&self) -> Vec<&ShelfManifest> {
        self.shelves.iter().map(|shelf| shelf.manifest()).collect()
    }

    pub(crate) fn resolve(&self) -> Vec<ResolvedAbility> {
        let mut map: BTreeMap<String, (u8, usize, usize, ResolvedAbility)> = BTreeMap::new();
        for (shelf_index, shelf) in self.shelves.iter().enumerate() {
            let manifest = shelf.manifest();
            let rank = manifest.origin.scope.rank();
            for (pack_index, pack) in manifest.packs.iter().enumerate() {
                for ability in &pack.abilities {
                    let (kind, name) = ability.key();
                    let key = format!("{kind}:{name}");
                    let resolved = ResolvedAbility {
                        shelf: manifest.id.clone(),
                        pack: pack.id.clone(),
                        origin: manifest.origin.clone(),
                        ability: ability.clone(),
                    };
                    let replace = map
                        .get(&key)
                        .map(|(old_rank, old_shelf, old_pack, _)| {
                            (rank, shelf_index, pack_index) >= (*old_rank, *old_shelf, *old_pack)
                        })
                        .unwrap_or(true);
                    if replace {
                        map.insert(key, (rank, shelf_index, pack_index, resolved));
                    }
                }
            }
        }
        map.into_values()
            .map(|(_, _, _, resolved)| resolved)
            .collect()
    }

    pub(crate) fn emit(&self, signal: &Signal, frame: &ShelfFrame) -> Result<Vec<Effect>> {
        let mut effects = Vec::new();
        for shelf in &self.shelves {
            if !wants_signal(shelf.manifest(), signal.kind()) {
                continue;
            }
            let next = shelf.on_signal(signal, frame)?;
            let blocked = next.iter().any(Effect::stops_flow);
            effects.extend(next);
            if blocked {
                break;
            }
        }
        Ok(effects)
    }

    /// Cheap check: does any registered shelf opt into this signal kind via a
    /// Hook ability? Lets callers skip emitting entirely when nothing listens.
    pub(crate) fn wants_any(&self, kind: SignalKind) -> bool {
        self.shelves
            .iter()
            .any(|shelf| wants_signal(shelf.manifest(), kind))
    }

    /// Collect Context/Note effects produced for a load/prompt signal into a
    /// single priority-ordered block bounded by `total_budget` bytes. Returns
    /// None when no shelf opts into the signal or produces context.
    pub(crate) fn collect_context(
        &self,
        signal: &Signal,
        frame: &ShelfFrame,
        total_budget: usize,
    ) -> Option<String> {
        if !self.wants_any(signal.kind()) {
            return None;
        }
        let effects = self.emit(signal, frame).ok()?;
        let mut items: Vec<(i16, String)> = effects
            .into_iter()
            .filter_map(|effect| match effect {
                Effect::Context { text, priority } => Some((priority, text)),
                Effect::Note { text } => Some((0, text)),
                _ => None,
            })
            .filter(|(_, text)| !text.trim().is_empty())
            .collect();
        if items.is_empty() {
            return None;
        }
        items.sort_by_key(|(priority, _)| std::cmp::Reverse(*priority));

        let mut out = String::new();
        for (_, text) in items {
            if !out.is_empty() && out.len() + text.len() + 1 > total_budget {
                break;
            }
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&text);
        }
        (!out.is_empty()).then_some(out)
    }

    /// Emit a tool "before" signal and return the first Block reason, if any.
    /// Lets behavioral shelves veto a tool call before it runs.
    pub(crate) fn tool_block_reason(
        &self,
        frame: &ShelfFrame,
        name: &str,
        input: &Value,
    ) -> Option<String> {
        if !self.wants_any(SignalKind::Tool) {
            return None;
        }
        let signal = Signal::Tool {
            phase: ToolPhase::Before,
            name: name.to_string(),
            input: input.clone(),
            output: None,
        };
        let effects = self.emit(&signal, frame).ok()?;
        effects.into_iter().find_map(|effect| match effect {
            Effect::Block { reason } => Some(reason),
            _ => None,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct ResolvedAbility {
    pub(crate) shelf: ShelfId,
    pub(crate) pack: PackId,
    pub(crate) origin: ShelfOrigin,
    pub(crate) ability: Ability,
}

fn wants_signal(manifest: &ShelfManifest, kind: SignalKind) -> bool {
    manifest.packs.iter().any(|pack| {
        pack.abilities.iter().any(|ability| match ability {
            Ability::Hook(hook) => hook.signals.contains(&kind),
            _ => false,
        })
    })
}

pub(crate) struct StaticShelf {
    manifest: ShelfManifest,
}

impl StaticShelf {
    pub(crate) fn new(manifest: ShelfManifest) -> Self {
        Self { manifest }
    }

    pub(crate) fn from_json_file(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading shelf manifest {}", path.display()))?;
        let mut manifest: ShelfManifest = serde_json::from_str(&text)
            .with_context(|| format!("parsing shelf manifest {}", path.display()))?;
        manifest
            .origin
            .path
            .get_or_insert_with(|| path.to_path_buf());
        Ok(Self { manifest })
    }
}

impl Shelf for StaticShelf {
    fn manifest(&self) -> &ShelfManifest {
        &self.manifest
    }

    /// A manifest-only shelf contributes its declared Context abilities as
    /// prompt context when a load/prompt signal fires. It carries no logic, so
    /// it never blocks or rewrites tools. Higher-scope shelves (project/run)
    /// inject at higher priority than core/user.
    fn on_signal(&self, signal: &Signal, _frame: &ShelfFrame) -> Result<Vec<Effect>> {
        match signal {
            Signal::Load | Signal::Prompt { .. } => {
                let priority = self.manifest.origin.scope.rank() as i16;
                let mut effects = Vec::new();
                for pack in &self.manifest.packs {
                    for ability in &pack.abilities {
                        if let Ability::Context(ctx) = ability {
                            let text = format!("{}: {}", ctx.name, empty_label(&ctx.description));
                            // budget is a token hint; cap injected bytes to ~4x.
                            let cap = ctx.budget.saturating_mul(4).clamp(64, 8_192);
                            effects.push(Effect::Context {
                                text: cap_chars_on_boundary(&text, cap),
                                priority,
                            });
                        }
                    }
                }
                Ok(effects)
            }
            _ => Ok(Vec::new()),
        }
    }
}

fn cap_chars_on_boundary(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

struct ShelfManifestCandidate {
    path: PathBuf,
    origin: ShelfOrigin,
}

fn shelf_manifest_candidates(root: &Path) -> Vec<ShelfManifestCandidate> {
    let mut candidates = Vec::new();
    let mut seen = HashSet::new();

    let bundled_shelves = Path::new(env!("CARGO_MANIFEST_DIR")).join("shelves");
    push_shelf_manifest_root(
        &mut candidates,
        &mut seen,
        bundled_shelves,
        ShelfScope::Core,
    );
    push_shelf_manifest_root(
        &mut candidates,
        &mut seen,
        crate::session::dext_state_dir().join("shelves"),
        ShelfScope::User,
    );
    push_shelf_manifest_root(
        &mut candidates,
        &mut seen,
        root.join(".dext/shelves"),
        ShelfScope::Project,
    );
    if let Some(paths) = std::env::var_os("DEXT_SHELVES_DIR") {
        for path in std::env::split_paths(&paths) {
            push_shelf_manifest_root(&mut candidates, &mut seen, path, ShelfScope::Run);
        }
    }

    candidates
}

fn push_shelf_manifest_root(
    candidates: &mut Vec<ShelfManifestCandidate>,
    seen: &mut HashSet<PathBuf>,
    shelf_root: PathBuf,
    scope: ShelfScope,
) {
    if !shelf_root.is_dir() {
        return;
    }
    push_shelf_manifest_dir(candidates, seen, &shelf_root, scope);
    let Ok(shelves) = std::fs::read_dir(&shelf_root) else {
        return;
    };
    let mut shelves = shelves.flatten().collect::<Vec<_>>();
    shelves.sort_by_key(|entry| entry.path());
    for shelf in shelves {
        let is_dir = shelf.file_type().map(|ft| ft.is_dir()).unwrap_or(false);
        if is_dir {
            push_shelf_manifest_dir(candidates, seen, &shelf.path(), scope);
        }
    }
}

fn push_shelf_manifest_dir(
    candidates: &mut Vec<ShelfManifestCandidate>,
    seen: &mut HashSet<PathBuf>,
    shelf_dir: &Path,
    scope: ShelfScope,
) {
    let path = shelf_dir.join("shelf.json");
    if !path.is_file() {
        return;
    }
    let key = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
    if !seen.insert(key) {
        return;
    }
    candidates.push(ShelfManifestCandidate {
        origin: ShelfOrigin {
            scope,
            path: Some(path.clone()),
        },
        path,
    });
}

pub(crate) fn render_registry_listing(registry: &ShelfRegistry) -> String {
    use std::fmt::Write as _;
    let opts = crate::list_render::ListOptions::detect(false);

    if registry.is_empty() {
        return "Shelves  none found\nsearch paths: .dext/shelves/*/shelf.json, DEXT_SHELVES_DIR, ~/.dext/shelves/*/shelf.json, bundled shelves".to_string();
    }

    let manifests = registry.manifests();
    let mut out = String::new();
    let _ = write!(
        out,
        "{}",
        crate::list_render::render_header("Shelves", manifests.len(), &opts)
    );

    for manifest in manifests {
        let ability_count: usize = manifest.packs.iter().map(|pack| pack.abilities.len()).sum();
        let meta = vec![
            ("id", manifest.id.as_str().to_string()),
            ("scope", scope_label(manifest.origin.scope).to_string()),
            ("packs", manifest.packs.len().to_string()),
            ("abilities", ability_count.to_string()),
        ];
        out.push_str(&crate::list_render::render_entry(
            &manifest.name,
            empty_label(&manifest.description),
            &meta,
            &opts,
        ));
    }

    let resolved = registry.resolve();
    if !resolved.is_empty() {
        out.push('\n');
        let _ = writeln!(
            out,
            "{}",
            crate::list_render::bold("Resolved abilities", opts.color)
        );
        for ability in resolved.iter().take(50) {
            out.push_str(&format_resolved_ability_styled(ability, &opts));
        }
        if resolved.len() > 50 {
            let _ = writeln!(out, "  … [{} more abilities omitted]", resolved.len() - 50);
        }
    }
    out.push_str(&crate::list_render::render_footer(
        &["/shelves", "dext shelves"],
        &opts,
    ));
    out
}

pub(crate) fn registry_summary_for_prompt(registry: &ShelfRegistry) -> Option<String> {
    if registry.is_empty() {
        return None;
    }
    let manifests = registry.manifests();
    let resolved = registry.resolve();
    let mut out = format!(
        "Typed shelf registry: {} shelf(s), {} resolved ability metadata entr{}.",
        manifests.len(),
        resolved.len(),
        if resolved.len() == 1 { "y" } else { "ies" }
    );
    if !resolved.is_empty() {
        out.push_str(" Available: ");
        out.push_str(
            &resolved
                .iter()
                .take(10)
                .map(compact_resolved_ability)
                .collect::<Vec<_>>()
                .join("; "),
        );
        if resolved.len() > 10 {
            out.push_str(&format!("; … +{}", resolved.len() - 10));
        }
        out.push('.');
    }
    out.push_str(" These are provider-neutral registry records, not extra provider-visible tools; use normal Dext tools or pack-local helpers to act on them.");
    Some(out)
}

fn compact_resolved_ability(resolved: &ResolvedAbility) -> String {
    let (kind, name) = resolved.ability.key();
    format!(
        "{kind}:{name} ({}/{}, {})",
        resolved.shelf.as_str(),
        resolved.pack.as_str(),
        ability_short_description(&resolved.ability)
    )
}

fn format_resolved_ability(resolved: &ResolvedAbility, indent: &str) -> String {
    let (kind, name) = resolved.ability.key();
    format!(
        "{indent}{kind}:{name} — {}\n{indent}  shelf: {} · pack: {} · scope: {}",
        ability_long_description(&resolved.ability),
        resolved.shelf.as_str(),
        resolved.pack.as_str(),
        scope_label(resolved.origin.scope)
    )
}

fn format_resolved_ability_styled(
    resolved: &ResolvedAbility,
    opts: &crate::list_render::ListOptions,
) -> String {
    let (kind, name) = resolved.ability.key();
    let title = format!("{kind}:{name}");
    let desc = ability_long_description(&resolved.ability);
    let meta = vec![
        ("shelf", resolved.shelf.as_str().to_string()),
        ("pack", resolved.pack.as_str().to_string()),
        ("scope", scope_label(resolved.origin.scope).to_string()),
    ];
    crate::list_render::render_entry(&title, &desc, &meta, opts)
}

fn ability_short_description(ability: &Ability) -> String {
    match ability {
        Ability::Tool(tool) => empty_label(&tool.description).to_string(),
        Ability::Command(command) => empty_label(&command.description).to_string(),
        Ability::Hook(hook) => format!("signals {}", signal_list(&hook.signals)),
        Ability::Context(context) => format!(
            "{}, budget {}",
            empty_label(&context.description),
            context.budget
        ),
    }
}

fn ability_long_description(ability: &Ability) -> String {
    match ability {
        Ability::Tool(tool) => format!(
            "{} · exposure: {} · grants: {}",
            empty_label(&tool.description),
            exposure_label(tool.exposure),
            grant_list(&tool.grants)
        ),
        Ability::Command(command) => format!(
            "{} · usage: {}",
            empty_label(&command.description),
            empty_label(&command.usage)
        ),
        Ability::Hook(hook) => format!("signals {}", signal_list(&hook.signals)),
        Ability::Context(context) => format!(
            "{} · budget: {}",
            empty_label(&context.description),
            context.budget
        ),
    }
}

fn empty_label(value: &str) -> &str {
    if value.trim().is_empty() {
        "(none)"
    } else {
        value.trim()
    }
}

fn scope_label(scope: ShelfScope) -> &'static str {
    match scope {
        ShelfScope::Core => "core",
        ShelfScope::User => "user",
        ShelfScope::Project => "project",
        ShelfScope::Run => "run",
    }
}

fn exposure_label(exposure: Exposure) -> &'static str {
    match exposure {
        Exposure::Hidden => "hidden",
        Exposure::OnDemand => "on_demand",
        Exposure::Visible => "visible",
    }
}

fn grant_label(grant: Grant) -> &'static str {
    match grant {
        Grant::Read => "read",
        Grant::Write => "write",
        Grant::Network => "network",
        Grant::Process => "process",
        Grant::Secret => "secret",
        Grant::Browser => "browser",
    }
}

fn signal_label(signal: SignalKind) -> &'static str {
    match signal {
        SignalKind::Load => "load",
        SignalKind::Prompt => "prompt",
        SignalKind::Tool => "tool",
        SignalKind::Turn => "turn",
        SignalKind::Compact => "compact",
        SignalKind::Shutdown => "shutdown",
    }
}

fn grant_list(grants: &[Grant]) -> String {
    if grants.is_empty() {
        return "none".to_string();
    }
    grants
        .iter()
        .map(|grant| grant_label(*grant))
        .collect::<Vec<_>>()
        .join(",")
}

fn signal_list(signals: &[SignalKind]) -> String {
    if signals.is_empty() {
        return "none".to_string();
    }
    signals
        .iter()
        .map(|signal| signal_label(*signal))
        .collect::<Vec<_>>()
        .join(",")
}

pub(crate) fn shelf_path(root: &Path, id: &ShelfId) -> PathBuf {
    root.join(".dext").join("shelves").join(id.as_str())
}

pub(crate) fn pack_path(root: &Path, shelf: &ShelfId, pack: &PackId) -> PathBuf {
    shelf_path(root, shelf).join("packs").join(pack.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    struct GuardShelf {
        manifest: ShelfManifest,
    }

    impl GuardShelf {
        fn new() -> Self {
            Self {
                manifest: ShelfManifest {
                    id: ShelfId::new("core").unwrap(),
                    name: "core".to_string(),
                    description: "built-in Dext shelf".to_string(),
                    origin: ShelfOrigin::core(),
                    mode: ShelfMode::Always,
                    packs: vec![PackManifest {
                        id: PackId::new("guard").unwrap(),
                        name: "guard".to_string(),
                        version: "0.0.0".to_string(),
                        description: "blocks dangerous process calls".to_string(),
                        abilities: vec![Ability::Hook(HookAbility {
                            name: "process_guard".to_string(),
                            signals: vec![SignalKind::Tool],
                        })],
                    }],
                },
            }
        }
    }

    impl Shelf for GuardShelf {
        fn manifest(&self) -> &ShelfManifest {
            &self.manifest
        }

        fn on_signal(&self, signal: &Signal, _frame: &ShelfFrame) -> Result<Vec<Effect>> {
            let Signal::Tool {
                phase: ToolPhase::Before,
                name,
                input,
                ..
            } = signal
            else {
                return Ok(Vec::new());
            };
            let command = input["command"].as_str().unwrap_or_default();
            if name == "bash" && command.contains("rm -rf") {
                return Ok(vec![Effect::Block {
                    reason: "destructive command blocked by pack".to_string(),
                }]);
            }
            Ok(Vec::new())
        }
    }

    fn search_shelf(
        scope: ShelfScope,
        shelf_id: &str,
        pack_id: &str,
        description: &str,
    ) -> StaticShelf {
        StaticShelf::new(ShelfManifest {
            id: ShelfId::new(shelf_id).unwrap(),
            name: shelf_id.to_string(),
            description: format!("{shelf_id} marketplace"),
            origin: ShelfOrigin { scope, path: None },
            mode: ShelfMode::Always,
            packs: vec![PackManifest {
                id: PackId::new(pack_id).unwrap(),
                name: pack_id.to_string(),
                version: "0.0.0".to_string(),
                description: description.to_string(),
                abilities: vec![Ability::Tool(ToolAbility {
                    name: "search".to_string(),
                    description: description.to_string(),
                    schema: json!({"type": "object"}),
                    grants: vec![Grant::Read],
                    exposure: Exposure::OnDemand,
                })],
            }],
        })
    }

    #[test]
    fn shelf_and_pack_ids_validate_segments() {
        assert!(ShelfId::new("community").is_ok());
        assert!(ShelfId::new("open/research").is_ok());
        assert!(ShelfId::new("Open/Search").is_err());
        assert!(ShelfId::new("open//search").is_err());
        assert!(ShelfId::new("open search").is_err());

        assert!(PackId::new("autoresearch").is_ok());
        assert!(PackId::new("AutoResearch").is_err());
    }

    #[test]
    fn project_shelf_pack_overrides_core_pack_ability() {
        let mut registry = ShelfRegistry::new();
        registry.register(search_shelf(
            ShelfScope::Core,
            "core",
            "search",
            "core search",
        ));
        registry.register(search_shelf(
            ShelfScope::Project,
            "project",
            "search",
            "project search",
        ));

        let resolved = registry.resolve();
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].shelf, ShelfId::new("project").unwrap());
        assert_eq!(resolved[0].pack, PackId::new("search").unwrap());
        match &resolved[0].ability {
            Ability::Tool(tool) => assert_eq!(tool.description, "project search"),
            other => panic!("expected tool, got {other:?}"),
        }
    }

    #[test]
    fn pack_hook_can_block_tool_flow() {
        let mut registry = ShelfRegistry::new();
        registry.register(GuardShelf::new());
        let effects = registry
            .emit(
                &Signal::Tool {
                    phase: ToolPhase::Before,
                    name: "bash".to_string(),
                    input: json!({"command": "rm -rf target"}),
                    output: None,
                },
                &ShelfFrame::new("."),
            )
            .unwrap();

        assert!(matches!(effects.as_slice(), [Effect::Block { .. }]));
    }

    #[test]
    fn non_hook_packs_do_not_receive_signals() {
        let mut registry = ShelfRegistry::new();
        registry.register(search_shelf(
            ShelfScope::Core,
            "core",
            "search",
            "core search",
        ));
        let effects = registry
            .emit(
                &Signal::Prompt {
                    text: "hello".to_string(),
                },
                &ShelfFrame::new("."),
            )
            .unwrap();
        assert!(effects.is_empty());
    }

    #[test]
    fn tool_block_reason_surfaces_guard_veto() {
        let mut registry = ShelfRegistry::new();
        registry.register(GuardShelf::new());
        let reason = registry.tool_block_reason(
            &ShelfFrame::new("."),
            "bash",
            &json!({"command": "rm -rf target"}),
        );
        assert_eq!(
            reason.as_deref(),
            Some("destructive command blocked by pack")
        );
        // A benign command is not vetoed.
        assert!(
            registry
                .tool_block_reason(&ShelfFrame::new("."), "bash", &json!({"command": "ls"}))
                .is_none()
        );
    }

    fn context_shelf(with_load_hook: bool) -> StaticShelf {
        let mut abilities = vec![Ability::Context(ContextAbility {
            name: "house-rules".to_string(),
            description: "always prefer rg over grep".to_string(),
            budget: 256,
        })];
        if with_load_hook {
            abilities.push(Ability::Hook(HookAbility {
                name: "loader".to_string(),
                signals: vec![SignalKind::Load],
            }));
        }
        StaticShelf::new(ShelfManifest {
            id: ShelfId::new("proj").unwrap(),
            name: "proj".to_string(),
            description: "project shelf".to_string(),
            origin: ShelfOrigin {
                scope: ShelfScope::Project,
                path: None,
            },
            mode: ShelfMode::Always,
            packs: vec![PackManifest {
                id: PackId::new("rules").unwrap(),
                name: "rules".to_string(),
                version: "0.0.0".to_string(),
                description: "house rules".to_string(),
                abilities,
            }],
        })
    }

    #[test]
    fn collect_context_injects_only_when_a_load_hook_opts_in() {
        // Context ability + a load-signal hook → injected.
        let mut registry = ShelfRegistry::new();
        registry.register(context_shelf(true));
        let block = registry.collect_context(&Signal::Load, &ShelfFrame::new("."), 1_000);
        assert!(
            block
                .as_deref()
                .is_some_and(|b| b.contains("always prefer rg over grep")),
            "{block:?}"
        );

        // Same Context ability but no hook → the loop stays silent.
        let mut registry = ShelfRegistry::new();
        registry.register(context_shelf(false));
        assert!(
            registry
                .collect_context(&Signal::Load, &ShelfFrame::new("."), 1_000)
                .is_none()
        );
    }

    #[test]
    fn packs_live_under_shelves_on_disk() {
        let root = Path::new("/repo");
        let shelf = ShelfId::new("community").unwrap();
        let pack = PackId::new("autoresearch").unwrap();
        assert_eq!(
            pack_path(root, &shelf, &pack),
            PathBuf::from("/repo/.dext/shelves/community/packs/autoresearch")
        );
    }

    #[test]
    fn static_shelf_loads_manifest_from_json_file() {
        let root = std::env::temp_dir().join(format!(
            "dext-shelf-manifest-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let manifest_path = root.join("shelf.json");
        std::fs::write(
            &manifest_path,
            r#"{
  "id": "community",
  "name": "Community",
  "description": "shared packs",
  "origin": {"scope": "project"},
  "mode": "always",
  "packs": [{
    "id": "research",
    "name": "Research",
    "version": "0.1.0",
    "description": "research helpers",
    "abilities": [{"ability": "context", "name": "notes", "description": "curated notes", "budget": 1024}]
  }]
}"#,
        )
        .unwrap();

        let shelf = StaticShelf::from_json_file(&manifest_path).unwrap();
        let manifest = shelf.manifest();
        assert_eq!(manifest.id, ShelfId::new("community").unwrap());
        assert_eq!(manifest.packs[0].id, PackId::new("research").unwrap());
        assert_eq!(
            manifest.origin.path.as_deref(),
            Some(manifest_path.as_path())
        );
        match &manifest.packs[0].abilities[0] {
            Ability::Context(context) => assert_eq!(context.budget, 1024),
            other => panic!("expected context ability, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn registry_discovers_manifests_and_resolves_precedence() {
        let _guard = crate::test_env_lock().lock().unwrap();
        let root = std::env::temp_dir().join(format!(
            "dext-shelf-registry-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let project_shelf = root.join(".dext/shelves/project");
        let env_shelf = root.join("env-shelves/community");
        std::fs::create_dir_all(&project_shelf).unwrap();
        std::fs::create_dir_all(&env_shelf).unwrap();
        std::fs::write(
            project_shelf.join("shelf.json"),
            r#"{
  "id": "project",
  "name": "Project",
  "description": "project abilities",
  "packs": [{
    "id": "search",
    "name": "Search",
    "version": "0.1.0",
    "description": "search helpers",
    "abilities": [{"ability": "tool", "name": "search", "description": "project search", "schema": {"type": "object"}, "grants": ["read"], "exposure": "on_demand"}]
  }]
}"#,
        )
        .unwrap();
        std::fs::write(
            env_shelf.join("shelf.json"),
            r#"{
  "id": "community",
  "name": "Community",
  "description": "env abilities",
  "packs": [{
    "id": "search",
    "name": "Search",
    "version": "0.1.0",
    "description": "search helpers",
    "abilities": [{"ability": "tool", "name": "search", "description": "run search", "schema": {"type": "object"}, "grants": ["network"], "exposure": "visible"}]
  }]
}"#,
        )
        .unwrap();
        unsafe {
            std::env::set_var("DEXT_SHELVES_DIR", root.join("env-shelves"));
            std::env::set_var("DEXT_HOME", root.join("home"));
        }

        let registry = ShelfRegistry::discover(&root);
        assert_eq!(registry.manifests().len(), 2);
        let resolved = registry.resolve();
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].shelf, ShelfId::new("community").unwrap());
        assert_eq!(resolved[0].origin.scope, ShelfScope::Run);
        match &resolved[0].ability {
            Ability::Tool(tool) => assert_eq!(tool.description, "run search"),
            other => panic!("expected tool, got {other:?}"),
        }
        let listing = render_registry_listing(&registry);
        assert!(listing.contains("scope: run"), "{listing}");
        assert!(listing.contains("tool:search"), "{listing}");
        assert!(listing.contains("run search"), "{listing}");
        let summary = registry_summary_for_prompt(&registry).unwrap();
        assert!(
            summary.contains("tool:search (community/search, run search)"),
            "{summary}"
        );

        unsafe {
            std::env::remove_var("DEXT_HOME");
            std::env::remove_var("DEXT_SHELVES_DIR");
        }
        let _ = std::fs::remove_dir_all(root);
    }
}
