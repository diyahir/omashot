//! MCP (Model Context Protocol) server over stdio so agents can drive Omacapture.
//!
//! Runs without GTK: captures go through grim, OCR through tesseract, and
//! rendering through the cairo-based annotation renderer. Interactive
//! captures spawn the GUI binary in `--wait` mode and read its JSON result.

use crate::annotate::model::*;
use crate::annotate::render::Renderer;
use crate::capture::{grim, hypr, Frame, Rect};
use anyhow::{anyhow, bail, Context, Result};
use base64::Engine;
use serde_json::{json, Value};
use std::io::{BufRead, Write};

const PROTOCOL_VERSION: &str = "2025-06-18";

static READ_ONLY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn read_only() -> bool {
    READ_ONLY.load(std::sync::atomic::Ordering::Relaxed)
}

/// Tools that write user-named files or change settings; hidden in read-only mode.
const WRITING_TOOLS: &[&str] = &["annotate", "redact", "set_config"];

/// Refuse to write outside the save folder, the private capture cache, the
/// source image's directory, and any `[mcp] allowed_write_dirs` entries.
fn ensure_write_allowed(target: &std::path::Path, source_dir: Option<&std::path::Path>) -> Result<()> {
    let cfg = crate::config::Config::load();
    let parent =
        target.parent().filter(|p| !p.as_os_str().is_empty()).map(|p| p.to_path_buf()).unwrap_or_else(|| std::path::PathBuf::from("."));
    let parent = std::fs::canonicalize(&parent).with_context(|| format!("output directory does not exist: {}", parent.display()))?;
    let mut roots: Vec<std::path::PathBuf> = vec![cfg.general.save_folder.clone(), crate::paths::temp_dir()];
    roots.extend(cfg.mcp.allowed_write_dirs.iter().cloned());
    if let Some(d) = source_dir {
        roots.push(d.to_path_buf());
    }
    let allowed = roots.iter().filter_map(|r| std::fs::canonicalize(r).ok()).any(|r| parent.starts_with(&r));
    if !allowed {
        bail!(
            "refusing to write {}: outside the allowed directories (save folder, capture cache, the source image's folder, and [mcp] allowed_write_dirs in config.toml)",
            target.display()
        );
    }
    Ok(())
}

pub fn run(read_only_mode: bool) -> Result<()> {
    READ_ONLY.store(read_only_mode, std::sync::atomic::Ordering::Relaxed);
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout().lock();
    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let msg: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                write_msg(&mut stdout, &json!({"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":format!("parse error: {e}")}}))?;
                continue;
            }
        };
        let id = msg.get("id").cloned();
        let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("");
        let params = msg.get("params").cloned().unwrap_or(Value::Null);
        if id.is_none() {
            // Notification: nothing to answer.
            continue;
        }
        let response = match handle(method, &params) {
            Ok(result) => json!({"jsonrpc":"2.0","id":id,"result":result}),
            Err(e) => {
                json!({"jsonrpc":"2.0","id":id,"error":{"code":-32603,"message":e.to_string()}})
            }
        };
        write_msg(&mut stdout, &response)?;
    }
    Ok(())
}

fn write_msg(out: &mut impl Write, v: &Value) -> Result<()> {
    out.write_all(serde_json::to_string(v)?.as_bytes())?;
    out.write_all(b"\n")?;
    out.flush()?;
    Ok(())
}

fn handle(method: &str, params: &Value) -> Result<Value> {
    match method {
        "initialize" => Ok(json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {"tools": {}, "resources": {}},
            "serverInfo": {"name": "omacapture", "version": env!("CARGO_PKG_VERSION")},
            "instructions": "Omacapture captures screenshots on a Wayland/Hyprland desktop and annotates images. \
                Use list_windows/list_monitors to discover targets, capture_* to grab pixels (images are returned \
                downscaled unless max_width is raised; the full-resolution file path is always returned), ocr to read \
                text, annotate to draw markup onto an image file, and open_editor to hand an image to the human."
        })),
        "ping" => Ok(json!({})),
        "tools/list" => {
            let tools: Vec<Value> = tool_definitions()
                .into_iter()
                .filter(|t| !read_only() || !WRITING_TOOLS.contains(&t.get("name").and_then(|n| n.as_str()).unwrap_or("")))
                .collect();
            Ok(json!({"tools": tools}))
        }
        "tools/call" => {
            let name = params.get("name").and_then(|n| n.as_str()).unwrap_or("");
            let args = params.get("arguments").cloned().unwrap_or(json!({}));
            match call_tool(name, &args) {
                Ok(content) => Ok(json!({"content": content, "isError": false})),
                Err(e) => Ok(json!({"content": [{"type":"text","text": e.to_string()}], "isError": true})),
            }
        }
        "resources/list" => Ok(json!({"resources": [
            {"uri": "omacapture://latest", "name": "Latest capture", "description": "The most recent screenshot in Omacapture's history", "mimeType": "image/png"},
            {"uri": "omacapture://history", "name": "Capture history", "description": "Recent captures as JSON", "mimeType": "application/json"}
        ]})),
        "resources/read" => {
            let uri = params.get("uri").and_then(|u| u.as_str()).unwrap_or("");
            let h = crate::history::History::open()?;
            match uri {
                "omacapture://latest" => {
                    let entry = h.list("", 1)?.into_iter().next().ok_or_else(|| anyhow!("no captures yet"))?;
                    let img = image::open(&entry.path)?.to_rgba8();
                    let png = crate::export::encode_png(&img)?;
                    Ok(
                        json!({"contents": [{"uri": uri, "mimeType": "image/png", "blob": base64::engine::general_purpose::STANDARD.encode(png)}]}),
                    )
                }
                "omacapture://history" => {
                    let entries: Vec<Value> = h
                        .list("", 50)?
                        .iter()
                        .map(|e| json!({"path": e.path, "width": e.width, "height": e.height, "created_at": e.created_at}))
                        .collect();
                    Ok(json!({"contents": [{"uri": uri, "mimeType": "application/json", "text": serde_json::to_string_pretty(&entries)?}]}))
                }
                _ => bail!("unknown resource: {uri}"),
            }
        }
        "prompts/list" => Ok(json!({"prompts": []})),
        _ => bail!("method not found: {method}"),
    }
}

