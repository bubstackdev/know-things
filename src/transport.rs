//! Bounded local adapters over the same coordinator and typed response stream.
use crate::{
    contracts::*,
    coordinator::{Coordinator, Run},
    store::Store,
};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
    io::{BufRead, BufReader, Read, Write},
    os::{
        fd::AsRawFd,
        unix::{
            fs::PermissionsExt,
            net::{UnixListener, UnixStream},
        },
    },
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};
pub const MAX_FRAME: usize = 1024 * 1024;
const WRITE_TIMEOUT: Duration = Duration::from_millis(50);
pub fn read_frame(reader: &mut impl Read) -> Result<Vec<u8>> {
    let mut size = [0; 4];
    reader.read_exact(&mut size)?;
    let size = u32::from_be_bytes(size) as usize;
    if size == 0 || size > MAX_FRAME {
        return Err(Error::new("invalid_request", "IPC frame exceeds limits"));
    }
    let mut bytes = vec![0; size];
    reader.read_exact(&mut bytes)?;
    Ok(bytes)
}
pub fn write_frame(writer: &mut impl Write, bytes: &[u8]) -> Result<()> {
    if bytes.len() > MAX_FRAME {
        return Err(Error::new(
            "resource_exhausted",
            "IPC response exceeds frame limit",
        ));
    }
    writer.write_all(&(bytes.len() as u32).to_be_bytes())?;
    writer.write_all(bytes)?;
    writer.flush()?;
    Ok(())
}
fn write_frame_until(stream: &mut UnixStream, response: &Response) -> Result<()> {
    let bytes = serde_json::to_vec(response)?;
    if bytes.len() > MAX_FRAME {
        return Err(Error::new(
            "resource_exhausted",
            "IPC response exceeds frame limit",
        ));
    }
    let mut frame = Vec::with_capacity(bytes.len() + 4);
    frame.extend((bytes.len() as u32).to_be_bytes());
    frame.extend(bytes);
    crate::process::write_all_until(stream, &frame, Instant::now() + WRITE_TIMEOUT)
}
pub fn call_stream(
    directory: &Path,
    request: &Request,
    mut write: impl FnMut(Response) -> Result<()>,
) -> Result<()> {
    let mut stream = UnixStream::connect(directory.join("coordinator.sock")).map_err(|_| {
        Error::new(
            "source_unavailable",
            "Coordinator unavailable; start `know-things serve DATA_DIR`",
        )
    })?;
    stream.set_read_timeout(Some(Duration::from_secs(125)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    write_frame(&mut stream, &serde_json::to_vec(request)?)?;
    loop {
        let response: Response = serde_json::from_slice(&read_frame(&mut stream)?)?;
        let terminal = response.status != "streaming";
        write(response)?;
        if terminal {
            return Ok(());
        }
    }
}
pub fn call(directory: &Path, request: &Request) -> Result<Response> {
    let mut result = None;
    call_stream(directory, request, |r| {
        result = Some(r);
        Ok(())
    })?;
    Ok(result.unwrap())
}
pub fn serve(directory: &Path, store: Store) -> Result<()> {
    serve_coordinator(directory, Coordinator::new(store, None))
}
pub fn serve_coordinator(directory: &Path, coordinator: Coordinator) -> Result<()> {
    let socket = directory.join("coordinator.sock");
    match fs::symlink_metadata(&socket) {
        Ok(meta) => {
            use std::os::unix::fs::FileTypeExt;
            if !meta.file_type().is_socket() {
                return Err(Error::new(
                    "invalid_request",
                    "IPC path is occupied by a non-socket",
                ));
            }
            fs::remove_file(&socket)?;
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    let listener = UnixListener::bind(&socket)?;
    fs::set_permissions(&socket, fs::Permissions::from_mode(0o600))?;
    let active = Arc::new(AtomicUsize::new(0));
    for stream in listener.incoming() {
        let mut stream = stream?;
        stream.set_nonblocking(true)?;
        if active.fetch_add(1, Ordering::SeqCst) >= 8 {
            active.fetch_sub(1, Ordering::SeqCst);
            continue;
        }
        let coordinator = coordinator.clone();
        let active = active.clone();
        std::thread::spawn(move || {
            let result = (|| -> Result<()> {
                let request = serde_json::from_slice(&read_frame_until(
                    &mut stream,
                    Instant::now() + Duration::from_secs(5),
                )?)?;
                stream.set_nonblocking(true)?;
                coordinator.dispatch(request, |r| write_frame_until(&mut stream, &r))
            })();
            if let Err(error) = result {
                let _ = write_frame_until(&mut stream, &error_response(String::new(), error));
            }
            active.fetch_sub(1, Ordering::SeqCst);
        });
    }
    Ok(())
}
fn error_response(id: String, error: Error) -> Response {
    Response {
        schema_version: 1,
        request_id: id,
        status: "error".into(),
        duration_ms: 0.0,
        corpus_generation: None,
        data: None,
        error: Some(error),
    }
}
fn write_json(output: &mut impl Write, value: &impl serde::Serialize) -> Result<()> {
    let mut bytes = serde_json::to_vec(value)?;
    if bytes.len() > MAX_FRAME {
        return Err(Error::new(
            "resource_exhausted",
            "JSON response exceeds 1 MiB",
        ));
    }
    bytes.push(b'\n');
    output.write_all(&bytes)?;
    output.flush()?;
    Ok(())
}
fn read_line(input: &mut impl BufRead) -> Result<Option<Vec<u8>>> {
    let mut bytes = Vec::new();
    if input
        .take((MAX_FRAME + 1) as u64)
        .read_until(b'\n', &mut bytes)?
        == 0
    {
        return Ok(None);
    }
    if bytes.len() > MAX_FRAME {
        return Err(Error::new("resource_exhausted", "Input exceeds 1 MiB"));
    }
    Ok(Some(bytes))
}
pub fn local(store: Store, input: impl Read, output: impl Write + Send) -> Result<()> {
    local_coordinator(Coordinator::new(store, None), input, output)
}
pub fn local_coordinator(
    coordinator: Coordinator,
    input: impl Read,
    output: impl Write + Send,
) -> Result<()> {
    let output = Arc::new(Mutex::new(output));
    let reconciling = Arc::new(AtomicBool::new(false));
    let mut reader = BufReader::new(input);
    std::thread::scope(|scope| -> Result<()> {
        while let Some(bytes) = read_line(&mut reader)? {
            let request = match serde_json::from_slice::<Request>(&bytes) {
                Ok(r) => r,
                Err(e) => {
                    write_json(
                        &mut *output.lock().unwrap(),
                        &error_response(String::new(), e.into()),
                    )?;
                    continue;
                }
            };
            if request.schema_version == 1
                && matches!(request.operation, Operation::Reconcile { .. })
            {
                if reconciling.swap(true, Ordering::SeqCst) {
                    let mut error = Error::new("busy", "A reconciliation is already running");
                    error.retryable = true;
                    write_json(
                        &mut *output.lock().unwrap(),
                        &error_response(request.request_id, error),
                    )?;
                    continue;
                }
                let coordinator = coordinator.clone();
                let output = output.clone();
                let reconciling = reconciling.clone();
                scope.spawn(move || {
                    let _ = coordinator
                        .dispatch(request, |r| write_json(&mut *output.lock().unwrap(), &r));
                    reconciling.store(false, Ordering::SeqCst);
                });
            } else if request.schema_version == 1
                && let Operation::Answer { request: answer } = request.operation
            {
                match coordinator.start_answer(&request.request_id, answer) {
                    Ok(run) => {
                        let output = output.clone();
                        scope.spawn(move || {
                            let _ = forward(run, |r| write_json(&mut *output.lock().unwrap(), &r));
                        });
                    }
                    Err(error) => write_json(
                        &mut *output.lock().unwrap(),
                        &error_response(request.request_id, error),
                    )?,
                }
            } else {
                coordinator.dispatch(request, |r| write_json(&mut *output.lock().unwrap(), &r))?;
            }
        }
        Ok(())
    })
}
fn forward(mut run: Run, mut write: impl FnMut(Response) -> Result<()>) -> Result<()> {
    while run.forward_next(&mut write)? {}
    Ok(())
}

/// Stdio owned by a supervising client. Never changes flags on a terminal.
/// One serialized frame is one bounded write; failed output remains failed.
pub struct DeadlineStdout {
    stream: std::io::Stdout,
    flags: i32,
    failed: bool,
}
impl DeadlineStdout {
    pub fn new() -> Result<Self> {
        let stream = std::io::stdout();
        let fd = stream.as_raw_fd();
        // SAFETY: inspecting and configuring this process's non-terminal stdout.
        if unsafe { libc::isatty(fd) } == 1 {
            return Err(Error::new(
                "invalid_request",
                "Resident stdio mode requires stdout to be a pipe or file",
            ));
        }
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(Self {
            stream,
            flags,
            failed: false,
        })
    }
}
impl Write for DeadlineStdout {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if self.failed {
            return Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "Output unavailable",
            ));
        }
        match crate::process::write_all_until(
            &mut self.stream,
            bytes,
            Instant::now() + WRITE_TIMEOUT,
        ) {
            Ok(()) => Ok(bytes.len()),
            Err(_) => {
                self.failed = true;
                Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "Output deadline exceeded",
                ))
            }
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
impl Drop for DeadlineStdout {
    fn drop(&mut self) {
        unsafe {
            libc::fcntl(self.stream.as_raw_fd(), libc::F_SETFL, self.flags);
        }
    }
}

trait Endpoint: Send + Sync {
    fn dispatch(
        &self,
        request: Request,
        write: &mut dyn FnMut(Response) -> Result<()>,
    ) -> Result<()>;
}
impl Endpoint for Coordinator {
    fn dispatch(
        &self,
        request: Request,
        write: &mut dyn FnMut(Response) -> Result<()>,
    ) -> Result<()> {
        Coordinator::dispatch(self, request, write)
    }
}
struct Remote(PathBuf);
impl Endpoint for Remote {
    fn dispatch(
        &self,
        request: Request,
        write: &mut dyn FnMut(Response) -> Result<()>,
    ) -> Result<()> {
        call_stream(&self.0, &request, write)
    }
}
struct McpCall {
    canceled: AtomicBool,
    answer: bool,
    run: Mutex<Option<String>>,
    progress: Option<Value>,
}
pub fn mcp(directory: &Path, input: impl Read, output: impl Write + Send) -> Result<()> {
    mcp_endpoint(Arc::new(Remote(directory.into())), input, output)
}
pub fn mcp_coordinator(
    coordinator: Coordinator,
    input: impl Read,
    output: impl Write + Send,
) -> Result<()> {
    mcp_endpoint(Arc::new(coordinator), input, output)
}
fn mcp_endpoint(
    endpoint: Arc<dyn Endpoint>,
    input: impl Read,
    output: impl Write + Send,
) -> Result<()> {
    let output = Arc::new(Mutex::new(output));
    let calls = Arc::new(Mutex::new(BTreeMap::<String, Arc<McpCall>>::new()));
    let mut reader = BufReader::new(input);
    let mut negotiated = false;
    let mut initialized = false;
    std::thread::scope(|scope| -> Result<()> {
        while let Some(bytes) = read_line(&mut reader)? {
            let message: Value = match serde_json::from_slice(&bytes) {
                Ok(m) => m,
                Err(_) => {
                    write_json(
                        &mut *output.lock().unwrap(),
                        &rpc_error(Value::Null, -32700, "Invalid JSON"),
                    )?;
                    continue;
                }
            };
            if message["jsonrpc"] != "2.0" || !message.is_object() {
                write_json(
                    &mut *output.lock().unwrap(),
                    &rpc_error(Value::Null, -32600, "Invalid JSON-RPC request"),
                )?;
                continue;
            }
            let Some(id) = message.get("id").cloned() else {
                match message["method"].as_str() {
                    Some("notifications/initialized") if negotiated => initialized = true,
                    Some("notifications/cancelled") => {
                        let key = message["params"]["requestId"].to_string();
                        let call = calls.lock().unwrap().get(&key).cloned();
                        if let Some(call) = call
                            && call.answer
                        {
                            call.canceled.store(true, Ordering::SeqCst);
                            if let Some(run) = call.run.lock().unwrap().clone() {
                                let _ = endpoint.dispatch(
                                    Request {
                                        schema_version: 1,
                                        request_id: "mcp-cancel".into(),
                                        operation: Operation::Cancel { run_id: run },
                                    },
                                    &mut |_| Ok(()),
                                );
                            }
                        }
                    }
                    _ => {}
                }
                continue;
            };
            if !(id.is_string() || id.is_i64() || id.is_u64()) || id.to_string().len() > 128 {
                write_json(
                    &mut *output.lock().unwrap(),
                    &rpc_error(Value::Null, -32600, "Invalid request ID"),
                )?;
                continue;
            }
            match message["method"].as_str() {
                Some("initialize") => {
                    negotiated = true;
                    let requested = message["params"]["protocolVersion"].as_str().unwrap_or("");
                    let version = if matches!(requested, "2024-11-05" | "2025-03-26" | "2025-06-18")
                    {
                        requested
                    } else {
                        "2025-06-18"
                    };
                    write_json(
                        &mut *output.lock().unwrap(),
                        &json!({"jsonrpc":"2.0","id":id,"result":{"protocolVersion":version,"capabilities":{"tools":{"listChanged":false}},"serverInfo":{"name":"know-things","version":env!("CARGO_PKG_VERSION")}}}),
                    )?;
                }
                Some("ping") => write_json(
                    &mut *output.lock().unwrap(),
                    &json!({"jsonrpc":"2.0","id":id,"result":{}}),
                )?,
                Some("tools/list") if initialized => write_json(
                    &mut *output.lock().unwrap(),
                    &json!({"jsonrpc":"2.0","id":id,"result":{"tools":[{"name":"know_things","description":"Operate on explicitly configured local documentation. Answer events stream through progress metadata when requested; citations identify exact source bytes.","inputSchema":operation_schema()}]}}),
                )?,
                Some("tools/call") if initialized => {
                    if message["params"]["name"] != "know_things" {
                        write_json(
                            &mut *output.lock().unwrap(),
                            &rpc_error(id, -32602, "Unknown tool"),
                        )?;
                        continue;
                    }
                    let operation = match serde_json::from_value::<Operation>(
                        message["params"]["arguments"].clone(),
                    ) {
                        Ok(op) => op,
                        Err(e) => {
                            write_json(
                                &mut *output.lock().unwrap(),
                                &tool_result(
                                    id.clone(),
                                    error_response(format!("mcp-{id}"), e.into()),
                                ),
                            )?;
                            continue;
                        }
                    };
                    let progress = message["params"]["_meta"]["progressToken"].clone();
                    if !progress.is_null()
                        && !(progress.is_string() || progress.is_i64() || progress.is_u64())
                    {
                        write_json(
                            &mut *output.lock().unwrap(),
                            &rpc_error(id, -32602, "Invalid progress token"),
                        )?;
                        continue;
                    }
                    let key = id.to_string();
                    let mut registry = calls.lock().unwrap();
                    if registry.contains_key(&key)
                        || (!progress.is_null()
                            && registry
                                .values()
                                .any(|c| c.progress.as_ref() == Some(&progress)))
                    {
                        write_json(
                            &mut *output.lock().unwrap(),
                            &rpc_error(id, -32600, "Duplicate active request or progress token"),
                        )?;
                        continue;
                    }
                    if registry.len() >= 8 {
                        write_json(
                            &mut *output.lock().unwrap(),
                            &tool_result(
                                id.clone(),
                                error_response(
                                    format!("mcp-{id}"),
                                    Error::new("busy", "MCP request capacity occupied"),
                                ),
                            ),
                        )?;
                        continue;
                    }
                    let call = Arc::new(McpCall {
                        canceled: AtomicBool::new(false),
                        answer: matches!(operation, Operation::Answer { .. }),
                        run: Mutex::new(None),
                        progress: (!progress.is_null()).then_some(progress),
                    });
                    registry.insert(key.clone(), call.clone());
                    drop(registry);
                    let endpoint = endpoint.clone();
                    let calls = calls.clone();
                    let output = output.clone();
                    scope.spawn(move||{
                        let mut terminal=false;
                        let result=endpoint.dispatch(Request{schema_version:1,request_id:format!("mcp-{id}"),operation},&mut|response|{
                            if response.data.as_ref().is_some_and(|d|d["event"]=="accepted") {*call.run.lock().unwrap()=response.data.as_ref().and_then(|d|d["runId"].as_str().map(String::from));}
                            if call.canceled.load(Ordering::SeqCst){return Err(Error::new("canceled","MCP request canceled"));}
                            if response.status=="streaming" {
                                if let Some(token)=&call.progress {
                                    let data=response.data.as_ref().unwrap();let human=match data["event"].as_str(){Some("answer_delta")=>data["text"].as_str().unwrap_or(""),Some("evidence_ready")=>"Evidence ready",_=>"Answer accepted"};
                                    write_json(&mut*output.lock().unwrap(),&json!({"jsonrpc":"2.0","method":"notifications/progress","params":{"progressToken":token,"progress":data["sequence"],"message":human,"_meta":{"io.know-things/answerEvent":data}}}))?;
                                }
                            }else{terminal=true;write_json(&mut*output.lock().unwrap(),&tool_result(id.clone(),response))?;}Ok(())
                        });
                        if let Err(error)=result && !terminal && !call.canceled.load(Ordering::SeqCst){let _=write_json(&mut*output.lock().unwrap(),&tool_result(id.clone(),error_response(format!("mcp-{id}"),error)));}
                        calls.lock().unwrap().remove(&key);
                    });
                }
                Some("tools/list" | "tools/call") => write_json(
                    &mut *output.lock().unwrap(),
                    &rpc_error(id, -32600, "Client must initialize first"),
                )?,
                _ => write_json(
                    &mut *output.lock().unwrap(),
                    &rpc_error(id, -32601, "Method not found"),
                )?,
            }
        }
        Ok(())
    })
}
fn rpc_error(id: Value, code: i32, message: &str) -> Value {
    json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message}})
}
fn tool_result(id: Value, response: Response) -> Value {
    json!({"jsonrpc":"2.0","id":id,"result":{"isError":response.error.is_some(),"content":[{"type":"text","text":serde_json::to_string(&response).expect("serializable response")}]}})
}
pub fn operation_schema() -> Value {
    let mut schema =
        serde_json::to_value(schemars::schema_for!(Operation)).expect("serializable schema");
    schema
        .as_object_mut()
        .unwrap()
        .insert("type".into(), json!("object"));
    schema
}

fn read_frame_until(stream: &mut UnixStream, deadline: Instant) -> Result<Vec<u8>> {
    let mut prefix = [0; 4];
    crate::process::read_all_until(stream, &mut prefix, deadline)?;
    let size = u32::from_be_bytes(prefix) as usize;
    if size == 0 || size > MAX_FRAME {
        return Err(Error::new("invalid_request", "IPC frame exceeds limits"));
    }
    let mut bytes = vec![0; size];
    crate::process::read_all_until(stream, &mut bytes, deadline)?;
    Ok(bytes)
}
