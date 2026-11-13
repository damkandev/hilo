//! `hilo mcp`: claim / release / who over MCP (JSON-RPC on stdio) for agents without hooks.
//! Cooperative: an agent that ignores it is still protected by the anchor's ordering rule.
use crate::ipc;
use crate::json::Json;
use std::io::{BufRead, Write};
use std::time::Duration;

fn tool(name: &str, desc: &str) -> Json {
    Json::obj(vec![
        ("name", Json::s(name)),
        ("description", Json::s(desc)),
        (
            "inputSchema",
            Json::obj(vec![
                ("type", Json::s("object")),
                ("properties", Json::obj(vec![("path", Json::obj(vec![("type", Json::s("string")), ("description", Json::s("File path, absolute or relative to the working directory"))]))])),
                ("required", Json::Arr(vec![Json::s("path")])),
            ]),
        ),
    ])
}

fn abs(p: &str) -> String {
    let path = std::path::Path::new(p);
    if path.is_absolute() {
        p.to_string()
    } else {
        std::env::current_dir().map(|d| d.join(path).to_string_lossy().into_owned()).unwrap_or_else(|_| p.into())
    }
}

pub fn run() -> i32 {
    let session = format!("mcp-{}", std::process::id());
    let call = |a: &[&str]| ipc::call(&ipc::args(a), Duration::from_secs(5));
    let stdin = std::io::stdin();
    let mut out = std::io::stdout();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let Some(req) = Json::parse(&line) else { continue };
        let Some(id) = req.get("id").cloned() else { continue }; // notifications need no answer
        let method = req.get("method").and_then(Json::str).unwrap_or("");
        let result = match method {
            "initialize" => Ok(Json::obj(vec![
                ("protocolVersion", req.at(&["params", "protocolVersion"]).cloned().unwrap_or(Json::s("2024-11-05"))),
                ("capabilities", Json::obj(vec![("tools", Json::obj(vec![]))])),
                ("serverInfo", Json::obj(vec![("name", Json::s("hilo")), ("version", Json::s(env!("CARGO_PKG_VERSION")))])),
                ("instructions", Json::s("Before editing a file, call claim with its path. If another agent holds it, work on something else. Call release when done.")),
            ])),
            "ping" => Ok(Json::obj(vec![])),
            "tools/list" => Ok(Json::obj(vec![(
                "tools",
                Json::Arr(vec![
                    tool("claim", "Take a file before editing it, so agents on other machines know. Says who holds it if it is taken."),
                    tool("release", "Let go of a file you claimed."),
                    tool("who", "Who is editing this file right now, if anyone."),
                ]),
            )])),
            "tools/call" => {
                let name = req.at(&["params", "name"]).and_then(Json::str).unwrap_or("");
                let path = abs(req.at(&["params", "arguments", "path"]).and_then(Json::str).unwrap_or(""));
                let r = match name {
                    "claim" => call(&["claim", &session, &path]).map(|(c, t)| if c == 0 { (false, format!("claimed {path}")) } else { (false, t) }),
                    "release" => call(&["release", &session, &path]).map(|_| (false, format!("released {path}"))),
                    "who" => call(&["who", &path]).map(|(_, t)| (false, t)),
                    _ => Ok((true, format!("unknown tool {name}"))),
                };
                let (is_err, text) = r.unwrap_or_else(|_| (false, "hilo daemon is not running; go ahead".into()));
                Ok(Json::obj(vec![
                    ("content", Json::Arr(vec![Json::obj(vec![("type", Json::s("text")), ("text", Json::s(&text))])])),
                    ("isError", Json::Bool(is_err)),
                ]))
            }
            _ => Err(Json::obj(vec![("code", Json::n(-32601)), ("message", Json::s("method not found"))])),
        };
        let mut resp = vec![("jsonrpc", Json::s("2.0")), ("id", id)];
        match result {
            Ok(r) => resp.push(("result", r)),
            Err(e) => resp.push(("error", e)),
        }
        if writeln!(out, "{}", Json::obj(resp).to_string()).and_then(|_| out.flush()).is_err() {
            break;
        }
    }
    let _ = call(&["release", &session]);
    0
}
