//! Built-in assistant registry — embeds the manifest + rule/avatar
//! assets into the binary via `include_dir`, with an optional filesystem
//! fallback for E2E tests.
//!
//! This kills the "binary must live next to an on-disk assets/ sibling"
//! assumption, which was fragile in two ways:
//!
//! 1. Dev: Electron launches the backend through a symlink
//!    (`~/.cargo/bin/aioncore` → `target/debug/aioncore`) and
//!    `std::env::current_exe().parent()` would resolve to the symlink's
//!    directory, not the real binary's, missing the `assets/` sibling.
//! 2. Prod: `AionUi/scripts/prepareAionuiBackend.js` only copies the
//!    binary from GitHub releases — the `assets/` directory never shipped.
//!
//! Embedding avoids both. E2E tests that want to inject a custom fixture
//! still can, via the `AIONUI_BUILTIN_ASSISTANTS_PATH` env var → disk
//! fallback path.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use include_dir::{Dir, include_dir};
use serde::Deserialize;
use serde_json::Value;
use tracing::{error, warn};

/// Assets compiled into the binary at build time. Paths are relative to
/// this embedded root, matching the on-disk layout under
/// `crates/aionui-app/assets/builtin-assistants/`.
static BUILTIN_ASSETS: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/../aionui-app/assets/builtin-assistants");

/// Single built-in assistant entry, loaded from `assistants.json`.
#[derive(Debug, Clone, Deserialize)]
pub struct BuiltinAssistant {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub name_i18n: HashMap<String, String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub description_i18n: HashMap<String, String>,
    #[serde(default)]
    pub avatar: Option<String>,
    pub agent_ref: String,
    #[serde(default)]
    pub enabled_skills: Vec<String>,
    #[serde(default)]
    pub custom_skill_names: Vec<String>,
    #[serde(default)]
    pub disabled_builtin_skills: Vec<String>,
    /// Relative to the asset root; may contain `{locale}`.
    #[serde(default)]
    pub rule_file: Option<String>,
    #[serde(default)]
    pub prompts: Vec<String>,
    #[serde(default)]
    pub prompts_i18n: HashMap<String, Vec<String>>,
    #[serde(default)]
    pub models: Vec<String>,
    /// Default position in the official assistant list. Lower comes first.
    /// Owned by this manifest (users cannot reorder official assistants), so
    /// this value is authoritative across versions. Defaults to 0.
    #[serde(default)]
    pub sort_order: i32,
    /// Whether this official assistant is enabled by default when a user has
    /// no overlay for it. Only the butler ships enabled; others default off so
    /// they don't crowd the user's selection lists. Defaults to false.
    #[serde(default)]
    pub default_enabled: bool,
}

#[derive(Debug, Deserialize)]
struct BuiltinManifest {
    #[serde(default)]
    #[allow(dead_code)]
    version: String,
    #[serde(default)]
    assistants: Vec<BuiltinAssistant>,
}

/// An avatar asset loaded from either the embedded bundle or a disk override.
///
/// Carries the raw bytes plus the file extension (lower-case, without the
/// leading dot) so the HTTP layer can set `Content-Type`.
#[derive(Debug, Clone)]
pub struct AvatarAsset {
    pub bytes: Vec<u8>,
    pub extension: Option<String>,
}

/// Source of built-in asset content.
///
/// The disk branch exists for E2E tests that point
/// `AIONUI_BUILTIN_ASSISTANTS_PATH` at a fixture directory.
enum Source {
    Embedded,
    Disk(PathBuf),
}

/// In-memory registry of built-in assistants.
pub struct BuiltinAssistantRegistry {
    assistants: HashMap<String, BuiltinAssistant>,
    source: Source,
}

impl BuiltinAssistantRegistry {
    /// Construct the registry.
    ///
    /// If `AIONUI_BUILTIN_ASSISTANTS_PATH` is set and points to a readable
    /// directory, read from disk (test-only override). Otherwise use the
    /// assets embedded at compile time.
    pub fn load() -> Self {
        if let Ok(env) = std::env::var("AIONUI_BUILTIN_ASSISTANTS_PATH") {
            let p = PathBuf::from(env);
            if p.exists() {
                return Self::load_from_dir(p);
            }
            warn!(
                "AIONUI_BUILTIN_ASSISTANTS_PATH points to missing directory; \
                 falling back to embedded assets"
            );
        }
        Self::load_embedded()
    }

