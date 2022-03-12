use std::sync::Arc;

use anyhow::Result;
use async_std::channel::unbounded;
use async_std::task::JoinHandle;
use log::trace;
use uuid::Uuid;
use wasmtime::{ResourceLimiter, Val};

use crate::mailbox::MessageMailbox;
use crate::runtimes::wasmtime::{WasmtimeCompiledModule, WasmtimeRuntime};
use crate::state::ProcessState;
use crate::{Process, Signal, WasmProcess};

/// Spawns a new wasm process from a compiled module.
///
/// A `Process` is created from a `module`, entry `function`, array of arguments and config. The
/// configuration will define some characteristics of the process, such as maximum memory, fuel
/// and host function properties (filesystem access, networking, ..).
///
/// After it's spawned the process will keep running in the background. A process can be killed
/// by sending a `Signal::Kill` to it. If you would like to block until the process is finished
/// you can `.await` on the returned `JoinHandle<()>`.
///
/// Note: The 'a lifetime is here just because Rust has a bug in handling `dyn Trait` in async:
/// https://github.com/rust-lang/rust/issues/63033
/// If it ever becomes an issue there are other workarounds that could be used instead.
pub async fn spawn_wasm<S>(
    runtime: WasmtimeRuntime,
    module: WasmtimeCompiledModule<S>,
    config: Arc<S::Config>,
    function: &str,
    params: Vec<Val>,
    link: Option<(Option<i64>, Arc<dyn Process>)>,
) -> Result<(JoinHandle<()>, Arc<dyn Process>)>
where
    S: ProcessState + Send + ResourceLimiter + 'static,
{
    // TODO: Switch to new_v1() for distributed Lunatic to assure uniqueness across nodes.
    let id = Uuid::new_v4();
    trace!("Spawning process: {}", id);
    let signal_mailbox = unbounded::<Signal>();
    let message_mailbox = MessageMailbox::default();
    let state = S::new(
        id,
        runtime.clone(),
        module.clone(),
        config,
        signal_mailbox.0.clone(),
        message_mailbox.clone(),
    )?;

    let mut instance = runtime.instantiate(&module, state).await?;
    let function = function.to_string();
    let fut = async move { instance.call(&function, params).await };
    let child_process = crate::new(fut, id, signal_mailbox.1, message_mailbox);
    let child_process_handle = WasmProcess::new(id, signal_mailbox.0.clone());

    // **Child link guarantees**:
    // The link signal is going to be put inside of the child's mailbox and is going to be
    // processed before any child code can run. This means that any failure inside the child
    // Wasm code will be correctly reported to the parent.
    //
    // We assume here that the code inside of `process::new()` will not fail during signal
    // handling.
    //
    // **Parent link guarantees**:
    // A `async_std::task::yield_now()` call is executed to allow the parent to link the child
    // before continuing any further execution. This should force the parent to process all
    // signals right away.
    //
    // The parent could have received a `kill` signal in its mailbox before this function was
    // called and this signal is going to be processed before the link is established (FIFO).
    // Only after the yield function we can guarantee that the child is going to be notified
    // if the parent fails. This is ok, as the actual spawning of the child happens after the
    // call, so the child wouldn't even exist if the parent failed before.
    //
    // TODO: The guarantees provided here don't hold anymore in a distributed environment and
    //       will require some rethinking. This function will be executed on a completely
    //       different computer and needs to be synced in a more robust way with the parent
    //       running somewhere else.
    if let Some((tag, process)) = link {
        // Send signal to itself to perform the linking
        process.send(Signal::Link(None, Arc::new(child_process_handle.clone())));
        // Suspend itself to process all new signals
        async_std::task::yield_now().await;
        // Send signal to child to link it
        signal_mailbox
            .0
            .try_send(Signal::Link(tag, process))
            .expect("receiver must exist at this point");
    }

    // Spawn a background process
    trace!("Process size: {}", std::mem::size_of_val(&child_process));
    let join = async_std::task::spawn(child_process);
    Ok((join, Arc::new(child_process_handle)))
}