//! MCP server over stdio (newline-delimited JSON-RPC 2.0), so agents such as
//! Claude Code can pull images and run sandboxed commands as tools.

use crate::container::{self, ExecOpts, RunResult};
use crate::store::Store;
use crate::{registry, vm};
use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use std::io::{self, BufRead, Write};
use std::time::Duration;

const PROTOCOL_VERSION: &str = "2025-06-18";

fn container_props() -> Value {
    json!({
        "image": {"type": "string", "description": "Image reference, e.g. alpine:3.20, python:3.12-slim, ghcr.io/org/app:tag, or a locally loaded image"},
        "command": {"type": "array", "items": {"type": "string"}, "description": "argv, e.g. [\"sh\",\"-c\",\"echo hi\"]. Defaults to the image CMD."},
        "entrypoint": {"type": "array", "items": {"type": "string"}, "description": "Override the image ENTRYPOINT ([] clears it)."},
        "env": {"type": "array", "items": {"type": "string"}, "description": "KEY=VALUE entries"},
        "workdir": {"type": "string"},
        "volumes": {"type": "array", "items": {"type": "string"}, "description": "host_path:guest_path[:ro] or volume_name:guest_path (image needs /bin/sh)"},
        "ports": {"type": "array", "items": {"type": "string"}, "description": "host_port:guest_port. Other containers reach a published port at 127.0.0.1:<host_port>."},
        "cpus": {"type": "integer", "minimum": 1, "maximum": 16},
        "memory_mib": {"type": "integer", "minimum": 128},
        "name": {"type": "string", "description": "Container name"},
        "network": {"type": "boolean", "default": true, "description": "false = no network access at all"}
    })
}

fn tools() -> Value {
    let mut run_props = container_props();
    run_props["timeout_secs"] = json!({"type": "integer", "minimum": 1, "default": 300});
    run_props["keep"] = json!({"type": "boolean", "default": false, "description": "Keep the stopped container afterwards (default: removed, unless `name` is set)"});
    json!([
        {
            "name": "run",
            "description": "Run a one-off command in a fresh, isolated Linux microVM built from an OCI/Docker image \
(pulled automatically) and wait for it. Returns exit code and output. For long-running services or several \
commands in the same environment, use start_container + exec instead.",
            "inputSchema": {"type": "object", "properties": run_props, "required": ["image"]}
        },
        {
            "name": "start_container",
            "description": "Start a container in the background (like `docker run -d`), e.g. a database, a web service, or \
a long-lived dev box (use command [\"sleep\",\"infinity\"]). If `name` refers to an existing stopped container \
it is restarted with its filesystem intact. Returns the container name.",
            "inputSchema": {"type": "object", "properties": container_props(), "required": ["image"]}
        },
        {
            "name": "exec",
            "description": "Run a command inside a running container (like `docker exec`); stdout and stderr are returned \
separately. Needs /bin/sh in the image.",
            "inputSchema": {"type": "object", "properties": {
                "container": {"type": "string"},
                "command": {"type": "array", "items": {"type": "string"}},
                "env": {"type": "array", "items": {"type": "string"}},
                "workdir": {"type": "string"},
                "timeout_secs": {"type": "integer", "minimum": 1, "default": 300}
            }, "required": ["container", "command"]}
        },
        {
            "name": "logs",
            "description": "Output of a background container (like `docker logs`).",
            "inputSchema": {"type": "object", "properties": {
                "container": {"type": "string"},
                "tail": {"type": "integer", "minimum": 1, "description": "Only the last N lines"}
            }, "required": ["container"]}
        },
        {
            "name": "stop_container",
            "description": "Stop a running container (SIGTERM, then SIGKILL after `timeout_secs`).",
            "inputSchema": {"type": "object", "properties": {"container": {"type": "string"}, "timeout_secs": {"type": "integer", "default": 10}}, "required": ["container"]}
        },
        {
            "name": "list_containers",
            "description": "List containers with their status, image, ports and exit codes (like `docker ps -a`).",
            "inputSchema": {"type": "object", "properties": {}}
        },
        {
            "name": "remove_container",
            "description": "Delete a container and its filesystem (force stops it first if running).",
            "inputSchema": {"type": "object", "properties": {"container": {"type": "string"}}, "required": ["container"]}
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
            "name": "remove_image",
            "description": "Delete a local image.",
            "inputSchema": {"type": "object", "properties": {"image": {"type": "string"}}, "required": ["image"]}
        }
    ])
}

