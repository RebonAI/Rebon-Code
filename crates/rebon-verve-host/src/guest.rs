//! What runs on the isolate's thread: a Verve runtime with the host's entry
//! loaded, and the transport the entry reads and writes through.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::path::Path;
use std::rc::Rc;
use std::time::Duration;

use verve::boa_engine::builtins::promise::PromiseState;
use verve::boa_engine::object::builtins::{JsFunction, JsPromise, JsUint8Array};
use verve::boa_engine::object::ObjectInitializer;
use verve::boa_engine::property::Attribute;
use verve::boa_engine::{Context, JsNativeError, JsResult, JsString, JsValue, NativeFunction};
use verve::isolate::{Guest, InterruptHandle, Outbox};
use verve::Runtime;

use crate::VerveHostSpec;

/// The entry module, loaded beside the host's `cli.mjs`.
const ENTRY: &str = include_str!("entry.mjs");
const ENTRY_NAME: &str = "verve-entry.mjs";

/// What the isolate tells the embedder.
pub(crate) enum HostEvent {
    Stdout(Vec<u8>),
    Stderr(Vec<u8>),
    Exit(i32),
}

/// The pipes, as the isolate's thread sees them. Kept in the context's host
/// data, where the transport's natives find it.
#[derive(Default)]
struct Transport {
    /// Chunks of stdin not yet read; `None` is its end, and stays once seen.
    input: RefCell<VecDeque<Option<Vec<u8>>>>,
    /// The `resolve` of a `read()` waiting for input.
    waiting: RefCell<Option<JsFunction>>,
    events: RefCell<Vec<HostEvent>>,
    exited: Cell<bool>,
}

pub(crate) struct HostGuest {
    runtime: Runtime,
    transport: Rc<Transport>,
    /// The entry's evaluation: a rejection here is a host that never started
    /// its loop (its imports failed), and has to be said and ended.
    entry: Option<JsPromise>,
}

impl HostGuest {
    /// Makes the runtime and starts the entry. Never fails: a host that cannot
    /// start says why on stderr and exits 2, as Node would.
    pub(crate) fn start(spec: &VerveHostSpec) -> Self {
        let mut runtime = Runtime::new();
        let transport = Rc::new(Transport::default());
        runtime.context().insert_data(Rc::clone(&transport));
        let mut guest = Self {
            runtime,
            transport,
            entry: None,
        };
        if let Err(message) = guest.boot(spec) {
            guest.fail(&message);
        }
        guest
    }

    fn boot(&mut self, spec: &VerveHostSpec) -> Result<(), String> {
        let entry = spec.host_script.with_file_name(ENTRY_NAME);
        let script = url::Url::from_file_path(&spec.host_script)
            .map_err(|()| format!("{} is not an absolute path", spec.host_script.display()))?;
        let source = ENTRY.replacen(
            "'HOST_SCRIPT'",
            &serde_json::Value::from(script.as_str()).to_string(),
            1,
        );
        self.runtime.set_permissions(spec.permissions.clone());
        self.runtime
            .set_worker_permissions(spec.permissions.clone());
        self.runtime.eval(&process_setup(spec, &entry))?;
        install_transport(self.runtime.context()).map_err(|error| error.to_string())?;
        let promise = self.runtime.load_esm(&entry, &source)?;
        self.entry = Some(promise);
        self.check_entry();
        Ok(())
    }

    /// Ends a host that could not run: the reason on stderr, exit code 2.
    fn fail(&self, message: &str) {
        if self.transport.exited.replace(true) {
            return;
        }
        let line = serde_json::json!({
            "level": "error",
            "code": "host_failure",
            "message": message.chars().take(512).collect::<String>(),
        });
        let mut events = self.transport.events.borrow_mut();
        events.push(HostEvent::Stderr(format!("{line}\n").into_bytes()));
        events.push(HostEvent::Exit(2));
    }