fn tool_definitions() -> Vec<Value> {
    let capture_common = json!({
        "cursor": {"type":"boolean","description":"Include the mouse cursor","default":false},
        "save": {"type":"boolean","description":"Write the capture to the configured screenshots folder (default) instead of a temp file","default":true},
        "path": {"type":"string","description":"Explicit output path (must end in .png, .jpg, or .webp) inside the save folder, the capture cache, or an [mcp] allowed_write_dirs entry. Refuses to replace an existing file unless overwrite is true. Ignored in --read-only mode"},
        "overwrite": {"type":"boolean","description":"Allow replacing an existing file at path","default":false},
        "max_width": {"type":"integer","description":"Downscale the returned image to at most this width; the saved file keeps full resolution","default":1280},
        "return_image": {"type":"boolean","description":"Attach the image to the response","default":true}
    });
    let with = |extra: Value| {
        let mut m = capture_common.as_object().unwrap().clone();
        for (k, v) in extra.as_object().unwrap() {
            m.insert(k.clone(), v.clone());
        }
        Value::Object(m)
    };
    vec![
        json!({
            "name": "list_monitors",
            "description": "List connected monitors with their logical geometry and scale.",
            "inputSchema": {"type":"object","properties":{}}
        }),
        json!({
            "name": "list_windows",
            "description": "List visible windows on the active workspaces (Hyprland): address, class, title, position, size, focus order.",
            "inputSchema": {"type":"object","properties":{"all_workspaces":{"type":"boolean","default":false}}}
        }),
        json!({
            "name": "capture_screen",
            "description": "Capture a whole monitor (default: the focused one).",
            "inputSchema": {"type":"object","properties": with(json!({"monitor":{"type":"string","description":"Output name such as HDMI-A-1; omit for the focused monitor"}}))}
        }),
        json!({
            "name": "capture_area",
            "description": "Capture a rectangle given in logical screen coordinates.",
            "inputSchema": {"type":"object","required":["x","y","width","height"],"properties": with(json!({
                "x":{"type":"integer"},"y":{"type":"integer"},"width":{"type":"integer"},"height":{"type":"integer"}}))}
        }),
        json!({
            "name": "capture_window",
            "description": "Capture one window, matched by Hyprland address, class, or title substring (case-insensitive). With no matcher the focused window is used.",
            "inputSchema": {"type":"object","properties": with(json!({
                "address":{"type":"string"},"class":{"type":"string"},"title":{"type":"string"}}))}
        }),
        json!({
            "name": "capture_interactive",
            "description": "Ask the human to pick a region (mode=area) or window (mode=window) on screen, then return the capture. Blocks until they finish or cancel.",
            "inputSchema": {"type":"object","properties": with(json!({
                "mode":{"type":"string","enum":["area","window"],"default":"area"},
                "annotate":{"type":"boolean","description":"Open the annotation editor before saving","default":false}}))}
        }),
        json!({
            "name": "describe_screen",
            "description": "One call to understand what is on screen: captures the focused monitor (or a window), lists every visible window with its geometry, and runs OCR so each text line comes back with a bounding box in screen coordinates. Use the boxes to target annotations or clicks; use `windows` to pick a capture_window target.",
            "inputSchema": {"type":"object","properties":{
                "monitor":{"type":"string","description":"Output name; omit for the focused monitor"},
                "window":{"type":"string","description":"Restrict to one window by class or title substring"},
                "ocr":{"type":"boolean","default":true},
                "max_width":{"type":"integer","default":1280},
                "return_image":{"type":"boolean","default":true}}}
        }),
        json!({
            "name": "compare",
            "description": "Diff two images (for example a capture before and after an action). Returns the percentage of changed pixels and the bounding boxes of changed regions, and writes an image with those regions outlined next to the first file. Images of different sizes are compared after resizing the second to the first.",
            "inputSchema": {"type":"object","required":["path_a","path_b"],"properties":{
                "path_a":{"type":"string"},"path_b":{"type":"string"},
                "threshold":{"type":"integer","description":"Per-channel difference (0-255) that counts as a change","default":24},
                "output":{"type":"string","description":"Where to write the outlined diff image; defaults to <path_a>-diff.png"},
                "overwrite":{"type":"boolean","default":false},
                "max_width":{"type":"integer","default":1280},
                "return_image":{"type":"boolean","default":true}}}
        }),
        json!({
            "name": "ocr",
            "description": "Extract text from an image file, or from a screen area if x/y/width/height are given.",
            "inputSchema": {"type":"object","properties":{
                "path":{"type":"string"},
                "x":{"type":"integer"},"y":{"type":"integer"},"width":{"type":"integer"},"height":{"type":"integer"},
                "languages":{"type":"string","description":"Tesseract language codes, e.g. eng+deu"},
                "words":{"type":"boolean","description":"Return word bounding boxes as JSON instead of plain text","default":false}}}
        }),
        json!({
            "name": "annotate",
            "description": "Draw annotations onto an image and write the result. Every editor tool is available: rect, filled_rect, oval, line, arrow, text, label, callout, highlight, blur, spotlight, counter, watermark, pencil, plus crop and canvas backgrounds. Coordinates are source-image pixels (see read_image or a capture result for the size). Call describe_annotations for the full per-type field reference. With open_in_editor=true the result is also saved as an editable session and opened for the human, who can keep editing every item.",
            "inputSchema": {"type":"object","required":["path","items"],"properties":{
                "path":{"type":"string","description":"Source image"},
                "output":{"type":"string","description":"Destination path (.png/.jpg/.webp) inside the save folder, the capture cache, the source image's folder, or an [mcp] allowed_write_dirs entry; defaults to <source>-annotated.<ext>. Refuses to replace an existing file unless overwrite is true"},
                "overwrite":{"type":"boolean","default":false},
                "items":{"type":"array","description":"Annotation items, drawn in order (blur always renders under markup)","items":{"type":"object","required":["type"],"properties":{
                    "type":{"type":"string","enum":["rect","filled_rect","oval","line","arrow","text","label","callout","highlight","blur","spotlight","counter","watermark","pencil"]},
                    "x":{"type":"number"},"y":{"type":"number"},"width":{"type":"number"},"height":{"type":"number"},
                    "x1":{"type":"number"},"y1":{"type":"number"},"x2":{"type":"number"},"y2":{"type":"number"},
                    "points":{"type":"array","description":"[[x,y],...] for pencil and freehand highlight","items":{"type":"array","items":{"type":"number"}}},
                    "text":{"type":"string"},
                    "color":{"type":"string","description":"Hex color like #ea6962; defaults to the theme red"},
                    "stroke_width":{"type":"number","description":"Line thickness (1-20)"},
                    "line_style":{"type":"string","enum":["solid","dashed","dotted"]},
                    "corner_radius":{"type":"number"},
                    "font_size":{"type":"number"},
                    "opacity":{"type":"number"},"rotation":{"type":"number"},
                    "style":{"type":"string","description":"arrow: straight|curved_right|curved_left; watermark: single|diagonal|tiled"},
                    "kind":{"type":"string","enum":["classic","tapered","outlined"],"description":"arrow body"},
                    "head_start":{"type":"string","enum":["none","arrow","circle"]},
                    "head_end":{"type":"string","enum":["none","arrow","circle"]},
                    "presentation":{"type":"string","enum":["plain","label","callout"]},
                    "tail_x":{"type":"number"},"tail_y":{"type":"number"},
                    "effect":{"type":"string","enum":["pixelate","gaussian","hexagonal","crystallize","pointillism","halftone","tape","washi"]},
                    "strength":{"type":"number","description":"Blur strength 1-20"},
                    "dim":{"type":"number","description":"Spotlight dim 0.1-0.9 (global)"},
                    "number":{"type":"integer"},"size":{"type":"number","description":"Counter size 1-12"}
                }}},
                "crop":{"type":"object","properties":{"x":{"type":"number"},"y":{"type":"number"},"width":{"type":"number"},"height":{"type":"number"}}},
                "background":{"type":"string","description":"'wallpaper' (the Omarchy frame: the user's whole current wallpaper, lightly blurred, at its own aspect with the capture floating on it), a gradient preset name (pink orange, blue purple, green blue, orange red, purple pink, blue green, yellow orange, cyan blue), a hex color, 'blurred', or 'none'"},
                "padding":{"type":"number"},
                "corner_radius":{"type":"number","description":"Rounded corners of the image inside the canvas"},
                "shadow":{"type":"number","description":"0-1"},
                "open_in_editor":{"type":"boolean","description":"Save an editable session and open the result in the editor for the human","default":false},
                "max_width":{"type":"integer","default":1280},
                "return_image":{"type":"boolean","default":true}}}
        }),
        json!({
            "name": "describe_annotations",
            "description": "Reference for the annotate tool: every item type with its required and optional fields, defaults, and an example.",
            "inputSchema": {"type":"object","properties":{}}
        }),
        json!({
            "name": "redact",
            "description": "Find sensitive text in an image with local OCR (emails, phone numbers, URLs, card numbers, API tokens, key=value credentials) and pixelate it. Returns the redacted image and the list of regions.",
            "inputSchema": {"type":"object","required":["path"],"properties":{
                "path":{"type":"string"},
                "output":{"type":"string","description":"Destination (.png/.jpg/.webp); defaults to <source>-annotated.<ext>. Refuses to replace an existing file unless overwrite is true"},
                "overwrite":{"type":"boolean","default":false},
                "effect":{"type":"string","enum":["pixelate","gaussian","crystallize","halftone","tape","washi"],"default":"pixelate"},
                "strength":{"type":"number","default":8},
                "extra_patterns":{"type":"array","description":"Additional regular expressions; any matching word is redacted too","items":{"type":"string"}},
                "open_in_editor":{"type":"boolean","default":false},
                "max_width":{"type":"integer","default":1280},
                "return_image":{"type":"boolean","default":true}}}
        }),
        json!({
            "name": "get_config",
            "description": "Read Omacapture's configuration (~/.config/omacapture/config.toml): every key with its current value, as TOML plus JSON. Sections: general (save_folder, filename_pattern, format, quality, include_cursor, sound, notifications, remember_last_area, delay_ms), post_capture (fullscreen/area/window/annotate_export each with save, copy, quick_access, annotate), quick_access (enabled, corner, auto_dismiss_secs, max_cards, thumbnail_width, keep_editing_after_drag), annotate (stroke_color, stroke_width, font_family, font_size, blur_style, blur_strength, watermark_text, ...), history (enabled, retention_days, max_entries), ocr (languages, copy_to_clipboard).",
            "inputSchema": {"type":"object","properties":{}}
        }),
        json!({
            "name": "set_config",
            "description": "Change Omacapture settings. Pass a partial object nested by section, e.g. {\"general\":{\"format\":\"jpg\",\"quality\":85},\"quick_access\":{\"corner\":\"top-right\"}}. Unknown keys or invalid values are rejected and nothing is written. The running app reloads the file immediately.",
            "inputSchema": {"type":"object","required":["changes"],"properties":{
                "changes":{"type":"object","description":"Partial config, nested by section"}}}
        }),
        json!({
            "name": "history_list",
            "description": "List recent captures from Omacapture's history.",
            "inputSchema": {"type":"object","properties":{"limit":{"type":"integer","default":20},"search":{"type":"string"}}}
        }),
        json!({
            "name": "open_editor",
            "description": "Open an image in Omacapture's annotation editor for the human. Returns immediately.",
            "inputSchema": {"type":"object","required":["path"],"properties":{"path":{"type":"string"}}}
        }),
        json!({
            "name": "read_image",
            "description": "Return an image file (optionally downscaled) so it can be looked at.",
            "inputSchema": {"type":"object","required":["path"],"properties":{"path":{"type":"string"},"max_width":{"type":"integer","default":1280}}}
        }),
    ]
}

