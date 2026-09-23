//! Web 管理端服务与嵌入式界面。
//!
//! 提供与 GUI 视觉与功能一致的网页管理界面，支持：
//! - ZCode 账号备份、切换、更新、修改别名与手机号、备份路径查看；
//! - CLI 工具账号管理（Gemini / agy、Codex、Claude）备份、切换、清空、修改备注；
//! - 全部账号类型的导入导出（移植文件 / 原生格式，CLI 工具支持剪贴板粘贴）；
//! - Gemini / agy 官方 OAuth 登录（获取授权链接、换取并保存 Token）；
//! - 缓存清理（安全清理与彻底清理，带路径预览与自动备份）；
//! - 置顶会话自动发送测试与执行；
//! - ZCode 桌面客户端进程启动、强行结束与重启。

use crate::identity;
use crate::{
    accounts,
    auto_send::{self, AutoSendRequest, SendSteps},
    claude, clean, cli_accounts, codebuddy, codex, gemini, launch_zcode, terminate_zcode,
    transfer, zcode_running, CleanOptions, Roots, CANDIDATES, SAFE_TAGS,
};
use serde_json::{json, Value};
use std::{
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    sync::Arc,
    thread,
};

pub const DEFAULT_WEB_PORT: u16 = 14596;

/// 启动嵌入式 Web 管理端服务（阻塞当前线程）。
pub fn run_server(port: u16) -> Result<(), String> {
    let roots = Arc::new(Roots::detect()?);
    let listener = TcpListener::bind(("0.0.0.0", port))
        .map_err(|e| format!("无法绑定 Web 端口 {port}: {e}"))?;
    println!("[Web] ZCode 账号管理 Web 端已启动: http://127.0.0.1:{port}");
    println!("[Web] 局域网访问: http://<本机IP>:{port}");
    println!("[Web] 按 Ctrl+C 可停止服务。");

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let roots = Arc::clone(&roots);
                thread::spawn(move || {
                    if let Err(e) = handle_connection(stream, &roots) {
                        // 网络异常（如客户端中途断开）无需打断主服务
                        let _ = e;
                    }
                });
            }
            Err(e) => {
                eprintln!("[Web] 接收连接异常: {e}");
            }
        }
    }
    Ok(())
}

/// 在后台线程启动 Web 服务（随 GUI 一同运行）。
#[allow(dead_code)]
pub fn start_background(port: u16) {
    thread::spawn(move || {
        if let Err(e) = run_server(port) {
            eprintln!("[Web] 后台 Web 服务启动失败: {e}");
        }
    });
}