    /// Load the compiled-in assets.
    pub fn load_embedded() -> Self {
        let content = match BUILTIN_ASSETS.get_file("assistants.json") {
            Some(f) => f.contents(),
            None => {
                // This can only happen if the embedded bundle itself is
                // missing the manifest — treat as a build error, but stay
                // graceful at runtime.
                error!("Embedded built-in manifest missing (assistants.json)");
                return Self::with_assistants(HashMap::new(), Source::Embedded);
            }
        };
        let assistants = parse_manifest_bytes(content);
        Self::with_assistants(assistants, Source::Embedded)
    }

    /// Load from an explicit on-disk directory. Preserved for
    /// `AIONUI_BUILTIN_ASSISTANTS_PATH` E2E fixtures — the three
    /// graceful-degradation branches below mirror the original filesystem
    /// behaviour.
    pub fn load_from_dir(assets_dir: PathBuf) -> Self {
        let manifest_path = assets_dir.join("assistants.json");
        let content = match std::fs::read_to_string(&manifest_path) {
            Ok(c) => c,
            Err(e) => {
                warn!("Built-in manifest missing at {}: {}", manifest_path.display(), e);
                return Self::with_assistants(HashMap::new(), Source::Disk(assets_dir));
            }
        };
        let assistants = parse_manifest_str(&content);
        Self::with_assistants(assistants, Source::Disk(assets_dir))
    }

    fn with_assistants(assistants: HashMap<String, BuiltinAssistant>, source: Source) -> Self {
        Self { assistants, source }
    }

    /// Construct an empty registry (safe fallback + test helper). Treated
    /// as embedded-source with zero entries; callers should prefer
    /// [`load`](Self::load) in production.
    pub fn empty() -> Self {
        Self::with_assistants(HashMap::new(), Source::Embedded)
    }

    pub fn has(&self, id: &str) -> bool {
        self.assistants.contains_key(id)
    }

    pub fn get(&self, id: &str) -> Option<&BuiltinAssistant> {
        self.assistants.get(id)
    }

    pub fn all(&self) -> impl Iterator<Item = &BuiltinAssistant> {
        self.assistants.values()
    }

    pub fn is_empty(&self) -> bool {
        self.assistants.is_empty()
    }

    pub fn len(&self) -> usize {
        self.assistants.len()
    }

    /// Read the rule file bytes for a built-in assistant. Substitutes
    /// `{locale}` in the manifest-declared `rule_file` path. Returns `None`
    /// when the assistant has no declared rule or the file is missing.
    pub fn rule_bytes(&self, id: &str, locale: &str) -> Option<Vec<u8>> {
        let rel = self.assistants.get(id)?.rule_file.as_ref()?;
        self.read_asset(&rel.replace("{locale}", locale))
    }

    /// Read the avatar asset for a built-in assistant along with its
    /// extension (for Content-Type inference). Returns `None` when the
    /// manifest does not declare an avatar or the file is missing.
    ///
    /// Note: when the manifest `avatar` field is an emoji string
    /// (like `"📝"`) rather than a relative path, no file is resolved and
    /// this method returns `None`. Callers treating an assistant without a
    /// shipped avatar should fall back to the text avatar on the client.
    pub fn avatar_asset(&self, id: &str) -> Option<AvatarAsset> {
        let a = self.assistants.get(id)?;
        let rel = a.avatar.as_ref()?;
        // Emoji / text avatars have no path separator and no extension.
        if !looks_like_relative_path(rel) {
            return None;
        }
        let bytes = self.read_asset(rel)?;
        let extension = Path::new(rel)
            .extension()
            .and_then(|e| e.to_str())
            .map(|s| s.to_ascii_lowercase());
        Some(AvatarAsset { bytes, extension })
    }