fn str_arg<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(|v| v.as_str())
}
fn int_arg(args: &Value, key: &str) -> Option<i64> {
    args.get(key).and_then(|v| v.as_i64().or_else(|| v.as_f64().map(|f| f.round() as i64)))
}
fn f_arg(args: &Value, key: &str) -> Option<f64> {
    args.get(key).and_then(|v| v.as_f64())
}
fn bool_arg(args: &Value, key: &str, default: bool) -> bool {
    args.get(key).and_then(|v| v.as_bool()).unwrap_or(default)
}

fn image_content(img: &image::RgbaImage, max_width: u32) -> Result<Value> {
    let max_width = if max_width == 0 { 1280 } else { max_width.min(8192) };
    let img = if img.width() > max_width {
        let h = (img.height() as f64 * max_width as f64 / img.width() as f64).round().max(1.0) as u32;
        image::imageops::resize(img, max_width, h, image::imageops::FilterType::Triangle)
    } else {
        img.clone()
    };
    let png = crate::export::encode_png(&img)?;
    Ok(json!({"type":"image","data": base64::engine::general_purpose::STANDARD.encode(png),"mimeType":"image/png"}))
}

fn text(v: impl Into<String>) -> Value {
    json!({"type":"text","text": v.into()})
}

/// Save a frame according to the tool args and build the standard response.
fn finish_capture(frame: Frame, args: &Value, extra: Value) -> Result<Vec<Value>> {
    let cfg = crate::config::Config::load();
    let explicit = if read_only() { None } else { str_arg(args, "path") };
    let path = if let Some(p) = explicit {
        let p = std::path::PathBuf::from(p);
        ensure_write_allowed(&p, None)?;
        crate::export::save_to(&frame.image, &p, cfg.general.quality, bool_arg(args, "overwrite", false))?;
        p
    } else if bool_arg(args, "save", true) && !read_only() {
        let p = crate::export::save(&frame.image, &cfg)?;
        if cfg.history.enabled {
            if let Ok(h) = crate::history::History::open() {
                let _ = h.insert(&p, frame.width(), frame.height(), None);
            }
        }
        p
    } else {
        crate::export::write_temp_png(&frame.image)?
    };
    let mut meta = json!({"path": path, "width": frame.width(), "height": frame.height(), "scale": frame.scale});
    if let Some(obj) = extra.as_object() {
        for (k, v) in obj {
            meta[k] = v.clone();
        }
    }
    let mut out = vec![text(serde_json::to_string_pretty(&meta)?)];
    if bool_arg(args, "return_image", true) {
        out.push(image_content(&frame.image, int_arg(args, "max_width").unwrap_or(1280).max(0) as u32)?);
    }
    Ok(out)
}

