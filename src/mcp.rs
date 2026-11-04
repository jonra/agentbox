//! MCP server over stdio (newline-delimited JSON-RPC 2.0), so agents such as
//! Claude Code can pull images and run sandboxed commands as tools.

use crate::store::Store;
use crate::{registry, vm};
use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use std::io::{self, BufRead, Write};
use std::time::Duration;

const PROTOCOL_VERSION: &str = "2025-06-18";

fn tools() -> Value {
    json!([
        {
            "name": "run",
            "description": "Run a command in a fresh, isolated Linux microVM built from an OCI/Docker image \
(pulled automatically). Returns exit code and combined stdout/stderr. Without `name` the filesystem is \
thrown away afterwards; with `name` it persists and later runs with the same name see earlier changes.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "image": {"type": "string", "description": "Image reference, e.g. alpine:3.20, python:3.12-slim, ghcr.io/org/app:tag"},
                    "command": {"type": "array", "items": {"type": "string"}, "description": "argv, e.g. [\"sh\",\"-c\",\"echo hi\"]. Defaults to the image CMD."},
                    "entrypoint": {"type": "array", "items": {"type": "string"}, "description": "Override the image ENTRYPOINT ([] clears it)."},
                    "env": {"type": "array", "items": {"type": "string"}, "description": "KEY=VALUE entries"},
                    "workdir": {"type": "string"},
                    "volumes": {"type": "array", "items": {"type": "string"}, "description": "host_path:guest_path[:ro] (image needs /bin/sh)"},
                    "ports": {"type": "array", "items": {"type": "string"}, "description": "host_port:guest_port"},
                    "cpus": {"type": "integer", "minimum": 1, "maximum": 16},
                    "memory_mib": {"type": "integer", "minimum": 128},
                    "name": {"type": "string", "description": "Persistent container name to create or reuse"},
                    "network": {"type": "boolean", "default": true, "description": "false = no network access at all"},
                    "timeout_secs": {"type": "integer", "minimum": 1, "default": 300}
                },
                "required": ["image"]
            }
        },
        {
            "name": "pull_image",
            "description": "Pull (or update) an OCI/Docker image for this machine's architecture.",
            "inputSchema": {"type": "object", "properties": {"image": {"type": "string"}}, "required": ["image"]}
        },
        {
            "name": "list_images",
            "description": "List locally available images.",
            "inputSchema": {"type": "object", "properties": {}}
        },
        {
            "name": "list_containers",
            "description": "List persistent (named) containers.",
            "inputSchema": {"type": "object", "properties": {}}
        },
        {
            "name": "remove_container",
            "description": "Delete a named container and its filesystem.",
            "inputSchema": {"type": "object", "properties": {"name": {"type": "string"}}, "required": ["name"]}
        },
        {
            "name": "remove_image",
            "description": "Delete a local image.",
            "inputSchema": {"type": "object", "properties": {"image": {"type": "string"}}, "required": ["image"]}
        }
    ])
}

fn str_arg<'a>(args: &'a Value, k: &str) -> Result<&'a str> {
    args.get(k).and_then(|v| v.as_str()).ok_or_else(|| anyhow!("missing string argument `{k}`"))
}

fn call_tool(store: &Store, name: &str, args: &Value) -> Result<(String, bool)> {
    match name {
        "run" => {
            let opts: vm::RunOpts = serde_json::from_value(args.clone())?;
            let timeout = args.get("timeout_secs").and_then(|v| v.as_u64()).unwrap_or(300);
            let r = vm::run_captured(store, &opts, Duration::from_secs(timeout))?;
            let mut text = format!(
                "exit_code: {}\nduration_ms: {}{}\n--- output ---\n{}",
                r.exit_code.map(|c| c.to_string()).unwrap_or_else(|| "none".into()),
                r.duration_ms,
                if r.timed_out { format!("\ntimed out after {timeout}s (VM killed)") } else { String::new() },
                r.output
            );
            if !r.stderr.trim().is_empty() {
                text.push_str(&format!("\n--- vmm stderr ---\n{}", r.stderr));
            }
            let failed = r.timed_out || r.exit_code != Some(0);
            Ok((text, failed))
        }
        "pull_image" => {
            let m = registry::pull(store, str_arg(args, "image")?, true)?;
            Ok((serde_json::to_string_pretty(&m)?, false))
        }
        "list_images" => Ok((serde_json::to_string_pretty(&store.images()?)?, false)),
        "list_containers" => Ok((serde_json::to_string_pretty(&store.containers()?)?, false)),
        "remove_container" => {
            store.remove_container(str_arg(args, "name")?)?;
            Ok(("removed".into(), false))
        }
        "remove_image" => {
            let r = registry::Reference::parse(str_arg(args, "image")?)?.canonical();
            store.remove_image(&r)?;
            Ok(("removed".into(), false))
        }
        other => Err(anyhow!("unknown tool {other}")),
    }
}

fn handle(store: &Store, req: &Value) -> Option<Value> {
    let id = req.get("id").cloned()?; // notifications get no response
    let method = req.get("method").and_then(|m| m.as_str()).unwrap_or_default();
    let params = req.get("params").cloned().unwrap_or(Value::Null);
    let result = match method {
        "initialize" => Ok(json!({
            "protocolVersion": params.get("protocolVersion").and_then(|v| v.as_str()).unwrap_or(PROTOCOL_VERSION),
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "agentbox", "version": env!("CARGO_PKG_VERSION")}
        })),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({"tools": tools()})),
        "tools/call" => {
            let name = params.get("name").and_then(|n| n.as_str()).unwrap_or_default();
            let args = params.get("arguments").cloned().unwrap_or(json!({}));
            Ok(match call_tool(store, name, &args) {
                Ok((text, is_error)) => json!({"content": [{"type": "text", "text": text}], "isError": is_error}),
                Err(e) => json!({"content": [{"type": "text", "text": format!("error: {e:#}")}], "isError": true}),
            })
        }
        _ => Err(json!({"code": -32601, "message": format!("method not found: {method}")})),
    };
    Some(match result {
        Ok(r) => json!({"jsonrpc": "2.0", "id": id, "result": r}),
        Err(e) => json!({"jsonrpc": "2.0", "id": id, "error": e}),
    })
}

pub fn serve(store: Store) -> Result<()> {
    let stdin = io::stdin();
    let mut stdout = io::stdout();
    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let resp = match serde_json::from_str::<Value>(&line) {
            Ok(req) => handle(&store, &req),
            Err(e) => Some(json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32700, "message": e.to_string()}})),
        };
        if let Some(r) = resp {
            writeln!(stdout, "{r}")?;
            stdout.flush()?;
        }
    }
    Ok(())
}