    /// Dispatch read to embedded bundle or disk, depending on source.
    fn read_asset(&self, rel: &str) -> Option<Vec<u8>> {
        match &self.source {
            Source::Embedded => BUILTIN_ASSETS.get_file(rel).map(|f| f.contents().to_vec()),
            Source::Disk(root) => std::fs::read(root.join(rel)).ok(),
        }
    }
}

impl Default for BuiltinAssistantRegistry {
    fn default() -> Self {
        Self::empty()
    }
}

fn parse_manifest_bytes(bytes: &[u8]) -> HashMap<String, BuiltinAssistant> {
    match serde_json::from_slice::<Value>(bytes).and_then(parse_manifest_value) {
        Ok(m) => m.assistants.into_iter().map(|a| (a.id.clone(), a)).collect(),
        Err(e) => {
            error!("Embedded built-in manifest parse failed: {e}");
            HashMap::new()
        }
    }
}

fn parse_manifest_str(content: &str) -> HashMap<String, BuiltinAssistant> {
    match serde_json::from_str::<Value>(content).and_then(parse_manifest_value) {
        Ok(m) => m.assistants.into_iter().map(|a| (a.id.clone(), a)).collect(),
        Err(e) => {
            error!("Built-in manifest parse failed: {e}");
            HashMap::new()
        }
    }
}

fn parse_manifest_value(value: Value) -> Result<BuiltinManifest, serde_json::Error> {
    if let Some(assistants) = value.get("assistants").and_then(Value::as_array) {
        for assistant in assistants {
            if assistant.get("skill_file").is_some() {
                return Err(serde_json::Error::io(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "builtin assistant legacy field `skill_file` is no longer supported",
                )));
            }
        }
    }
    serde_json::from_value(value)
}

