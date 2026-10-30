//! Project icon discovery (`core/src/project/icon.ts`): the first small image a repository ships
//! (Xcode asset catalogs, Icon Composer bundles, Tauri/Electron build icons, favicons, logos),
//! stored as a `data:` URL in `project.icon_url`.

use std::path::{Path, PathBuf};

use base64::Engine;
use once_cell::sync::Lazy;
use regex::Regex;
use rusqlite::OptionalExtension;
use serde_json::Value;

use crate::bus::Bus;

/// Largest image (and data URL) an icon may be.
const MAX_BYTES: u64 = 64 * 1024;
const MAX_URL: usize = 64 * 1024;
const IMAGES: &[&str] = &[".svg", ".png", ".ico", ".jpg", ".jpeg", ".webp"];

static WINDOWS_ICON: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)^icon-(\d+)\.png$").unwrap());
static COLOR: Lazy<Regex> = Lazy::new(|| Regex::new(r"^(gray|srgb|display-p3):([\d.,]+)$").unwrap());

/// `CANDIDATES`, in preference order.
fn candidates() -> Vec<String> {
    let mut out = vec![
        "Assets.xcassets/AppIcon.appiconset".to_string(),
        "ios/App/Assets.xcassets/AppIcon.appiconset".to_string(),
    ];
    for base in [
        "assets/app-icon",
        "build/icon",
        "src-tauri/icons/icon",
        "resources/icon",
        "icons/icon",
        "extension/icons/icon",
    ] {
        out.push(format!("{base}.svg"));
        out.push(format!("{base}.png"));
    }
    for file in ["favicon.svg", "icon.svg", "favicon.png", "apple-touch-icon.png", "icon.png", "favicon.ico"] {
        for dir in ["public/", "app/", "src/app/", "site/public/", ""] {
            out.push(format!("{dir}{file}"));
        }
    }
    for base in ["assets/icon", "assets/logo", "public/logo", "logo"] {
        out.push(format!("{base}.svg"));
        out.push(format!("{base}.png"));
    }
    out
}

/// A discovered icon: its path relative to the project root and its `data:` URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Icon {
    pub path: String,
    pub url: String,
}

/// `path.extname(file).toLowerCase()`.
fn extname(file: &Path) -> String {
    file.extension().map(|ext| format!(".{}", ext.to_string_lossy().to_lowercase())).unwrap_or_default()
}

fn contains(root: &Path, file: &Path) -> bool {
    oc_git::paths::contains(root, file)
}

fn utf16_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    a.encode_utf16().cmp(b.encode_utf16())
}

/// `read`: a regular file inside the root, with an allowed extension, of 1 byte to 64 KiB.
fn read(root: &Path, name: &Path, extensions: &[&str]) -> Option<(PathBuf, Vec<u8>)> {
    let file = std::fs::canonicalize(name).ok()?;
    if file == root || !contains(root, &file) || !extensions.contains(&extname(&file).as_str()) {
        return None;
    }
    let info = std::fs::metadata(&file).ok()?;
    if !info.is_file() || info.len() == 0 || info.len() > MAX_BYTES {
        return None;
    }
    let bytes = std::fs::read(&file).ok()?;
    (!bytes.is_empty() && bytes.len() as u64 <= MAX_BYTES).then_some((file, bytes))
}

fn mime(file: &Path) -> &'static str {
    match extname(file).as_str() {
        ".ico" => "image/x-icon",
        ".svg" => "image/svg+xml",
        ".png" => "image/png",
        ".jpg" | ".jpeg" => "image/jpeg",
        ".webp" => "image/webp",
        ".json" => "application/json",
        _ => "application/octet-stream",
    }
}

fn data_url(file: &Path, bytes: &[u8]) -> String {
    format!("data:{};base64,{}", mime(file), base64::engine::general_purpose::STANDARD.encode(bytes))
}

/// JS `String(number)` for the small values a backdrop uses.
fn js_number(value: f64) -> String {
    if value == value.trunc() && value.abs() < 1e21 {
        return format!("{}", value as i64);
    }
    if value != 0.0 && value.abs() < 1e-6 {
        return format!("{value:e}");
    }
    format!("{value}")
}

