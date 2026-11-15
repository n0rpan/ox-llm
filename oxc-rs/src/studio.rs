//! Loopback Studio. Image/model adapters are ported against the reference routes.
use axum::{
    extract::{Request, State},
    http::{HeaderMap, HeaderValue, Method, StatusCode},
    response::{IntoResponse, Response},
    Router,
};
use serde_json::{json, Value};
use std::{net::Ipv4Addr, path::PathBuf, sync::Arc};

#[path = "studio_files.rs"]
mod files;
#[path = "studio_jobs.rs"]
mod jobs;
#[path = "studio_metadata.rs"]
mod metadata;
#[path = "studio_plans.rs"]
mod plans;

struct Studio {
    port: u16,
    project: PathBuf,
    stack: PathBuf,
    output: PathBuf,
    sha_cache: std::sync::Mutex<Option<(PathBuf, u64, i128, String)>>,
    lease: PathBuf,
    jobs: std::sync::Mutex<jobs::Store>,
    admission: tokio::sync::Mutex<()>,
    job_tasks: std::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>,
    stopping: tokio::sync::watch::Sender<bool>,
}
fn single<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    let mut values = headers.get_all(name).iter();
    let value = values.next()?.to_str().ok()?;
    if values.next().is_none() {
        Some(value)
    } else {
        None
    }
}
fn reply(code: u16, body: Vec<u8>, ctype: &'static str) -> Response {
    let length = body.len();
    let mut response = (
        StatusCode::from_u16(code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
        [("content-type", ctype)],
        body,
    )
        .into_response();
    response
        .headers_mut()
        .insert("content-length", HeaderValue::from(length));
    response
}
fn data(code: u16, value: Value) -> Response {
    reply(
        code,
        crate::paths::python_json(&value).into_bytes(),
        "application/json",
    )
}
fn error(code: u16, message: &str) -> Response {
    data(code, json!({"error":message}))
}
fn post_ok(headers: &HeaderMap, port: u16) -> bool {
    for name in ["origin", "sec-fetch-site"] {
        if headers.get_all(name).iter().count() > 1 {
            return false;
        }
    }
    if headers.contains_key("sec-fetch-site")
        && !single(headers, "sec-fetch-site").is_some_and(|v| matches!(v, "same-origin" | "none"))
    {
        return false;
    }
    !headers.contains_key("origin")
        || single(headers, "origin").is_some_and(|v| {
            [
                format!("http://127.0.0.1:{port}"),
                format!("http://localhost:{port}"),
                format!("http://[::1]:{port}"),
            ]
            .iter()
            .any(|s| s == v)
        })
}
async fn handle(State(studio): State<Arc<Studio>>, request: Request) -> Response {
    if !single(request.headers(), "host").is_some_and(|host| {
        [
            format!("127.0.0.1:{}", studio.port),
            format!("localhost:{}", studio.port),
            format!("[::1]:{}", studio.port),
        ]
        .contains(&host.trim().to_lowercase())
    }) {
        return error(403, "Host not allowed");
    }
    let path = request.uri().path().to_owned();
    if request.method() == Method::GET {
        return match path.as_str() {
            "/" => reply(
                200,
                include_bytes!("studio.html").to_vec(),
                "text/html; charset=utf-8",
            ),
            "/activity" => data(200, studio.activity()),
            "/job" => studio.job(request.uri().query().unwrap_or_default()),
            "/file" => studio.file(request.uri().query().unwrap_or_default()),
            "/models" => match studio.models() {
                Ok(value) => data(200, value),
                Err(()) => files::silent_close(),
            },
            "/engine" => data(200, studio.engine_info()),
            "/estimate" => match studio.estimate(request.uri().query().unwrap_or_default()) {
                Ok(value) => data(200, value),
                Err(()) => error(400, "bad estimate request"),
            },
            _ => reply(404, b"not found".to_vec(), "application/json"),
        };
    }
    if request.method() != Method::POST {
        return error(501, "Unsupported method");
    }
    if !post_ok(request.headers(), studio.port) {
        return error(403, "Cross-origin request refused");
    }
    let raw = single(request.headers(), "content-length").unwrap_or("0");
    let Ok(length) = raw.trim().parse::<i128>() else {
        return error(400, "bad length");
    };
    if length < 0
        || length
            > if path == "/upload" {
                25 * 1024 * 1024
            } else {
                1024 * 1024
            }
    {
        return error(413, "request too large");
    }
    if path == "/upload" {
        let content_type = request
            .headers()
            .get("content-type")
            .map(|v| {
                v.as_bytes()
                    .iter()
                    .map(|b| char::from(*b))
                    .collect::<String>()
                    .into_bytes()
            })
            .unwrap_or_default();
        let Ok(body) = axum::body::to_bytes(request.into_body(), 25 * 1024 * 1024).await else {
            return error(400, "bad multipart");
        };
        return studio.upload(&content_type, &body);
    }
    if ["/generate", "/upscale", "/refine", "/edit", "/engine"].contains(&path.as_str()) {
        let Ok(bytes) = axum::body::to_bytes(request.into_body(), 1024 * 1024).await else {
            return error(400, "Invalid request");
        };
        let value = if bytes.is_empty() {
            Some(json!({}))
        } else {
            serde_json::from_slice::<Value>(&bytes).ok()
        };
        let Some(value) = value.filter(Value::is_object) else {
            return error(400, "Invalid request");
        };
        if path == "/engine" {
            if !value["engine"]
                .as_str()
                .is_some_and(|s| ["auto", "oxi", "sdcpp"].contains(&s))
            {
                return error(400, "Engine must be auto, oxi or sdcpp");
            }
            if studio
                .set_engine(value["engine"].as_str().expect("engine choice"))
                .is_err()
            {
                return error(500, "The image engine setting could not be saved.");
            }
            return data(200, studio.engine_info());
        }
        if path == "/generate" {
            return match studio.generation_input(&value) {
                Ok(input) => studio.start_generation(input, &value).await,
                Err(plans::PlanError::Reply(code, message)) => error(code, &message),
                Err(plans::PlanError::Close) => files::silent_close(),
            };
        }
        if path == "/edit" {
            return match studio.edit_input(&value) {
                Err(plans::PlanError::Reply(code, message)) => error(code, &message),
                Err(plans::PlanError::Close) => files::silent_close(),
                Ok(()) => error(400, "Upload the photo to edit first"),
            };
        }
        return error(
            400,
            "That image is no longer available here; generate it again",
        );
    }
    reply(404, b"not found".to_vec(), "application/json")
}
pub async fn run() -> i32 {
    let Ok(port) = std::env::var("STUDIO_PORT")
        .unwrap_or_else(|_| "8090".into())
        .parse::<u16>()
    else {
        return 1;
    };
    let home = crate::paths::home();
    let project = std::env::var_os("LOCAL_IMAGE_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join("Work/prompt-studio"));
    let output = std::env::var_os("IMG_OUTDIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join("Pictures/sd"));
    if std::fs::create_dir_all(output.join("_uploads")).is_err() {
        return 1;
    }
    let lease = std::env::var_os("LOCAL_AI_LEASE")
        .map(PathBuf::from)
        .unwrap_or_else(|| project.join("generation.lock"));
    if lease
        .parent()
        .is_some_and(|p| std::fs::create_dir_all(p).is_err())
    {
        return 1;
    }
    let listener = match tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, port)).await {
        Ok(v) => v,
        Err(_) => return 1,
    };
    let port = listener.local_addr().map(|a| a.port()).unwrap_or(port);
    let stack = std::env::var_os("SD_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".local/share/ai-stack"));
    let (stopping, _) = tokio::sync::watch::channel(false);
    let studio = Arc::new(Studio {
        port,
        project,
        stack,
        output,
        sha_cache: std::sync::Mutex::new(None),
        lease,
        jobs: std::sync::Mutex::new(jobs::Store::default()),
        admission: tokio::sync::Mutex::new(()),
        job_tasks: std::sync::Mutex::new(Vec::new()),
        stopping,
    });
    let router = Router::new().fallback(handle).with_state(studio.clone());
    let Ok(mut term) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
    else {
        return 1;
    };
    let mut connections = tokio::task::JoinSet::new();
    loop {
        tokio::select! {
            incoming=listener.accept()=>match incoming{Ok((stream,_))=>{connections.spawn(crate::transport::serve_legacy(stream,router.clone()));},Err(_)=>return 1},
            _=term.recv()=>break,_=tokio::signal::ctrl_c()=>break,Some(_)=connections.join_next()=>{},
        }
    }
    connections.abort_all();
    while connections.join_next().await.is_some() {}
    let _ = studio.stopping.send(true);
    let tasks = std::mem::take(&mut *studio.job_tasks.lock().expect("image tasks"));
    for task in tasks {
        let _ = task.await;
    }
    0
}