fn call_tool(name: &str, args: &Value) -> Result<Vec<Value>> {
    if read_only() && WRITING_TOOLS.contains(&name) {
        bail!("{name} is not available: this server runs in --read-only mode");
    }
    match name {
        "list_monitors" => {
            let mons = hypr::monitors().context("hyprctl monitors")?;
            let v: Vec<Value> = mons
                .iter()
                .map(|m| {
                    let r = m.logical_rect();
                    json!({"name": m.name, "x": r.x, "y": r.y, "width": r.w, "height": r.h, "scale": m.scale, "focused": m.focused, "workspace": m.active_workspace.id})
                })
                .collect();
            Ok(vec![text(serde_json::to_string_pretty(&v)?)])
        }
        "list_windows" => {
            let clients = if bool_arg(args, "all_workspaces", false) { hypr::all_windows()? } else { hypr::visible_windows()? };
            let v: Vec<Value> = clients
                .iter()
                .map(|c| json!({"address": c.address, "class": c.class, "title": c.title, "x": c.at[0], "y": c.at[1], "width": c.size[0], "height": c.size[1], "workspace": c.workspace.id, "floating": c.floating, "focus_order": c.focus_history_id}))
                .collect();
            Ok(vec![text(serde_json::to_string_pretty(&v)?)])
        }
        "capture_screen" => {
            let mons = hypr::monitors().context("hyprctl monitors")?;
            let m = match str_arg(args, "monitor") {
                Some(n) => mons.iter().find(|m| m.name.eq_ignore_ascii_case(n)).ok_or_else(|| anyhow!("no monitor named {n}"))?,
                None => mons.iter().find(|m| m.focused).or(mons.first()).ok_or_else(|| anyhow!("no monitors"))?,
            };
            let frame = grim::capture_output(&m.name, m.scale, bool_arg(args, "cursor", false))?;
            finish_capture(frame, args, json!({"monitor": m.name}))
        }
        "capture_area" => {
            let r = Rect::new(
                int_arg(args, "x").ok_or_else(|| anyhow!("x required"))? as i32,
                int_arg(args, "y").ok_or_else(|| anyhow!("y required"))? as i32,
                int_arg(args, "width").ok_or_else(|| anyhow!("width required"))?.max(1) as i32,
                int_arg(args, "height").ok_or_else(|| anyhow!("height required"))?.max(1) as i32,
            );
            let scale = hypr::monitors()
                .ok()
                .and_then(|ms| ms.iter().find(|m| m.logical_rect().intersect(&r).is_some()).map(|m| m.scale))
                .unwrap_or(1.0);
            let frame = grim::capture_region(r, scale, bool_arg(args, "cursor", false))?;
            finish_capture(frame, args, json!({"region": r}))
        }
        "capture_window" => {
            let clients = hypr::visible_windows()?;
            let lower = |s: &str| s.to_ascii_lowercase();
            let c = if let Some(a) = str_arg(args, "address") {
                clients.iter().find(|c| c.address == a)
            } else if let Some(cl) = str_arg(args, "class") {
                clients.iter().find(|c| lower(&c.class).contains(&lower(cl)))
            } else if let Some(t) = str_arg(args, "title") {
                clients.iter().find(|c| lower(&c.title).contains(&lower(t)))
            } else {
                clients.iter().min_by_key(|c| c.focus_history_id)
            }
            .ok_or_else(|| anyhow!("no matching window"))?;
            let r = c.rect();
            let scale = hypr::monitors().ok().and_then(|ms| ms.iter().find(|m| m.id == c.monitor).map(|m| m.scale)).unwrap_or(1.0);
            let frame = grim::capture_region(r, scale, bool_arg(args, "cursor", false))?;
            finish_capture(frame, args, json!({"window": {"address": c.address, "class": c.class, "title": c.title}, "region": r}))
        }
        "capture_interactive" => {
            let mode = str_arg(args, "mode").unwrap_or("area");
            let exe = std::env::current_exe()?;
            let mut cmd = std::process::Command::new(exe);
            cmd.arg("--wait");
            match mode {
                "window" => {
                    cmd.arg("window");
                }
                _ => {
                    cmd.arg("area");
                    if bool_arg(args, "annotate", false) {
                        cmd.arg("--annotate");
                    }
                }
            }
            let out = cmd.stderr(std::process::Stdio::null()).output().context("launching interactive capture")?;
            let stdout = String::from_utf8_lossy(&out.stdout);
            let line = stdout.lines().rev().find(|l| l.starts_with('{')).ok_or_else(|| anyhow!("capture cancelled"))?;
            let v: Value = serde_json::from_str(line)?;
            let path = v.get("path").and_then(|p| p.as_str()).ok_or_else(|| anyhow!("capture cancelled"))?;
            let img = image::open(path)?.to_rgba8();
            let mut out = vec![text(serde_json::to_string_pretty(&v)?)];
            if bool_arg(args, "return_image", true) {
                out.push(image_content(&img, int_arg(args, "max_width").unwrap_or(1280).max(0) as u32)?);
            }
            Ok(out)
        }
        "describe_screen" => describe_screen(args),
        "compare" => compare_tool(args),
        "ocr" => {
            let cfg = crate::config::Config::load();
            let langs = str_arg(args, "languages").map(|s| s.to_string()).unwrap_or(cfg.ocr.languages);
            let png = if let Some(p) = str_arg(args, "path") {
                let img = image::open(p)?.to_rgba8();
                crate::export::encode_png(&img)?
            } else {
                let r = Rect::new(
                    int_arg(args, "x").ok_or_else(|| anyhow!("path or x/y/width/height required"))? as i32,
                    int_arg(args, "y").unwrap_or(0) as i32,
                    int_arg(args, "width").unwrap_or(1).max(1) as i32,
                    int_arg(args, "height").unwrap_or(1).max(1) as i32,
                );
                crate::export::encode_png(&grim::capture_region(r, 1.0, false)?.image)?
            };
            if bool_arg(args, "words", false) {
                let words = crate::ocr::words(&png, &langs)?;
                let v: Vec<Value> =
                    words.iter().map(|w| json!({"text": w.text, "x": w.x, "y": w.y, "width": w.w, "height": w.h})).collect();
                Ok(vec![text(serde_json::to_string_pretty(&v)?)])
            } else {
                Ok(vec![text(crate::ocr::recognize(&png, &langs)?)])
            }
        }
        "annotate" => annotate_tool(args),
        "describe_annotations" => Ok(vec![text(ANNOTATION_REFERENCE)]),
        "redact" => redact_tool(args),
        "get_config" => {
            let cfg = crate::config::Config::load();
            Ok(vec![
                text(format!("# {}\n{}", crate::paths::config_file().display(), cfg.to_toml())),
                text(serde_json::to_string_pretty(&serde_json::to_value(&cfg)?)?),
            ])
        }
        "set_config" => {
            let patch = args.get("changes").cloned().ok_or_else(|| anyhow!("changes required"))?;
            if !patch.is_object() {
                bail!("changes must be an object nested by section");
            }
            let touches = |section: &str, key: Option<&str>| {
                patch.get(section).map(|v| key.map(|k| v.get(k).is_some()).unwrap_or(true)).unwrap_or(false)
            };
            if touches("general", Some("save_folder")) || touches("mcp", None) || touches("keybinds", None) {
                bail!("general.save_folder, [mcp], and [keybinds] cannot be changed over MCP; edit them in Preferences or config.toml");
            }
            let mut cfg = crate::config::Config::load();
            cfg.merge_json(&patch).context("invalid setting")?;
            cfg.save()?;
            Ok(vec![text(format!("Saved {}\n{}", crate::paths::config_file().display(), cfg.to_toml()))])
        }
        "history_list" => {
            let h = crate::history::History::open()?;
            let entries = h.list(str_arg(args, "search").unwrap_or(""), int_arg(args, "limit").unwrap_or(20).clamp(1, 500) as u32)?;
            let v: Vec<Value> = entries
                .iter()
                .map(|e| json!({"id": e.id, "path": e.path, "created_at": chrono::DateTime::from_timestamp(e.created_at, 0).map(|t| t.to_rfc3339()).unwrap_or_default(), "width": e.width, "height": e.height, "editable_session": e.session_path.is_some()}))
                .collect();
            Ok(vec![text(serde_json::to_string_pretty(&v)?)])
        }
        "open_editor" => {
            let path = str_arg(args, "path").ok_or_else(|| anyhow!("path required"))?;
            let exe = std::env::current_exe()?;
            std::process::Command::new(exe)
                .arg("annotate")
                .arg(path)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()?;
            Ok(vec![text(format!("Opened {path} in the editor"))])
        }
        "read_image" => {
            let path = str_arg(args, "path").ok_or_else(|| anyhow!("path required"))?;
            let img = image::open(path)?.to_rgba8();
            Ok(vec![
                text(serde_json::to_string(&json!({"path": path, "width": img.width(), "height": img.height()}))?),
                image_content(&img, int_arg(args, "max_width").unwrap_or(1280).max(0) as u32)?,
            ])
        }
        _ => bail!("unknown tool: {name}"),
    }
}