    /// A rejected entry is a host whose loop never ran.
    fn check_entry(&mut self) {
        let Some(entry) = &self.entry else { return };
        if let PromiseState::Rejected(reason) = entry.state() {
            self.entry = None;
            let message = verve::describe(
                self.runtime.context(),
                &verve::boa_engine::JsError::from_opaque(reason),
            );
            self.fail(&format!("the Verve host entry failed: {message}"));
        }
    }

    /// A host that can never move again ends, as a Node process does when its
    /// event loop empties: its entry has not finished, yet nothing could wake
    /// it — no read waiting on stdin, no timer, no worker, no socket. That is
    /// a host whose loop is waiting on a plugin's promise nobody will settle,
    /// typically during a shutdown; Node exits it with 13, for an unsettled
    /// top-level await, and so does this rather than wait to be killed.
    fn check_stalled(&mut self) {
        let Some(entry) = &self.entry else { return };
        if !matches!(entry.state(), PromiseState::Pending)
            || self.transport.exited.get()
            || self.transport.waiting.borrow().is_some()
            || !self.transport.input.borrow().is_empty()
            || self.runtime.next_due().is_some()
        {
            return;
        }
        self.entry = None;
        self.transport.exited.set(true);
        let line = serde_json::json!({
            "level": "warn",
            "code": "unsettled_top_level_await",
            "message": "the host has nothing left that could wake it: \
                        a promise it waits on will never settle",
        });
        let mut events = self.transport.events.borrow_mut();
        events.push(HostEvent::Stderr(format!("{line}\n").into_bytes()));
        events.push(HostEvent::Exit(13));
    }

    /// A chunk of stdin, or its end: answers a waiting `read()` or keeps it.
    pub(crate) fn input(&mut self, chunk: Option<Vec<u8>>) {
        let waiting = self.transport.waiting.borrow_mut().take();
        let Some(resolve) = waiting else {
            self.transport.input.borrow_mut().push_back(chunk);
            return;
        };
        if chunk.is_none() {
            // The end stays seen for any later read.
            self.transport.input.borrow_mut().push_back(None);
        }
        let context = self.runtime.context();
        let value = match chunk {
            Some(bytes) => match JsUint8Array::from_iter(bytes, context) {
                Ok(array) => array.into(),
                Err(error) => {
                    let message = error.to_string();
                    self.fail(&format!("cannot hand stdin to the host: {message}"));
                    return;
                }
            },
            None => JsValue::null(),
        };
        if let Err(error) = resolve.call(&JsValue::undefined(), &[value], context) {
            verve::report(context, &error);
        }
        self.runtime.run_microtasks();
    }
}

impl Guest for HostGuest {
    type View = ();
    type Event = HostEvent;

    fn tick(&mut self) {
        // What the network has for the realm first: a response's chunk is
        // what a timer or a stream waiting on it acts on.
        self.runtime.run_network_until(None);
        self.runtime.run_due();
        self.runtime.run_workers();
        self.runtime.run_microtasks();
        self.check_entry();
        self.check_stalled();
    }

    fn next_due(&self) -> Option<Duration> {
        let now = self.runtime.now();
        self.runtime
            .next_due()
            .map(|at| Duration::from_secs_f64((at - now).max(0.) / 1000.))
    }

    fn drain(&mut self, out: &mut Outbox<HostEvent>) {
        // The console is diagnostics here: stdout is the protocol channel.
        for line in self.runtime.take_console() {
            let mut bytes = line.text.into_bytes();
            bytes.push(b'\n');
            out.push(HostEvent::Stderr(bytes));
        }
        for event in self.transport.events.borrow_mut().drain(..) {
            out.push(event);
        }
    }

    fn publish(&mut self, _: u64, _: bool) -> Option<()> {
        None
    }

    fn interrupt_handle(&mut self) -> Option<InterruptHandle> {
        Some(self.runtime.interrupt_handle())
    }
}

