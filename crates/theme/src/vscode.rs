//! Offline VS Code theme normalization and conversion.
//!
//! Conversion is intentionally deterministic and report-producing. Its output
//! is a curation draft, never a runtime dependency on a VSIX or remote source.

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::{
    AccentRoles, Appearance, Color, SurfaceTreatment, ThemeFamily, ThemeRegistry, ThemeSource,
    ThemeVariant, ValidationIssue,
};

const MAX_SOURCE_BYTES: u64 = 4 * 1024 * 1024;
const MAX_INCLUDE_DEPTH: usize = 32;
const MAX_PACKAGE_VARIANTS: usize = 128;

#[derive(Debug, Clone)]
pub struct ImportOptions {
    pub id: String,
    pub family_id: String,
    pub name: String,
    pub appearance: Appearance,
    pub source_url: String,
    pub revision: String,
    pub license: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportResult {
    pub theme: ThemeVariant,
    pub report: ImportReport,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportReport {
    pub source_files: Vec<String>,
    pub source_hash: String,
    pub mappings: Vec<ImportMapping>,
    pub fallbacks: Vec<String>,
    pub dropped: Vec<String>,
    pub warnings: Vec<String>,
    /// Deterministic repairs made after mapping so the compiled Zeron roles
    /// remain usable without erasing the source theme's syntax identity.
    #[serde(default)]
    pub adjustments: Vec<ImportAdjustment>,
    pub accent_candidates: Vec<AccentCandidate>,
    /// Resolved-palette checks run after mapping and hardening. Structural
    /// errors block install; contrast findings remain reviewable.
    #[serde(default)]
    pub validation: Vec<ValidationIssue>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportAdjustment {
    pub zeron_role: String,
    pub original: String,
    pub resolved: String,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportMapping {
    pub zeron_role: String,
    pub vscode_key: String,
    pub value: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccentCandidate {
    pub vscode_key: String,
    pub value: String,
}

#[derive(Debug, Clone, Default)]
struct NormalizedTheme {
    colors: BTreeMap<String, String>,
    token_colors: Vec<TokenRule>,
    semantic_token_colors: BTreeMap<String, SemanticStyle>,
    files: Vec<PathBuf>,
}

#[derive(Debug, Clone, Default)]
struct TokenRule {
    scopes: Vec<String>,
    foreground: Option<String>,
    font_style: Option<String>,
}

#[derive(Debug, Clone, Default)]
struct SemanticStyle {
    foreground: Option<String>,
    font_style: Option<String>,
}

#[derive(Debug, Clone)]
pub struct CompileOptions {
    pub family_id: String,
    pub family_name: String,
    pub source_url: String,
    pub revision: String,
    pub license: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DetectedThemeSource {
    File,
    Package,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VariantFailure {
    pub id: String,
    pub name: String,
    pub path: PathBuf,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceCompilation {
    pub path: PathBuf,
    pub source_kind: DetectedThemeSource,
    pub family: ThemeFamily,
    pub reports: BTreeMap<String, ImportReport>,
    pub failures: Vec<VariantFailure>,
}

/// Detect and compile either a single VS Code theme file or an extension
/// package. Package variants are isolated: one bad variant is reported without
/// hiding the valid variants that the user can still choose to import.
pub fn compile_source(path: &Path, options: CompileOptions) -> Result<SourceCompilation> {
    let path = path
        .canonicalize()
        .with_context(|| format!("could not resolve {}", path.display()))?;
    let package_json = if path.is_dir() {
        let candidate = path.join("package.json");
        candidate.exists().then_some(candidate)
    } else if path.file_name().and_then(|name| name.to_str()) == Some("package.json") {
        Some(path.clone())
    } else {
        None
    };

    if let Some(package_json) = package_json {
        return compile_package(&path, &package_json, options);
    }
    compile_single_file(path, options)
}

fn compile_single_file(path: PathBuf, options: CompileOptions) -> Result<SourceCompilation> {
    let root = path
        .parent()
        .ok_or_else(|| anyhow!("{} has no parent directory", path.display()))?
        .canonicalize()
        .with_context(|| format!("could not resolve theme root for {}", path.display()))?;
    let normalized = load_theme(&path, &root, &mut Vec::new())?;
    let appearance = detect_appearance(&path, None, &normalized);
    let name = theme_declared_name(&path).unwrap_or_else(|| options.family_name.clone());
    let imported = convert(
        normalized,
        ImportOptions {
            id: options.family_id.clone(),
            family_id: options.family_id.clone(),
            name,
            appearance,
            source_url: options.source_url,
            revision: options.revision,
            license: options.license,
        },
    )?;
    let reports = BTreeMap::from([(imported.theme.id.clone(), imported.report)]);
    Ok(SourceCompilation {
        path,
        source_kind: DetectedThemeSource::File,
        family: ThemeFamily {
            id: options.family_id,
            name: options.family_name,
            variants: vec![imported.theme],
        },
        reports,
        failures: Vec::new(),
    })
}

fn compile_package(
    selected_path: &Path,
    package_json: &Path,
    options: CompileOptions,
) -> Result<SourceCompilation> {
    let source = read_bounded(package_json, "package manifest")?;
    let manifest: Value = json5::from_str(&source)
        .with_context(|| format!("could not parse {}", package_json.display()))?;
    let declarations = manifest
        .pointer("/contributes/themes")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            anyhow!(
                "{} does not declare contributes.themes",
                package_json.display()
            )
        })?;
    if declarations.is_empty() {
        bail!("{} declares no theme variants", package_json.display());
    }
    if declarations.len() > MAX_PACKAGE_VARIANTS {
        bail!(
            "{} declares {} theme variants; the limit is {MAX_PACKAGE_VARIANTS}",
            package_json.display(),
            declarations.len()
        );
    }
    let package_root = package_json
        .parent()
        .ok_or_else(|| anyhow!("{} has no parent directory", package_json.display()))?;
    let package_root = package_root
        .canonicalize()
        .with_context(|| format!("could not resolve package root {}", package_root.display()))?;
    let family_name = manifest
        .get("displayName")
        .or_else(|| manifest.get("name"))
        .and_then(Value::as_str)
        .filter(|name| !name.trim().is_empty())
        .unwrap_or(&options.family_name)
        .to_owned();
    let mut variants = Vec::new();
    let mut reports = BTreeMap::new();
    let mut failures = Vec::new();
    let mut used_ids = HashSet::new();
    for (index, declaration) in declarations.iter().enumerate() {
        let label = declaration
            .get("label")
            .and_then(Value::as_str)
            .filter(|label| !label.trim().is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| format!("Variant {}", index + 1));
        let mut id = format!("{}-{}", options.family_id, slug(&label));
        if !used_ids.insert(id.clone()) {
            id = format!("{id}-{}", index + 1);
            used_ids.insert(id.clone());
        }
        let relative = match declaration.get("path").and_then(Value::as_str) {
            Some(path) => path,
            None => {
                failures.push(VariantFailure {
                    id,
                    name: label,
                    path: package_json.to_path_buf(),
                    message: "theme declaration has no path".into(),
                });
                continue;
            }
        };
        let theme_path = package_root.join(relative);
        let result = (|| {
            let normalized = load_theme(&theme_path, &package_root, &mut Vec::new())?;
            let appearance = detect_appearance(
                &theme_path,
                declaration.get("uiTheme").and_then(Value::as_str),
                &normalized,
            );
            convert(
                normalized,
                ImportOptions {
                    id: id.clone(),
                    family_id: options.family_id.clone(),
                    name: label.clone(),
                    appearance,
                    source_url: options.source_url.clone(),
                    revision: options.revision.clone(),
                    license: options.license.clone(),
                },
            )
        })();
        match result {
            Ok(imported) => {
                reports.insert(imported.theme.id.clone(), imported.report);
                variants.push(imported.theme);
            }
            Err(error) => failures.push(VariantFailure {
                id,
                name: label,
                path: theme_path,
                message: error.to_string(),
            }),
        }
    }
    if variants.is_empty() {
        let details = failures
            .iter()
            .map(|failure| format!("{}: {}", failure.name, failure.message))
            .collect::<Vec<_>>()
            .join("; ");
        bail!("no package variants compiled successfully: {details}");
    }
    Ok(SourceCompilation {
        path: selected_path.to_path_buf(),
        source_kind: DetectedThemeSource::Package,
        family: ThemeFamily {
            id: options.family_id,
            name: family_name,
            variants,
        },
        reports,
        failures,
    })
}

fn theme_declared_name(path: &Path) -> Option<String> {
    let source = read_bounded(path, "theme source").ok()?;
    let value: Value = json5::from_str(&source).ok()?;
    value
        .get("name")
        .and_then(Value::as_str)
        .filter(|name| !name.trim().is_empty())
        .map(str::to_owned)
}

fn detect_appearance(
    path: &Path,
    ui_theme: Option<&str>,
    normalized: &NormalizedTheme,
) -> Appearance {
    if let Some(ui_theme) = ui_theme {
        match ui_theme.to_ascii_lowercase().as_str() {
            "vs" | "hc-light" => return Appearance::Light,
            "vs-dark" | "hc-black" => return Appearance::Dark,
            // Future or third-party values still get the more reliable
            // explicit-type/background inference below.
            _ => {}
        }
    }
    if let Ok(source) = read_bounded(path, "theme source")
        && let Ok(value) = json5::from_str::<Value>(&source)
        && let Some(kind) = value.get("type").and_then(Value::as_str)
    {
        if kind.eq_ignore_ascii_case("light") {
            return Appearance::Light;
        }
        if kind.eq_ignore_ascii_case("dark") {
            return Appearance::Dark;
        }
    }
    normalized
        .colors
        .get("editor.background")
        .and_then(|value| value.parse::<Color>().ok())
        .map(|background| {
            if Color::BLACK.contrast(background) > Color::WHITE.contrast(background) {
                Appearance::Light
            } else {
                Appearance::Dark
            }
        })
        .unwrap_or(Appearance::Dark)
}

fn slug(value: &str) -> String {
    let mut output = String::new();
    let mut separator = false;
    for character in value.chars().flat_map(char::to_lowercase) {
        if character.is_ascii_alphanumeric() {
            if separator && !output.is_empty() {
                output.push('-');
            }
            separator = false;
            output.push(character);
        } else {
            separator = true;
        }
    }
    if output.is_empty() {
        "theme".into()
    } else {
        output
    }
}

pub fn import_file(path: &Path, options: ImportOptions) -> Result<ImportResult> {
    let canonical = path
        .canonicalize()
        .with_context(|| format!("could not resolve {}", path.display()))?;
    let root = canonical
        .parent()
        .ok_or_else(|| anyhow!("{} has no parent directory", canonical.display()))?
        .to_path_buf();
    let normalized = load_theme(&canonical, &root, &mut Vec::new())?;
    convert(normalized, options)
}

fn load_theme(path: &Path, root: &Path, stack: &mut Vec<PathBuf>) -> Result<NormalizedTheme> {
    let path = path
        .canonicalize()
        .with_context(|| format!("could not resolve {}", path.display()))?;
    ensure_contained(&path, root, "theme source")?;
    if stack.contains(&path) {
        bail!("VS Code theme include cycle: {}", path.display());
    }
    if stack.len() >= MAX_INCLUDE_DEPTH {
        bail!("VS Code theme include depth exceeds {MAX_INCLUDE_DEPTH}");
    }
    stack.push(path.clone());
    let source = read_bounded(&path, "theme source")?;
    let value: Value = json5::from_str(&source)
        .with_context(|| format!("could not parse JSONC theme {}", path.display()))?;
    let object = value
        .as_object()
        .ok_or_else(|| anyhow!("{} is not a JSON object", path.display()))?;

    let mut theme = if let Some(include) = object.get("include").and_then(Value::as_str) {
        let include_path = resolve_relative(&path, include);
        load_theme(&include_path, root, stack)?
    } else {
        NormalizedTheme::default()
    };
    theme.files.push(path.clone());

    if let Some(colors) = object.get("colors").and_then(Value::as_object) {
        for (key, value) in colors {
            if let Some(value) = value.as_str() {
                theme.colors.insert(key.clone(), value.into());
            }
        }
    }

    if let Some(token_colors) = object.get("tokenColors") {
        match token_colors {
            Value::Array(rules) => theme.token_colors.extend(parse_token_rules(rules)),
            Value::String(relative) => {
                let token_path = resolve_relative(&path, relative);
                let token_path = token_path
                    .canonicalize()
                    .with_context(|| format!("could not resolve {}", token_path.display()))?;
                ensure_contained(&token_path, root, "external token file")?;
                theme.files.push(token_path.clone());
                theme.token_colors.extend(load_token_file(&token_path)?);
            }
            _ => {}
        }
    }

    if let Some(semantic) = object.get("semanticTokenColors").and_then(Value::as_object) {
        for (selector, style) in semantic {
            theme
                .semantic_token_colors
                .insert(selector.clone(), parse_semantic_style(style));
        }
    }
    stack.pop();
    Ok(theme)
}

fn ensure_contained(path: &Path, root: &Path, kind: &str) -> Result<()> {
    if !path.starts_with(root) {
        bail!(
            "{kind} {} resolves outside selected theme root {}; symlinks are allowed only when their targets remain inside the root",
            path.display(),
            root.display()
        );
    }
    Ok(())
}

fn read_bounded(path: &Path, kind: &str) -> Result<String> {
    let size = fs::metadata(path)
        .with_context(|| format!("could not inspect {kind} {}", path.display()))?
        .len();
    if size > MAX_SOURCE_BYTES {
        bail!(
            "{kind} {} is {size} bytes; the limit is {MAX_SOURCE_BYTES}",
            path.display()
        );
    }
    fs::read_to_string(path).with_context(|| format!("could not read {kind} {}", path.display()))
}

fn resolve_relative(owner: &Path, relative: &str) -> PathBuf {
    owner
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(relative)
}

fn parse_token_rules(values: &[Value]) -> Vec<TokenRule> {
    values
        .iter()
        .filter_map(|value| {
            let object = value.as_object()?;
            let scopes = match object.get("scope") {
                Some(Value::String(scope)) => scope
                    .split(',')
                    .map(str::trim)
                    .filter(|scope| !scope.is_empty())
                    .map(str::to_owned)
                    .collect(),
                Some(Value::Array(scopes)) => scopes
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect(),
                _ => Vec::new(),
            };
            let settings = object.get("settings")?.as_object()?;
            Some(TokenRule {
                scopes,
                foreground: settings
                    .get("foreground")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                font_style: settings
                    .get("fontStyle")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            })
        })
        .collect()
}

fn load_token_file(path: &Path) -> Result<Vec<TokenRule>> {
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default();
    if extension.eq_ignore_ascii_case("tmTheme") || extension.eq_ignore_ascii_case("plist") {
        return load_textmate_plist(path);
    }
    let source = read_bounded(path, "token file")?;
    let value: Value = json5::from_str(&source)
        .with_context(|| format!("could not parse token file {}", path.display()))?;
    let values = value
        .as_array()
        .or_else(|| value.get("tokenColors").and_then(Value::as_array))
        .ok_or_else(|| anyhow!("{} does not contain token rules", path.display()))?;
    Ok(parse_token_rules(values))
}

fn load_textmate_plist(path: &Path) -> Result<Vec<TokenRule>> {
    let size = fs::metadata(path)
        .with_context(|| format!("could not inspect TextMate plist {}", path.display()))?
        .len();
    if size > MAX_SOURCE_BYTES {
        bail!(
            "TextMate plist {} is {size} bytes; the limit is {MAX_SOURCE_BYTES}",
            path.display()
        );
    }
    let value = plist::Value::from_file(path)
        .with_context(|| format!("could not parse TextMate plist {}", path.display()))?;
    let settings = value
        .as_dictionary()
        .and_then(|dict| dict.get("settings"))
        .and_then(plist::Value::as_array)
        .ok_or_else(|| anyhow!("{} has no TextMate settings array", path.display()))?;
    Ok(settings
        .iter()
        .filter_map(|value| {
            let dict = value.as_dictionary()?;
            let scopes = dict
                .get("scope")
                .and_then(plist::Value::as_string)
                .map(|scope| {
                    scope
                        .split(',')
                        .map(str::trim)
                        .filter(|scope| !scope.is_empty())
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default();
            let style = dict.get("settings")?.as_dictionary()?;
            Some(TokenRule {
                scopes,
                foreground: style
                    .get("foreground")
                    .and_then(plist::Value::as_string)
                    .map(str::to_owned),
                font_style: style
                    .get("fontStyle")
                    .and_then(plist::Value::as_string)
                    .map(str::to_owned),
            })
        })
        .collect())
}

fn parse_semantic_style(value: &Value) -> SemanticStyle {
    match value {
        Value::String(foreground) => SemanticStyle {
            foreground: Some(foreground.clone()),
            font_style: None,
        },
        Value::Object(style) => SemanticStyle {
            foreground: style
                .get("foreground")
                .and_then(Value::as_str)
                .map(str::to_owned),
            font_style: style
                .get("fontStyle")
                .and_then(Value::as_str)
                .map(str::to_owned),
        },
        _ => SemanticStyle::default(),
    }
}

fn convert(theme: NormalizedTheme, options: ImportOptions) -> Result<ImportResult> {
    let registry = ThemeRegistry::builtin();
    let base_id = if options.appearance.is_dark() {
        "zeron-dark"
    } else {
        "zeron-light"
    };
    let mut output = registry
        .variant(base_id)
        .expect("Zeron base theme exists")
        .clone();
    let fallback_background = output.colors.background;
    output.id = options.id.clone();
    output.family_id = options.family_id;
    output.name = options.name;
    output.appearance = options.appearance;
    // VS Code themes are authored against a solid editor/workbench background.
    // Keep that as their recommendation; users can independently force frost.
    output.recommended_surface_treatment = SurfaceTreatment::Opaque;

    let mut report = ImportReport {
        source_files: theme
            .files
            .iter()
            .map(|path| path.display().to_string())
            .collect(),
        ..ImportReport::default()
    };
    report.fallbacks.push(
        "surface treatment inferred as opaque because VS Code palettes target solid workbench backgrounds; the user's Zeron surface preference can override it"
            .into(),
    );
    macro_rules! apply {
        ($role:expr, $keys:expr, $target:expr) => {{
            if let Some((key, value)) = first_color(&theme.colors, $keys, &mut report.warnings) {
                *$target = value;
                report.mappings.push(ImportMapping {
                    zeron_role: $role.into(),
                    vscode_key: key.into(),
                    value: value.to_string(),
                });
            } else {
                report.fallbacks.push(format!(
                    "{} retained the Zeron {} fallback",
                    $role,
                    if options.appearance.is_dark() {
                        "dark"
                    } else {
                        "light"
                    }
                ));
            }
        }};
    }

    apply!(
        "background",
        &["editor.background"],
        &mut output.colors.background
    );
    apply!(
        "shell",
        &[
            "sideBar.background",
            "activityBar.background",
            "panel.background"
        ],
        &mut output.colors.shell
    );
    apply!(
        "raised",
        &["list.hoverBackground", "input.background"],
        &mut output.colors.raised
    );
    apply!(
        "card",
        &["panel.background", "sideBar.background"],
        &mut output.colors.card
    );
    apply!(
        "dialog",
        &["editorWidget.background", "quickInput.background"],
        &mut output.colors.dialog
    );
    apply!(
        "overlay",
        &[
            "dropdown.background",
            "menu.background",
            "editorWidget.background"
        ],
        &mut output.colors.overlay
    );
    apply!(
        "hover",
        &["list.hoverBackground", "toolbar.hoverBackground"],
        &mut output.colors.hover
    );
    apply!(
        "active",
        &[
            "list.activeSelectionBackground",
            "list.inactiveSelectionBackground"
        ],
        &mut output.colors.active
    );
    apply!(
        "border",
        &["panel.border", "widget.border", "contrastBorder"],
        &mut output.colors.border
    );
    apply!(
        "borderStrong",
        &["focusBorder", "contrastActiveBorder"],
        &mut output.colors.border_strong
    );
    apply!(
        "text",
        &["foreground", "editor.foreground"],
        &mut output.colors.text
    );
    apply!(
        "textMuted",
        &["descriptionForeground", "tab.inactiveForeground"],
        &mut output.colors.text_muted
    );
    apply!(
        "textFaint",
        &["disabledForeground", "input.placeholderForeground"],
        &mut output.colors.text_faint
    );
    apply!("solid", &["button.background"], &mut output.colors.solid);
    apply!(
        "onSolid",
        &["button.foreground"],
        &mut output.colors.on_solid
    );
    apply!(
        "danger",
        &[
            "editorError.foreground",
            "errorForeground",
            "gitDecoration.deletedResourceForeground"
        ],
        &mut output.colors.danger
    );
    apply!(
        "warning",
        &[
            "editorWarning.foreground",
            "gitDecoration.modifiedResourceForeground"
        ],
        &mut output.colors.warning
    );
    apply!(
        "success",
        &[
            "gitDecoration.addedResourceForeground",
            "testing.iconPassed"
        ],
        &mut output.colors.success
    );
    apply!("input", &["input.background"], &mut output.colors.input);
    apply!(
        "cursor",
        &["editorCursor.foreground"],
        &mut output.colors.cursor
    );
    apply!(
        "diffAdd",
        &[
            "diffEditor.insertedTextBackground",
            "gitDecoration.addedResourceForeground"
        ],
        &mut output.colors.diff_add
    );
    apply!(
        "diffDelete",
        &[
            "diffEditor.removedTextBackground",
            "gitDecoration.deletedResourceForeground"
        ],
        &mut output.colors.diff_delete
    );
    apply!(
        "diffHunk",
        &[
            "diffEditor.diagonalFill",
            "editor.findMatchHighlightBackground"
        ],
        &mut output.colors.diff_hunk
    );

    let accent_keys = [
        "focusBorder",
        "textLink.foreground",
        "button.background",
        "activityBarBadge.background",
        "progressBar.background",
        "editorCursor.foreground",
    ];
    for key in accent_keys {
        if let Some(value) = theme.colors.get(key)
            && let Ok(color) = value.parse::<Color>()
        {
            report.accent_candidates.push(AccentCandidate {
                vscode_key: key.into(),
                value: color.to_string(),
            });
        }
    }
    if let Some(candidate) = report.accent_candidates.first() {
        let primary = candidate.value.parse()?;
        output.accent = AccentRoles::derive(primary, options.appearance, output.colors.background);
        report.mappings.push(ImportMapping {
            zeron_role: "accent.*".into(),
            vscode_key: candidate.vscode_key.clone(),
            value: candidate.value.clone(),
        });
    } else {
        report
            .fallbacks
            .push("accent.* retained the Zeron fallback; curate a native accent".into());
    }

    apply!(
        "terminal.background",
        &["terminal.background"],
        &mut output.terminal.background
    );
    apply!(
        "terminal.foreground",
        &["terminal.foreground"],
        &mut output.terminal.foreground
    );
    apply!(
        "terminal.selection",
        &["terminal.selectionBackground"],
        &mut output.terminal.selection
    );
    let ansi_keys = [
        "terminal.ansiBlack",
        "terminal.ansiRed",
        "terminal.ansiGreen",
        "terminal.ansiYellow",
        "terminal.ansiBlue",
        "terminal.ansiMagenta",
        "terminal.ansiCyan",
        "terminal.ansiWhite",
        "terminal.ansiBrightBlack",
        "terminal.ansiBrightRed",
        "terminal.ansiBrightGreen",
        "terminal.ansiBrightYellow",
        "terminal.ansiBrightBlue",
        "terminal.ansiBrightMagenta",
        "terminal.ansiBrightCyan",
        "terminal.ansiBrightWhite",
    ];
    for (index, key) in ansi_keys.iter().enumerate() {
        apply!(
            &format!("terminal.ansi[{index}]"),
            &[*key],
            &mut output.terminal.ansi[index]
        );
    }

    map_syntax(&theme, &mut output, &mut report);
    harden_variant(&theme, &mut output, fallback_background, &mut report);
    let mut hasher = Sha256::new();
    for path in &theme.files {
        hasher.update(read_bounded(path, "theme source while hashing")?.as_bytes());
    }
    report.source_hash = format!("sha256:{:x}", hasher.finalize());
    output.source = ThemeSource {
        format: "vscode".into(),
        url: options.source_url,
        revision: options.revision,
        license: options.license,
        asset_hash: String::new(),
    };
    let encoded = serde_json::to_vec(&output).context("could not hash generated theme")?;
    output.source.asset_hash = format!("sha256:{:x}", Sha256::digest(encoded));
    report.validation = ThemeRegistry {
        families: vec![ThemeFamily {
            id: output.family_id.clone(),
            name: output.name.clone(),
            variants: vec![output.clone()],
        }],
    }
    .validate();
    Ok(ImportResult {
        theme: output,
        report,
    })
}

fn harden_variant(
    source: &NormalizedTheme,
    output: &mut ThemeVariant,
    fallback_background: Color,
    report: &mut ImportReport,
) {
    output.colors.background = flatten_foundation(
        "background",
        output.colors.background,
        fallback_background,
        report,
    );
    let background = output.colors.background;
    output.colors.shell = flatten_foundation("shell", output.colors.shell, background, report);
    output.colors.raised = flatten_foundation("raised", output.colors.raised, background, report);
    output.colors.card = flatten_foundation("card", output.colors.card, background, report);
    output.colors.dialog = flatten_foundation("dialog", output.colors.dialog, background, report);
    output.colors.overlay =
        flatten_foundation("overlay", output.colors.overlay, background, report);
    output.colors.input = flatten_foundation("input", output.colors.input, background, report);
    output.colors.solid = flatten_foundation("solid", output.colors.solid, background, report);
    output.terminal.background = flatten_foundation(
        "terminal.background",
        output.terminal.background,
        background,
        report,
    );

    let text_backgrounds = [
        background,
        output.colors.shell,
        output.colors.raised,
        output.colors.card,
        output.colors.dialog,
        output.colors.overlay,
        output.colors.input,
    ];
    output.colors.text = harden_foreground(
        source,
        report,
        "text",
        output.colors.text,
        &[
            "foreground",
            "sideBar.foreground",
            "editorWidget.foreground",
            "quickInput.foreground",
            "input.foreground",
            "menu.foreground",
            "editor.foreground",
        ],
        &text_backgrounds,
        4.5,
        None,
    );
    output.colors.text_muted = harden_foreground(
        source,
        report,
        "textMuted",
        output.colors.text_muted,
        &[
            "descriptionForeground",
            "tab.inactiveForeground",
            "sideBarSectionHeader.foreground",
        ],
        &text_backgrounds,
        4.5,
        Some(output.colors.text),
    );
    output.colors.text_faint = harden_foreground(
        source,
        report,
        "textFaint",
        output.colors.text_faint,
        &[
            "disabledForeground",
            "input.placeholderForeground",
            "descriptionForeground",
            "foreground",
        ],
        &text_backgrounds,
        3.0,
        Some(output.colors.text_muted),
    );
    output.colors.on_solid = harden_foreground(
        source,
        report,
        "onSolid",
        output.colors.on_solid,
        &["button.foreground", "foreground", "editor.foreground"],
        &[output.colors.solid],
        4.5,
        Some(output.colors.text),
    );
    output.terminal.foreground = harden_foreground(
        source,
        report,
        "terminal.foreground",
        output.terminal.foreground,
        &["terminal.foreground", "editor.foreground", "foreground"],
        &[output.terminal.background],
        4.5,
        Some(output.colors.text),
    );

    for (role, color) in [
        ("danger", &mut output.colors.danger),
        ("warning", &mut output.colors.warning),
        ("success", &mut output.colors.success),
        ("cursor", &mut output.colors.cursor),
        ("borderStrong", &mut output.colors.border_strong),
    ] {
        let original = *color;
        *color = ensure_contrast_across(original, &text_backgrounds[..2], 3.0, None);
        record_adjustment(
            report,
            role,
            original,
            *color,
            "raised to 3:1 across the main and shell surfaces",
        );
    }

    harden_accent(output, report, &text_backgrounds[..4]);
}

fn flatten_foundation(
    role: &str,
    color: Color,
    background: Color,
    report: &mut ImportReport,
) -> Color {
    if color.a == 255 {
        return color;
    }
    let resolved = color.blend_over(background);
    record_adjustment(
        report,
        role,
        color,
        resolved,
        "flattened a translucent foundational surface against the theme background",
    );
    resolved
}

fn harden_foreground(
    source: &NormalizedTheme,
    report: &mut ImportReport,
    role: &str,
    current: Color,
    candidate_keys: &[&str],
    backgrounds: &[Color],
    minimum: f32,
    preferred_target: Option<Color>,
) -> Color {
    let current_contrast = minimum_contrast(current, backgrounds);
    if current_contrast >= minimum {
        return current;
    }

    for key in candidate_keys {
        let Some(value) = source.colors.get(*key) else {
            continue;
        };
        let Ok(candidate) = value.parse::<Color>() else {
            continue;
        };
        if minimum_contrast(candidate, backgrounds) >= minimum {
            record_adjustment(
                report,
                role,
                current,
                candidate,
                format!("used {key} because the mapped color reached only {current_contrast:.2}:1"),
            );
            if let Some(mapping) = report
                .mappings
                .iter_mut()
                .find(|mapping| mapping.zeron_role == role)
            {
                mapping.vscode_key = (*key).into();
                mapping.value = candidate.to_string();
            }
            return candidate;
        }
    }

    let resolved = ensure_contrast_across(current, backgrounds, minimum, preferred_target);
    record_adjustment(
        report,
        role,
        current,
        resolved,
        format!(
            "preserved the source hue while raising worst-case contrast from {current_contrast:.2}:1 to {:.2}:1",
            minimum_contrast(resolved, backgrounds)
        ),
    );
    resolved
}

fn ensure_contrast_across(
    color: Color,
    backgrounds: &[Color],
    minimum: f32,
    preferred_target: Option<Color>,
) -> Color {
    if minimum_contrast(color, backgrounds) >= minimum {
        return color;
    }
    let mut targets = Vec::with_capacity(3);
    if let Some(target) = preferred_target {
        targets.push(target);
    }
    targets.extend([Color::BLACK, Color::WHITE]);

    let mut best = color;
    let mut best_contrast = minimum_contrast(color, backgrounds);
    for target in targets {
        for step in 1..=100 {
            let candidate = color.mix(target, step as f32 / 100.0);
            let contrast = minimum_contrast(candidate, backgrounds);
            if contrast > best_contrast {
                best = candidate;
                best_contrast = contrast;
            }
            if contrast >= minimum {
                return candidate;
            }
        }
    }
    best
}

fn minimum_contrast(color: Color, backgrounds: &[Color]) -> f32 {
    backgrounds
        .iter()
        .map(|background| color.contrast(*background))
        .fold(f32::INFINITY, f32::min)
}

fn harden_accent(output: &mut ThemeVariant, report: &mut ImportReport, backgrounds: &[Color]) {
    let current = output.accent.primary;
    if minimum_contrast(current, backgrounds) >= 3.0 {
        return;
    }
    for candidate in &report.accent_candidates {
        let Ok(primary) = candidate.value.parse::<Color>() else {
            continue;
        };
        if minimum_contrast(primary, backgrounds) >= 3.0 {
            output.accent =
                AccentRoles::derive(primary, output.appearance, output.colors.background);
            record_adjustment(
                report,
                "accent.*",
                current,
                output.accent.primary,
                format!("used {} to keep interactions at 3:1", candidate.vscode_key),
            );
            return;
        }
    }
    let primary = ensure_contrast_across(current, backgrounds, 3.0, None);
    output.accent = AccentRoles::derive(primary, output.appearance, output.colors.background);
    record_adjustment(
        report,
        "accent.*",
        current,
        output.accent.primary,
        "preserved the source hue while raising interaction contrast to 3:1",
    );
}

fn record_adjustment(
    report: &mut ImportReport,
    role: &str,
    original: Color,
    resolved: Color,
    reason: impl Into<String>,
) {
    if original == resolved {
        return;
    }
    report.adjustments.push(ImportAdjustment {
        zeron_role: role.into(),
        original: original.to_string(),
        resolved: resolved.to_string(),
        reason: reason.into(),
    });
}

fn first_color<'a>(
    colors: &'a BTreeMap<String, String>,
    keys: &[&'a str],
    warnings: &mut Vec<String>,
) -> Option<(&'a str, Color)> {
    for key in keys {
        if let Some(value) = colors.get(*key) {
            match value.parse() {
                Ok(color) => return Some((key, color)),
                Err(_) => warnings.push(format!("ignored unsupported color {key}={value}")),
            }
        }
    }
    None
}

fn map_syntax(theme: &NormalizedTheme, output: &mut ThemeVariant, report: &mut ImportReport) {
    for rule in &theme.token_colors {
        let Some(foreground) = rule.foreground.as_deref() else {
            continue;
        };
        let Ok(color) = foreground.parse::<Color>() else {
            report
                .warnings
                .push(format!("ignored TextMate color {foreground}"));
            continue;
        };
        if let Some(style) = rule.font_style.as_deref().filter(|value| !value.is_empty()) {
            report.dropped.push(format!(
                "TextMate fontStyle `{style}` for {}",
                rule.scopes.join(", ")
            ));
        }
        for scope in &rule.scopes {
            if let Some(role) = syntax_role_for_scope(scope) {
                output.syntax.insert(role.into(), color);
                report.mappings.push(ImportMapping {
                    zeron_role: format!("syntax.{role}"),
                    vscode_key: scope.clone(),
                    value: color.to_string(),
                });
            }
        }
    }
    for (selector, style) in &theme.semantic_token_colors {
        if let Some(font_style) = style
            .font_style
            .as_deref()
            .filter(|value| !value.is_empty())
        {
            report
                .dropped
                .push(format!("semantic fontStyle `{font_style}` for {selector}"));
        }
        let Some(foreground) = style.foreground.as_deref() else {
            continue;
        };
        let Ok(color) = foreground.parse::<Color>() else {
            continue;
        };
        if let Some(role) = syntax_role_for_semantic(selector) {
            output.syntax.insert(role.into(), color);
            report.mappings.push(ImportMapping {
                zeron_role: format!("syntax.{role}"),
                vscode_key: format!("semantic:{selector}"),
                value: color.to_string(),
            });
        }
    }
}

fn syntax_role_for_scope(scope: &str) -> Option<&'static str> {
    let scope = scope.to_ascii_lowercase();
    let pairs = [
        ("comment", "comment"),
        ("invalid", "invalid"),
        ("keyword", "keyword"),
        ("storage", "keyword"),
        ("string", "string"),
        ("constant.numeric", "number"),
        ("constant.language", "boolean"),
        ("entity.name.type", "type"),
        ("support.type", "typeBuiltin"),
        ("entity.name.function", "function"),
        ("support.function", "functionBuiltin"),
        ("entity.other.attribute", "attribute"),
        ("entity.name.tag", "tag"),
        ("variable.parameter", "parameter"),
        ("variable.other.property", "property"),
        ("variable", "variable"),
        ("constant", "constant"),
        ("keyword.operator", "operator"),
        ("punctuation", "punctuation"),
    ];
    pairs
        .into_iter()
        .find_map(|(needle, role)| scope.contains(needle).then_some(role))
}

fn syntax_role_for_semantic(selector: &str) -> Option<&'static str> {
    let token = selector.split(['.', ':']).next()?.to_ascii_lowercase();
    match token.as_str() {
        "comment" => Some("comment"),
        "keyword" | "modifier" => Some("keyword"),
        "string" | "regexp" => Some("string"),
        "number" => Some("number"),
        "type" | "class" | "interface" | "enum" | "struct" => Some("type"),
        "function" | "method" => Some("function"),
        "property" | "enummember" => Some("property"),
        "parameter" => Some("parameter"),
        "variable" => Some("variable"),
        "macro" => Some("macro"),
        "decorator" => Some("attribute"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options() -> ImportOptions {
        ImportOptions {
            id: "test-dark".into(),
            family_id: "test".into(),
            name: "Test Dark".into(),
            appearance: Appearance::Dark,
            source_url: "https://example.test/theme".into(),
            revision: "abc123".into(),
            license: "MIT".into(),
        }
    }

    #[test]
    fn resolves_jsonc_includes_and_semantic_overlays() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("base.json"),
            r##"{
          // comments and trailing commas are valid VS Code theme input
          "colors": { "editor.background": "#101010", "foreground": "#eeeeee", },
          "tokenColors": [{ "scope": "comment", "settings": { "foreground": "#777777" } }],
        }"##,
        )
        .unwrap();
        fs::write(
            dir.path().join("theme.json"),
            r##"{
          "include": "./base.json",
          "colors": { "focusBorder": "#00aaff", "terminal.ansiRed": "#ff0000" },
          "semanticTokenColors": { "function": "#00ff00" },
        }"##,
        )
        .unwrap();
        let imported = import_file(&dir.path().join("theme.json"), options()).unwrap();
        assert_eq!(imported.theme.colors.background, "#101010".parse().unwrap());
        assert_eq!(
            imported.theme.syntax["function"],
            "#00ff00".parse().unwrap()
        );
        assert_eq!(imported.theme.syntax["comment"], "#777777".parse().unwrap());
        assert_eq!(imported.theme.terminal.ansi[1], "#ff0000".parse().unwrap());
        assert_eq!(imported.report.source_files.len(), 2);
        assert!(!imported.report.accent_candidates.is_empty());
    }

    #[test]
    fn reports_dropped_font_styles() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("theme.json"), r##"{
          "tokenColors": [{ "scope": "keyword", "settings": { "foreground": "#ff00ff", "fontStyle": "italic bold" } }]
        }"##).unwrap();
        let imported = import_file(&dir.path().join("theme.json"), options()).unwrap();
        assert!(
            imported
                .report
                .dropped
                .iter()
                .any(|item| item.contains("italic bold"))
        );
    }

    #[test]
    fn hardens_ayu_style_low_contrast_workbench_roles_without_blocking_import() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ayu-mirage.json");
        fs::write(
            &path,
            r##"{
              "name": "Ayu Mirage",
              "type": "dark",
              "colors": {
                "editor.background": "#242936",
                "foreground": "#707a8c",
                "descriptionForeground": "#707a8c",
                "editor.foreground": "#cccac2",
                "focusBorder": "#ffcc66",
                "terminal.background": "#242936",
                "terminal.foreground": "#707a8c"
              }
            }"##,
        )
        .unwrap();

        let imported = import_file(&path, options()).unwrap();
        assert_eq!(
            imported.theme.colors.text,
            "#cccac2".parse::<Color>().unwrap()
        );
        assert!(
            imported
                .theme
                .colors
                .text_muted
                .contrast(imported.theme.colors.background)
                >= 4.5
        );
        assert!(
            imported
                .theme
                .terminal
                .foreground
                .contrast(imported.theme.terminal.background)
                >= 4.5
        );
        assert!(imported.report.adjustments.iter().any(|adjustment| {
            adjustment.zeron_role == "text" && adjustment.reason.contains("editor.foreground")
        }));
        assert!(
            imported
                .report
                .validation
                .iter()
                .all(|issue| !issue.is_blocking())
        );
    }