/// Capture + windows + OCR lines, all in screen coordinates.
fn describe_screen(args: &Value) -> Result<Vec<Value>> {
    let cfg = crate::config::Config::load();
    let mons = hypr::monitors().context("hyprctl monitors")?;
    let windows = hypr::visible_windows().unwrap_or_default();
    let lower = |s: &str| s.to_ascii_lowercase();
    // Region to capture: a window, a named monitor, or the focused monitor.
    let (region, scale, label) = if let Some(w) = str_arg(args, "window") {
        let c = windows
            .iter()
            .find(|c| lower(&c.class).contains(&lower(w)) || lower(&c.title).contains(&lower(w)))
            .ok_or_else(|| anyhow!("no visible window matches {w:?}"))?;
        let scale = mons.iter().find(|m| m.id == c.monitor).map(|m| m.scale).unwrap_or(1.0);
        (c.rect(), scale, json!({"window": {"address": c.address, "class": c.class, "title": c.title}}))
    } else {
        let m = match str_arg(args, "monitor") {
            Some(n) => mons.iter().find(|m| m.name.eq_ignore_ascii_case(n)).ok_or_else(|| anyhow!("no monitor named {n}"))?,
            None => mons.iter().find(|m| m.focused).or(mons.first()).ok_or_else(|| anyhow!("no monitors"))?,
        };
        (m.logical_rect(), m.scale, json!({"monitor": m.name}))
    };
    let frame = grim::capture_region(region, scale, false)?;
    // Windows inside the captured region, front-most first.
    let wins: Vec<Value> = windows
        .iter()
        .filter(|c| c.rect().intersect(&region).is_some())
        .map(|c| json!({"address": c.address, "class": c.class, "title": c.title, "x": c.at[0], "y": c.at[1], "width": c.size[0], "height": c.size[1], "focus_order": c.focus_history_id}))
        .collect();
    let mut lines = Vec::new();
    if bool_arg(args, "ocr", true) {
        let png = crate::export::encode_png(&frame.image)?;
        match crate::ocr::words(&png, &cfg.ocr.languages) {
            Ok(words) => {
                // Group words into lines and report screen-space boxes (logical pixels).
                let grouped = crate::annotate::canvas::group_lines(&words);
                for l in grouped {
                    let ws: Vec<&crate::ocr::Word> =
                        words.iter().filter(|w| ((w.y as f64 + w.h as f64 / 2.0) - l.y).abs() < l.height * 0.6).collect();
                    if ws.is_empty() {
                        continue;
                    }
                    let x0 = ws.iter().map(|w| w.x).min().unwrap();
                    let x1 = ws.iter().map(|w| w.x + w.w).max().unwrap();
                    let y0 = ws.iter().map(|w| w.y).min().unwrap();
                    let y1 = ws.iter().map(|w| w.y + w.h).max().unwrap();
                    let mut sorted = ws.clone();
                    sorted.sort_by_key(|w| w.x);
                    let text = sorted.iter().map(|w| w.text.as_str()).collect::<Vec<_>>().join(" ");
                    let to_screen = |v: i32| (v as f64 / scale).round() as i32;
                    lines.push(json!({
                        "text": text,
                        "x": region.x + to_screen(x0), "y": region.y + to_screen(y0),
                        "width": to_screen(x1 - x0), "height": to_screen(y1 - y0),
                        "image_x": x0, "image_y": y0, "image_width": x1 - x0, "image_height": y1 - y0
                    }));
                }
            }
            Err(e) => tracing::info!("OCR unavailable: {e}"),
        }
    }
    let path = crate::export::write_temp_png(&frame.image)?;
    let mut meta = json!({
        "path": path, "region": region, "scale": scale, "width": frame.width(), "height": frame.height(),
        "coordinate_note": "x/y/width/height are logical screen pixels (use with capture_area or clicks); image_* fields are pixels in the returned file",
        "windows": wins, "text_lines": lines
    });
    if let Some(obj) = label.as_object() {
        for (k, v) in obj {
            meta[k] = v.clone();
        }
    }
    let mut out = vec![text(serde_json::to_string_pretty(&meta)?)];
    if bool_arg(args, "return_image", true) {
        out.push(image_content(&frame.image, int_arg(args, "max_width").unwrap_or(1280).max(0) as u32)?);
    }
    Ok(out)
}

