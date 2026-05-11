#![allow(dead_code)]

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
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

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ShelfMode {
    Passive,
    OnDemand,
    Always,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct ShelfManifest {
    pub(crate) id: ShelfId,
    pub(crate) name: String,
    pub(crate) description: String,
    pub(crate) origin: ShelfOrigin,
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

    pub(crate) fn register(&mut self, shelf: impl Shelf + 'static) {
        self.shelves.push(Box::new(shelf));
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
}

impl Shelf for StaticShelf {
    fn manifest(&self) -> &ShelfManifest {
        &self.manifest
    }
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
    fn packs_live_under_shelves_on_disk() {
        let root = Path::new("/repo");
        let shelf = ShelfId::new("community").unwrap();
        let pack = PackId::new("autoresearch").unwrap();
        assert_eq!(
            pack_path(root, &shelf, &pack),
            PathBuf::from("/repo/.dext/shelves/community/packs/autoresearch")
        );
    }
}