/// Heuristic for distinguishing a relative-path avatar (`"rules/x.svg"`)
/// from an inline emoji/text avatar (`"📝"`). Path-like strings contain a
/// `/` or at least one `.` extension separator.
fn looks_like_relative_path(s: &str) -> bool {
    s.contains('/') || (Path::new(s).extension().is_some() && !s.starts_with('.'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write_manifest(dir: &Path, body: &str) {
        std::fs::write(dir.join("assistants.json"), body).unwrap();
    }

    // -----------------------------------------------------------------------
    // Embedded-source sanity: the bundle shipped with the crate must be
    // well-formed and non-empty. Acts as a compile-time → runtime bridge
    // guard (if the manifest is ever broken or the include_dir path is
    // wrong, this test fails immediately rather than at user-hit-404 time).
    // -----------------------------------------------------------------------

    #[test]
    fn load_embedded_has_expected_builtins() {
        let reg = BuiltinAssistantRegistry::load_embedded();
        assert!(!reg.is_empty(), "embedded registry should contain the shipped presets");
        // Sanity-check a couple of known ids from the committed manifest.
        assert!(reg.has("word-creator"));
        assert!(reg.has("cowork"));
    }

    #[test]
    fn load_embedded_rule_bytes_available_for_shipped_preset() {
        let reg = BuiltinAssistantRegistry::load_embedded();
        let bytes = reg
            .rule_bytes("word-creator", "en-US")
            .expect("shipped word-creator en-US rule should resolve from the embedded bundle");
        assert!(!bytes.is_empty());
        let text = std::str::from_utf8(&bytes).expect("rule file should be valid utf-8");
        assert!(text.len() > 100, "rule file should have real content");
    }

    #[test]
    fn module_experts_ship_with_their_skill_avatar_and_both_rule_languages() {
        let reg = BuiltinAssistantRegistry::load_embedded();
        let skill = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../aionui-app/assets/builtin-skills/woody-module-expert/SKILL.md");
        let skill_text = std::fs::read_to_string(&skill).expect("the module expert skill ships with the experts");
        assert!(skill_text.starts_with("---\nname: woody-module-expert\n"));
        // Live runs missed entry points and protocol bits that only outside callers showed.
        assert!(skill_text.contains("look up the callers outside your files"));
        // Experts in team tasks often skipped loading it, so its whole text is preloaded.
        let frontmatter_end = skill_text[4..].find("\n---\n").expect("frontmatter closes") + 4;
        assert!(skill_text[..frontmatter_end].contains("\npreload: true"));
        // Preloading removed the first load call, and the codebase skill used to ride along with it.
        assert!(skill_text.contains(
            "Your first action, before any search or file read, is to load the `woody-codebase-memory` skill"
        ));
        for id in [
            "woody-alarm-expert",
            "woody-profile-expert",
            "woody-display-expert",
            "woody-status-expert",
            "woody-qos-expert",
            "woody-clock-expert",
        ] {
            let expert = reg.get(id).unwrap_or_else(|| panic!("{id} is in the manifest"));
            assert_eq!(expert.agent_ref, "aionrs", "{id}");
            assert!(expert.default_enabled, "{id}");
            assert_eq!(expert.enabled_skills, vec!["woody-module-expert".to_owned()], "{id}");
            assert!(reg.avatar_asset(id).is_some(), "{id} avatar");
            for locale in ["en-US", "zh-CN"] {
                let rule = reg
                    .rule_bytes(id, locale)
                    .unwrap_or_else(|| panic!("{id} {locale} rule"));
                let text = String::from_utf8(rule).expect("utf-8 rule");
                assert!(
                    text.contains("woody-module-expert"),
                    "{id} {locale} rule loads the skill"
                );
                // In live team runs most experts started on the task without the skill.
                let in_team = if locale == "zh-CN" {
                    "包括在团队里收到任务或消息时"
                } else {
                    "including when a task or a message reaches you in a team"
                };
                assert!(
                    text.contains(in_team),
                    "{id} {locale} rule loads the skill in a team too"
                );
                let codebase_first = if locale == "zh-CN" {
                    "你的第一个动作是用 `skills show` 加载 `woody-codebase-memory` 技能"
                } else {
                    "Your first action is to load the `woody-codebase-memory` skill with `skills show`"
                };
                assert!(
                    text.contains(codebase_first),
                    "{id} {locale} rule loads the codebase skill first"
                );
            }
        }
    }

    #[test]
    fn module_expert_skill_requires_comprehensive_feature_cards() {
        let skill = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../aionui-app/assets/builtin-skills/woody-module-expert/SKILL.md");
        let text = std::fs::read_to_string(&skill).expect("expert skill");
        for phrase in [
            "alarms, load profile, display, status word, QoS, clock",
            "never `woody-*-expert.md`",
            "Account for every `.c` and `.h` file you own",
            "full lifecycle",
            "Core data structures & state machines",
            "positional initializers",
            "every copy or factory-reset site",
            "field NVM upgrade compatibility",
            "Protocol objects & who changes parameters",
            "external globals this module reads or writes",
            "concurrency or reentrancy constraints",
            "without repeating the leading `# <Expert Name>` H1 title",
            "files checked",
            "user notes alone never count as a completed card",
            "every owned file is accounted for and every required section above is present",
        ] {
            assert!(text.contains(phrase), "expert skill requires: {phrase}");
        }
    }

    #[test]
    fn alarm_expert_owns_event_logs_and_tamper_unless_experts_json_says_otherwise() {
        let reg = BuiltinAssistantRegistry::load_embedded();
        for (locale, words) in [
            (
                "en-US",
                [
                    "event logs",
                    "tamper detection",
                    "`event`",
                    "`tamper`",
                    "follow the list",
                    // The protocol's alarm bit is often built in the status module.
                    "alarm descriptor",
                ],
            ),
            (
                "zh-CN",
                ["事件记录", "防窃电", "`event`", "`tamper`", "以清单为准", "告警描述符"],
            ),
        ] {
            let rule = reg
                .rule_bytes("woody-alarm-expert", locale)
                .unwrap_or_else(|| panic!("alarm {locale} rule"));
            let text = String::from_utf8(rule).expect("utf-8 rule");
            for word in words {
                assert!(text.contains(word), "alarm {locale} rule mentions {word}");
            }
        }
        let alarm = reg.get("woody-alarm-expert").expect("alarm expert");
        for locale in ["en-US", "zh-CN"] {
            let prompts = &alarm.prompts_i18n[locale];
            assert_eq!(prompts.len(), 4, "{locale} prompts");
            assert!(
                prompts.iter().any(|p| p.contains("cover-open") || p.contains("开盖")),
                "{locale} has a tamper prompt"
            );
        }
    }

    #[test]
    fn profile_expert_owns_billing_and_its_schedule_unless_experts_json_says_otherwise() {
        let reg = BuiltinAssistantRegistry::load_embedded();
        for (locale, words) in [
            (
                "en-US",
                [
                    "billing profile",
                    "Single Action Schedule",
                    "`billing`",
                    "`schedule`",
                    "follow the list",
                    // Billing is also caught up after a time setting, not only at power-up.
                    "time-setting catch-up",
                ],
            ),
            (
                "zh-CN",
                [
                    "结算曲线",
                    "单动作调度",
                    "`billing`",
                    "`schedule`",
                    "以清单为准",
                    "校时补结算",
                ],
            ),
        ] {
            let rule = reg
                .rule_bytes("woody-profile-expert", locale)
                .unwrap_or_else(|| panic!("profile {locale} rule"));
            let text = String::from_utf8(rule).expect("utf-8 rule");
            for word in words {
                assert!(text.contains(word), "profile {locale} rule mentions {word}");
            }
        }
        // The clock expert keeps the trigger but hands billing to its owner.
        for locale in ["en-US", "zh-CN"] {
            let rule = reg.rule_bytes("woody-clock-expert", locale).expect("clock rule");
            let text = String::from_utf8(rule).expect("utf-8 rule");
            assert!(
                text.contains("usually the profile expert") || text.contains("通常是曲线专家"),
                "clock {locale} rule names who owns billing"
            );
        }
        let profile = reg.get("woody-profile-expert").expect("profile expert");
        for locale in ["en-US", "zh-CN"] {
            let prompts = &profile.prompts_i18n[locale];
            assert_eq!(prompts.len(), 4, "{locale} prompts");
            assert!(
                prompts.iter().any(|p| p.contains("billing") || p.contains("结算")),
                "{locale} has a billing prompt"
            );
        }
    }

    #[test]
    fn status_module_files_stay_with_the_status_expert_although_every_module_calls_them() {
        // A lead once doubted who owns the status module because every module calls its getters.
        let reg = BuiltinAssistantRegistry::load_embedded();
        for (id, locale, word) in [
            ("woody-status-expert", "en-US", "does not make its files shared"),
            ("woody-status-expert", "zh-CN", "不会因此变成共享文件"),
            ("woody-meter-lead", "en-US", "not by who calls it"),
            ("woody-meter-lead", "zh-CN", "不按谁调用它"),
            // A live lead named the status expert, then still marked the work unowned at the end.
            ("woody-meter-lead", "en-US", "the final summary included"),
            ("woody-meter-lead", "zh-CN", "包括最后的汇总"),
            // It then also proposed an expert that does not exist for the protocol object layer.
            ("woody-meter-lead", "en-US", "have no built-in expert"),
            ("woody-meter-lead", "zh-CN", "仍是无人负责"),
            ("woody-alarm-expert", "en-US", "which belongs to the status expert"),
            ("woody-alarm-expert", "zh-CN", "状态模块（归状态专家）"),
        ] {
            let rule = reg
                .rule_bytes(id, locale)
                .unwrap_or_else(|| panic!("{id} {locale} rule"));
            let text = String::from_utf8(rule).expect("utf-8 rule");
            assert!(text.contains(word), "{id} {locale} rule says {word}");
        }
        let skill = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../aionui-app/assets/builtin-skills/woody-meter-business/SKILL.md");
        let skill_text = std::fs::read_to_string(&skill).expect("meter business skill");
        assert!(skill_text.contains("Ownership follows what a module is for, not who calls it."));
        assert!(skill_text.contains("Name that expert as the proposed owner"));
        assert!(skill_text.contains("the last one included"));
    }

    #[test]
    fn review_findings_are_in_the_expert_and_lead_rules() {
        // A live review found experts naming switches without values, a display expert
        // confusing a live tamper copy with the latched register, a QoS expert missing a
        // compiled-out freeze path, and a lead that read a requirement table's columns swapped.
        let reg = BuiltinAssistantRegistry::load_embedded();
        for (id, locale, word) in [
            ("woody-display-expert", "en-US", "whether it is live or latched"),
            ("woody-display-expert", "zh-CN", "它是实时还是锁存"),
            ("woody-qos-expert", "en-US", "keeps a stale last-period value"),
            ("woody-qos-expert", "zh-CN", "上一周期的值会一直停在旧值"),
            ("woody-qos-expert", "en-US", "closes that period at power-up"),
            ("woody-qos-expert", "zh-CN", "上电后负荷曲线会关闭这个周期"),
            ("woody-profile-expert", "en-US", "name each callback and its owner"),
            ("woody-profile-expert", "zh-CN", "说出每个回调和它的负责人"),
            (
                "woody-profile-expert",
                "en-US",
                "check the days or entry-count macros too",
            ),
            ("woody-profile-expert", "zh-CN", "同时检查天数或条目数宏"),
            (
                "woody-clock-expert",
                "en-US",
                "say that a compiled-out entry does not exist",
            ),
            ("woody-clock-expert", "zh-CN", "编译掉的入口写明不存在"),
            (
                "woody-clock-expert",
                "en-US",
                "search the set handlers and the command handlers",
            ),
            ("woody-clock-expert", "zh-CN", "先查协议对象的设置函数和命令处理"),
            (
                "woody-meter-lead",
                "en-US",
                "prints every data row as `header name = value` pairs",
            ),
            ("woody-meter-lead", "zh-CN", "把每一行数据打印成“表头名 = 值”的形式"),
            ("woody-meter-lead", "en-US", "say which verdicts you checked yourself"),
            ("woody-meter-lead", "zh-CN", "汇报里写明哪些结论是你复核过的"),
            ("woody-meter-lead", "en-US", "name the column to judge by"),
            ("woody-meter-lead", "zh-CN", "照抄表头名写明按哪一列判断"),
            (
                "woody-meter-lead",
                "en-US",
                "write in a \"Checked\" column of the report",
            ),
            ("woody-meter-lead", "zh-CN", "在报告的“复核”列写出你打开的"),
            (
                "woody-meter-lead",
                "en-US",
                "checking is your own job, not implementation work to hand out",
            ),
            (
                "woody-meter-lead",
                "zh-CN",
                "复核是你自己的工作，不算交给成员的实现工作",
            ),
            ("woody-meter-lead", "en-US", "code that is compiled out does not count"),
            ("woody-meter-lead", "zh-CN", "编译不进去的代码不算支持"),
            // Two live runs wrote summary counts that did not match the verdict table.
            ("woody-meter-lead", "en-US", "by counting its rows one by one"),
            ("woody-meter-lead", "zh-CN", "要在逐条表格写完后按表格逐行数出来再写"),
        ] {
            let rule = reg
                .rule_bytes(id, locale)
                .unwrap_or_else(|| panic!("{id} {locale} rule"));
            let text = String::from_utf8(rule).expect("utf-8 rule");
            assert!(text.contains(word), "{id} {locale} rule says {word}");
        }
        let skills = Path::new(env!("CARGO_MANIFEST_DIR")).join("../aionui-app/assets/builtin-skills");
        let expert = std::fs::read_to_string(skills.join("woody-module-expert/SKILL.md")).expect("expert skill");
        for word in [
            "read the switch's `#define` in the configuration header",
            "Never write \"if it is enabled\"",
            "give the value for each variant",
            "whether it follows the live condition or stays set until something clears it",
            "search those handlers too",
            "Open each caller line",
            "write \"no owner\", and never name an expert that does not exist",
            "Before your first edit",
            "Never infer a unit from the comparison itself.",
            "search the driver and board configuration too",
            "ask the dispatcher which column holds the project's requirement",
            "never use an extract file another agent wrote",
            "Grep for `define <NAME>` across the whole workspace",
        ] {
            assert!(expert.contains(word), "expert skill says {word}");
        }
        let business = std::fs::read_to_string(skills.join("woody-meter-business/SKILL.md")).expect("business skill");
        for word in [
            "prints every data row as `header name = value` pairs",
            "a garbled or failed printout means you have not read the table yet",
            "exactly as the script printed it, header names included",
            "Before you pass an expert's verdict on to the user, check it.",
            "the header name of the column to judge by",
            "give each such verdict a \"Checked\" mark in the report",
            "In a team this is your own job, not implementation work to hand out.",
            "code that is compiled out does not count",
            "only after the verdict table is written, by counting its rows one by one",
        ] {
            assert!(business.contains(word), "business skill says {word}");
        }
    }

    #[test]
    fn meter_lead_ships_with_its_skill_avatar_and_both_rule_languages() {
        let reg = BuiltinAssistantRegistry::load_embedded();
        let skill = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../aionui-app/assets/builtin-skills/woody-meter-business/SKILL.md");
        let skill_text = std::fs::read_to_string(&skill).expect("the meter business skill ships with the lead");
        assert!(skill_text.starts_with("---\nname: woody-meter-business\n"));

        let lead = reg.get("woody-meter-lead").expect("the meter lead is in the manifest");
        assert_eq!(lead.agent_ref, "aionrs");
        assert!(lead.default_enabled);
        assert_eq!(lead.enabled_skills, vec!["woody-meter-business".to_owned()]);
        assert!(reg.avatar_asset("woody-meter-lead").is_some(), "lead avatar");
        for locale in ["en-US", "zh-CN"] {
            let rule = reg
                .rule_bytes("woody-meter-lead", locale)
                .unwrap_or_else(|| panic!("lead {locale} rule"));
            let text = String::from_utf8(rule).expect("utf-8 rule");
            assert!(
                text.contains("woody-meter-business"),
                "lead {locale} rule loads the skill"
            );
        }
        for expert in [
            "woody-alarm-expert",
            "woody-profile-expert",
            "woody-display-expert",
            "woody-status-expert",
            "woody-qos-expert",
            "woody-clock-expert",
        ] {
            let expert = reg.get(expert).expect("expert is in the manifest");
            assert!(
                lead.sort_order < expert.sort_order,
                "the lead is listed before {}",
                expert.id
            );
        }
    }

    #[test]
    fn embedded_rule_missing_locale_returns_none() {
        let reg = BuiltinAssistantRegistry::load_embedded();
        // The manifest declares rule_file as "rules/{id}.{locale}.md"; a
        // made-up locale can't resolve.
        assert!(reg.rule_bytes("word-creator", "xx-YY").is_none());
    }

    // -----------------------------------------------------------------------
    // Disk-source fallback (used by E2E fixtures via
    // AIONUI_BUILTIN_ASSISTANTS_PATH). Graceful-degradation semantics must
    // stay intact.
    // -----------------------------------------------------------------------

    #[test]
    fn load_from_dir_missing_dir_returns_empty() {
        let tmp = TempDir::new().unwrap();
        let missing = tmp.path().join("nope");
        let reg = BuiltinAssistantRegistry::load_from_dir(missing);
        assert!(reg.is_empty());
    }

    #[test]
    fn load_from_dir_missing_manifest_returns_empty() {
        let tmp = TempDir::new().unwrap();
        let reg = BuiltinAssistantRegistry::load_from_dir(tmp.path().to_path_buf());
        assert!(reg.is_empty());
    }

    #[test]
    fn load_from_dir_malformed_manifest_returns_empty() {
        let tmp = TempDir::new().unwrap();
        write_manifest(tmp.path(), "{not valid json");
        let reg = BuiltinAssistantRegistry::load_from_dir(tmp.path().to_path_buf());
        assert!(reg.is_empty());
    }

    #[test]
    fn load_from_dir_rejects_legacy_skill_file_entries() {
        let tmp = TempDir::new().unwrap();
        write_manifest(
            tmp.path(),
            r#"{
              "version": "1.0.0",
              "assistants": [{
                "id": "legacy",
                "name": "Legacy",
                "agent_ref": "gemini",
                "skill_file": "skills/legacy.en-US.md"
              }]
            }"#,
        );
        let reg = BuiltinAssistantRegistry::load_from_dir(tmp.path().to_path_buf());
        assert!(reg.is_empty(), "legacy skill_file manifest should be rejected");
    }

    #[test]
    fn load_from_dir_reads_bytes_from_disk() {
        let tmp = TempDir::new().unwrap();
        let rules_dir = tmp.path().join("rules");
        std::fs::create_dir_all(&rules_dir).unwrap();
        std::fs::write(rules_dir.join("office.en-US.md"), "office rule body").unwrap();
        write_manifest(
            tmp.path(),
            r#"{
                "version": "1.0.0",
                "assistants": [{
                    "id": "builtin-office",
                    "name": "Office",
                    "agent_ref": "gemini",
                    "rule_file": "rules/office.{locale}.md"
                }]
            }"#,
        );

        let reg = BuiltinAssistantRegistry::load_from_dir(tmp.path().to_path_buf());
        assert_eq!(reg.len(), 1);
        assert!(reg.has("builtin-office"));

        let bytes = reg
            .rule_bytes("builtin-office", "en-US")
            .expect("disk-source rule_bytes should read the fixture");
        assert_eq!(bytes, b"office rule body");
    }

    #[test]
    fn load_from_dir_missing_asset_returns_none() {
        let tmp = TempDir::new().unwrap();
        write_manifest(
            tmp.path(),
            r#"{
                "assistants": [{
                    "id": "x",
                    "name": "X",
                    "agent_ref": "gemini",
                    "rule_file": "rules/x.{locale}.md"
                }]
            }"#,
        );
        let reg = BuiltinAssistantRegistry::load_from_dir(tmp.path().to_path_buf());
        assert!(reg.rule_bytes("x", "en-US").is_none());
    }

    // -----------------------------------------------------------------------
    // load() env-var routing
    // -----------------------------------------------------------------------

    #[test]
    fn load_respects_env_var_disk_override() {
        let tmp = TempDir::new().unwrap();
        write_manifest(
            tmp.path(),
            r#"{"assistants":[{"id":"env-only","name":"E","agent_ref": "gemini"}]}"#,
        );
        // SAFETY: env-var mutation is only unsafe if another thread reads
        // environment concurrently. This test is self-contained.
        // SAFETY: cargo test runs tests in parallel by default, so guard
        // against interference from other tests by using a unique env-var
        // value and checking via a dedicated loader call.
        let key = "AIONUI_BUILTIN_ASSISTANTS_PATH";
        let prev = std::env::var(key).ok();
        // SAFETY: set_var is sound when no other thread is concurrently
        // reading env. Tests within this module do not share mutation, and
        // the env key is not observed by other tests.
        unsafe {
            std::env::set_var(key, tmp.path());
        }
        let reg = BuiltinAssistantRegistry::load();
        assert!(reg.has("env-only"));
        assert!(!reg.has("word-creator"));
        match prev {
            Some(v) => unsafe { std::env::set_var(key, v) },
            None => unsafe { std::env::remove_var(key) },
        }
    }

    // -----------------------------------------------------------------------
    // Avatar asset — emoji vs file
    // -----------------------------------------------------------------------

    #[test]
    fn avatar_asset_is_none_for_inline_emoji_avatar() {
        let reg = BuiltinAssistantRegistry::load_embedded();
        // word-form-creator still ships with an inline emoji avatar.
        assert!(reg.avatar_asset("word-form-creator").is_none());
    }

    #[test]
    fn embedded_avatar_asset_returns_bytes_and_extension_for_shipped_file_avatar() {
        let reg = BuiltinAssistantRegistry::load_embedded();
        let asset = reg
            .avatar_asset("word-creator")
            .expect("shipped word-creator avatar should resolve from the embedded bundle");
        assert!(!asset.bytes.is_empty());
        assert_eq!(asset.extension.as_deref(), Some("jpg"));
    }

    #[test]
    fn avatar_asset_returns_bytes_and_extension_for_file_avatar() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("duck.svg"), b"<svg/>").unwrap();
        write_manifest(
            tmp.path(),
            r#"{"assistants":[{
                "id": "with-file-avatar",
                "name": "F",
                "agent_ref": "gemini",
                "avatar": "duck.svg"
            }]}"#,
        );
        let reg = BuiltinAssistantRegistry::load_from_dir(tmp.path().to_path_buf());
        let asset = reg.avatar_asset("with-file-avatar").unwrap();
        assert_eq!(asset.bytes, b"<svg/>");
        assert_eq!(asset.extension.as_deref(), Some("svg"));
    }
}