/// Pixel diff of two images with changed-region boxes and an outlined output image.
fn compare_tool(args: &Value) -> Result<Vec<Value>> {
    let pa = std::path::PathBuf::from(str_arg(args, "path_a").ok_or_else(|| anyhow!("path_a required"))?);
    let pb = std::path::PathBuf::from(str_arg(args, "path_b").ok_or_else(|| anyhow!("path_b required"))?);
    let a = image::open(&pa).with_context(|| format!("opening {}", pa.display()))?.to_rgba8();
    let mut b = image::open(&pb).with_context(|| format!("opening {}", pb.display()))?.to_rgba8();
    if b.dimensions() != a.dimensions() {
        b = image::imageops::resize(&b, a.width(), a.height(), image::imageops::FilterType::Triangle);
    }
    let threshold = int_arg(args, "threshold").unwrap_or(24).clamp(0, 255) as i32;
    let (w, h) = a.dimensions();
    // Work on a coarse grid so regions are stable and the output is small.
    let cell: u32 = 8;
    let (gw, gh) = (w.div_ceil(cell), h.div_ceil(cell));
    let mut changed = vec![false; (gw * gh) as usize];
    let mut changed_px: u64 = 0;
    for y in 0..h {
        for x in 0..w {
            let (pa_, pb_) = (a.get_pixel(x, y), b.get_pixel(x, y));
            if (0..3).any(|c| (pa_[c] as i32 - pb_[c] as i32).abs() > threshold) {
                changed_px += 1;
                changed[((y / cell) * gw + x / cell) as usize] = true;
            }
        }
    }
    // Connected components on the grid → bounding boxes.
    let mut seen = vec![false; changed.len()];
    let mut regions: Vec<RectF> = Vec::new();
    for start in 0..changed.len() {
        if !changed[start] || seen[start] {
            continue;
        }
        let (mut x0, mut y0, mut x1, mut y1) = (u32::MAX, u32::MAX, 0u32, 0u32);
        let mut stack = vec![start];
        seen[start] = true;
        while let Some(i) = stack.pop() {
            let (cx, cy) = (i as u32 % gw, i as u32 / gw);
            x0 = x0.min(cx);
            y0 = y0.min(cy);
            x1 = x1.max(cx);
            y1 = y1.max(cy);
            for (dx, dy) in [(-1i64, 0i64), (1, 0), (0, -1), (0, 1), (-1, -1), (1, 1), (-1, 1), (1, -1)] {
                let (nx, ny) = (cx as i64 + dx, cy as i64 + dy);
                if nx < 0 || ny < 0 || nx >= gw as i64 || ny >= gh as i64 {
                    continue;
                }
                let j = (ny as u32 * gw + nx as u32) as usize;
                if changed[j] && !seen[j] {
                    seen[j] = true;
                    stack.push(j);
                }
            }
        }
        regions.push(RectF::new(
            (x0 * cell) as f64,
            (y0 * cell) as f64,
            ((x1 - x0 + 1) * cell).min(w - x0 * cell) as f64,
            ((y1 - y0 + 1) * cell).min(h - y0 * cell) as f64,
        ));
    }
    regions.sort_by(|r1, r2| (r2.w * r2.h).partial_cmp(&(r1.w * r1.h)).unwrap());
    let percent = if w * h == 0 { 0.0 } else { changed_px as f64 * 100.0 / (w as f64 * h as f64) };
    // Outline the regions on image A.
    let mut doc = Document::new(Frame { image: a.clone(), scale: 1.0 });
    let style = Style { color: Color::rgba(1.0, 0.23, 0.19, 1.0), width: 3.0, ..Style::default() };
    for (i, r) in regions.iter().take(50).enumerate() {
        doc.add(Kind::Rect { rect: r.inflate(2.0), filled: false }, style.clone());
        if i < 20 {
            doc.add(Kind::Counter { center: Pt::new(r.x.max(12.0), r.y.max(12.0)), number: (i + 1) as u32, size: 4.0 }, style.clone());
        }
    }
    let mut renderer = Renderer::new(&doc.source);
    let out = renderer.render_export(&doc);
    let output = match str_arg(args, "output") {
        Some(o) => std::path::PathBuf::from(o),
        None => pa.with_file_name(format!(
            "{}-diff.png",
            pa.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| "image".into())
        )),
    };
    ensure_write_allowed(&output, pa.parent())?;
    crate::export::save_to(&out, &output, 100, bool_arg(args, "overwrite", false))?;
    let region_json: Vec<Value> = regions.iter().map(|r| json!({"x": r.x, "y": r.y, "width": r.w, "height": r.h})).collect();
    let mut content = vec![text(serde_json::to_string_pretty(&json!({
        "changed_percent": (percent * 100.0).round() / 100.0,
        "changed_regions": region_json.len(),
        "regions": region_json.iter().take(50).collect::<Vec<_>>(),
        "diff_image": output,
        "identical": changed_px == 0
    }))?)];
    if bool_arg(args, "return_image", true) && changed_px > 0 {
        content.push(image_content(&out, int_arg(args, "max_width").unwrap_or(1280).max(0) as u32)?);
    }
    Ok(content)
}

fn color_of(v: &Value, key: &str, default: Color) -> Color {
    v.get(key).and_then(|c| c.as_str()).and_then(Color::parse).unwrap_or(default)
}