fn handle_connection(mut stream: TcpStream, roots: &Roots) -> Result<(), String> {
    let mut reader = BufReader::new(stream.try_clone().map_err(|e| e.to_string())?);
    let mut request_line = String::new();
    reader
        .read_line(&mut request_line)
        .map_err(|e| e.to_string())?;
    let parts: Vec<&str> = request_line.trim().split_whitespace().collect();
    if parts.len() < 2 {
        return Ok(());
    }
    let method = parts[0];
    let uri = parts[1];

    // 读取请求头
    let mut content_length = 0usize;
    loop {
        let mut line = String::new();
        let bytes = reader.read_line(&mut line).map_err(|e| e.to_string())?;
        if bytes == 0 || line == "\r\n" || line == "\n" {
            break;
        }
        let line_lower = line.to_ascii_lowercase();
        if line_lower.starts_with("content-length:") {
            if let Some(val) = line.split(':').nth(1) {
                content_length = val.trim().parse().unwrap_or(0);
            }
        }
    }

    // 读取请求体
    let mut body = vec![0u8; content_length];
    if content_length > 0 {
        reader.read_exact(&mut body).map_err(|e| e.to_string())?;
    }

    if method == "OPTIONS" {
        return send_response(&mut stream, "204 No Content", "text/plain", b"", true);
    }

    // 路由分发
    let path = uri.split('?').next().unwrap_or(uri);
    match (method, path) {
        ("GET", "/") | ("GET", "/index.html") => send_response(
            &mut stream,
            "200 OK",
            "text/html; charset=utf-8",
            HTML_PAGE.as_bytes(),
            false,
        ),
        ("GET", "/favicon.ico") => {
            send_response(&mut stream, "204 No Content", "image/x-icon", b"", false)
        }
        ("GET", "/api/status") => {
            let status = get_system_status(roots);
            send_json(&mut stream, &status)
        }
        ("POST", "/api/zcode/process") => {
            let req: Value = serde_json::from_slice(&body).unwrap_or_default();
            let action = req.get("action").and_then(Value::as_str).unwrap_or("");
            let res = match action {
                "start" => launch_zcode().map(|_| "ZCode 已启动"),
                "terminate" => terminate_zcode().map(|_| "ZCode 进程已结束"),
                "restart" => {
                    let _ = terminate_zcode();
                    thread::sleep(std::time::Duration::from_millis(500));
                    launch_zcode().map(|_| "ZCode 已重启")
                }
                _ => Err("未知操作".to_string()),
            };
            match res {
                Ok(msg) => send_json(&mut stream, &json!({ "success": true, "message": msg })),
                Err(err) => send_json(&mut stream, &json!({ "success": false, "error": err })),
            }
        }
        ("GET", "/api/accounts") => {
            let profiles = accounts::list_accounts(roots).unwrap_or_default();
            let active_id = accounts::active_account(roots);
            let candidates_info: Vec<Value> = CANDIDATES
                .iter()
                .map(|c| {
                    let p = roots.resolve(*c);
                    json!({
                        "tag": c.tag,
                        "relative": c.relative,
                        "path": p.to_string_lossy(),
                        "exists": p.exists(),
                    })
                })
                .collect();
            send_json(
                &mut stream,
                &json!({
                    "success": true,
                    "active_id": active_id,
                    "accounts": profiles,
                    "candidates": candidates_info,
                }),
            )
        }
        ("POST", "/api/accounts/save") => {
            let req: Value = serde_json::from_slice(&body).unwrap_or_default();
            let name = req.get("name").and_then(Value::as_str);
            match accounts::save_current_account(roots, name, None) {
                Ok(profile) => {
                    send_json(&mut stream, &json!({ "success": true, "profile": profile }))
                }
                Err(err) => send_json(&mut stream, &json!({ "success": false, "error": err })),
            }
        }
        ("POST", "/api/accounts/switch") => {
            let req: Value = serde_json::from_slice(&body).unwrap_or_default();
            let id = req.get("id").and_then(Value::as_str).unwrap_or("");
            let all = accounts::list_accounts(roots).unwrap_or_default();
            if let Some(target) = all.iter().find(|p| p.manifest.id == id) {
                let was_running = zcode_running();
                if was_running {
                    if let Err(e) = terminate_zcode() {
                        return send_json(
                            &mut stream,
                            &json!({ "success": false, "error": format!("关闭 ZCode 失败: {e}") }),
                        );
                    }
                }
                match accounts::switch_account(roots, target) {
                    Ok(()) => {
                        let mut restarted = false;
                        if was_running {
                            if let Ok(()) = launch_zcode() {
                                restarted = true;
                            }
                        }
                        send_json(
                            &mut stream,
                            &json!({ "success": true, "restarted": restarted }),
                        )
                    }
                    Err(err) => send_json(&mut stream, &json!({ "success": false, "error": err })),
                }
            } else {
                send_json(
                    &mut stream,
                    &json!({ "success": false, "error": "未找到指定账号" }),
                )
            }
        }
        ("POST", "/api/accounts/update") => {
            let req: Value = serde_json::from_slice(&body).unwrap_or_default();
            let id = req.get("id").and_then(Value::as_str).unwrap_or("");
            let all = accounts::list_accounts(roots).unwrap_or_default();
            if let Some(target) = all.iter().find(|p| p.manifest.id == id) {
                match accounts::save_current_account(
                    roots,
                    Some(&target.manifest.name),
                    Some(target),
                ) {
                    Ok(profile) => {
                        send_json(&mut stream, &json!({ "success": true, "profile": profile }))
                    }
                    Err(err) => send_json(&mut stream, &json!({ "success": false, "error": err })),
                }
            } else {
                send_json(
                    &mut stream,
                    &json!({ "success": false, "error": "未找到指定账号" }),
                )
            }
        }
        ("POST", "/api/accounts/delete") => {
            let req: Value = serde_json::from_slice(&body).unwrap_or_default();
            let id = req.get("id").and_then(Value::as_str).unwrap_or("");
            let all = accounts::list_accounts(roots).unwrap_or_default();
            if let Some(target) = all.iter().find(|p| p.manifest.id == id) {
                match accounts::delete_account(roots, target) {
                    Ok(()) => send_json(&mut stream, &json!({ "success": true })),
                    Err(err) => send_json(&mut stream, &json!({ "success": false, "error": err })),
                }
            } else {
                send_json(
                    &mut stream,
                    &json!({ "success": false, "error": "未找到指定账号" }),
                )
            }
        }
        ("POST", "/api/accounts/alias") => {
            let req: Value = serde_json::from_slice(&body).unwrap_or_default();
            let id = req.get("id").and_then(Value::as_str).unwrap_or("");
            let alias = req.get("alias").and_then(Value::as_str);
            let all = accounts::list_accounts(roots).unwrap_or_default();
            if let Some(target) = all.iter().find(|p| p.manifest.id == id) {
                match accounts::set_alias(roots, target, alias) {
                    Ok(profile) => {
                        send_json(&mut stream, &json!({ "success": true, "profile": profile }))
                    }
                    Err(err) => send_json(&mut stream, &json!({ "success": false, "error": err })),
                }
            } else {
                send_json(
                    &mut stream,
                    &json!({ "success": false, "error": "未找到指定账号" }),
                )
            }
        }
        ("POST", "/api/accounts/phone") => {
            let req: Value = serde_json::from_slice(&body).unwrap_or_default();
            let id = req.get("id").and_then(Value::as_str).unwrap_or("");
            let phone = req.get("phone").and_then(Value::as_str);
            let all = accounts::list_accounts(roots).unwrap_or_default();
            if let Some(target) = all.iter().find(|p| p.manifest.id == id) {
                match accounts::set_phone(roots, target, phone) {
                    Ok(profile) => {
                        send_json(&mut stream, &json!({ "success": true, "profile": profile }))
                    }
                    Err(err) => send_json(&mut stream, &json!({ "success": false, "error": err })),
                }
            } else {
                send_json(
                    &mut stream,
                    &json!({ "success": false, "error": "未找到指定账号" }),
                )
            }
        }
        ("GET", p) if p.starts_with("/api/tools/") && p.ends_with("/accounts") => {
            let key = p
                .strip_prefix("/api/tools/")
                .unwrap()
                .strip_suffix("/accounts")
                .unwrap();
            let Some(store) = get_store_by_key(key) else {
                return send_json(
                    &mut stream,
                    &json!({ "success": false, "error": "未知工具" }),
                );
            };
            let profiles = store.list_accounts(roots).unwrap_or_default();
            let active_id = store.active_account(roots);
            let identity = store.identity(roots);
            let paths_info: Vec<Value> = store
                .paths
                .iter()
                .map(|tp| {
                    let path = tp
                        .relative
                        .split('/')
                        .fold(store.base(roots), |p, part| p.join(part));
                    json!({
                        "tag": tp.tag,
                        "relative": tp.relative,
                        "path": path.to_string_lossy(),
                        "exists": path.exists(),
                    })
                })
                .collect();
            send_json(
                &mut stream,
                &json!({
                    "success": true,
                    "key": store.key,
                    "display": store.display,
                    "tab": store.tab,
                    "cli_running": store.cli_running(),
                    "active_id": active_id,
                    "identity": {
                        "email": identity.email,
                        "auth_type": identity.auth_type,
                        "fingerprint": identity.fingerprint,
                        "describe": identity.describe(),
                    },
                    "accounts": profiles,
                    "paths": paths_info,
                }),
            )
        }
        ("POST", p) if p.starts_with("/api/tools/") => {
            let sub = p.strip_prefix("/api/tools/").unwrap();
            let parts: Vec<&str> = sub.split('/').collect();
            if parts.len() != 2 {
                return send_json(
                    &mut stream,
                    &json!({ "success": false, "error": "无效路径" }),
                );
            }
            let (key, action) = (parts[0], parts[1]);
            let Some(store) = get_store_by_key(key) else {
                return send_json(
                    &mut stream,
                    &json!({ "success": false, "error": "未知工具" }),
                );
            };
            let req: Value = serde_json::from_slice(&body).unwrap_or_default();
            match action {
                "save" => {
                    let name = req.get("name").and_then(Value::as_str);
                    match store.save_current_account(roots, name, None) {
                        Ok(profile) => {
                            send_json(&mut stream, &json!({ "success": true, "profile": profile }))
                        }
                        Err(err) => {
                            send_json(&mut stream, &json!({ "success": false, "error": err }))
                        }
                    }
                }
                "switch" => {
                    let id = req.get("id").and_then(Value::as_str).unwrap_or("");
                    let all = store.list_accounts(roots).unwrap_or_default();
                    if let Some(target) = all.iter().find(|p| p.manifest.id == id) {
                        match store.switch_account(roots, target) {
                            Ok(()) => send_json(&mut stream, &json!({ "success": true })),
                            Err(err) => {
                                send_json(&mut stream, &json!({ "success": false, "error": err }))
                            }
                        }
                    } else {
                        send_json(
                            &mut stream,
                            &json!({ "success": false, "error": "未找到指定账号" }),
                        )
                    }
                }
                "update" => {
                    let id = req.get("id").and_then(Value::as_str).unwrap_or("");
                    let all = store.list_accounts(roots).unwrap_or_default();
                    if let Some(target) = all.iter().find(|p| p.manifest.id == id) {
                        match store.save_current_account(
                            roots,
                            Some(&target.manifest.name),
                            Some(target),
                        ) {
                            Ok(profile) => send_json(
                                &mut stream,
                                &json!({ "success": true, "profile": profile }),
                            ),
                            Err(err) => {
                                send_json(&mut stream, &json!({ "success": false, "error": err }))
                            }
                        }
                    } else {
                        send_json(
                            &mut stream,
                            &json!({ "success": false, "error": "未找到指定账号" }),
                        )
                    }
                }
                "delete" => {
                    let id = req.get("id").and_then(Value::as_str).unwrap_or("");
                    let all = store.list_accounts(roots).unwrap_or_default();
                    if let Some(target) = all.iter().find(|p| p.manifest.id == id) {
                        match store.delete_account(roots, target) {
                            Ok(()) => send_json(&mut stream, &json!({ "success": true })),
                            Err(err) => {
                                send_json(&mut stream, &json!({ "success": false, "error": err }))
                            }
                        }
                    } else {
                        send_json(
                            &mut stream,
                            &json!({ "success": false, "error": "未找到指定账号" }),
                        )
                    }
                }
                "clear" => match store.clear_account(roots) {
                    Ok(backup_name) => send_json(
                        &mut stream,
                        &json!({ "success": true, "backup_name": backup_name }),
                    ),
                    Err(err) => send_json(&mut stream, &json!({ "success": false, "error": err })),
                },
                "export" => {
                    // ids 为空数组或缺省 = 全部账号
                    let ids: Vec<String> = req
                        .get("ids")
                        .and_then(Value::as_array)
                        .map(|ids| {
                            ids.iter()
                                .filter_map(Value::as_str)
                                .map(str::to_string)
                                .collect()
                        })
                        .unwrap_or_default();
                    match transfer::export_tool_accounts(store, roots, &ids) {
                        Ok(json) => {
                            let count = serde_json::from_str::<Value>(&json)
                                .ok()
                                .and_then(|value| {
                                    value
                                        .get("accounts")
                                        .and_then(Value::as_array)
                                        .map(|accounts| accounts.len())
                                })
                                .unwrap_or(0);
                            send_json(
                                &mut stream,
                                &json!({ "success": true, "json": json, "count": count }),
                            )
                        }
                        Err(err) => {
                            send_json(&mut stream, &json!({ "success": false, "error": err }))
                        }
                    }
                }
                "import" => {
                    let text = req.get("json").and_then(Value::as_str).unwrap_or("");
                    match transfer::import_tool_text(store, roots, text) {
                        Ok(result) => send_json(
                            &mut stream,
                            &json!({
                                "success": true,
                                "imported": result.imported,
                                "skipped": result.skipped,
                            }),
                        ),
                        Err(err) => {
                            send_json(&mut stream, &json!({ "success": false, "error": err }))
                        }
                    }
                }
                "alias" => {
                    let id = req.get("id").and_then(Value::as_str).unwrap_or("");
                    let alias = req.get("alias").and_then(Value::as_str);
                    let all = store.list_accounts(roots).unwrap_or_default();
                    if let Some(target) = all.iter().find(|p| p.manifest.id == id) {
                        match accounts::set_alias(roots, target, alias) {
                            Ok(profile) => send_json(
                                &mut stream,
                                &json!({ "success": true, "profile": profile }),
                            ),
                            Err(err) => {
                                send_json(&mut stream, &json!({ "success": false, "error": err }))
                            }
                        }
                    } else {
                        send_json(
                            &mut stream,
                            &json!({ "success": false, "error": "未找到指定账号" }),
                        )
                    }
                }
                _ => send_json(
                    &mut stream,
                    &json!({ "success": false, "error": "未知工具操作" }),
                ),
            }
        }
        ("GET", "/api/gemini/auth-url") => {
            let url = gemini::build_agy_auth_url();
            send_json(&mut stream, &json!({ "success": true, "url": url }))
        }
        ("POST", "/api/gemini/exchange") => {
            let req: Value = serde_json::from_slice(&body).unwrap_or_default();
            let raw_code = req.get("code").and_then(Value::as_str).unwrap_or("");
            // 提取授权码（支持直接粘贴整个重定向 URL）
            let code = extract_auth_code(raw_code);
            match gemini::exchange_and_save_token(roots, &code) {
                Ok(email) => send_json(&mut stream, &json!({ "success": true, "email": email })),
                Err(err) => send_json(&mut stream, &json!({ "success": false, "error": err })),
            }
        }
        ("POST", "/api/accounts/export") => {
            let req: Value = serde_json::from_slice(&body).unwrap_or_default();
            let ids: Vec<String> = req
                .get("ids")
                .and_then(Value::as_array)
                .map(|ids| {
                    ids.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            match transfer::export_zcode_accounts(roots, &ids) {
                Ok(json) => {
                    let count = serde_json::from_str::<Value>(&json)
                        .ok()
                        .and_then(|value| {
                            value
                                .get("accounts")
                                .and_then(Value::as_array)
                                .map(|accounts| accounts.len())
                        })
                        .unwrap_or(0);
                    send_json(
                        &mut stream,
                        &json!({ "success": true, "json": json, "count": count }),
                    )
                }
                Err(err) => send_json(&mut stream, &json!({ "success": false, "error": err })),
            }
        }
        ("POST", "/api/accounts/import") => {
            let req: Value = serde_json::from_slice(&body).unwrap_or_default();
            let text = req.get("json").and_then(Value::as_str).unwrap_or("");
            match transfer::import_zcode_text(roots, text) {
                Ok(result) => send_json(
                    &mut stream,
                    &json!({
                        "success": true,
                        "imported": result.imported,
                        "skipped": result.skipped,
                    }),
                ),
                Err(err) => send_json(&mut stream, &json!({ "success": false, "error": err })),
            }
        }
        ("POST", "/api/codebuddy/import") => {
            let req: Value = serde_json::from_slice(&body).unwrap_or_default();
            let text = req.get("json").and_then(Value::as_str).unwrap_or("");
            match transfer::import_tool_text(&codebuddy::STORE, roots, text) {
                Ok(result) => send_json(
                    &mut stream,
                    &json!({
                        "success": true,
                        "imported": result.imported,
                        "skipped": result.skipped,
                    }),
                ),
                Err(error) => send_json(&mut stream, &json!({ "success": false, "error": error })),
            }
        }
        ("GET", "/api/clean/candidates") => {
            let list: Vec<Value> = CANDIDATES
                .iter()
                .map(|c| {
                    let p = roots.resolve(*c);
                    json!({
                        "tag": c.tag,
                        "relative": c.relative,
                        "path": p.to_string_lossy(),
                        "exists": p.exists(),
                        "is_safe": SAFE_TAGS.contains(&c.tag),
                    })
                })
                .collect();
            send_json(&mut stream, &json!({ "success": true, "candidates": list }))
        }
        ("POST", "/api/clean") => {
            let req: Value = serde_json::from_slice(&body).unwrap_or_default();
            let safe = req.get("safe").and_then(Value::as_bool).unwrap_or(true);
            let no_backup = req
                .get("no_backup")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let options = CleanOptions {
                safe,
                no_backup,
                backup_dir: None,
            };
            match clean(roots, options) {
                Ok(()) => send_json(&mut stream, &json!({ "success": true })),
                Err(err) => send_json(&mut stream, &json!({ "success": false, "error": err })),
            }
        }
        ("POST", "/api/send") => {
            let req: Value = serde_json::from_slice(&body).unwrap_or_default();
            let pinned = req.get("pinned").and_then(Value::as_u64).unwrap_or(1) as usize;
            let message = req
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let dry_run = req.get("dry_run").and_then(Value::as_bool).unwrap_or(false);
            let request = AutoSendRequest {
                pinned_index: pinned,
                message,
                steps: if dry_run {
                    SendSteps::locate_only()
                } else {
                    SendSteps::full()
                },
            };
            let mut logs = Vec::new();
            let mut progress = |line: &str| {
                logs.push(line.to_string());
            };
            match auto_send::run(&request, &mut progress) {
                Ok(()) => send_json(
                    &mut stream,
                    &json!({ "success": true, "logs": logs.join("\n") }),
                ),
                Err(err) => send_json(
                    &mut stream,
                    &json!({ "success": false, "error": err, "logs": logs.join("\n") }),
                ),
            }
        }
        _ => send_response(
            &mut stream,
            "404 Not Found",
            "text/plain",
            b"Not Found",
            false,
        ),
    }
}

fn get_store_by_key(key: &str) -> Option<&'static cli_accounts::ToolStore> {
    match key {
        "gemini" => Some(&gemini::STORE),
        "codex" => Some(&codex::STORE),
        "claude" => Some(&claude::STORE),
        "codebuddy" => Some(&codebuddy::STORE),
        _ => None,
    }
}

fn get_system_status(roots: &Roots) -> Value {
    let running = zcode_running();
    let zcode_id = identity::detect(roots);
    let active_zcode = accounts::active_account(roots);

    let tools = [
        &gemini::STORE,
        &codex::STORE,
        &claude::STORE,
        &codebuddy::STORE,
    ]
    .iter()
    .map(|store| {
        let id = store.identity(roots);
        json!({
            "key": store.key,
            "display": store.display,
            "tab": store.tab,
            "active_id": store.active_account(roots),
            "cli_running": store.cli_running(),
            "describe": id.describe(),
        })
    })
    .collect::<Vec<_>>();

    json!({
        "success": true,
        "zcode_running": running,
        "active_account": active_zcode,
        "identity": {
            "username": zcode_id.username,
            "user_id": zcode_id.user_id,
            "provider": zcode_id.provider,
            "fingerprint": zcode_id.fingerprint,
            "describe": zcode_id.describe(),
        },
        "tools": tools,
    })
}

fn extract_auth_code(input: &str) -> String {
    let input = input.trim();
    if input.contains("code=") {
        if let Some(pos) = input.find("code=") {
            let rest = &input[pos + 5..];
            let code_part = rest.split('&').next().unwrap_or(rest);
            // 简单 urldecode
            let mut decoded = String::new();
            let mut chars = code_part.chars();
            while let Some(ch) = chars.next() {
                if ch == '%' {
                    let hex: String = chars.by_ref().take(2).collect();
                    if let Ok(byte) = u8::from_str_radix(&hex, 16) {
                        decoded.push(byte as char);
                    }
                } else if ch == '+' {
                    decoded.push(' ');
                } else {
                    decoded.push(ch);
                }
            }
            return decoded;
        }
    }
    input.to_string()
}

fn send_json(stream: &mut TcpStream, data: &Value) -> Result<(), String> {
    let bytes = serde_json::to_vec(data).map_err(|e| e.to_string())?;
    send_response(
        stream,
        "200 OK",
        "application/json; charset=utf-8",
        &bytes,
        true,
    )
}

fn send_response(
    stream: &mut TcpStream,
    status: &str,
    content_type: &str,
    body: &[u8],
    cors: bool,
) -> Result<(), String> {
    let cors_headers = if cors {
        "Access-Control-Allow-Origin: *\r\nAccess-Control-Allow-Methods: GET, POST, OPTIONS\r\nAccess-Control-Allow-Headers: Content-Type\r\n"
    } else {
        ""
    };
    let header = format!(
        "HTTP/1.1 {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n{}\r\n",
        status,
        content_type,
        body.len(),
        cors_headers
    );
    stream
        .write_all(header.as_bytes())
        .map_err(|e| e.to_string())?;
    stream.write_all(body).map_err(|e| e.to_string())?;
    stream.flush().map_err(|e| e.to_string())?;
    Ok(())
}

/// 嵌入式完整网页应用单文件 (HTML + CSS + JS)
const HTML_PAGE: &str = r###"<!DOCTYPE html>
<html lang="zh-CN">
<head>
  <meta charset="UTF-8">
  <meta name="viewport" content="width=device-width, initial-scale=1.0">
  <title>ZCode 账号管理</title>
  <style>
    :root {
      --bg-primary: #181825;
      --bg-secondary: #1e1e2e;
      --bg-card: #262739;
      --bg-hover: #313244;
      --border-color: #3b3c54;
      --text-main: #cdd6f4;
      --text-dim: #a6adc8;
      --accent: #89b4fa;
      --accent-hover: #b4befe;
      --green: #a6e3a1;
      --red: #f38ba8;
      --yellow: #f9e2af;
      --blue: #74c7ec;
      --radius: 8px;
    }
    * { box-sizing: border-box; margin: 0; padding: 0; }
    body {
      background-color: var(--bg-primary);
      color: var(--text-main);
      font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, "PingFang SC", "Hiragino Sans GB", "Microsoft YaHei", sans-serif;
      font-size: 14px;
      line-height: 1.5;
    }
    header {
      background-color: var(--bg-secondary);
      border-bottom: 1px solid var(--border-color);
      padding: 12px 24px;
      display: flex;
      align-items: center;
      justify-content: space-between;
      position: sticky;
      top: 0;
      z-index: 100;
    }
    .header-left { display: flex; align-items: center; gap: 16px; }
    .logo { font-size: 18px; font-weight: 700; color: #fff; display: flex; align-items: center; gap: 8px; }
    .logo svg { width: 22px; height: 22px; fill: var(--accent); }
    .status-badge {
      display: inline-flex;
      align-items: center;
      gap: 6px;
      padding: 4px 10px;
      border-radius: 20px;
      font-size: 12px;
      font-weight: 500;
    }
    .status-running { background: rgba(166, 227, 161, 0.15); color: var(--green); border: 1px solid rgba(166, 227, 161, 0.3); }
    .status-stopped { background: rgba(166, 173, 200, 0.15); color: var(--text-dim); border: 1px solid rgba(166, 173, 200, 0.3); }
    .dot { width: 8px; height: 8px; border-radius: 50%; display: inline-block; }
    .dot-green { background-color: var(--green); }
    .dot-gray { background-color: var(--text-dim); }

    .header-right { display: flex; align-items: center; gap: 12px; }
    button {
      cursor: pointer;
      border: 1px solid var(--border-color);
      background: var(--bg-card);
      color: var(--text-main);
      padding: 6px 14px;
      border-radius: var(--radius);
      font-size: 13px;
      transition: all 0.15s ease;
      display: inline-flex;
      align-items: center;
      gap: 6px;
    }
    button:hover { background: var(--bg-hover); border-color: var(--accent); color: #fff; }
    button.primary { background: #3b5bdb; border-color: #4c6ef5; color: #fff; }
    button.primary:hover { background: #4c6ef5; }
    button.danger { background: rgba(243, 139, 168, 0.1); border-color: rgba(243, 139, 168, 0.3); color: var(--red); }
    button.danger:hover { background: rgba(243, 139, 168, 0.25); border-color: var(--red); }
    button.success { background: rgba(166, 227, 161, 0.15); border-color: rgba(166, 227, 161, 0.4); color: var(--green); }
    button.success:hover { background: rgba(166, 227, 161, 0.3); }

    input, textarea, select {
      background: var(--bg-primary);
      border: 1px solid var(--border-color);
      color: var(--text-main);
      padding: 7px 12px;
      border-radius: var(--radius);
      font-size: 13px;
      outline: none;
    }
    input:focus, textarea:focus { border-color: var(--accent); }

    /* Nav Tabs */
    nav.main-tabs {
      background-color: var(--bg-secondary);
      border-bottom: 1px solid var(--border-color);
      display: flex;
      padding: 0 24px;
      gap: 8px;
    }
    .tab-btn {
      padding: 12px 18px;
      background: none;
      border: none;
      border-bottom: 2px solid transparent;
      border-radius: 0;
      color: var(--text-dim);
      font-weight: 500;
      font-size: 14px;
    }
    .tab-btn:hover { color: var(--text-main); background: none; border-color: transparent; }
    .tab-btn.active { color: var(--accent); border-bottom: 2px solid var(--accent); }

    .container {
      max-width: 1100px;
      margin: 24px auto;
      padding: 0 20px;
    }

    /* Sub Tabs (for CLI) */
    .sub-tabs {
      display: flex;
      gap: 8px;
      margin-bottom: 20px;
      border-bottom: 1px solid var(--border-color);
      padding-bottom: 10px;
    }
    .sub-tab-btn {
      background: var(--bg-card);
      padding: 6px 16px;
      border-radius: 20px;
      font-size: 13px;
    }
    .sub-tab-btn.active {
      background: var(--accent);
      color: #181825;
      font-weight: 600;
      border-color: var(--accent);
    }

    .card {
      background: var(--bg-card);
      border: 1px solid var(--border-color);
      border-radius: var(--radius);
      padding: 20px;
      margin-bottom: 20px;
    }
    .card-title {
      font-size: 16px;
      font-weight: 600;
      margin-bottom: 12px;
      display: flex;
      justify-content: space-between;
      align-items: center;
    }

    .info-row {
      display: flex;
      align-items: center;
      gap: 12px;
      margin-bottom: 8px;
      color: var(--text-dim);
    }
    .info-row strong { color: var(--text-main); }

    .collapsible {
      margin-top: 14px;
      border-top: 1px solid var(--border-color);
      padding-top: 12px;
    }
    .collapsible-header {
      cursor: pointer;
      color: var(--accent);
      user-select: none;
      display: flex;
      align-items: center;
      gap: 6px;
      font-size: 13px;
    }
    .collapsible-content {
      margin-top: 10px;
      display: none;
      background: var(--bg-primary);
      padding: 12px;
      border-radius: var(--radius);
      border: 1px solid var(--border-color);
    }
    .collapsible-content.open { display: block; }
    .path-table {
      width: 100%;
      border-collapse: collapse;
      font-size: 12px;
      font-family: monospace;
    }
    .path-table th, .path-table td {
      padding: 6px 8px;
      border-bottom: 1px solid var(--border-color);
      text-align: left;
    }
    .path-table th { color: var(--text-dim); font-weight: normal; }
    .badge-exist { color: var(--green); }
    .badge-missing { color: var(--text-dim); opacity: 0.6; }

    /* Account List Grid */
    .account-grid {
      display: grid;
      grid-template-columns: repeat(auto-fill, minmax(320px, 1fr));
      gap: 16px;
      margin-top: 16px;
    }
    .account-card {
      background: var(--bg-secondary);
      border: 1px solid var(--border-color);
      border-radius: var(--radius);
      padding: 16px;
      display: flex;
      flex-direction: column;
      justify-content: space-between;
      position: relative;
    }
    .account-card.active-card {
      border-color: var(--green);
      box-shadow: 0 0 10px rgba(166, 227, 161, 0.1);
    }
    .account-header {
      display: flex;
      justify-content: space-between;
      align-items: flex-start;
      margin-bottom: 10px;
    }
    .account-name {
      font-size: 15px;
      font-weight: 600;
      color: #fff;
    }
    .account-alias-badge {
      background: rgba(137, 180, 250, 0.15);
      color: var(--accent);
      padding: 2px 8px;
      border-radius: 12px;
      font-size: 12px;
      margin-left: 6px;
    }
    .tag-active {
      background: rgba(166, 227, 161, 0.2);
      color: var(--green);
      padding: 2px 8px;
      border-radius: 12px;
      font-size: 11px;
      font-weight: bold;
    }
    .account-meta {
      font-size: 12px;
      color: var(--text-dim);
      margin-bottom: 12px;
      line-height: 1.6;
    }
    .account-actions {
      display: flex;
      gap: 6px;
      flex-wrap: wrap;
      margin-top: auto;
      padding-top: 10px;
      border-top: 1px solid var(--border-color);
    }
    .account-actions button { padding: 4px 8px; font-size: 12px; }

    /* Modal dialogs */
    .modal-overlay {
      position: fixed;
      top: 0; left: 0; right: 0; bottom: 0;
      background: rgba(0, 0, 0, 0.7);
      display: none;
      align-items: center;
      justify-content: center;
      z-index: 1000;
    }
    .modal-overlay.open { display: flex; }
    .modal {
      background: var(--bg-card);
      border: 1px solid var(--border-color);
      border-radius: var(--radius);
      width: 520px;
      max-width: 90vw;
      padding: 24px;
      box-shadow: 0 10px 30px rgba(0, 0, 0, 0.5);
    }
    .modal-title { font-size: 18px; font-weight: 600; margin-bottom: 16px; }
    .modal-body { margin-bottom: 20px; }
    .modal-footer { display: flex; justify-content: flex-end; gap: 10px; }
    .form-group { margin-bottom: 14px; }
    .form-group label { display: block; font-size: 13px; color: var(--text-dim); margin-bottom: 6px; }
    .form-group input, .form-group textarea { width: 100%; }

    /* Alert Banner */
    .alert-banner {
      background: rgba(249, 226, 175, 0.15);
      border: 1px solid rgba(249, 226, 175, 0.3);
      color: var(--yellow);
      padding: 10px 16px;
      border-radius: var(--radius);
      margin-bottom: 16px;
      display: flex;
      align-items: center;
      gap: 10px;
    }

    /* Terminal Console */
    .console-box {
      background: #11111b;
      border: 1px solid var(--border-color);
      border-radius: var(--radius);
      padding: 12px;
      font-family: monospace;
      font-size: 12px;
      color: #a6adc8;
      max-height: 250px;
      overflow-y: auto;
      white-space: pre-wrap;
    }
  </style>
</head>
<body>

  <header>
    <div class="header-left">
      <div class="logo">
        <svg viewBox="0 0 24 24"><path d="M12 2L2 7l10 5 10-5-10-5zM2 17l10 5 10-5M2 12l10 5 10-5"/></svg>
        ZCode 账号管理
      </div>
      <div id="zcodeStatusBadge" class="status-badge status-stopped">
        <span class="dot dot-gray"></span>
        <span id="zcodeStatusText">检查中...</span>
      </div>
      <div id="zcodeProcessBtns" style="display:flex; gap:6px;">
        <button id="btnRestartZcode" onclick="controlZcode('restart')">重启 ZCode</button>
        <button id="btnStopZcode" class="danger" onclick="controlZcode('terminate')">结束</button>
      </div>
    </div>
    <div class="header-right">
      <span id="headerActiveAccount" style="color:var(--text-dim); font-size:13px;"></span>
      <button onclick="refreshAll()" title="刷新全部数据">
        <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><path d="M23 4v6h-6M1 20v-6h6M3.51 9a9 9 0 0114.85-3.36L23 10M1 14l4.64 4.36A9 9 0 0020.49 15"/></svg>
        刷新
      </button>
    </div>
  </header>

  <nav class="main-tabs">
    <button class="tab-btn active" onclick="switchPage('accounts')">ZCode 账号</button>
    <button class="tab-btn" onclick="switchPage('cli')">CLI 账号</button>
    <button class="tab-btn" onclick="switchPage('clean')">清理缓存</button>
    <button class="tab-btn" onclick="switchPage('autosend')">自动发送</button>
  </nav>

  <div class="container">
    <!-- PAGE 1: ZCODE ACCOUNTS -->
    <div id="page-accounts">
      <div class="card">
        <div class="card-title">
          <span>当前 ZCode 登录状态</span>
          <span id="zcodeIdentityBadge" style="font-size:13px; font-weight:normal; color:var(--text-dim);"></span>
        </div>
        <div id="zcodeIdentityContent">
          <div class="info-row"><span>加载中...</span></div>
        </div>

        <div class="collapsible">
          <div class="collapsible-header" onclick="toggleCollapsible('zcodePathsContent')">
            <span id="zcodePathsArrow">▶</span> 备份路径配置（候选配置文件列表）
          </div>
          <div id="zcodePathsContent" class="collapsible-content">
            <table class="path-table">
              <thead><tr><th>项目标签</th><th>相对路径</th><th>系统绝对路径</th><th>状态</th></tr></thead>
              <tbody id="zcodePathsTbody"></tbody>
            </table>
          </div>
        </div>

        <div style="margin-top: 16px; display:flex; gap:10px; align-items:center; flex-wrap:wrap;">
          <input type="text" id="newAccountName" placeholder="备份名称（留空自动使用当前账号识别名）" style="flex:1; min-width:180px;">
          <button class="primary" onclick="saveCurrentAccount()">备份当前账号</button>
          <button class="success" onclick="openTransferImport('zcode')" title="从本工具导出的移植文件导入 ZCode 账号；快照包含大量文件，仅支持文件导入">导入账号</button>
          <button onclick="exportAccounts('zcode', null)" title="把全部 ZCode 账号备份导出为单个 JSON 移植文件">导出全部</button>
        </div>
      </div>

      <div class="card">
        <div class="card-title">
          <span>已保存的 ZCode 账号备份</span>
          <span id="accountCountBadge" style="font-size:12px; color:var(--text-dim);"></span>
        </div>
        <div id="accountsList" class="account-grid"></div>
      </div>
    </div>

    <!-- PAGE 2: CLI ACCOUNTS -->
    <div id="page-cli" style="display:none;">
      <div class="sub-tabs">
        <button class="sub-tab-btn active" onclick="switchCliTab('gemini')">Gemini / Antigravity (agy)</button>
        <button class="sub-tab-btn" onclick="switchCliTab('codex')">Codex</button>
        <button class="sub-tab-btn" onclick="switchCliTab('claude')">Claude Code</button>
        <button class="sub-tab-btn" onclick="switchCliTab('codebuddy')">CodeBuddy CLI</button>
      </div>

      <div id="cliRunningWarning" class="alert-banner" style="display:none;">
        <svg width="18" height="18" viewBox="0 0 24 24" fill="currentColor"><path d="M1 21h22L12 2 1 21zm12-3h-2v-2h2v2zm0-4h-2v-4h2v4z"/></svg>
        <span id="cliRunningWarningText">检测到运行中的 CLI 会话，建议切换账号前先退出终端会话，避免凭据被覆盖。</span>
      </div>

      <div class="card">
        <div class="card-title">
          <span id="cliToolTitle">当前工具状态</span>
          <span id="cliIdentityBadge" style="font-size:13px; font-weight:normal; color:var(--text-dim);"></span>
        </div>
        <div id="cliIdentityContent">
          <div class="info-row"><span>加载中...</span></div>
        </div>

        <div class="collapsible">
          <div class="collapsible-header" onclick="toggleCollapsible('cliPathsContent')">
            <span id="cliPathsArrow">▶</span> 备份路径配置（该工具配置文件列表）
          </div>
          <div id="cliPathsContent" class="collapsible-content">
            <table class="path-table">
              <thead><tr><th>标签</th><th>相对路径</th><th>系统绝对路径</th><th>状态</th></tr></thead>
              <tbody id="cliPathsTbody"></tbody>
            </table>
          </div>
        </div>

        <div style="margin-top: 16px; display:flex; gap:10px; align-items:center; flex-wrap: wrap;">
          <div id="geminiLoginBtnContainer" style="display:none;">
            <button class="primary" style="background:#2563eb;" onclick="openGeminiLoginModal()">
              <svg width="14" height="14" viewBox="0 0 24 24" fill="currentColor"><path d="M12 2C6.48 2 2 6.48 2 12s4.48 10 10 10 10-4.48 10-10S17.52 2 12 2zm-1 14H9V8h2v8zm4 0h-2V8h2v8z"/></svg>
              登录 Gemini / agy 账号
            </button>
          </div>
          <button class="success" onclick="openTransferImport('cli')" title="支持本工具导出的移植文件与各工具原生凭据 JSON；可粘贴或选择文件">
            导入账号
          </button>
          <button onclick="exportAccounts('cli', null)" title="把当前工具全部账号备份导出为单个 JSON 移植文件">导出全部</button>
          <input type="text" id="newCliAccountName" placeholder="备份名称（留空自动命名）" style="flex:1; min-width:200px;">
          <button class="primary" onclick="saveCurrentCliAccount()">备份当前账号</button>
          <button class="danger" onclick="clearCliAccount()">恢复未登录状态</button>
        </div>
      </div>

      <div class="card">
        <div class="card-title">
          <span id="cliListTitle">已保存的备份列表</span>
          <span id="cliAccountCountBadge" style="font-size:12px; color:var(--text-dim);"></span>
        </div>
        <div id="cliAccountsList" class="account-grid"></div>
      </div>
    </div>

    <!-- PAGE 3: CLEANUP -->
    <div id="page-clean" style="display:none;">
      <div class="card">
        <div class="card-title">清理 ZCode 本地登录与权益缓存</div>
        <p style="color:var(--text-dim); margin-bottom:16px;">
          清理有助于排查登录失效、权益同步异常或会话缓存混乱问题。支持安全清理与彻底清理。
        </p>
        <div class="form-group">
          <label style="font-size:14px; color:var(--text-main); font-weight:600; margin-bottom:8px;">清理模式：</label>
          <div style="display:flex; flex-direction:column; gap:8px;">
            <label style="display:flex; align-items:center; gap:8px; cursor:pointer;">
              <input type="radio" name="cleanMode" value="safe" checked>
              <strong>安全清理（推荐）</strong> - 仅清除编码规划缓存、会话 Cookie 及埋点状态；保留登录凭据及用户配置。
            </label>
            <label style="display:flex; align-items:center; gap:8px; cursor:pointer;">
              <input type="radio" name="cleanMode" value="full">
              <strong>彻底清理</strong> - 清除全部凭据、Session、本地数据库与配置，彻底恢复到初始未登录状态。
            </label>
          </div>
        </div>
        <div class="form-group" style="margin-top:14px;">
          <label style="display:flex; align-items:center; gap:8px; cursor:pointer;">
            <input type="checkbox" id="cleanAutoBackup" checked>
            <span>清理前自动备份现有配置到 .zcode/reset_backups/ 目录</span>
          </label>
        </div>
        <div style="margin-top:16px;">
          <button class="danger" style="padding:8px 20px;" onclick="executeClean()">开始清理</button>
        </div>
      </div>

      <div class="card">
        <div class="card-title">待清理项目清单及状态</div>
        <table class="path-table">
          <thead><tr><th>标签</th><th>相对路径</th><th>安全清理覆盖</th><th>当前状态</th></tr></thead>
          <tbody id="cleanCandidatesTbody"></tbody>
        </table>
      </div>
    </div>

    <!-- PAGE 4: AUTO SEND -->
    <div id="page-autosend" style="display:none;">
      <div class="card">
        <div class="card-title">ZCode 置顶会话自动发送测试</div>
        <p style="color:var(--text-dim); margin-bottom:16px;">
          向正在运行的 ZCode 桌面客户端置顶会话窗口发送指定消息。
        </p>
        <div class="form-group">
          <label>置顶会话序号 (pinned index)：</label>
          <input type="number" id="autoSendPinned" value="1" min="1" style="width:120px;">
        </div>
        <div class="form-group">
          <label>消息内容：</label>
          <textarea id="autoSendMessage" rows="3" placeholder="输入要发送的消息内容..."></textarea>
        </div>
        <div style="display:flex; gap:10px;">
          <button onclick="executeAutoSend(true)">定位会话（测试）</button>
          <button class="primary" onclick="executeAutoSend(false)">立即发送</button>
        </div>
      </div>

      <div class="card">
        <div class="card-title">执行输出日志</div>
        <div id="autoSendConsole" class="console-box">就绪。</div>
      </div>
    </div>
  </div>

  <!-- MODAL: ZCode Account Edit (Alias & Phone) -->
  <div id="accountEditModal" class="modal-overlay">
    <div class="modal">
      <div class="modal-title">编辑 ZCode 账号信息</div>
      <div class="modal-body">
        <input type="hidden" id="editAccountId">
        <div class="form-group">
          <label>账号备注名（别名）：</label>
          <input type="text" id="editAccountAlias" placeholder="留空清除别名">
        </div>
        <div class="form-group">
          <label>关联手机号：</label>
          <input type="text" id="editAccountPhone" placeholder="留空清除手机号">
        </div>
      </div>
      <div class="modal-footer">
        <button onclick="closeAccountEditModal()">取消</button>
        <button class="primary" onclick="saveAccountEdit()">保存</button>
      </div>
    </div>
  </div>

  <!-- MODAL: CLI Account Alias Edit -->
  <div id="cliAliasModal" class="modal-overlay">
    <div class="modal">
      <div class="modal-title">修改 CLI 账号备注</div>
      <div class="modal-body">
        <input type="hidden" id="editCliId">
        <div class="form-group">
          <label>账号备注（别名）：</label>
          <input type="text" id="editCliAlias" placeholder="留空清除别名">
        </div>
      </div>
      <div class="modal-footer">
        <button onclick="closeCliAliasModal()">取消</button>
        <button class="primary" onclick="saveCliAlias()">保存</button>
      </div>
    </div>
  </div>

    <!-- MODAL: Gemini / agy OAuth Login -->
  <div id="geminiLoginModal" class="modal-overlay">
    <div class="modal" style="width:580px;">
      <div class="modal-title" style="display:flex; align-items:center; gap:8px;">
        <svg width="20" height="20" viewBox="0 0 24 24" fill="#89b4fa"><path d="M12 2C6.48 2 2 6.48 2 12s4.48 10 10 10 10-4.48 10-10S17.52 2 12 2zm-1 14H9V8h2v8zm4 0h-2V8h2v8z"/></svg>
        登录 Gemini / Antigravity 账号
      </div>
      <div class="modal-body">
        <p style="color:var(--text-dim); margin-bottom:12px; font-size:13px;">
          通过 Google OAuth 授权连接到官方 Antigravity / Gemini 服务并保存凭据到本地。
        </p>

        <div style="background:var(--bg-primary); padding:12px; border-radius:var(--radius); border:1px solid var(--border-color); margin-bottom:14px;">
          <div style="font-weight:600; margin-bottom:6px;">步骤 1：打开 Google 授权登录页面</div>
          <div style="font-size:12px; color:var(--text-dim); margin-bottom:8px;">
            点击下方按钮在浏览器中打开授权页，使用您的 Google 账号登录并同意授权：
          </div>
          <div style="display:flex; gap:8px;">
            <button class="primary" onclick="openAuthUrl()">在浏览器中打开授权页面</button>
            <button onclick="copyAuthUrl()">复制链接</button>
          </div>
        </div>

        <div style="background:var(--bg-primary); padding:12px; border-radius:var(--radius); border:1px solid var(--border-color);">
          <div style="font-weight:600; margin-bottom:6px;">步骤 2：粘贴授权码或回调链接</div>
          <div style="font-size:12px; color:var(--text-dim); margin-bottom:8px;">
            授权成功后，若跳转到 localhost 失败页，直接复制地址栏完整的 URL（包含 <code>code=...</code>）粘贴到下方：
          </div>
          <input type="text" id="geminiAuthCodeInput" placeholder="4/0A... 或 http://localhost:8085/oauth2callback?code=..." style="width:100%;">
        </div>

        <div id="geminiLoginError" style="color:var(--red); font-size:12px; margin-top:10px; display:none;"></div>
      </div>
      <div class="modal-footer">
        <button onclick="closeGeminiLoginModal()">取消</button>
        <button class="primary" id="btnSubmitGeminiLogin" onclick="submitGeminiLogin()">完成登录</button>
      </div>
    </div>

    <!-- MODAL: Transfer Import (通用账号导入) -->
    <div id="transferImportModal" class="modal-overlay">
      <div class="modal" style="max-width:680px;">
        <div class="modal-header">
          <h3 id="transferImportTitle">导入账号</h3>
          <button class="icon-btn" onclick="closeTransferImportModal()">×</button>
        </div>
        <p id="transferImportHint" style="color:var(--text-dim); margin:0 0 12px;"></p>
        <p style="color:var(--yellow); margin:0 0 12px; font-size:12px;">⚠ 导入内容包含登录凭据，请确认来源可信。</p>
        <div style="margin-bottom:12px;">
          <input type="file" id="transferImportFile" accept=".json,application/json" style="display:none;" onchange="onTransferImportFile(event)">
          <button onclick="document.getElementById('transferImportFile').click()">选择 JSON 文件…</button>
          <span id="transferImportFileName" style="color:var(--text-dim); font-size:12px; margin-left:8px;"></span>
        </div>
        <div id="transferImportPasteArea">
          <div style="display:flex; align-items:center; gap:8px; margin-bottom:8px;">
            <strong style="font-size:13px;">或粘贴 JSON</strong>
            <button onclick="pasteTransferImportFromClipboard()">从剪贴板粘贴</button>
          </div>
          <textarea id="transferImportInput" rows="10" placeholder='粘贴导出的移植文件 JSON 或凭据 JSON' style="width:100%; border-radius:8px; padding:10px; background:var(--bg-hover); color:var(--text-main); border:1px solid var(--border-color); font-family:monospace; font-size:12px;"></textarea>
        </div>
        <div style="display:flex; justify-content:flex-end; gap:8px; margin-top:14px;">
          <button onclick="closeTransferImportModal()">取消</button>
          <button class="primary" onclick="submitTransferImport()">导入</button>
        </div>
      </div>
    </div>

    <!-- MODAL: Transfer Export (通用账号导出) -->
    <div id="transferExportModal" class="modal-overlay">
      <div class="modal" style="max-width:680px;">
        <div class="modal-header">
          <h3 id="transferExportTitle">导出账号</h3>
          <button class="icon-btn" onclick="closeTransferExportModal()">×</button>
        </div>
        <p style="color:var(--yellow); margin:0 0 12px; font-size:12px;">⚠ 导出文件包含登录凭据，请像密码一样保管，不要上传或分享。</p>
        <textarea id="transferExportPreview" rows="10" readonly style="width:100%; border-radius:8px; padding:10px; background:var(--bg-hover); color:var(--text-dim); border:1px solid var(--border-color); font-family:monospace; font-size:12px;"></textarea>
        <div style="display:flex; justify-content:flex-end; gap:8px; margin-top:14px; flex-wrap:wrap;">
          <button id="transferExportCopyBtn" onclick="copyTransferExport()">复制到剪贴板</button>
          <button class="primary" onclick="downloadTransferExport()">下载 JSON 文件</button>
          <button onclick="closeTransferExportModal()">关闭</button>
        </div>
      </div>
    </div>
  </div>

  <script>
    let currentCliTab = 'gemini';
    let currentAuthUrl = '';

    async function apiGet(url) {
      const res = await fetch(url);
      return await res.json();
    }
    async function apiPost(url, data) {
      const res = await fetch(url, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify(data || {})
      });
      return await res.json();
    }

    function switchPage(page) {
      document.querySelectorAll('.tab-btn').forEach(btn => btn.classList.remove('active'));
      document.querySelectorAll('[id^="page-"]').forEach(p => p.style.display = 'none');
      const target = document.getElementById(`page-${page}`);
      if (target) target.style.display = 'block';
      event.target.classList.add('active');
      if (page === 'accounts') loadAccountsPage();
      if (page === 'cli') loadCliPage();
      if (page === 'clean') loadCleanPage();
    }

    function toggleCollapsible(id) {
      const el = document.getElementById(id);
      const isClosed = !el.classList.contains('open');
      el.classList.toggle('open', isClosed);
      const arrowId = id.replace('Content', 'Arrow');
      const arrow = document.getElementById(arrowId);
      if (arrow) arrow.innerText = isClosed ? '▼' : '▶';
    }

    async function refreshAll() {
      await loadStatus();
      const accountsVisible = document.getElementById('page-accounts').style.display !== 'none';
      const cliVisible = document.getElementById('page-cli').style.display !== 'none';
      const cleanVisible = document.getElementById('page-clean').style.display !== 'none';
      if (accountsVisible) await loadAccountsPage();
      if (cliVisible) await loadCliPage();
      if (cleanVisible) await loadCleanPage();
    }

    async function loadStatus() {
      const res = await apiGet('/api/status');
      if (!res.success) return;

      const badge = document.getElementById('zcodeStatusBadge');
      const text = document.getElementById('zcodeStatusText');
      const dot = badge.querySelector('.dot');
      if (res.zcode_running) {
        badge.className = 'status-badge status-running';
        text.innerText = 'ZCode 运行中';
        dot.className = 'dot dot-green';
        document.getElementById('btnStopZcode').style.display = 'inline-flex';
      } else {
        badge.className = 'status-badge status-stopped';
        text.innerText = 'ZCode 未运行';
        dot.className = 'dot dot-gray';
        document.getElementById('btnStopZcode').style.display = 'none';
      }

      const activeBadge = document.getElementById('headerActiveAccount');
      if (res.active_account) {
        activeBadge.innerText = `激活账号: ${res.active_account}`;
      } else {
        activeBadge.innerText = '';
      }

      // Update ZCode Identity in Card
      const idContent = document.getElementById('zcodeIdentityContent');
      const idBadge = document.getElementById('zcodeIdentityBadge');
      if (res.identity) {
        idBadge.innerText = res.identity.describe;
        let html = '';
        if (res.identity.username) html += `<div class="info-row"><span>账号/邮箱：</span><strong>${res.identity.username}</strong></div>`;
        if (res.identity.user_id) html += `<div class="info-row"><span>用户 ID：</span><strong>${res.identity.user_id}</strong></div>`;
        if (res.identity.provider) html += `<div class="info-row"><span>服务提供商：</span><strong>${res.identity.provider}</strong></div>`;
        if (res.identity.fingerprint) html += `<div class="info-row"><span>凭据指纹：</span><small>${res.identity.fingerprint}</small></div>`;
        if (!html) html = `<div class="info-row"><span>未检测到登录状态</span></div>`;
        idContent.innerHTML = html;
      }
    }

    async function controlZcode(action) {
      const btn = event.target;
      btn.disabled = true;
      try {
        const res = await apiPost('/api/zcode/process', { action });
        if (res.success) {
          alert(res.message || '操作成功');
        } else {
          alert('失败: ' + res.error);
        }
        await loadStatus();
      } finally {
        btn.disabled = false;
      }
    }

    // --- ZCODE ACCOUNTS ---
    async function loadAccountsPage() {
      const res = await apiGet('/api/accounts');
      if (!res.success) return;

      // Candidates
      const tbody = document.getElementById('zcodePathsTbody');
      tbody.innerHTML = res.candidates.map(c => `
        <tr>
          <td><strong>${c.tag}</strong></td>
          <td>${c.relative}</td>
          <td><small>${c.path}</small></td>
          <td>${c.exists ? '<span class="badge-exist">✓ 存在</span>' : '<span class="badge-missing">未创建</span>'}</td>
        </tr>
      `).join('');

      // Profiles
      const list = document.getElementById('accountsList');
      const badge = document.getElementById('accountCountBadge');
      badge.innerText = `共 ${res.accounts.length} 个备份`;

      if (res.accounts.length === 0) {
        list.innerHTML = '<div style="color:var(--text-dim); grid-column:1/-1; padding:20px 0;">暂无账号备份，点击上方“备份当前账号”保存。</div>';
        return;
      }

      list.innerHTML = res.accounts.map(p => {
        const isActive = res.active_id === p.manifest.id;
        const aliasTag = p.manifest.alias ? `<span class="account-alias-badge">${p.manifest.alias}</span>` : '';
        const phoneTag = p.manifest.phone ? `<div>手机：<strong>${p.manifest.phone}</strong></div>` : '';
        const activeTag = isActive ? '<span class="tag-active">当前激活</span>' : '';
        const updated = new Date(p.manifest.updated_at * 1000).toLocaleString();

        return `
          <div class="account-card ${isActive ? 'active-card' : ''}">
            <div>
              <div class="account-header">
                <div>
                  <span class="account-name">${p.manifest.name}</span>
                  ${aliasTag}
                </div>
                ${activeTag}
              </div>
              <div class="account-meta">
                ${phoneTag}
                <div>标识：${p.manifest.identity || '无标识信息'}</div>
                <div>文件数：${p.manifest.item_count} 项 · 更新于：${updated}</div>
              </div>
            </div>
            <div class="account-actions">
              <button class="primary" onclick="switchAccount('${p.manifest.id}')">切换</button>
              <button onclick="updateAccount('${p.manifest.id}')">更新备份</button>
              <button onclick="exportAccounts('zcode', '${p.manifest.id}')" title="导出为 JSON 移植文件">导出</button>
              <button onclick="openAccountEditModal('${p.manifest.id}', '${p.manifest.alias || ''}', '${p.manifest.phone || ''}')">编辑</button>
              <button class="danger" onclick="deleteAccount('${p.manifest.id}')">删除</button>
            </div>
          </div>
        `;
      }).join('');
    }

    async function saveCurrentAccount() {
      const input = document.getElementById('newAccountName');
      const name = input.value.trim() || undefined;
      const res = await apiPost('/api/accounts/save', { name });
      if (res.success) {
        input.value = '';
        await loadAccountsPage();
        await loadStatus();
      } else {
        alert('备份失败: ' + res.error);
      }
    }

    async function switchAccount(id) {
      if (!confirm('确认切换到该账号？当前状态将自动暂存。')) return;
      const res = await apiPost('/api/accounts/switch', { id });
      if (res.success) {
        if (res.restarted) {
          alert('账号切换成功！ZCode 客户端已自动重启。');
        } else {
          alert('账号切换成功！');
        }
        await loadAccountsPage();
        await loadStatus();
      } else {
        alert('切换失败: ' + res.error);
      }
    }

    async function updateAccount(id) {
      if (!confirm('确认用当前登录状态覆盖此备份？')) return;
      const res = await apiPost('/api/accounts/update', { id });
      if (res.success) {
        alert('更新备份成功！');
        await loadAccountsPage();
      } else {
        alert('更新失败: ' + res.error);
      }
    }

    async function deleteAccount(id) {
      if (!confirm('确认删除此备份？删除后无法找回。')) return;
      const res = await apiPost('/api/accounts/delete', { id });
      if (res.success) {
        await loadAccountsPage();
        await loadStatus();
      } else {
        alert('删除失败: ' + res.error);
      }
    }

    function openAccountEditModal(id, alias, phone) {
      document.getElementById('editAccountId').value = id;
      document.getElementById('editAccountAlias').value = alias;
      document.getElementById('editAccountPhone').value = phone;
      document.getElementById('accountEditModal').classList.add('open');
    }
    function closeAccountEditModal() {
      document.getElementById('accountEditModal').classList.remove('open');
    }
    async function saveAccountEdit() {
      const id = document.getElementById('editAccountId').value;
      const alias = document.getElementById('editAccountAlias').value.trim() || null;
      const phone = document.getElementById('editAccountPhone').value.trim() || null;
      await apiPost('/api/accounts/alias', { id, alias });
      await apiPost('/api/accounts/phone', { id, phone });
      closeAccountEditModal();
      await loadAccountsPage();
    }

    // --- CLI ACCOUNTS ---
    async function switchCliTab(tab) {
      currentCliTab = tab;
      document.querySelectorAll('.sub-tab-btn').forEach(b => b.classList.remove('active'));
      event.target.classList.add('active');
      await loadCliPage();
    }

    async function loadCliPage() {
      const res = await apiGet(`/api/tools/${currentCliTab}/accounts`);
      if (!res.success) return;

      document.getElementById('cliToolTitle').innerText = `${res.display} 登录状态`;
      document.getElementById('cliListTitle').innerText = `${res.tab} 备份列表`;

      // Warning banner
      const banner = document.getElementById('cliRunningWarning');
      banner.style.display = res.cli_running ? 'flex' : 'none';

      // Gemini login button container
      const geminiBtn = document.getElementById('geminiLoginBtnContainer');
      geminiBtn.style.display = (currentCliTab === 'gemini') ? 'block' : 'none';

      // Tool Identity
      const idBadge = document.getElementById('cliIdentityBadge');
      const idContent = document.getElementById('cliIdentityContent');
      idBadge.innerText = res.identity.describe;

      let html = '';
      if (res.identity.email) html += `<div class="info-row"><span>邮箱：</span><strong>${res.identity.email}</strong></div>`;
      if (res.identity.auth_type) html += `<div class="info-row"><span>认证方式：</span><strong>${res.identity.auth_type}</strong></div>`;
      if (res.identity.fingerprint) html += `<div class="info-row"><span>凭据指纹：</span><small>${res.identity.fingerprint}</small></div>`;
      if (!html) html = `<div class="info-row"><span>未检测到登录状态</span></div>`;
      idContent.innerHTML = html;

      // Paths
      const tbody = document.getElementById('cliPathsTbody');
      tbody.innerHTML = res.paths.map(p => `
        <tr>
          <td><strong>${p.tag}</strong></td>
          <td>${p.relative}</td>
          <td><small>${p.path}</small></td>
          <td>${p.exists ? '<span class="badge-exist">✓ 存在</span>' : '<span class="badge-missing">未创建</span>'}</td>
        </tr>
      `).join('');

      // Account profiles
      const list = document.getElementById('cliAccountsList');
      const countBadge = document.getElementById('cliAccountCountBadge');
      countBadge.innerText = `共 ${res.accounts.length} 个备份`;

      if (res.accounts.length === 0) {
        list.innerHTML = '<div style="color:var(--text-dim); grid-column:1/-1; padding:20px 0;">暂无账号备份。</div>';
        return;
      }

      list.innerHTML = res.accounts.map(p => {
        const isActive = res.active_id === p.manifest.id;
        const aliasTag = p.manifest.alias ? `<span class="account-alias-badge">${p.manifest.alias}</span>` : '';
        const activeTag = isActive ? '<span class="tag-active">当前激活</span>' : '';
        const updated = new Date(p.manifest.updated_at * 1000).toLocaleString();

        return `
          <div class="account-card ${isActive ? 'active-card' : ''}">
            <div>
              <div class="account-header">
                <div>
                  <span class="account-name">${p.manifest.name}</span>
                  ${aliasTag}
                </div>
                ${activeTag}
              </div>
              <div class="account-meta">
                <div>标识：${p.manifest.identity || '无标识信息'}</div>
                <div>更新于：${updated}</div>
              </div>
            </div>
            <div class="account-actions">
              <button class="primary" onclick="switchCliAccount('${p.manifest.id}')">切换</button>
              <button onclick="updateCliAccount('${p.manifest.id}')">更新备份</button>
              <button onclick="exportAccounts('cli', '${p.manifest.id}')" title="导出为 JSON 移植文件，可复制到剪贴板或下载">导出</button>
              <button onclick="openCliAliasModal('${p.manifest.id}', '${p.manifest.alias || ''}')">修改备注</button>
              <button class="danger" onclick="deleteCliAccount('${p.manifest.id}')">删除</button>
            </div>
          </div>
        `;
      }).join('');
    }

    async function switchCliAccount(id) {
      if (!confirm(`确认切换到该 ${currentCliTab.toUpperCase()} 账号？切换后正在运行的命令行会话请重启生效。`)) return;
      const res = await apiPost(`/api/tools/${currentCliTab}/switch`, { id });
      if (res.success) {
        alert(`切换成功！已激活该账号。若有正在运行的 ${currentCliTab.toUpperCase()} 会话，请重启后生效。`);
        await loadCliPage();
        await loadStatus();
      } else {
        alert('切换失败: ' + res.error);
      }
    }

    async function saveCurrentCliAccount() {
      const input = document.getElementById('newCliAccountName');
      const name = input.value.trim() || undefined;
      const res = await apiPost(`/api/tools/${currentCliTab}/save`, { name });
      if (res.success) {
        input.value = '';
        await loadCliPage();
      } else {
        alert('备份失败: ' + res.error);
      }
    }

    async function updateCliAccount(id) {
      if (!confirm('确认用当前登录状态覆盖此备份？')) return;
      const res = await apiPost(`/api/tools/${currentCliTab}/update`, { id });
      if (res.success) {
        await loadCliPage();
      } else {
        alert('更新失败: ' + res.error);
      }
    }

    async function deleteCliAccount(id) {
      if (!confirm('确认删除此备份？')) return;
      const res = await apiPost(`/api/tools/${currentCliTab}/delete`, { id });
      if (res.success) {
        await loadCliPage();
      } else {
        alert('删除失败: ' + res.error);
      }
    }

    async function clearCliAccount() {
      if (!confirm('确认清空登录凭据并恢复到未登录状态？当前登录状态将自动保存到备份列表中。')) return;
      const res = await apiPost(`/api/tools/${currentCliTab}/clear`, {});
      if (res.success) {
        alert(res.backup_name ? `已保存备份「${res.backup_name}」并恢复未登录状态` : '已恢复未登录状态');
        await loadCliPage();
      } else {
        alert('清空失败: ' + res.error);
      }
    }

    function openCliAliasModal(id, alias) {
      document.getElementById('editCliId').value = id;
      document.getElementById('editCliAlias').value = alias;
      document.getElementById('cliAliasModal').classList.add('open');
    }
    function closeCliAliasModal() {
      document.getElementById('cliAliasModal').classList.remove('open');
    }
    async function saveCliAlias() {
      const id = document.getElementById('editCliId').value;
      const alias = document.getElementById('editCliAlias').value.trim() || null;
      await apiPost(`/api/tools/${currentCliTab}/alias`, { id, alias });
      closeCliAliasModal();
      await loadCliPage();
    }

    // --- TRANSFER IMPORT / EXPORT (通用导入导出) ---
    // 'zcode' 或 'cli'（cli 使用当前 currentCliTab 工具）
    let transferImportTarget = 'zcode';
    let currentExportJson = '';
    let currentExportFileName = 'accounts.json';
    let currentExportAllowClipboard = false;

    const TRANSFER_IMPORT_HINTS = {
      zcode: '选择本工具导出的移植文件（zam-zcode-accounts.json）。ZCode 快照包含大量文件（会话、本地存储等），仅支持文件导入。',
      cli_gemini: '支持：本工具导出的移植文件、另一台机器的 oauth_creds.json / antigravity-oauth-token。导入只创建备份，不改变当前登录状态。',
      cli_codex: '支持：本工具导出的移植文件、~/.codex/auth.json（ChatGPT OAuth 或 API Key）。导入只创建备份，不改变当前登录状态。',
      cli_claude: '支持：本工具导出的移植文件、~/.claude/settings.json（端点 Token）、.credentials.json（OAuth 凭据）。导入只创建备份，不改变当前登录状态。',
      cli_codebuddy: '支持：本工具导出的移植文件、WorkBuddy / wb-switch JSON 数组、单个 ~/.codebuddy/settings.json。导入只创建备份，不改变当前登录状态。'
    };

    function openTransferImport(target) {
      transferImportTarget = target;
      const isZcode = target === 'zcode';
      const hintKey = isZcode ? 'zcode' : ('cli_' + currentCliTab);
      const toolName = isZcode ? 'ZCode' : currentCliTab.toUpperCase();
      document.getElementById('transferImportTitle').innerText = `导入 ${toolName} 账号`;
      document.getElementById('transferImportHint').innerText = TRANSFER_IMPORT_HINTS[hintKey] || '导入只创建备份，不改变当前登录状态。';
      // ZCode 快照文件较大，只走文件通道；CLI 工具同时支持剪贴板粘贴
      document.getElementById('transferImportPasteArea').style.display = isZcode ? 'none' : 'block';
      document.getElementById('transferImportInput').value = '';
      const fileInput = document.getElementById('transferImportFile');
      fileInput.value = '';
      document.getElementById('transferImportFileName').innerText = '';
      document.getElementById('transferImportModal').classList.add('open');
    }
    function closeTransferImportModal() {
      document.getElementById('transferImportModal').classList.remove('open');
    }
    function onTransferImportFile(event) {
      const file = event.target.files[0];
      if (!file) return;
      document.getElementById('transferImportFileName').innerText = `已选择：${file.name}`;
      const reader = new FileReader();
      reader.onload = () => {
        // ZCode 模式没有文本框，文件内容暂存到输入框（隐藏区域也能存值）
        document.getElementById('transferImportInput').value = reader.result;
      };
      reader.readAsText(file);
    }
    async function pasteTransferImportFromClipboard() {
      try {
        const text = await navigator.clipboard.readText();
        if (text && text.trim()) {
          document.getElementById('transferImportInput').value = text;
        } else {
          alert('剪贴板为空');
        }
      } catch (e) {
        alert('无法读取剪贴板（浏览器要求 HTTPS 或 localhost 访问），请直接在文本框中按 Ctrl+V 粘贴');
      }
    }
    async function submitTransferImport() {
      const text = document.getElementById('transferImportInput').value.trim();
      if (!text) { alert('请先选择 JSON 文件或粘贴内容'); return; }
      const url = transferImportTarget === 'zcode' ? '/api/accounts/import' : `/api/tools/${currentCliTab}/import`;
      const res = await apiPost(url, { json: text });
      if (res.success) {
        closeTransferImportModal();
        if (transferImportTarget === 'zcode') { await loadAccountsPage(); } else { await loadCliPage(); }
        alert(`导入完成：成功 ${res.imported} 个，跳过 ${res.skipped} 个`);
      } else {
        alert('导入失败: ' + res.error);
      }
    }

    async function exportAccounts(target, id) {
      const isZcode = target === 'zcode';
      const toolName = isZcode ? 'zcode' : currentCliTab;
      const url = isZcode ? '/api/accounts/export' : `/api/tools/${currentCliTab}/export`;
      const body = id ? { ids: [id] } : {};
      const res = await apiPost(url, body);
      if (!res.success) { alert('导出失败: ' + res.error); return; }
      currentExportJson = res.json;
      const date = new Date().toISOString().slice(0, 10).replace(/-/g, '');
      currentExportFileName = `zam-${toolName}-accounts-${date}.json`;
      // CLI 快照是少量小文件，可走剪贴板；ZCode 快照文件多体积大，只走文件
      currentExportAllowClipboard = !isZcode;
      const scope = id ? '单个账号' : `全部 ${res.count} 个账号`;
      document.getElementById('transferExportTitle').innerText = `导出 ${isZcode ? 'ZCode' : toolName.toUpperCase()} 账号（${scope}）`;
      const preview = res.json.length > 4000 ? res.json.slice(0, 4000) + '\n…（内容过长已截断，完整内容请下载文件）' : res.json;
      document.getElementById('transferExportPreview').value = preview;
      document.getElementById('transferExportCopyBtn').style.display = currentExportAllowClipboard ? '' : 'none';
      document.getElementById('transferExportModal').classList.add('open');
    }
    function closeTransferExportModal() {
      document.getElementById('transferExportModal').classList.remove('open');
    }
    async function copyTransferExport() {
      try {
        await navigator.clipboard.writeText(currentExportJson);
        alert('已复制到剪贴板，可在另一台电脑的导入窗口粘贴');
      } catch (e) {
        alert('复制失败（浏览器剪贴板权限），请使用「下载 JSON 文件」');
      }
    }
    function downloadTransferExport() {
      const blob = new Blob([currentExportJson], { type: 'application/json' });
      const a = document.createElement('a');
      a.href = URL.createObjectURL(blob);
      a.download = currentExportFileName;
      a.click();
      URL.revokeObjectURL(a.href);
    }
    async function openGeminiLoginModal() {
      const res = await apiGet('/api/gemini/auth-url');
      if (res.success) {
        currentAuthUrl = res.url;
      }
      document.getElementById('geminiAuthCodeInput').value = '';
      document.getElementById('geminiLoginError').style.display = 'none';
      document.getElementById('geminiLoginModal').classList.add('open');
    }
    function closeGeminiLoginModal() {
      document.getElementById('geminiLoginModal').classList.remove('open');
    }
    function openAuthUrl() {
      if (currentAuthUrl) {
        window.open(currentAuthUrl, '_blank');
      }
    }
    function copyAuthUrl() {
      if (currentAuthUrl) {
        navigator.clipboard.writeText(currentAuthUrl);
        alert('已复制授权链接到剪贴板');
      }
    }
    async function submitGeminiLogin() {
      const input = document.getElementById('geminiAuthCodeInput');
      const errEl = document.getElementById('geminiLoginError');
      const btn = document.getElementById('btnSubmitGeminiLogin');
      const code = input.value.trim();
      if (!code) {
        errEl.innerText = '请先粘贴授权码或回调 URL';
        errEl.style.display = 'block';
        return;
      }
      btn.disabled = true;
      btn.innerText = '正在换取 Token...';
      errEl.style.display = 'none';

      try {
        const res = await apiPost('/api/gemini/exchange', { code });
        if (res.success) {
          alert(`登录成功！账号: ${res.email}`);
          closeGeminiLoginModal();
          await loadCliPage();
        } else {
          errEl.innerText = res.error || '登录失败，请检查授权码是否正确';
          errEl.style.display = 'block';
        }
      } catch (err) {
        errEl.innerText = '请求失败: ' + err.message;
        errEl.style.display = 'block';
      } finally {
        btn.disabled = false;
        btn.innerText = '完成登录';
      }
    }

    // --- CLEANUP ---
    async function loadCleanPage() {
      const res = await apiGet('/api/clean/candidates');
      if (!res.success) return;
      const tbody = document.getElementById('cleanCandidatesTbody');
      tbody.innerHTML = res.candidates.map(c => `
        <tr>
          <td><strong>${c.tag}</strong></td>
          <td>${c.relative}</td>
          <td>${c.is_safe ? '<span style="color:var(--green)">✓ 包含在安全清理</span>' : '<span style="color:var(--yellow)">仅彻底清理</span>'}</td>
          <td>${c.exists ? '<span class="badge-exist">✓ 存在</span>' : '<span class="badge-missing">未创建</span>'}</td>
        </tr>
      `).join('');
    }

    async function executeClean() {
      const safe = document.querySelector('input[name="cleanMode"]:checked').value === 'safe';
      const no_backup = !document.getElementById('cleanAutoBackup').checked;
      const msg = safe ? '确认执行安全清理？' : '确认执行彻底清理？所有未备份的登录状态将被清除！';
      if (!confirm(msg)) return;

      const res = await apiPost('/api/clean', { safe, no_backup });
      if (res.success) {
        alert('清理完成！');
        await loadCleanPage();
        await loadStatus();
      } else {
        alert('清理失败: ' + res.error);
      }
    }

    // --- AUTO SEND ---
    async function executeAutoSend(dry_run) {
      const pinned = parseInt(document.getElementById('autoSendPinned').value, 10) || 1;
      const message = document.getElementById('autoSendMessage').value;
      const consoleEl = document.getElementById('autoSendConsole');

      if (!dry_run && !message.trim()) {
        alert('请输入要发送的消息内容');
        return;
      }

      consoleEl.innerText = `[${new Date().toLocaleTimeString()}] 开始执行自动发送 (${dry_run ? '测试定位' : '真实发送'})...\n`;
      const res = await apiPost('/api/send', { pinned, message, dry_run });
      if (res.logs) {
        consoleEl.innerText += res.logs + '\n';
      }
      if (res.success) {
        consoleEl.innerText += `[${new Date().toLocaleTimeString()}] 执行完成！`;
      } else {
        consoleEl.innerText += `[${new Date().toLocaleTimeString()}] 执行失败: ${res.error}\n`;
      }
    }

    // Init
    window.addEventListener('DOMContentLoaded', async () => {
      await loadStatus();
      await loadAccountsPage();
    });
  </script>
</body>
</html>
"###;