/// `process.cwd()`, `process.argv` as Node would have it for the host's own
/// arguments, and `process.env`.
fn process_setup(spec: &VerveHostSpec, entry: &Path) -> String {
    let text = |path: &Path| serde_json::Value::from(path.to_string_lossy().into_owned());
    let mut argv = vec![serde_json::Value::from("verve"), text(entry)];
    argv.extend(spec.host_args.iter().cloned().map(serde_json::Value::from));
    let mut setup = format!(
        "process.chdir({});\nprocess.argv = {};\n",
        text(&spec.working_directory),
        serde_json::Value::from(argv),
    );
    if let Some(vars) = &spec.environment {
        let env: serde_json::Map<String, serde_json::Value> = vars
            .iter()
            .map(|(key, value)| {
                (
                    key.to_string_lossy().into_owned(),
                    serde_json::Value::from(value.to_string_lossy().into_owned()),
                )
            })
            .collect();
        setup.push_str(&format!(
            "process.env = {};\n",
            serde_json::Value::from(env)
        ));
    }
    setup
}

fn transport(context: &Context) -> JsResult<Rc<Transport>> {
    context
        .get_data::<Rc<Transport>>()
        .cloned()
        .ok_or_else(|| JsNativeError::error().with_message("no transport").into())
}

/// `globalThis.__rebonVerveTransport`: what the entry reads and writes.
fn install_transport(context: &mut Context) -> JsResult<()> {
    let object = ObjectInitializer::new(context)
        .function(NativeFunction::from_fn_ptr(read), JsString::from("read"), 0)
        .function(
            NativeFunction::from_fn_ptr(write),
            JsString::from("write"),
            1,
        )
        .function(
            NativeFunction::from_fn_ptr(diagnostic),
            JsString::from("diagnostic"),
            1,
        )
        .function(NativeFunction::from_fn_ptr(exit), JsString::from("exit"), 1)
        .build();
    context.register_global_property(
        JsString::from("__rebonVerveTransport"),
        object,
        Attribute::all(),
    )
}

/// `read()`: the next chunk of stdin, or `null` at its end.
fn read(_: &JsValue, _: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let transport = transport(context)?;
    let next = transport.input.borrow_mut().pop_front();
    match next {
        Some(Some(bytes)) => {
            let array = JsUint8Array::from_iter(bytes, context)?;
            Ok(JsPromise::resolve(array, context)?.into())
        }
        Some(None) => {
            transport.input.borrow_mut().push_front(None);
            Ok(JsPromise::resolve(JsValue::null(), context)?.into())
        }
        None => {
            if transport.waiting.borrow().is_some() {
                return Err(JsNativeError::error()
                    .with_message("stdin is already being read")
                    .into());
            }
            let (promise, resolvers) = JsPromise::new_pending(context);
            *transport.waiting.borrow_mut() = Some(resolvers.resolve);
            Ok(promise.into())
        }
    }
}

/// `write(bytes)`: protocol output.
fn write(_: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let value = args.first().cloned().unwrap_or_default();
    let bytes = match value.as_object() {
        Some(object) => JsUint8Array::from_object(object.clone())?.to_vec(context)?,
        None if value.is_string() => value
            .to_string(context)?
            .to_std_string_escaped()
            .into_bytes(),
        None => {
            return Err(JsNativeError::typ()
                .with_message("write takes bytes or a string")
                .into())
        }
    };
    transport(context)?
        .events
        .borrow_mut()
        .push(HostEvent::Stdout(bytes));
    Ok(JsValue::undefined())
}

/// `diagnostic(text)`: a line for stderr, as the host formats it.
fn diagnostic(_: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let text = args
        .first()
        .cloned()
        .unwrap_or_default()
        .to_string(context)?
        .to_std_string_escaped();
    transport(context)?
        .events
        .borrow_mut()
        .push(HostEvent::Stderr(text.into_bytes()));
    Ok(JsValue::undefined())
}

/// `exit(code)`: the host's loop returned.
fn exit(_: &JsValue, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let code = args.first().cloned().unwrap_or_default().to_i32(context)?;
    let transport = transport(context)?;
    if !transport.exited.replace(true) {
        transport.events.borrow_mut().push(HostEvent::Exit(code));
    }
    Ok(JsValue::undefined())
}