/// Build annotation items from JSON and render them onto the image.
fn annotate_tool(args: &Value) -> Result<Vec<Value>> {
    let path = std::path::PathBuf::from(str_arg(args, "path").ok_or_else(|| anyhow!("path required"))?);
    let img = image::open(&path).with_context(|| format!("opening {}", path.display()))?.to_rgba8();
    let cfg = crate::config::Config::load();
    let mut doc = Document::new(Frame { image: img, scale: 1.0 });
    let base_style = Style {
        color: Color::parse(&cfg.annotate.stroke_color).unwrap_or_default_color(),
        width: cfg.annotate.stroke_width,
        font_family: cfg.annotate.font_family.clone(),
        font_size: cfg.annotate.font_size,
        ..Style::default()
    };
    let items = args.get("items").and_then(|i| i.as_array()).cloned().unwrap_or_default();
    let mut counter = 1u32;
    for it in &items {
        let kind_name = str_arg(it, "type").unwrap_or("rect");
        let mut style = base_style.clone();
        style.color = color_of(it, "color", style.color);
        if let Some(w) = f_arg(it, "width")
            .filter(|_| !matches!(kind_name, "rect" | "filled_rect" | "oval" | "blur" | "spotlight" | "watermark" | "text"))
        {
            style.width = w;
        }
        if let Some(w) = f_arg(it, "stroke_width") {
            style.width = w;
        }
        if let Some(f) = f_arg(it, "font_size") {
            style.font_size = f;
        }
        if let Some(r) = f_arg(it, "corner_radius") {
            style.corner_radius = r;
        }
        if let Some(o) = f_arg(it, "opacity") {
            style.opacity = o;
        }
        if let Some(r) = f_arg(it, "rotation") {
            style.rotation = r;
        }
        style.line_style = match str_arg(it, "line_style") {
            Some("dashed") => LineStyle::Dashed,
            Some("dotted") => LineStyle::Dotted,
            _ => LineStyle::Solid,
        };
        let rect = || -> Result<RectF> {
            Ok(RectF::new(
                f_arg(it, "x").ok_or_else(|| anyhow!("{kind_name}: x required"))?,
                f_arg(it, "y").ok_or_else(|| anyhow!("{kind_name}: y required"))?,
                f_arg(it, "width").ok_or_else(|| anyhow!("{kind_name}: width required"))?,
                f_arg(it, "height").ok_or_else(|| anyhow!("{kind_name}: height required"))?,
            ))
        };
        let endpoints = || -> Result<(Pt, Pt)> {
            Ok((
                Pt::new(
                    f_arg(it, "x1").ok_or_else(|| anyhow!("{kind_name}: x1 required"))?,
                    f_arg(it, "y1").ok_or_else(|| anyhow!("y1 required"))?,
                ),
                Pt::new(
                    f_arg(it, "x2").ok_or_else(|| anyhow!("{kind_name}: x2 required"))?,
                    f_arg(it, "y2").ok_or_else(|| anyhow!("y2 required"))?,
                ),
            ))
        };
        let points = || -> Vec<Pt> {
            it.get("points")
                .and_then(|p| p.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|p| match p {
                            Value::Array(xy) if xy.len() >= 2 => Some(Pt::new(xy[0].as_f64()?, xy[1].as_f64()?)),
                            Value::Object(_) => Some(Pt::new(f_arg(p, "x")?, f_arg(p, "y")?)),
                            _ => None,
                        })
                        .collect()
                })
                .unwrap_or_default()
        };
        let kind = match kind_name {
            "rect" | "rectangle" => Kind::Rect { rect: rect()?, filled: false },
            "filled_rect" | "filled_rectangle" => Kind::Rect { rect: rect()?, filled: true },
            "oval" | "ellipse" | "circle" => Kind::Oval { rect: rect()? },
            "line" => {
                let (a, b) = endpoints()?;
                Kind::Line { a, b }
            }
            "arrow" => {
                let (a, b) = endpoints()?;
                let astyle = match str_arg(it, "style") {
                    Some("curved_right") => ArrowStyle::CurvedRight,
                    Some("curved_left") => ArrowStyle::CurvedLeft,
                    _ => ArrowStyle::Straight,
                };
                let head = |k: &str, d: Head| match str_arg(it, k) {
                    Some("none") => Head::None,
                    Some("arrow") => Head::Arrow,
                    Some("circle") => Head::Circle,
                    _ => d,
                };
                Kind::Arrow {
                    a,
                    b,
                    ctrl: crate::annotate::canvas::curve_ctrl(a, b, astyle),
                    style: astyle,
                    kind: match str_arg(it, "kind") {
                        Some("tapered") => ArrowType::Tapered,
                        Some("outlined") => ArrowType::Outlined,
                        _ => ArrowType::Classic,
                    },
                    head_start: head("head_start", Head::None),
                    head_end: head("head_end", Head::Arrow),
                }
            }
            "text" | "label" | "callout" => {
                let presentation = match (kind_name, str_arg(it, "presentation")) {
                    ("label", _) | (_, Some("label")) => TextPresentation::Label,
                    ("callout", _) | (_, Some("callout")) => TextPresentation::Callout,
                    _ => TextPresentation::Plain,
                };
                let tail = match (f_arg(it, "tail_x"), f_arg(it, "tail_y")) {
                    (Some(x), Some(y)) => Some(Pt::new(x, y)),
                    _ => None,
                };
                Kind::Text {
                    pos: Pt::new(f_arg(it, "x").unwrap_or(0.0), f_arg(it, "y").unwrap_or(0.0)),
                    text: str_arg(it, "text").unwrap_or("").to_string(),
                    presentation,
                    tail,
                }
            }
            "highlight" | "highlighter" => {
                let pts = if it.get("points").is_some() {
                    points()
                } else {
                    let r = rect()?;
                    style.width = (r.h / 3.0).max(2.0);
                    vec![Pt::new(r.x, r.y + r.h / 2.0), Pt::new(r.right(), r.y + r.h / 2.0)]
                };
                if style.color == base_style.color && it.get("color").is_none() {
                    style.color = Color::parse("#ffcc00").unwrap();
                }
                Kind::Highlight { points: pts }
            }
            "blur" | "pixelate" | "redact" => Kind::Blur {
                rect: rect()?,
                effect: match str_arg(it, "effect") {
                    Some("gaussian") => BlurEffect::Gaussian,
                    Some("hexagonal") => BlurEffect::Hexagonal,
                    Some("crystallize") => BlurEffect::Crystallize,
                    Some("pointillism") => BlurEffect::Pointillism,
                    Some("halftone") => BlurEffect::Halftone,
                    Some("tape") => BlurEffect::Tape,
                    Some("washi") => BlurEffect::Washi,
                    _ => BlurEffect::Pixelate,
                },
                strength: f_arg(it, "strength").unwrap_or(cfg.annotate.blur_strength).clamp(1.0, 20.0),
            },
            "spotlight" => {
                if let Some(d) = f_arg(it, "dim") {
                    doc.sheet.spotlight_dim = d;
                }
                Kind::Spotlight { rect: rect()? }
            }
            "counter" | "number" => {
                let n = int_arg(it, "number").map(|n| n as u32).unwrap_or(counter);
                counter = n + 1;
                Kind::Counter {
                    center: Pt::new(f_arg(it, "x").unwrap_or(0.0), f_arg(it, "y").unwrap_or(0.0)),
                    number: n,
                    size: f_arg(it, "size").unwrap_or(5.0),
                }
            }
            "watermark" => {
                let r = if it.get("x").is_some() { rect()? } else { doc.image_rect() };
                if it.get("opacity").is_none() {
                    style.opacity = 0.35;
                }
                Kind::Watermark {
                    rect: r,
                    text: str_arg(it, "text").unwrap_or("").to_string(),
                    style: match str_arg(it, "style") {
                        Some("single") => WatermarkStyle::Single,
                        Some("tiled") => WatermarkStyle::Tiled,
                        _ => WatermarkStyle::Diagonal,
                    },
                }
            }
            "pencil" | "freehand" => Kind::Pencil { points: points() },
            other => bail!("unknown item type: {other}"),
        };
        doc.add(kind, style);
    }
    if let Some(c) = args.get("crop") {
        doc.sheet.crop = Some(RectF::new(
            f_arg(c, "x").unwrap_or(0.0),
            f_arg(c, "y").unwrap_or(0.0),
            f_arg(c, "width").unwrap_or(doc.width()),
            f_arg(c, "height").unwrap_or(doc.height()),
        ));
    }
    if let Some(bg) = str_arg(args, "background") {
        doc.sheet.canvas.background = match bg {
            "none" => Background::None,
            "wallpaper" | "omarchy" | "omarchy-frame" => {
                let (w, h) = (doc.width(), doc.height());
                doc.sheet.canvas = Canvas::omarchy_frame(w, h);
                doc.sheet.canvas.background.clone()
            }
            "blurred" => Background::Blurred { strength: 8.0, dim: 0.15 },
            hex if hex.starts_with('#') => Background::Solid { color: Color::parse(hex).unwrap_or(Color::rgba(0.1, 0.1, 0.1, 1.0)) },
            name => {
                let lname = name.to_ascii_lowercase().replace(['-', '_'], " ");
                let (_, a, b) = GRADIENTS.iter().find(|g| g.0.to_ascii_lowercase() == lname).cloned().unwrap_or(GRADIENTS[1]);
                Background::Gradient { from: Color::parse(a).unwrap(), to: Color::parse(b).unwrap(), angle: 135.0 }
            }
        };
        if doc.sheet.canvas.background != Background::None && f_arg(args, "padding").is_none() && !doc.sheet.canvas.is_omarchy_frame() {
            doc.sheet.canvas.padding = 48.0;
        }
    }
    if let Some(p) = f_arg(args, "padding") {
        doc.sheet.canvas.padding = p.clamp(0.0, 1024.0);
    }
    if let Some(r) = f_arg(args, "corner_radius") {
        doc.sheet.canvas.corner_radius = r.clamp(0.0, 512.0);
    }
    if let Some(s) = f_arg(args, "shadow") {
        doc.sheet.canvas.shadow = s.clamp(0.0, 1.0);
    }
    finish_document(args, &path, doc, items.len())
}

