//! PipeWire integration layer. [`PwContext`] owns the main loop,
//! `Context`, and `Core`.

pub mod command;
pub mod filter;
pub mod metadata;
pub mod registry;
pub mod sink;
pub mod tap;

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use pipewire::{context::Context, core::Core, loop_::Signal, main_loop::MainLoop};

use crate::error::DaemonError;
use crate::pw::registry::RegistryWatcher;
use crate::pw::sink::VirtualSink;
use crate::state::SharedState;

/// block `SIGTERM`/`SIGINT` so PipeWire's signalfd source can receive
/// them instead of the kernel's default (terminate).
///
/// **must be called before any thread is spawned** — threads spawned
/// beforehand keep the unblocked mask and die on signal before the
/// signalfd reads.
///
/// # Errors
/// [`DaemonError::PipeWire`] if `sigprocmask` fails.
pub fn block_termination_signals() -> Result<(), DaemonError> {
    use nix::sys::signal::{SigSet, SigmaskHow};

    let mut set = SigSet::empty();
    set.add(Signal::SIGTERM);
    set.add(Signal::SIGINT);
    nix::sys::signal::sigprocmask(SigmaskHow::SIG_BLOCK, Some(&set), None)
        .map_err(|e| DaemonError::pipewire(format!("sigprocmask: {e}")))?;
    Ok(())
}

/// owns the PipeWire main loop, context, and core for the daemon's run.
/// single-threaded by design.
pub struct PwContext {
    main_loop: MainLoop,
    _context: Context,
    core: Core,
    /// `RefCell` because creation happens after construction (must be
    /// inside the main loop for the roundtrip).
    sink: RefCell<VirtualSink>,
    /// registry watcher + routing engine; `None` until
    /// [`Self::start_routing`].
    routing: RefCell<Option<RegistryWatcher>>,
}

impl PwContext {
    /// init PipeWire, create the main loop/context, connect, and block
    /// termination signals (see [`block_termination_signals`]).
    ///
    /// # Errors
    /// [`DaemonError::PipeWire`] on failure; usually `Context::connect`
    /// when no server is reachable on `$PIPEWIRE_RUNTIME_DIR`.
    pub fn new() -> Result<Self, DaemonError> {
        // idempotent if `runtime::run` already called it.
        block_termination_signals()?;
        pipewire::init();
        let main_loop = MainLoop::new(None)
            .map_err(|e| DaemonError::pipewire(format!("MainLoop::new: {e}")))?;
        let context = Context::new(&main_loop)
            .map_err(|e| DaemonError::pipewire(format!("Context::new: {e}")))?;
        let core = context
            .connect(None)
            .map_err(|e| DaemonError::pipewire(format!("Context::connect: {e}")))?;
        tracing::info!("connected to pipewire");
        Ok(Self {
            main_loop,
            _context: context,
            core,
            sink: RefCell::new(VirtualSink::new()),
            routing: RefCell::new(None),
        })
    }

    /// watch the registry and route new playback streams. idempotent;
    /// calling twice replaces the previous watcher.
    ///
    /// # Errors
    /// [`DaemonError::PipeWire`] if obtaining the registry fails.
    pub fn start_routing(&self, daemon: SharedState) -> Result<(), DaemonError> {
        let registry = self
            .core
            .get_registry()
            .map_err(|e| DaemonError::pipewire(format!("get_registry: {e}")))?;
        let watcher = RegistryWatcher::new(Rc::new(registry), self.core.clone(), daemon);
        *self.routing.borrow_mut() = Some(watcher);
        tracing::info!("registry watcher + routing engine installed");
        Ok(())
    }

    #[must_use]
    pub fn main_loop(&self) -> &MainLoop {
        &self.main_loop
    }

    #[must_use]
    pub fn core(&self) -> &Core {
        &self.core
    }

    /// routing state, if [`Self::start_routing`] ran. lets `runtime`
    /// install the filter-rebuild handles afterwards.
    #[must_use]
    pub fn routing_state(&self) -> Option<Rc<RefCell<crate::pw::registry::RoutingState>>> {
        self.routing.borrow().as_ref().map(|w| w.state().clone())
    }