    #[test]
    fn hardens_and_reports_low_contrast_faint_text() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("faint.json");
        fs::write(
            &path,
            r##"{
              "colors": {
                "editor.background": "#111111",
                "foreground": "#eeeeee",
                "descriptionForeground": "#aaaaaa",
                "disabledForeground": "#202020"
              }
            }"##,
        )
        .unwrap();

        let imported = import_file(&path, options()).unwrap();
        let faint = imported.theme.colors.text_faint;
        assert!(faint.contrast(imported.theme.colors.background) >= 3.0);
        let adjustment = imported
            .report
            .adjustments
            .iter()
            .find(|adjustment| adjustment.zeron_role == "textFaint")
            .expect("textFaint adjustment is reported");
        assert_eq!(adjustment.original, "#202020");
        assert_eq!(adjustment.resolved, faint.to_string());
        assert!(adjustment.reason.contains("descriptionForeground"));
        let mapping = imported
            .report
            .mappings
            .iter()
            .find(|mapping| mapping.zeron_role == "textFaint")
            .unwrap();
        assert_eq!(mapping.vscode_key, "descriptionForeground");
        assert_eq!(mapping.value, faint.to_string());
    }

    #[test]
    fn rejects_package_declarations_and_includes_outside_selected_root() {
        let dir = tempfile::tempdir().unwrap();
        let package = dir.path().join("package");
        fs::create_dir_all(&package).unwrap();
        fs::write(
            package.join("package.json"),
            r#"{"contributes":{"themes":[{"label":"Escape","path":"../outside.json"}]}}"#,
        )
        .unwrap();
        fs::write(
            dir.path().join("outside.json"),
            r##"{"colors":{"editor.background":"#111111"}}"##,
        )
        .unwrap();
        let error = compile_source(
            &package,
            CompileOptions {
                family_id: "escape".into(),
                family_name: "Escape".into(),
                source_url: "local".into(),
                revision: "local".into(),
                license: "User supplied".into(),
            },
        )
        .unwrap_err();
        assert!(error.to_string().contains("outside selected theme root"));

        let nested = package.join("nested");
        fs::create_dir_all(&nested).unwrap();
        fs::write(
            nested.join("theme.json"),
            r#"{"include":"../../outside.json"}"#,
        )
        .unwrap();
        let error = import_file(&nested.join("theme.json"), options()).unwrap_err();
        assert!(error.to_string().contains("outside selected theme root"));

        fs::write(
            nested.join("tokens.json"),
            r#"{"tokenColors":"../../outside.json"}"#,
        )
        .unwrap();
        let error = import_file(&nested.join("tokens.json"), options()).unwrap_err();
        assert!(error.to_string().contains("outside selected theme root"));
    }

    #[test]
    fn bounds_include_depth_and_source_size() {
        let dir = tempfile::tempdir().unwrap();
        for index in 0..=MAX_INCLUDE_DEPTH {
            let source = if index == MAX_INCLUDE_DEPTH {
                "{}".to_string()
            } else {
                format!(r#"{{"include":"{}.json"}}"#, index + 1)
            };
            fs::write(dir.path().join(format!("{index}.json")), source).unwrap();
        }
        let error = import_file(&dir.path().join("0.json"), options()).unwrap_err();
        assert!(error.to_string().contains("include depth exceeds"));

        let large = dir.path().join("large.json");
        let file = fs::File::create(&large).unwrap();
        file.set_len(MAX_SOURCE_BYTES + 1).unwrap();
        let error = import_file(&large, options()).unwrap_err();
        assert!(error.to_string().contains("the limit is"));

        let package = dir.path().join("package");
        fs::create_dir_all(&package).unwrap();
        let declarations = (0..=MAX_PACKAGE_VARIANTS)
            .map(|index| format!(r#"{{"label":"{index}","path":"{index}.json"}}"#))
            .collect::<Vec<_>>()
            .join(",");
        fs::write(
            package.join("package.json"),
            format!(r#"{{"contributes":{{"themes":[{declarations}]}}}}"#),
        )
        .unwrap();
        let error = compile_source(
            &package,
            CompileOptions {
                family_id: "large".into(),
                family_name: "Large".into(),
                source_url: "local".into(),
                revision: "local".into(),
                license: "User supplied".into(),
            },
        )
        .unwrap_err();
        assert!(error.to_string().contains("declares 129 theme variants"));
    }

    #[test]
    fn flattens_translucent_foundational_surfaces() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("transparent.json");
        fs::write(
            &path,
            r##"{
              "colors": {
                "editor.background": "#101820cc",
                "sideBar.background": "#ffffff10",
                "foreground": "#ffffff"
              }
            }"##,
        )
        .unwrap();

        let imported = import_file(&path, options()).unwrap();
        assert_eq!(imported.theme.colors.background.a, 255);
        assert_eq!(imported.theme.colors.shell.a, 255);
        assert!(imported.report.adjustments.iter().any(|adjustment| {
            adjustment.zeron_role == "background"
                && adjustment.reason.contains("translucent foundational")
        }));
    }

    #[test]
    fn appearance_detection_handles_high_contrast_and_unknown_ui_themes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("theme.json");
        fs::write(&path, r##"{"colors":{"editor.background":"#ffffff"}}"##).unwrap();
        let mut normalized = NormalizedTheme::default();
        normalized
            .colors
            .insert("editor.background".into(), "#ffffff".into());

        assert_eq!(
            detect_appearance(&path, Some("hc-light"), &normalized),
            Appearance::Light
        );
        assert_eq!(
            detect_appearance(&path, Some("third-party-theme-kind"), &normalized),
            Appearance::Light
        );
    }

    #[test]
    fn package_detection_classifies_variants_and_keeps_partial_failures() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("themes")).unwrap();
        fs::write(
            dir.path().join("package.json"),
            r#"{
              "displayName": "Package Family",
              "contributes": { "themes": [
                { "label": "Package Light", "uiTheme": "vs", "path": "./themes/light.json" },
                { "label": "Package Dark", "uiTheme": "vs-dark", "path": "./themes/dark.json" },
                { "label": "Broken", "uiTheme": "vs-dark", "path": "./themes/missing.json" }
              ] }
            }"#,
        )
        .unwrap();
        fs::write(
            dir.path().join("themes/light.json"),
            r##"{"colors":{"editor.background":"#ffffff","foreground":"#222222"}}"##,
        )
        .unwrap();
        fs::write(
            dir.path().join("themes/dark.json"),
            r##"{"colors":{"editor.background":"#111111","foreground":"#eeeeee"}}"##,
        )
        .unwrap();

        let compiled = compile_source(
            dir.path(),
            CompileOptions {
                family_id: "package-family".into(),
                family_name: "Fallback name".into(),
                source_url: dir.path().display().to_string(),
                revision: "local".into(),
                license: "User supplied".into(),
            },
        )
        .unwrap();
        assert_eq!(compiled.source_kind, DetectedThemeSource::Package);
        assert_eq!(compiled.family.name, "Package Family");
        assert_eq!(compiled.family.variants.len(), 2);
        assert_eq!(compiled.failures.len(), 1);
        assert!(compiled.family.variants.iter().any(|variant| {
            variant.name == "Package Light" && variant.appearance == Appearance::Light
        }));
        assert!(compiled.family.variants.iter().any(|variant| {
            variant.name == "Package Dark" && variant.appearance == Appearance::Dark
        }));
    }

    #[test]
    fn standalone_file_detects_declared_appearance() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("paper.json");
        fs::write(
            &path,
            r##"{
              "name": "Paper",
              "type": "light",
              "colors": { "editor.background": "#fafafa", "foreground": "#222222" }
            }"##,
        )
        .unwrap();
        let compiled = compile_source(
            &path,
            CompileOptions {
                family_id: "paper".into(),
                family_name: "Paper".into(),
                source_url: path.display().to_string(),
                revision: "local".into(),
                license: "User supplied".into(),
            },
        )
        .unwrap();
        assert_eq!(compiled.source_kind, DetectedThemeSource::File);
        assert_eq!(compiled.family.variants[0].name, "Paper");
        assert_eq!(compiled.family.variants[0].appearance, Appearance::Light);
    }
}