/// Render a document, write it, optionally persist an editable session and open the editor.
fn finish_document(args: &Value, path: &std::path::Path, doc: Document, item_count: usize) -> Result<Vec<Value>> {
    let cfg = crate::config::Config::load();
    let mut renderer = Renderer::new(&doc.source);
    let out = renderer.render_export(&doc);
    let output = match str_arg(args, "output") {
        Some(o) => std::path::PathBuf::from(o),
        None => {
            let stem = path.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| "image".into());
            let ext = path.extension().map(|e| e.to_string_lossy().to_string()).unwrap_or_else(|| "png".into());
            let mut n = 1;
            loop {
                let candidate =
                    path.with_file_name(format!("{stem}-annotated{}.{ext}", if n == 1 { String::new() } else { format!("-{n}") }));
                if !candidate.exists() {
                    break candidate;
                }
                n += 1;
            }
        }
    };
    ensure_write_allowed(&output, path.parent())?;
    crate::export::save_to(&out, &output, cfg.general.quality, bool_arg(args, "overwrite", false))?;
    if cfg.history.enabled {
        if let Ok(h) = crate::history::History::open() {
            let _ = h.insert(&output, out.width(), out.height(), None);
        }
    }
    let editable = bool_arg(args, "open_in_editor", false);
    let mut session_path = None;
    if editable {
        // Persist the original pixels + items so the editor reopens everything as live objects.
        match crate::annotate::session::save(&output, &doc.source, &doc.sheet) {
            Ok(sp) => {
                if let Ok(h) = crate::history::History::open() {
                    let _ = h.set_session(&output, &sp);
                }
                session_path = Some(sp);
            }
            Err(e) => tracing::warn!("session save failed: {e}"),
        }
        let exe = std::env::current_exe()?;
        std::process::Command::new(exe)
            .arg("annotate")
            .arg(&output)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()?;
    }
    let mut content = vec![text(serde_json::to_string_pretty(&json!({
        "path": output, "width": out.width(), "height": out.height(), "items": item_count,
        "editable_session": session_path, "opened_in_editor": editable
    }))?)];
    if bool_arg(args, "return_image", true) {
        content.push(image_content(&out, int_arg(args, "max_width").unwrap_or(1280).max(0) as u32)?);
    }
    Ok(content)
}

fn redact_tool(args: &Value) -> Result<Vec<Value>> {
    let path = std::path::PathBuf::from(str_arg(args, "path").ok_or_else(|| anyhow!("path required"))?);
    let img = image::open(&path).with_context(|| format!("opening {}", path.display()))?.to_rgba8();
    let cfg = crate::config::Config::load();
    let png = crate::export::encode_png(&img)?;
    let words = crate::ocr::words(&png, &cfg.ocr.languages)?;
    let strength = f_arg(args, "strength").unwrap_or(8.0).clamp(1.0, 20.0);
    let effect = match str_arg(args, "effect") {
        Some("gaussian") => BlurEffect::Gaussian,
        Some("crystallize") => BlurEffect::Crystallize,
        Some("halftone") => BlurEffect::Halftone,
        Some("tape") => BlurEffect::Tape,
        Some("washi") => BlurEffect::Washi,
        _ => BlurEffect::Pixelate,
    };
    let mut extra: Vec<regex::Regex> = Vec::new();
    if let Some(patterns) = args.get("extra_patterns").and_then(|p| p.as_array()) {
        for v in patterns {
            let p = v.as_str().ok_or_else(|| anyhow!("extra_patterns must be strings"))?;
            extra.push(regex::Regex::new(p).with_context(|| format!("invalid extra pattern {p:?}"))?);
        }
    }
    let mut doc = Document::new(Frame { image: img, scale: 1.0 });
    let style = Style::default();
    let mut regions = Vec::new();
    let mut items = crate::annotate::redact::redaction_items(&words, &style, strength);
    for w in &words {
        if extra.iter().any(|r| r.is_match(&w.text)) {
            let r = RectF::new(w.x as f64, w.y as f64, w.w as f64, w.h as f64).inflate(3.0);
            items.push((Kind::Blur { rect: r, effect, strength }, style.clone()));
        }
    }
    for (kind, st) in items {
        if let Kind::Blur { rect, .. } = &kind {
            regions.push(json!({"x": rect.x, "y": rect.y, "width": rect.w, "height": rect.h}));
        }
        let kind = match kind {
            Kind::Blur { rect, strength, .. } => Kind::Blur { rect, effect, strength },
            k => k,
        };
        doc.add(kind, st);
    }
    let count = regions.len();
    let mut content = finish_document(args, &path, doc, count)?;
    content.insert(1, text(serde_json::to_string_pretty(&json!({"redacted_regions": regions}))?));
    Ok(content)
}

const ANNOTATION_REFERENCE: &str = r##"Omacapture annotation items (coordinates in source-image pixels, origin top-left).

Common optional fields on every item: color (hex, default theme red), stroke_width (1-20, default 3),
line_style (solid|dashed|dotted), corner_radius, font_size (default 16), opacity, rotation (degrees).

rect          x, y, width, height            outline rectangle; corner_radius rounds it
filled_rect   x, y, width, height            rectangle with a translucent fill of the same color
oval          x, y, width, height            ellipse inside the box
line          x1, y1, x2, y2
arrow         x1, y1, x2, y2                 style: straight|curved_right|curved_left; kind: classic|tapered|outlined;
                                             head_start / head_end: none|arrow|circle (default none / arrow)
text          x, y, text                     plain text with a legibility outline
label         x, y, text                     text on a filled box (corner_radius applies)
callout       x, y, text, tail_x, tail_y     label with a pointer tail to (tail_x, tail_y)
highlight     x, y, width, height            translucent bar across a line of text (or points: [[x,y],...] freehand)
blur          x, y, width, height            effect: pixelate|gaussian|hexagonal|crystallize|pointillism|halftone|tape|washi; strength 1-20
spotlight     x, y, width, height            everything outside all spotlights is dimmed; dim 0.1-0.9
counter       x, y                           numbered circle; number (auto-increments), size 1-12
watermark     text, [x, y, width, height]    style: single|diagonal|tiled; opacity default 0.35; whole image when no box
pencil        points: [[x,y],...]            freehand stroke

Document-level options: crop {x,y,width,height}; background ('wallpaper' = the user's blurred Omarchy wallpaper, gradient preset name, hex color, 'blurred', 'none');
padding (px, default 48 when a background is set); corner_radius (image corners); shadow 0-1; open_in_editor.

Example:
{"path":"shot.png","background":"blue purple","items":[
  {"type":"rect","x":40,"y":40,"width":300,"height":120,"color":"#ea6962"},
  {"type":"arrow","x1":400,"y1":300,"x2":330,"y2":150,"kind":"tapered","color":"#a9b665"},
  {"type":"callout","x":420,"y":310,"text":"Look here","tail_x":360,"tail_y":200,"font_size":24},
  {"type":"blur","x":500,"y":50,"width":250,"height":100,"effect":"pixelate","strength":8},
  {"type":"counter","x":100,"y":250},{"type":"counter","x":150,"y":250},
  {"type":"highlight","x":40,"y":200,"width":200,"height":24},
  {"type":"spotlight","x":250,"y":200,"width":120,"height":80,"dim":0.4},
  {"type":"watermark","text":"DRAFT","style":"tiled"}]}"##;

trait ColorDefault {
    fn unwrap_or_default_color(self) -> Color;
}
impl ColorDefault for Option<Color> {
    fn unwrap_or_default_color(self) -> Color {
        self.unwrap_or(Color::rgba(1.0, 0.23, 0.19, 1.0))
    }
}