fn str_arg<'a>(args: &'a Value, k: &str) -> Result<&'a str> {
    args.get(k).and_then(|v| v.as_str()).ok_or_else(|| anyhow!("missing string argument `{k}`"))
}

fn strings(args: &Value, k: &str) -> Vec<String> {
    args.get(k)
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|s| s.as_str().map(String::from)).collect())
        .unwrap_or_default()
}

fn report(r: &RunResult, timeout: u64) -> (String, bool) {
    let mut text = format!(
        "container: {}\nexit_code: {}\nduration_ms: {}{}\n--- stdout ---\n{}",
        r.container,
        r.exit_code.map(|c| c.to_string()).unwrap_or_else(|| "none".into()),
        r.duration_ms,
        if r.timed_out { format!("\ntimed out after {timeout}s") } else { String::new() },
        r.stdout
    );
    if !r.stderr.trim().is_empty() {
        text.push_str(&format!("\n--- stderr ---\n{}", r.stderr));
    }
    (text, r.timed_out || r.exit_code != Some(0))
}

fn call_tool(store: &Store, name: &str, args: &Value) -> Result<(String, bool)> {
    match name {
        "run" => {
            let mut opts: vm::RunOpts = serde_json::from_value(args.clone())?;
            let keep = args.get("keep").and_then(|v| v.as_bool()).unwrap_or(false);
            opts.rm = !keep && opts.name.is_none();
            let timeout = args.get("timeout_secs").and_then(|v| v.as_u64()).unwrap_or(300);
            let r = container::run_captured(store, &opts, Duration::from_secs(timeout))?;
            Ok(report(&r, timeout))
        }
        "start_container" => {
            let opts: vm::RunOpts = serde_json::from_value(args.clone())?;
            let existing = opts.name.as_deref().and_then(|n| container::find(store, n).ok());
            let c = match existing {
                Some(c) if c.state(store).running() => return Ok((format!("{} is already running", c.name), false)),
                Some(c) => c,
                None => container::create(store, &opts)?,
            };
            container::start_detached(store, &c)?;
            Ok((format!("started {} ({})", c.name, c.short_id()), false))
        }
        "exec" => {
            let c = container::find(store, str_arg(args, "container")?)?;
            let timeout = args.get("timeout_secs").and_then(|v| v.as_u64()).unwrap_or(300);
            let o = ExecOpts {
                command: strings(args, "command"),
                env: strings(args, "env"),
                workdir: args.get("workdir").and_then(|v| v.as_str()).map(String::from),
                timeout: Some(Duration::from_secs(timeout)),
            };
            Ok(report(&container::exec_captured(store, &c, &o)?, timeout))
        }
        "logs" => {
            let c = container::find(store, str_arg(args, "container")?)?;
            let tail = args.get("tail").and_then(|v| v.as_u64()).map(|n| n as usize);
            let mut buf = vec![];
            container::logs(store, &c, false, tail, &mut buf)?;
            let mut text = String::from_utf8_lossy(&buf).into_owned();
            if text.len() > 200_000 {
                text = format!("[... truncated ...]\n{}", &text[text.len() - 200_000..]);
            }
            Ok((text, false))
        }
        "stop_container" => {
            let c = container::find(store, str_arg(args, "container")?)?;
            let t = args.get("timeout_secs").and_then(|v| v.as_u64()).unwrap_or(10);
            container::stop(store, &c, Duration::from_secs(t))?;
            let st = c.state(store);
            Ok((format!("stopped {} (exit code {})", c.name, st.exit_code.map(|x| x.to_string()).unwrap_or("?".into())), false))
        }
        "list_containers" => {
            let list: Vec<Value> = container::list(store)?
                .into_iter()
                .map(|c| {
                    let st = c.state(store);
                    json!({"name": c.name, "id": c.short_id(), "image": c.image, "status": st.status,
                           "exit_code": st.exit_code, "ports": c.config.ports, "command": c.config.command})
                })
                .collect();
            Ok((serde_json::to_string_pretty(&list)?, false))
        }
        "remove_container" => {
            let c = container::find(store, str_arg(args, "container")?)?;
            container::remove(store, &c, true)?;
            Ok((format!("removed {}", c.name), false))
        }
        "pull_image" => {
            let m = registry::pull(store, str_arg(args, "image")?, true)?;
            Ok((serde_json::to_string_pretty(&m)?, false))
        }
        "list_images" => Ok((serde_json::to_string_pretty(&store.images()?)?, false)),
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