/// JS `Number(text)` for the digits-and-dots fields of a Composer color.
fn js_parse(text: &str) -> f64 {
    if text.is_empty() {
        return 0.0;
    }
    text.parse::<f64>().unwrap_or(f64::NAN)
}

/// `{solid?, "linear-gradient"?}` of a Composer manifest.
#[derive(Debug, Default)]
struct Fill {
    solid: Option<String>,
    gradient: Option<Vec<String>>,
}

/// `backdrop`: wrap a Composer SVG layer in its fill, embedding the layer as an image so its
/// content stays in an inert image document.
fn backdrop(url: &str, fill: Option<&Fill>) -> String {
    let colors: Vec<String> = match fill {
        Some(Fill { solid: Some(solid), .. }) if !solid.is_empty() => vec![solid.clone()],
        Some(Fill { gradient: Some(gradient), .. }) => gradient.clone(),
        _ => Vec::new(),
    };
    let colors: Vec<Option<String>> = colors
        .iter()
        .map(|color| {
            let found = COLOR.captures(color)?;
            let space = found.get(1)?.as_str();
            let values: Vec<f64> = found.get(2)?.as_str().split(',').map(js_parse).collect();
            let expected = if space == "gray" { 2 } else { 4 };
            if values.len() != expected || values.iter().any(|n| !n.is_finite() || *n < 0.0 || *n > 1.0) {
                return None;
            }
            let rgb = if space == "gray" { vec![values[0]; 3] } else { values[..3].to_vec() };
            let rgb: Vec<String> = rgb.into_iter().map(js_number).collect();
            let space = if space == "display-p3" { "display-p3" } else { "srgb" };
            Some(format!("color({space} {} / {})", rgb.join(" "), js_number(*values.last()?)))
        })
        .collect();
    if colors.is_empty() || colors.iter().any(Option::is_none) {
        return url.to_string();
    }
    let colors: Vec<String> = colors.into_iter().flatten().collect();
    let gradient = if colors.len() > 1 {
        let stops: String = colors
            .iter()
            .enumerate()
            .map(|(i, color)| {
                format!(r#"<stop offset="{}" stop-color="{color}"/>"#, js_number(i as f64 / (colors.len() - 1) as f64))
            })
            .collect();
        format!(r#"<defs><linearGradient id="bg" x2="0" y2="1">{stops}</linearGradient></defs>"#)
    } else {
        String::new()
    };
    let paint = if colors.len() > 1 { "url(#bg)".to_string() } else { colors[0].clone() };
    let markup = format!(
        r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 128 128">{gradient}<rect width="128" height="128" rx="28" fill="{paint}"/><image width="128" height="128" href="{url}"/></svg>"#
    );
    data_url(Path::new("icon.svg"), markup.as_bytes())
}

/// The Icon Composer manifest (`icon.json`): layer image names and the fill. `None` when it does
/// not decode.
fn composer(text: &str) -> Option<(Vec<Option<String>>, Option<Fill>)> {
    let value: Value = serde_json::from_str(text).ok()?;
    let object = value.as_object()?;
    let mut layers = Vec::new();
    for group in object.get("groups")?.as_array()? {
        for layer in group.as_object()?.get("layers")?.as_array()? {
            let layer = layer.as_object()?;
            layers.push(match layer.get("image-name") {
                None => None,
                Some(name) => Some(name.as_str()?.to_string()),
            });
        }
    }
    let fill = match object.get("fill") {
        None => None,
        Some(fill) => {
            let fill = fill.as_object()?;
            let solid = match fill.get("solid") {
                None => None,
                Some(solid) => Some(solid.as_str()?.to_string()),
            };
            let gradient = match fill.get("linear-gradient") {
                None => None,
                Some(list) => Some(
                    list.as_array()?
                        .iter()
                        .map(|item| item.as_str().map(str::to_string))
                        .collect::<Option<Vec<_>>>()?,
                ),
            };
            Some(Fill { solid, gradient })
        }
    };
    Some((layers, fill))
}

fn relative(root: &Path, file: &Path) -> String {
    oc_git::paths::relative(root, file)
}

/// `discover`: the first usable icon under `directory`, or `None`.
pub fn discover(directory: &Path) -> Option<Icon> {
    let root = std::fs::canonicalize(directory).ok()?;
    let mut children: Vec<String> = std::fs::read_dir(&root)
        .map(|entries| {
            entries
                .flatten()
                .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .filter(|name| !name.starts_with('.'))
                .collect()
        })
        .unwrap_or_default();
    children.sort_by(|a, b| utf16_cmp(a, b));
    let catalogs = children.into_iter().take(128).map(|name| format!("{name}/Assets.xcassets/AppIcon.appiconset"));
    // Prefer flattened icons: a small Composer layer may be only a glow beneath an oversized foreground.
    let names = candidates()
        .into_iter()
        .chain(["AppIcon.icon", "app/AppIcon.icon", "assets/AppIcon.icon"].map(String::from))
        .chain(catalogs)
        .chain(std::iter::once("resources/windows".to_string()));
    for name in names {
        let Ok(candidate) = std::fs::canonicalize(root.join(&name)) else { continue };
        if candidate == root || !contains(&root, &candidate) {
            continue;
        }
        if name.ends_with(".icon") {
            let config = read(&root, &candidate.join("icon.json"), &[".json"])
                .and_then(|(_, bytes)| composer(&String::from_utf8_lossy(&bytes)));
            let Some((layers, fill)) = config else { continue };
            for layer in layers.into_iter().flatten().filter(|name| !name.is_empty()) {
                let Some((file, bytes)) = read(&root, &candidate.join("Assets").join(&layer), &[".svg", ".png"]) else {
                    continue;
                };
                let plain = data_url(&file, &bytes);
                let url = if file.to_string_lossy().ends_with(".svg") {
                    backdrop(&plain, fill.as_ref())
                } else {
                    plain.clone()
                };
                if url.len() <= MAX_URL {
                    return Some(Icon { path: relative(&root, &file), url });
                }
                if plain.len() <= MAX_URL {
                    return Some(Icon { path: relative(&root, &file), url: plain });
                }
            }
            continue;
        }
        let windows = name == "resources/windows";
        let files: Vec<PathBuf> = if name.ends_with(".appiconset") || windows {
            let mut names: Vec<String> = std::fs::read_dir(&candidate)
                .map(|entries| {
                    entries
                        .flatten()
                        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
                        .map(|entry| entry.file_name().to_string_lossy().into_owned())
                        .filter(|file| {
                            if windows {
                                WINDOWS_ICON.is_match(file)
                            } else {
                                IMAGES.contains(&extname(Path::new(file)).as_str())
                            }
                        })
                        .collect()
                })
                .unwrap_or_default();
            let rank = |file: &str| -> f64 {
                if windows {
                    let size = WINDOWS_ICON.captures(file).map(|c| js_parse(&c[1])).unwrap_or(f64::NAN);
                    (size - 128.0).abs()
                } else if file.contains("128") || file.contains("256") {
                    0.0
                } else {
                    1.0
                }
            };
            names.sort_by(|a, b| utf16_cmp(a, b));
            names.sort_by(|a, b| rank(a).partial_cmp(&rank(b)).unwrap_or(std::cmp::Ordering::Equal));
            names.into_iter().take(128).map(|file| candidate.join(file)).collect()
        } else {
            vec![candidate.clone()]
        };
        for file in files {
            let Some((file, bytes)) = read(&root, &file, IMAGES) else { continue };
            let url = data_url(&file, &bytes);
            if url.len() <= MAX_URL {
                return Some(Icon { path: relative(&root, &file), url });
            }
        }
    }
    None
}

/// `refresh`: discover the project's icon and store it when it changed, returning the updated
/// row's `project.updated` payload. The `global` project never gets one.
pub fn refresh(bus: &Bus, id: &str) -> anyhow::Result<Option<Value>> {
    if id == crate::project::GLOBAL {
        return Ok(None);
    }
    let row: Option<(String, Option<String>)> = bus.db().with(|c| {
        c.query_row("SELECT worktree, icon_url FROM project WHERE id = ?", [id], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()
    })?;
    let Some((worktree, current)) = row else { return Ok(None) };
    let Some(image) = discover(Path::new(&worktree)) else { return Ok(None) };
    if current.as_deref() == Some(image.url.as_str()) {
        return Ok(None);
    }
    bus.db().with(|c| {
        c.query_row(
            "UPDATE project SET icon_url = ? WHERE id = ? RETURNING *",
            rusqlite::params![image.url, id],
            crate::project::project_json,
        )
        .optional()
    })
}

/// `persist` forks this: refresh in the background and publish `project.updated` on a change.
pub fn spawn_refresh(bus: &Bus, id: &str) {
    let Ok(handle) = tokio::runtime::Handle::try_current() else { return };
    let bus = bus.clone();
    let id = id.to_string();
    handle.spawn(async move {
        let published = bus.clone();
        let row = tokio::task::spawn_blocking(move || refresh(&bus, &id)).await;
        if let Ok(Ok(Some(info))) = row {
            published.publish("project.updated", info, None);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    const SVG: &str = r#"<svg xmlns="http://www.w3.org/2000/svg"/>"#;
    const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+aWZkAAAAASUVORK5CYII=";

    fn write(path: impl AsRef<Path>, content: impl AsRef<[u8]>) {
        let path = path.as_ref();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    fn url(content: &str) -> String {
        format!("data:image/svg+xml;base64,{}", base64::engine::general_purpose::STANDARD.encode(content))
    }

    // project-icon.test.ts "discovers the expected icon with a <kind>"
    #[test]
    fn discovers_the_expected_icon() {
        for kind in ["ranked appiconset", "escaping symlink", "oversized file"] {
            let tmp = tempfile::tempdir().unwrap();
            let root = tmp.path().join("project");
            write(root.join("public/favicon.svg"), SVG);
            if kind == "ranked appiconset" {
                write(root.join("Assets.xcassets/AppIcon.appiconset/000.png"), "other");
                write(root.join("Assets.xcassets/AppIcon.appiconset/icon256.svg"), format!("{SVG} "));
            }
            if kind == "escaping symlink" {
                write(tmp.path().join("outside.svg"), "outside");
                std::fs::create_dir_all(root.join("assets")).unwrap();
                #[cfg(unix)]
                std::os::unix::fs::symlink(tmp.path().join("outside.svg"), root.join("assets/app-icon.svg")).unwrap();
            }
            if kind == "oversized file" {
                write(root.join("assets/app-icon.svg"), "x".repeat(64 * 1024 + 1));
            }
            let expected = if kind == "ranked appiconset" { url(&format!("{SVG} ")) } else { url(SVG) };
            assert_eq!(discover(&root).unwrap().url, expected, "{kind}");
        }
    }

    // project-icon.test.ts "discovers <fixture> without a database"
    #[test]
    fn discovers_fixtures_without_a_database() {
        let fixtures: [(&str, &str, Option<&str>); 6] = [
            ("root Composer SVG", "AppIcon.icon/Assets/logo.svg", Some("AppIcon.icon/icon.json")),
            ("app Composer PNG", "app/AppIcon.icon/Assets/logo.png", Some("app/AppIcon.icon/icon.json")),
            ("assets Composer SVG", "assets/AppIcon.icon/Assets/logo.svg", Some("assets/AppIcon.icon/icon.json")),
            ("direct-child Xcode catalog", "OpenCode/Assets.xcassets/AppIcon.appiconset/icon128.svg", None),
            ("Windows nearest usable size", "resources/windows/icon-96.png", None),
            ("Scrawl oversized flat icon", "site/public/icon.svg", None),
        ];
        for (name, file, manifest) in fixtures {
            let tmp = tempfile::tempdir().unwrap();
            let base = tmp.path();
            if file.ends_with(".png") {
                write(base.join(file), base64::engine::general_purpose::STANDARD.decode(PNG).unwrap());
            } else {
                write(base.join(file), SVG);
            }
            if let Some(manifest) = manifest {
                let layer = Path::new(file).file_name().unwrap().to_string_lossy().into_owned();
                write(
                    base.join(manifest),
                    serde_json::json!({"groups": [{"layers": [{"image-name": layer}]}]}).to_string(),
                );
            }
            if name == "Windows nearest usable size" {
                write(base.join("resources/windows/icon-128.png"), "x".repeat(65537));
                write(base.join("resources/windows/icon-32.png"), "other");
            }
            if name == "Scrawl oversized flat icon" {
                write(base.join("assets/app-icon.png"), "x".repeat(65537));
            }
            assert_eq!(discover(base).map(|icon| icon.path), Some(file.to_string()), "{name}");
        }
    }

    // project-icon.test.ts "Composer backgrounds stay inert and malformed, oversized or escaping manifests fall through"
    #[test]
    fn composer_backgrounds_and_malformed_manifests() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path();
        let manifest = base.join("AppIcon.icon/icon.json");
        write(base.join("AppIcon.icon/Assets/logo.svg"), SVG);
        for fill in [
            serde_json::json!({"solid": "display-p3:0.075,0.063,0.063,1"}),
            serde_json::json!({"linear-gradient": ["gray:0.2,1", "srgb:0,0,0,1"]}),
        ] {
            write(
                &manifest,
                serde_json::json!({"fill": fill, "groups": [{"layers": [{"image-name": "logo.svg"}]}]}).to_string(),
            );
            let icon = discover(base).expect("Composer icon");
            let encoded = &icon.url[icon.url.find(',').unwrap() + 1..];
            let markup = String::from_utf8(base64::engine::general_purpose::STANDARD.decode(encoded).unwrap()).unwrap();
            assert!(markup.contains(&format!(r#"<image width="128" height="128" href="{}""#, url(SVG))));
            assert!(markup.contains("<rect"));
            assert!(icon.url.len() <= 65536);
        }
        write(&manifest, serde_json::json!({"fill": {"solid": "display-p3:0.075,0.063,0.063,1"}, "groups": [{"layers": [{"image-name": "logo.svg"}]}]}).to_string());
        let icon = discover(base).unwrap();
        let markup = String::from_utf8(
            base64::engine::general_purpose::STANDARD.decode(&icon.url[icon.url.find(',').unwrap() + 1..]).unwrap(),
        )
        .unwrap();
        assert_eq!(
            markup,
            format!(
                r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 128 128"><rect width="128" height="128" rx="28" fill="color(display-p3 0.075 0.063 0.063 / 1)"/><image width="128" height="128" href="{}"/></svg>"#,
                url(SVG)
            )
        );
        for content in [
            "{".to_string(),
            "x".repeat(65537),
            serde_json::json!({"groups": [{"layers": [{"image-name": "missing.svg"}]}]}).to_string(),
        ] {
            write(&manifest, content);
            assert_eq!(discover(base), None);
        }
        #[cfg(unix)]
        {
            let outside = tempfile::tempdir().unwrap();
            write(
                outside.path().join("icon.json"),
                serde_json::json!({"groups": [{"layers": [{"image-name": "logo.svg"}]}]}).to_string(),
            );
            std::fs::remove_file(&manifest).unwrap();
            std::os::unix::fs::symlink(outside.path().join("icon.json"), &manifest).unwrap();
            assert_eq!(discover(base), None);
        }
    }

    #[test]
    fn gradient_stops_use_js_numbers() {
        let fill = Fill {
            solid: None,
            gradient: Some(vec!["gray:0.2,1".into(), "srgb:0,0,0,1".into(), "srgb:1,1,1,.5".into()]),
        };
        let out = backdrop("data:x", Some(&fill));
        let markup = String::from_utf8(
            base64::engine::general_purpose::STANDARD.decode(&out[out.find(',').unwrap() + 1..]).unwrap(),
        )
        .unwrap();
        assert!(markup.contains(r#"<stop offset="0" stop-color="color(srgb 0.2 0.2 0.2 / 1)"/><stop offset="0.5" stop-color="color(srgb 0 0 0 / 1)"/><stop offset="1" stop-color="color(srgb 1 1 1 / 0.5)"/>"#));
        let invalid = Fill { solid: Some("srgb:2,0,0,1".into()), gradient: None };
        assert_eq!(backdrop("data:x", Some(&invalid)), "data:x");
    }
}