    /// create `headroom-processed` and roundtrip to confirm. must be
    /// called before [`Self::run_until_signal`].
    ///
    /// # Errors
    /// [`DaemonError::PipeWire`] if `create_object` fails, the
    /// `support.null-audio-sink` factory isn't available, or the
    /// roundtrip times out.
    pub fn create_processed_sink(&self, sample_rate: u32) -> Result<(), DaemonError> {
        self.sink.borrow_mut().create(&self.core, sample_rate)?;
        self.roundtrip()?;
        tracing::info!(sample_rate, "headroom-processed virtual sink created");
        Ok(())
    }

    /// block until all queued requests are acked by the server.
    fn roundtrip(&self) -> Result<(), DaemonError> {
        let done = Rc::new(Cell::new(false));
        let done_cb = done.clone();
        let loop_for_cb = self.main_loop.clone();

        let pending = self
            .core
            .sync(0)
            .map_err(|e| DaemonError::pipewire(format!("core.sync: {e}")))?;

        let _listener = self
            .core
            .add_listener_local()
            .done(move |id, seq| {
                if id == pipewire::core::PW_ID_CORE && seq == pending {
                    done_cb.set(true);
                    loop_for_cb.quit();
                }
            })
            .register();

        while !done.get() {
            self.main_loop.run();
        }
        Ok(())
    }

    /// run the main loop until SIGTERM/SIGINT.
    ///
    /// # Errors
    /// [`DaemonError::PipeWire`] if installing the signal sources fails.
    pub fn run_until_signal(&self) -> Result<(), DaemonError> {
        // SIGTERM: graceful service stop (systemd).
        let ml = self.main_loop.clone();
        let _sig_term = self
            .main_loop
            .loop_()
            .add_signal_local(Signal::SIGTERM, move || {
                tracing::info!("SIGTERM received, shutting down");
                ml.quit();
            });

        // SIGINT: Ctrl-C in foreground.
        let ml = self.main_loop.clone();
        let _sig_int = self
            .main_loop
            .loop_()
            .add_signal_local(Signal::SIGINT, move || {
                tracing::info!("SIGINT received, shutting down");
                ml.quit();
            });

        // drain IPC → PipeWire commands at 50 ms. operator-grade only;
        // NOT for spike-reactive gain reduction (Layer A) — see
        // `pw::command` module docs.
        let _cmd_timer = {
            let routing = self.routing.borrow();
            routing.as_ref().map(|watcher| {
                let state = watcher.state().clone();
                let back = state.clone();
                let timer = self.main_loop.loop_().add_timer(move |_expirations| {
                    state.borrow_mut().drain_pw_commands(&back);
                });
                let _ = timer.update_timer(
                    Some(Duration::from_millis(50)),
                    Some(Duration::from_millis(50)),
                );
                timer
            })
        };

        // drain Layer A measurement rings + issue `Props.channelVolumes`
        // writes. 5 ms keeps detection-to-write inside one quantum at
        // typical 21 ms quanta
        let _layer_a_timer = {
            let routing = self.routing.borrow();
            routing.as_ref().map(|watcher| {
                let state = watcher.state().clone();
                let back = state.clone();
                let timer = self.main_loop.loop_().add_timer(move |_expirations| {
                    state.borrow_mut().drain_layer_a(&back);
                });
                let _ = timer.update_timer(
                    Some(Duration::from_millis(5)),
                    Some(Duration::from_millis(5)),
                );
                timer
            })
        };

        tracing::info!("entering pipewire main loop");
        self.main_loop.run();
        tracing::info!("main loop exited");

        // graceful shutdown: restore every attenuated app's volume,
        // then pump the loop a few times so the writes flush before
        // the connection tears down. best-effort (no SIGKILL).
        if let Some(watcher) = self.routing.borrow().as_ref() {
            watcher.state().borrow().restore_all_managed_volumes();
        }
        for _ in 0..10 {
            self.main_loop.loop_().iterate(Duration::from_millis(5));
        }
        Ok(())
    }
}
